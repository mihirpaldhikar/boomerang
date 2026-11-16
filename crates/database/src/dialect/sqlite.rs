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

use crate::core::{ColumnRef, DatabaseSchema, Ident, IndexRef, Lit, TableRef, ValueType};
use crate::dialect::postgres::{CheckOpts, translate_check};
use crate::{DatabaseError, Symbol};
use hashbrown::HashMap;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{AssertSqlSafe, Pool, Sqlite};
use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;

const META_DDL: &str = "CREATE TABLE IF NOT EXISTS __meta__ (\
    key TEXT NOT NULL PRIMARY KEY,\
    value BLOB NOT NULL\
) WITHOUT ROWID;\n";

const SYNC_LOG_DDL: &str = "CREATE TABLE IF NOT EXISTS __sync_log__ (\
    table_name  TEXT NOT NULL,\
    record_id   TEXT NOT NULL,\
    op_type     TEXT NOT NULL CHECK (op_type IN ('I','U','D')),\
    version     INTEGER NOT NULL DEFAULT 1,\
    updated_at  INTEGER NOT NULL,\
    PRIMARY KEY (table_name, record_id)\
) WITHOUT ROWID, STRICT;\n";

const CHECK_OPTS: CheckOpts = CheckOpts {
    allow_regexp: false,
};

pub struct SqliteDatabase<'a> {
    pool: Pool<Sqlite>,
    schema: &'a DatabaseSchema,
    _database_file: Option<Arc<NamedTempFile>>,
}

impl Clone for SqliteDatabase<'_> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            schema: self.schema,
            _database_file: self._database_file.clone(),
        }
    }
}

impl Drop for SqliteDatabase<'_> {
    fn drop(&mut self) {
        let Some(file) = self._database_file.take() else {
            return;
        };

        let Some(file) = Arc::into_inner(file) else {
            return;
        };

        Self::remove_sidecar_files(file.path());
        if let Err(err) = file.close() {
            eprintln!("failed to remove sqlite temp file: {err}");
        }
    }
}

impl<'a> SqliteDatabase<'a> {
    pub async fn new(ref_schema: &'a DatabaseSchema) -> Result<Self, DatabaseError> {
        let database_file =
            NamedTempFile::new().map_err(|_| DatabaseError::ConnectionPoolFailed)?;

        let pool = Self::pool_options()
            .connect_with(Self::performance_options(database_file.path()))
            .await
            .map_err(|_| DatabaseError::ConnectionPoolFailed)?;

        let instance = Self {
            pool,
            schema: ref_schema,
            _database_file: Some(Arc::new(database_file)),
        };

        instance.create_tables().await?;

        Ok(instance)
    }

    pub async fn from(ref_schema: &'a DatabaseSchema, bytes: &[u8]) -> Result<Self, DatabaseError> {
        if bytes.len() < 100 || &bytes[0..16] != b"SQLite format 3\0" {
            return Err(DatabaseError::InvalidSqliteBytes);
        }

        let mut database_file =
            NamedTempFile::new().map_err(|_| DatabaseError::ConnectionPoolFailed)?;

        {
            use std::io::Write;
            database_file
                .write_all(bytes)
                .and_then(|_| database_file.flush())
                .map_err(|_| DatabaseError::InvalidSqliteBytes)?;
        }

        let pool = Self::pool_options()
            .connect_with(Self::performance_options(database_file.path()))
            .await
            .map_err(|_| DatabaseError::ConnectionPoolFailed)?;

        let _ = sqlx::raw_sql("PRAGMA analysis_limit=400; PRAGMA optimize;")
            .execute(&pool)
            .await;

        let instance = Self {
            pool,
            schema: ref_schema,
            _database_file: Some(Arc::new(database_file)),
        };

        instance.drop_triggers().await?;

        match instance.stored_schema_hash().await {
            Some(stored) if &stored == ref_schema.hash() => {}
            Some(_) => instance.alter_tables().await?,
            None => instance.create_tables().await?,
        }

        Ok(instance)
    }

