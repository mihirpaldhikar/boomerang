/*
 * Copyright (c) Mihir Paldhikar
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */
use crate::DatabaseError;
use crate::core::{Action, DatabaseSchema, DatabaseSchemaBuilder, Ident, Lit, RawForeignKey};
use futures::TryStreamExt;
use sqlx::AssertSqlSafe;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;

const DATABASE_SCHEMA_QUERY: &'static str = include_str!("../../../../sql/database_schema.sql");

pub struct PostgresDatabase {
    pool: sqlx::Pool<sqlx::Postgres>,
    schema: Arc<DatabaseSchema>,
}

impl Clone for PostgresDatabase {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            schema: Arc::clone(&self.schema),
        }
    }
}

impl PostgresDatabase {
    pub async fn new(connection_url: &str) -> Result<Self, DatabaseError> {
        let pool = PgPoolOptions::new()
            .connect(connection_url)
            .await
            .map_err(|_| DatabaseError::ConnectionPoolFailed)?;

        let mut tx = pool
            .begin()
            .await
            .map_err(|_| DatabaseError::FailedToAcquireTransaction)?;

        Self::perform_setup(&mut tx).await?;

        let schema = Self::load_schema(&mut tx).await?;

        sqlx::query("SET LOCAL boomerang.sync_write = 'true'")
            .execute(&mut *tx)
            .await
            .map_err(|err| DatabaseError::SqlxError(err))?;

        Self::backfill_sync_logs(&schema, &mut tx).await?;

        Self::setup_triggers(&schema, &mut tx).await?;

        tx.commit()
            .await
            .map_err(|_| DatabaseError::FailedToCommitTransaction)?;

        Ok(Self {
            pool,
            schema: Arc::new(schema),
        })
    }

    pub fn schema(&self) -> &DatabaseSchema {
        &self.schema
    }
}

