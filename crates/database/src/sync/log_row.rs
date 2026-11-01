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
use sqlx::Row;
use sqlx::postgres::PgRow;
use sqlx::sqlite::SqliteRow;

/// Postgres log query.
///
/// The merge requires byte-wise ordering identical to Rust's `str::cmp`. The explicit
/// `COLLATE "C"` overrides the database and column collation, which could otherwise order
/// keys differently from the SQLite side. `op_type` is cast to `text` because the column is
/// `char(1)`, which does not decode into `String` directly.
pub(crate) const PG_SQL: &str = r#"
    SELECT table_name, record_id, op_type::text AS op_type, version, updated_at
    FROM boomerang.sync_log
    ORDER BY table_name COLLATE "C", record_id COLLATE "C"
"#;

/// SQLite log query, ordered byte-wise to match [`PG_SQL`] and Rust's `str::cmp`.
pub(crate) const SQLITE_SQL: &str = r#"
    SELECT table_name, record_id, op_type, version, updated_at
    FROM __sync_log__
    ORDER BY table_name COLLATE BINARY, record_id COLLATE BINARY
"#;

pub(crate) const TABLE: usize = 0;
pub(crate) const RECORD: usize = 1;
pub(crate) const OP: usize = 2;
pub(crate) const VERSION: usize = 3;
pub(crate) const UPDATED_AT: usize = 4;

pub(crate) trait LogRow {
    fn table(&self) -> Result<&str, sqlx::Error>;
    fn record_id(&self) -> Result<&str, sqlx::Error>;
    fn op(&self) -> Result<&str, sqlx::Error>;
    fn version(&self) -> Result<i64, sqlx::Error>;
    fn updated_at(&self) -> Result<i64, sqlx::Error>;
}

macro_rules! impl_log_row {
    ($($row:ty),*) => {$(
        impl LogRow for $row {
            #[inline]
            fn table(&self) -> Result<&str, sqlx::Error> { Row::try_get(self, TABLE) }

            #[inline]
            fn record_id(&self) -> Result<&str, sqlx::Error> { Row::try_get(self, RECORD) }

            #[inline]
            fn op(&self) -> Result<&str, sqlx::Error> { Row::try_get(self, OP) }

            #[inline]
            fn version(&self) -> Result<i64, sqlx::Error> { Row::try_get(self, VERSION) }

            #[inline]
            fn updated_at(&self) -> Result<i64, sqlx::Error> { Row::try_get(self, UPDATED_AT) }
        }
    )*};
}
impl_log_row!(PgRow, SqliteRow);