    pub async fn bytes(mut self) -> Result<Vec<u8>, DatabaseError> {
        self.setup_triggers().await?;

        let (busy, _log, _checkpointed): (i64, i64, i64) =
            sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
                .fetch_one(&self.pool)
                .await
                .map_err(|err| DatabaseError::SetupFailed {
                    step: "checkpointing sqlite database",
                    reason: err.to_string(),
                })?;

        if busy != 0 {
            return Err(DatabaseError::SetupFailed {
                step: "checkpointing sqlite database",
                reason: "checkpoint could not complete (database busy)".into(),
            });
        }

        self.pool.close().await;

        let Some(file) = self._database_file.take() else {
            return Err(DatabaseError::SetupFailed {
                step: "exporting sqlite database",
                reason: "temp file already taken".into(),
            });
        };

        let bytes = std::fs::read(file.path()).map_err(|err| DatabaseError::SetupFailed {
            step: "exporting sqlite database",
            reason: err.to_string(),
        })?;

        if let Some(file) = Arc::into_inner(file) {
            Self::remove_sidecar_files(file.path());
            if let Err(err) = file.close() {
                eprintln!("failed to remove sqlite temp file: {err}");
            }
        }

        Ok(bytes)
    }

    pub async fn close(mut self) {
        self.pool.close().await;

        if let Some(file) = self._database_file.take()
            && let Some(file) = Arc::into_inner(file)
        {
            Self::remove_sidecar_files(file.path());
            if let Err(err) = file.close() {
                eprintln!("failed to remove sqlite temp file: {err}");
            }
        }
    }
}