impl PostgresDatabase {
    async fn perform_setup(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DatabaseError> {
        match sqlx::query_scalar::<_, bool>(
            r#"
            SELECT count(p.proname) = 1
            FROM pg_catalog.pg_namespace n
                     JOIN
                 pg_catalog.pg_proc p ON
                     p.pronamespace = n.oid
            WHERE p.prokind = 'f'
              AND n.nspname = 'boomerang'
              AND p.proname = 'database_schema';
        "#,
        )
        .fetch_one(&mut **tx)
        .await
        {
            Ok(has_database_schema) => {
                if !has_database_schema
                    && let Err(err) = sqlx::raw_sql(DATABASE_SCHEMA_QUERY)
                        .execute(&mut **tx)
                        .await
                {
                    return Err(DatabaseError::SetupFailed {
                        step: "creating database schema functions",
                        reason: err.to_string(),
                    });
                }
            }
            Err(err) => {
                return Err(DatabaseError::SetupFailed {
                    step: "checking for database schema functions",
                    reason: err.to_string(),
                });
            }
        }

        if let Err(r) = sqlx::raw_sql(
            r#"
            CREATE TABLE IF NOT EXISTS boomerang.sync_log
            (
                table_name text COLLATE "C" NOT NULL,
                record_id  text COLLATE "C" NOT NULL,
                op_type    char(1)          NOT NULL CHECK (op_type IN ('I', 'U', 'D')),
                version    bigint           NOT NULL DEFAULT 1,
                updated_at bigint           NOT NULL,
                CONSTRAINT sync_log_pkey PRIMARY KEY (table_name, record_id)
            ) WITH (fillfactor = 70);
        "#,
        )
        .execute(&mut **tx)
        .await
        {
            return Err(DatabaseError::SetupFailed {
                step: "creating sync log table",
                reason: r.to_string(),
            });
        }

        Ok(())
    }

    async fn load_schema(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<DatabaseSchema, DatabaseError> {
        let mut builder = DatabaseSchemaBuilder::with_capacity(200, 2000);

        {
            let mut rows = sqlx::query(
                "SELECT * FROM boomerang.database_schema('public') ORDER BY table_name, attnum;",
            )
            .fetch(&mut **tx);

            while let Some(row) = rows
                .try_next()
                .await
                .map_err(|err| DatabaseError::SqlxError(err))?
            {
                use sqlx::Row;

                let table_name: &str = row
                    .try_get("table_name")
                    .map_err(|err| DatabaseError::SqlxError(err))?;
                let column_name: &str = row
                    .try_get("column_name")
                    .map_err(|err| DatabaseError::SqlxError(err))?;

                let fk_constraints: Vec<String> = row.try_get("fk_constraints").unwrap_or_default();
                let fk_namespaces: Vec<String> = row.try_get("fk_namespaces").unwrap_or_default();
                let fk_tables: Vec<String> = row.try_get("fk_tables").unwrap_or_default();
                let fk_columns: Vec<String> = row.try_get("fk_columns").unwrap_or_default();
                let fk_on_update: Vec<i16> = row.try_get("fk_on_update").unwrap_or_default();
                let fk_on_delete: Vec<i16> = row.try_get("fk_on_delete").unwrap_or_default();
                let check_defs: Vec<String> = row.try_get("check_defs").unwrap_or_default();
                let domain_checks: Vec<String> = row.try_get("domain_checks").unwrap_or_default();

                let foreign_keys_iter = fk_constraints
                    .iter()
                    .zip(&fk_namespaces)
                    .zip(&fk_tables)
                    .zip(&fk_columns)
                    .zip(&fk_on_update)
                    .zip(&fk_on_delete)
                    .map(|(((((c, n), t), col), upd), del)| RawForeignKey {
                        constraint_name: c,
                        namespace: n,
                        referenced_table: t,
                        referenced_column: col,
                        on_update: Action::from(*upd),
                        on_delete: Action::from(*del),
                    });

                builder.add_row(
                    table_name,
                    column_name,
                    row.try_get("data_type")
                        .map_err(|err| DatabaseError::SqlxError(err))?,
                    row.try_get("nullable")
                        .map_err(|err| DatabaseError::SqlxError(err))?,
                    row.try_get("is_primary")
                        .map_err(|err| DatabaseError::SqlxError(err))?,
                    row.try_get("is_unique")
                        .map_err(|err| DatabaseError::SqlxError(err))?,
                    row.try_get("default_expr").unwrap_or(None),
                    foreign_keys_iter,
                    check_defs.iter().map(String::as_str),
                    domain_checks.iter().map(String::as_str),
                );
            }
        }

        let schema_hash: [u8; 32] = match sqlx::query_scalar(
            r#"
            SELECT sha256(convert_to(coalesce(string_agg(r.j, E'\n' ORDER BY r.table_name, r.ord), ''), 'UTF8'))
            FROM (
                SELECT s.table_name,
                       row_number() OVER (PARTITION BY s.table_name ORDER BY s.attnum) AS ord,
                       (to_jsonb(s) - 'attnum')::text AS j
                FROM boomerang.database_schema('public') s
            ) r"#,
        )
            .fetch_one(&mut **tx)
            .await
        {
            Ok(hash) => hash,
            Err(_) => return Err(DatabaseError::SchemaHashComputationFailed),
        };

        Ok(builder.build(schema_hash))
    }

    async fn backfill_sync_logs(
        schema: &DatabaseSchema,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DatabaseError> {
        let log_row_count: i64 = sqlx::query_scalar("SELECT count(*) FROM boomerang.sync_log")
            .fetch_one(&mut **tx)
            .await
            .map_err(|err| DatabaseError::SqlxError(err))?;

        if log_row_count > 0 {
            return Ok(());
        }

        for table in schema.tables() {
            let table_name = table.name();

            let primary_col = match table.primary_key() {
                Some(col) => col.name(),
                None => {
                    return Err(DatabaseError::BackfillFailed {
                        table: table_name.to_string(),
                        reason: "Table has no primary key",
                    });
                }
            };

            sqlx::query(AssertSqlSafe(format!(
                r#"
                    INSERT INTO boomerang.sync_log (table_name, record_id, op_type, version, updated_at)
                    SELECT $1, {pk}::text, 'I', 1, (extract(epoch from now()) * 1000)::bigint
                    FROM {schema}.{table}
                    ON CONFLICT (table_name, record_id) DO NOTHING
                "#,
                pk = Ident(primary_col),
                schema = Ident("public"),
                table = Ident(table_name),
            )))
                .bind(format!("public.{table_name}"))
                .execute(&mut **tx)
                .await
                .map_err(|err| DatabaseError::SqlxError(err))?;
        }

        Ok(())
    }

    async fn setup_triggers(
        schema: &DatabaseSchema,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DatabaseError> {
        for table in schema.tables() {
            let table_name = table.name();

            let primary_col = match table.primary_key() {
                Some(col) => col.name(),
                None => {
                    return Err(DatabaseError::SetupFailed {
                        step: "creating table trigger",
                        reason: format!("Table {} has no primary key", table_name),
                    });
                }
            };

            sqlx::raw_sql(AssertSqlSafe(format!(
                r#"
                    CREATE OR REPLACE FUNCTION {func}() RETURNS trigger
                    LANGUAGE plpgsql AS $$
                    BEGIN
                        IF current_setting('boomerang.sync_write', true) = 'true' THEN
                            RETURN NULL;
                        END IF;

                        INSERT INTO boomerang.sync_log AS l (table_name, record_id, op_type, version, updated_at)
                        VALUES (
                            {name},
                            (CASE WHEN TG_OP = 'DELETE' THEN OLD.{pk} ELSE NEW.{pk} END)::text,
                            left(TG_OP, 1),
                            1,
                            (extract(epoch FROM clock_timestamp()) * 1000)::bigint
                        )
                        ON CONFLICT (table_name, record_id) DO UPDATE
                        SET op_type    = EXCLUDED.op_type,
                            version    = l.version + 1,
                            updated_at = EXCLUDED.updated_at;

                        RETURN NULL;
                    END $$;

                    CREATE OR REPLACE TRIGGER {trg}
                    AFTER INSERT OR UPDATE OR DELETE ON {table}
                    FOR EACH ROW EXECUTE FUNCTION {func}();
                 "#,
                func = Ident(&format!("sync_log_touch_{table_name}")),
                trg = Ident(&format!("trg_{table_name}_sync_log")),
                name = Lit(&format!("public.{table_name}")),
                table = Ident(table_name),
                pk = Ident(primary_col),
            )))
                .execute(&mut **tx)
                .await
                .map_err(|err| DatabaseError::SqlxError(err))?;
        }

        Ok(())
    }
}
