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
use crate::core::{Action, DatabaseSchema, DatabaseSchemaBuilder, RawForeignKey};
use futures::TryStreamExt;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;

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
    // TODO: Must return Result.
    pub async fn new(connection_url: &str) -> Self {
        let pool = PgPoolOptions::new().connect(connection_url).await.unwrap();
        let schema = Self::load_schema(pool.clone()).await.unwrap();

        Self {
            pool,
            schema: Arc::new(schema),
        }
    }

    pub fn schema(&self) -> &DatabaseSchema {
        &self.schema
    }
}

impl PostgresDatabase {
    async fn load_schema(pool: sqlx::Pool<sqlx::Postgres>) -> Result<DatabaseSchema, sqlx::Error> {
        let mut rows =
            sqlx::query("SELECT * FROM database_schema('public') ORDER BY table_name, attnum;")
                .fetch(&pool);

        let mut builder = DatabaseSchemaBuilder::with_capacity(200, 2000);

        while let Some(row) = rows.try_next().await? {
            use sqlx::Row;

            let table_name: &str = row.try_get("table_name")?;
            let column_name: &str = row.try_get("column_name")?;

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
                row.try_get("data_type")?,
                row.try_get("nullable")?,
                row.try_get("is_primary")?,
                row.try_get("is_unique")?,
                row.try_get("default_expr").unwrap_or(None),
                foreign_keys_iter,
                check_defs.iter().map(String::as_str),
                domain_checks.iter().map(String::as_str),
            );
        }

        // TODO: Figure out how to handle schema hash.
        Ok(builder.build([0u8; 32]))
    }
}