impl<'a> SqliteDatabase<'a> {
    fn performance_options(path: &std::path::Path) -> SqliteConnectOptions {
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Off)
            .foreign_keys(false)
            .busy_timeout(Duration::from_secs(5))
            .pragma("cache_size", "-65536")
            .pragma("mmap_size", "268435456")
            .pragma("temp_store", "MEMORY")
            .pragma("wal_autocheckpoint", "4096")
            .pragma("journal_size_limit", "67108864")
            .statement_cache_capacity(256)
    }

    fn pool_options() -> SqlitePoolOptions {
        let readers = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(4)
            .clamp(2, 16);

        SqlitePoolOptions::new()
            .max_connections(readers)
            .min_connections(1)
            .test_before_acquire(false)
            .acquire_timeout(Duration::from_secs(10))
    }

    fn remove_sidecar_files(db_path: &std::path::Path) {
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = db_path.as_os_str().to_owned();
            sidecar.push(suffix);
            let sidecar = std::path::PathBuf::from(sidecar);
            if sidecar.exists() {
                let _ = std::fs::remove_file(sidecar);
            }
        }
    }

    async fn create_tables(&self) -> Result<(), DatabaseError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| DatabaseError::FailedToAcquireTransaction)?;

        sqlx::raw_sql(AssertSqlSafe(Self::build_ddl(self.schema)))
            .execute(&mut *tx)
            .await
            .map_err(|err| DatabaseError::SetupFailed {
                step: "creating sqlite tables",
                reason: err.to_string(),
            })?;

        tx.commit()
            .await
            .map_err(|_| DatabaseError::FailedToCommitTransaction)
    }

    async fn alter_tables(&self) -> Result<(), DatabaseError> {
        use sqlx::Row;

        let rows = sqlx::query(
            "SELECT m.name, p.name, p.type, p.pk \
             FROM sqlite_master AS m JOIN pragma_table_info(m.name) AS p \
             WHERE m.type = 'table' \
               AND m.name NOT LIKE 'sqlite_%' \
               AND m.name NOT IN ('__sync_log__', '__meta__')",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| DatabaseError::SetupFailed {
            step: "introspecting sqlite schema",
            reason: err.to_string(),
        })?;

        let mut existing: HashMap<&str, HashMap<&str, (&str, bool)>> =
            HashMap::with_capacity(self.schema.len());
        for row in &rows {
            let table: &str = row.try_get(0).map_err(DatabaseError::SqlxError)?;
            let column: &str = row.try_get(1).map_err(DatabaseError::SqlxError)?;
            let ty: &str = row.try_get(2).map_err(DatabaseError::SqlxError)?;
            let pk: i64 = row.try_get(3).map_err(DatabaseError::SqlxError)?;
            existing
                .entry(table)
                .or_default()
                .insert(column, (ty, pk != 0));
        }
        let live_tables: Vec<&str> = existing.keys().copied().collect();

        let stale_indexes: Vec<String> = sqlx::query_scalar(
            r#"SELECT name FROM sqlite_master
       WHERE type = 'index'
         AND sql IS NOT NULL
         AND name NOT LIKE 'sqlite\_%' ESCAPE '\'
         AND name NOT LIKE '\_\_uniq\_%' ESCAPE '\'
         AND tbl_name NOT IN ('__sync_log__', '__meta__')"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| DatabaseError::SetupFailed {
            step: "listing sqlite indexes",
            reason: err.to_string(),
        })?;

        let schema = self.schema;
        let mut sql = String::with_capacity(512 + schema.columns.len() * 24);

        for name in &stale_indexes {
            let _ = writeln!(sql, "DROP INDEX IF EXISTS {};", Ident(name));
        }

        for table in schema.tables() {
            let table_name = table.name();

            let Some(mut live_cols) = existing.remove(table_name) else {
                Self::write_create_table(&mut sql, schema, table);
                continue;
            };

            let mut rebuild = false;
            let mut added: Vec<ColumnRef<'_>> = Vec::new();

            for column in table.columns() {
                match live_cols.remove(column.name()) {
                    Some((ty, pk)) => {
                        let want = Self::sqlite_type(column.data_type());
                        if !ty.eq_ignore_ascii_case(want) || pk != column.is_primary_key() {
                            rebuild = true;
                            break;
                        }
                    }
                    None if column.is_primary_key() => {
                        rebuild = true;
                        break;
                    }
                    None => added.push(column),
                }
            }
            rebuild = rebuild || live_cols.values().any(|&(_, pk)| pk);

            if rebuild {
                let _ = writeln!(sql, "DROP TABLE IF EXISTS {};", Ident(table_name));
                Self::write_create_table(&mut sql, schema, table);
                continue;
            }

            for column in added {
                let _ = write!(
                    sql,
                    "ALTER TABLE {} ADD COLUMN {} {}",
                    Ident(table_name),
                    Ident(column.name()),
                    Self::sqlite_type(column.data_type()),
                );

                if let Some(default) = column
                    .default_value()
                    .and_then(|s| schema.resolve_symbol(s))
                    .and_then(Self::sqlite_default)
                {
                    let _ = write!(sql, " DEFAULT {default}");
                    if !column.is_nullable() {
                        sql.push_str(" NOT NULL");
                    }
                }
                sql.push_str(";\n");

                if column.is_unique() {
                    let _ = writeln!(
                        sql,
                        "CREATE UNIQUE INDEX IF NOT EXISTS {} ON {}({});",
                        Ident(&format!("__uniq_{table_name}_{}__", column.name())),
                        Ident(table_name),
                        Ident(column.name()),
                    );
                }
            }

            for (column, _) in live_cols {
                let _ = writeln!(
                    sql,
                    "DROP INDEX IF EXISTS {};",
                    Ident(&format!("__uniq_{table_name}_{column}__")),
                );

                let _ = writeln!(
                    sql,
                    "ALTER TABLE {} DROP COLUMN {};",
                    Ident(table_name),
                    Ident(column),
                );
            }
        }

        for (table, _) in existing {
            let _ = writeln!(sql, "DROP TABLE IF EXISTS {};", Ident(table));
        }

        for table in schema.tables() {
            Self::write_indexes(&mut sql, table);
        }

        Self::write_hash_upsert(&mut sql, schema.hash());

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| DatabaseError::FailedToAcquireTransaction)?;

        if sqlx::raw_sql(AssertSqlSafe(sql))
            .execute(&mut *tx)
            .await
            .is_ok()
        {
            return tx
                .commit()
                .await
                .map_err(|_| DatabaseError::FailedToCommitTransaction);
        }
        drop(tx);

        let mut drop_all = String::with_capacity(32 * live_tables.len());
        for table in live_tables {
            let _ = writeln!(drop_all, "DROP TABLE IF EXISTS {};", Ident(table));
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| DatabaseError::FailedToAcquireTransaction)?;

        sqlx::raw_sql(AssertSqlSafe(drop_all))
            .execute(&mut *tx)
            .await
            .map_err(|err| DatabaseError::SetupFailed {
                step: "rebuilding sqlite tables",
                reason: err.to_string(),
            })?;

        tx.commit()
            .await
            .map_err(|_| DatabaseError::FailedToCommitTransaction)?;

        self.create_tables().await
    }
}

impl<'a> SqliteDatabase<'a> {
    #[inline]
    const fn sqlite_type(data_type: &ValueType) -> &'static str {
        match data_type {
            ValueType::Bool | ValueType::Int => "INTEGER",
            ValueType::Float => "REAL",
            ValueType::Blob | ValueType::Null => "BLOB",
            ValueType::Decimal
            | ValueType::Text
            | ValueType::Json
            | ValueType::Uuid
            | ValueType::Timestamp
            | ValueType::TimestampTz
            | ValueType::Date
            | ValueType::Time
            | ValueType::TimeTz
            | ValueType::Custom(_)
            | ValueType::BoolArray
            | ValueType::IntArray
            | ValueType::FloatArray
            | ValueType::DecimalArray
            | ValueType::TextArray => "TEXT",
        }
    }

    fn sqlite_default(pg_default: &str) -> Option<&str> {
        let mut expr = pg_default.trim();
        if let Some(pos) = expr.find("::") {
            expr = expr[..pos].trim_end();
        }
        let first = *expr.as_bytes().first()?;
        let is_literal = first == b'\''
            || first.is_ascii_digit()
            || (first == b'-' && expr[1..].bytes().next().is_some_and(|b| b.is_ascii_digit()))
            || expr.eq_ignore_ascii_case("true")
            || expr.eq_ignore_ascii_case("false")
            || expr.eq_ignore_ascii_case("null")
            || expr.eq_ignore_ascii_case("current_timestamp");
        is_literal.then_some(expr)
    }

    async fn stored_schema_hash(&self) -> Option<[u8; 32]> {
        sqlx::query_scalar::<_, Vec<u8>>("SELECT value FROM __meta__ WHERE key = 'schema_hash'")
            .fetch_optional(&self.pool)
            .await
            .ok()
            .flatten()
            .and_then(|bytes| bytes.try_into().ok())
    }

    fn build_ddl(schema: &DatabaseSchema) -> String {
        let mut sql = String::with_capacity(
            256 + schema.len() * 96
                + schema.columns.len() * 64
                + schema.foreign_key_pool.len() * 96
                + schema.index_pool.len() * 128,
        );

        for table in schema.tables() {
            Self::write_create_table(&mut sql, schema, table);
            Self::write_indexes(&mut sql, table);
        }

        sql.push_str(SYNC_LOG_DDL);
        Self::write_hash_upsert(&mut sql, schema.hash());
        sql
    }

    fn write_hash_upsert(sql: &mut String, hash: &[u8; 32]) {
        sql.push_str(META_DDL);
        sql.push_str("INSERT INTO __meta__ (key, value) VALUES ('schema_hash', X'");
        for byte in hash {
            let _ = write!(sql, "{byte:02x}");
        }
        sql.push_str("') ON CONFLICT(key) DO UPDATE SET value = excluded.value;\n");
    }

    fn write_create_table(sql: &mut String, schema: &DatabaseSchema, table: TableRef<'_>) {
        let _ = write!(sql, "CREATE TABLE IF NOT EXISTS {} (", Ident(table.name()));

        let pk_cols: Vec<&str> = match table.indexes().find(|i| i.is_primary()) {
            Some(ix) => ix.keys().iter().map(|k| ix.key_text(k)).collect(),
            None => table
                .columns()
                .filter(|c| c.is_primary_key())
                .map(|c| c.name())
                .collect(),
        };
        let composite_pk = pk_cols.len() > 1;

        let mut first = true;
        for column in table.columns() {
            if !first {
                sql.push(',');
            }
            first = false;

            let _ = write!(
                sql,
                "{} {}",
                Ident(column.name()),
                Self::sqlite_type(column.data_type())
            );

            if column.is_primary_key() {
                if composite_pk {
                    sql.push_str(" NOT NULL");
                } else {
                    sql.push_str(" PRIMARY KEY NOT NULL");
                }
            } else {
                if column.is_unique() {
                    sql.push_str(" UNIQUE");
                }
                if !column.is_nullable() {
                    sql.push_str(" NOT NULL");
                }
            }

            if let Some(symbol) = column.default_value()
                && let Some(pg_default) = schema.resolve_symbol(symbol)
                && let Some(default_value) = Self::sqlite_default(pg_default)
            {
                let _ = write!(sql, " DEFAULT {default_value}");
            }
        }

        if composite_pk {
            sql.push_str(",PRIMARY KEY(");
            let mut first_pk = true;
            for column in table.columns().filter(|c| c.is_primary_key()) {
                if !first_pk {
                    sql.push(',');
                }
                first_pk = false;
                let _ = write!(sql, "{}", Ident(column.name()));
            }
            sql.push(')');
        }

        let mut seen: Vec<Symbol> = Vec::new();
        for column in table.columns() {
            for fk in column.foreign_keys() {
                let name = fk.constraint_name();
                if seen.contains(&name) {
                    continue;
                }
                seen.push(name);

                let Some(ref_table) = schema.table_at(fk.table_index()) else {
                    continue;
                };
                let mut locals: Vec<String> = Vec::new();
                let mut refs: Vec<String> = Vec::new();
                let mut ok = true;

                for c in table.columns() {
                    for f in c
                        .foreign_keys()
                        .iter()
                        .filter(|f| f.constraint_name() == name)
                    {
                        let ref_col = ref_table
                            .column_names()
                            .get(f.column_index())
                            .and_then(|&s| schema.resolve_symbol(s));
                        match ref_col {
                            Some(rc) => {
                                locals.push(Ident(c.name()).to_string());
                                refs.push(Ident(rc).to_string());
                            }
                            None => ok = false,
                        }
                    }
                }
                if !ok || locals.is_empty() {
                    continue;
                }
                let _ = write!(
                    sql,
                    ",FOREIGN KEY({}) REFERENCES {}({}) ON UPDATE {} ON DELETE {}",
                    locals.join(","),
                    Ident(ref_table.name()),
                    refs.join(","),
                    fk.on_update(),
                    fk.on_delete(),
                );
            }
        }

        for column in table.columns() {
            let col_ident = Ident(column.name()).to_string();

            let labels: Vec<&str> = column
                .enum_labels()
                .iter()
                .filter_map(|&s| schema.resolve_symbol(s))
                .collect();
            if !labels.is_empty() {
                let list = labels
                    .iter()
                    .map(|l| format!("'{}'", l.replace('\'', "''")))
                    .collect::<Vec<_>>()
                    .join(",");
                let _ = write!(sql, ",CHECK({col_ident} IN ({list}))");
            }

            for &sym in column.domain_checks() {
                Self::push_check(
                    sql,
                    schema.resolve_symbol(sym),
                    Some(&col_ident),
                    table.name(),
                    column.name(),
                );
            }

            for &sym in column.constraints() {
                Self::push_check(
                    sql,
                    schema.resolve_symbol(sym),
                    None,
                    table.name(),
                    column.name(),
                );
            }
        }

        sql.push_str(");\n");
    }

    async fn setup_triggers(&self) -> Result<(), DatabaseError> {
        let schema = self.schema;
        let mut sql = String::with_capacity(schema.len() * 3 * 320);

        for table in schema.tables() {
            let table_name = table.name();

            let primary_col = match table.primary_key() {
                Some(col) => col.name(),
                None => {
                    return Err(DatabaseError::SetupFailed {
                        step: "creating sqlite triggers",
                        reason: format!("Table {table_name} has no primary key"),
                    });
                }
            };

            for (event, op, row) in [
                ("INSERT", 'I', "NEW"),
                ("UPDATE", 'U', "NEW"),
                ("DELETE", 'D', "OLD"),
            ] {
                let _ = write!(
                    sql,
                    "CREATE TRIGGER IF NOT EXISTS {trg} AFTER {event} ON {tbl} FOR EACH ROW \
                     BEGIN \
                     INSERT INTO __sync_log__ (table_name, record_id, op_type, version, updated_at) \
                     VALUES ({name}, CAST({row}.{pk} AS TEXT), '{op}', 1, \
                     CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)) \
                     ON CONFLICT (table_name, record_id) DO UPDATE SET \
                     op_type = excluded.op_type, \
                     version = version + 1, \
                     updated_at = excluded.updated_at; \
                     END;\n",
                    trg = Ident(&format!("__trg_{table_name}_{event}_sync_log__")),
                    tbl = Ident(table_name),
                    name = Lit(&format!("public.{table_name}")),
                    pk = Ident(primary_col),
                );
            }
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| DatabaseError::FailedToAcquireTransaction)?;

        sqlx::raw_sql(AssertSqlSafe(sql))
            .execute(&mut *tx)
            .await
            .map_err(|err| DatabaseError::SetupFailed {
                step: "creating sqlite triggers",
                reason: err.to_string(),
            })?;

        tx.commit()
            .await
            .map_err(|_| DatabaseError::FailedToCommitTransaction)
    }

    async fn drop_triggers(&self) -> Result<(), DatabaseError> {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master \
             WHERE type = 'trigger' AND name GLOB '__trg_*_sync_log__'",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| DatabaseError::SetupFailed {
            step: "listing sqlite triggers",
            reason: err.to_string(),
        })?;

        if names.is_empty() {
            return Ok(());
        }

        let mut sql = String::with_capacity(names.len() * 48);
        for name in &names {
            let _ = writeln!(sql, "DROP TRIGGER IF EXISTS {};", Ident(name));
        }

        sqlx::raw_sql(AssertSqlSafe(sql))
            .execute(&self.pool)
            .await
            .map_err(|err| DatabaseError::SetupFailed {
                step: "dropping sqlite triggers",
                reason: err.to_string(),
            })?;

        Ok(())
    }

    fn push_check(
        sql: &mut String,
        def: Option<&str>,
        value: Option<&str>,
        table: &str,
        col: &str,
    ) {
        let Some(def) = def.filter(|d| !d.trim().is_empty()) else {
            return;
        };
        match translate_check(def, value, &CHECK_OPTS) {
            Some(expr) => {
                sql.push_str(",CHECK ");
                sql.push_str(&expr);
            }
            None => eprintln!("sqlite: skipping untranslatable CHECK on {table}.{col}: {def}"),
        }
    }
}

impl<'a> SqliteDatabase<'a> {
    fn write_indexes(sql: &mut String, table: TableRef<'_>) {
        for index in table.indexes() {
            if let Some(stmt) = Self::index_ddl(&index, table.name()) {
                sql.push_str(&stmt);
            }
        }
    }

    fn index_ddl(index: &IndexRef<'_>, table: &str) -> Option<String> {
        let name = index.name();
        let unique = index.is_unique();
        let keys = index.keys();

        if index.is_primary() {
            return None;
        }

        if index.is_constraint() && !unique {
            eprintln!("sqlite: skipping EXCLUDE constraint index {name} on {table}");
            return None;
        }

        if unique && keys.len() == 1 && !keys[0].is_expr() && index.predicate().is_none() {
            return None;
        }

        if !matches!(index.method(), "btree" | "hash") {
            eprintln!(
                "sqlite: skipping {} index {name} on {table}",
                index.method()
            );
            return None;
        }

        let mut cols: Vec<String> = Vec::with_capacity(keys.len() + index.includes().len());
        for key in keys {
            let text = index.key_text(key);
            let mut part = if key.is_expr() {
                match translate_check(text, None, &CHECK_OPTS) {
                    Some(expr) => expr,
                    None => {
                        eprintln!(
                            "sqlite: skipping index {name} on {table}: untranslatable expression {text}"
                        );
                        return None;
                    }
                }
            } else {
                Ident(text).to_string()
            };
            if key.is_desc() {
                part.push_str(" DESC");
            }
            cols.push(part);
        }

        if !unique {
            for inc in index.includes() {
                cols.push(Ident(index.key_text(inc)).to_string());
            }
        }

        let mut where_clause = String::new();
        if let Some(pred) = index.predicate() {
            match translate_check(pred, None, &CHECK_OPTS) {
                Some(expr) => where_clause = format!(" WHERE {expr}"),
                None if unique => {
                    eprintln!(
                        "sqlite: skipping unique index {name} on {table}: untranslatable predicate {pred}"
                    );
                    return None;
                }
                None => {
                    eprintln!(
                        "sqlite: index {name} on {table}: dropping untranslatable predicate {pred}"
                    );
                }
            }
        }

        Some(format!(
            "CREATE {}INDEX IF NOT EXISTS {} ON {} ({}){};\n",
            if unique { "UNIQUE " } else { "" },
            Ident(name),
            Ident(table),
            cols.join(","),
            where_clause,
        ))
    }
}
