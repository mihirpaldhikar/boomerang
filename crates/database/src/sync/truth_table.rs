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
//! Merge-join of the Postgres and SQLite sync logs.
//!
//! Both logs are read as streams ordered by `(table_name, record_id)` and walked in lockstep.
//! For each key, the entries from the two sides are compared and the winning side is reported
//! as a [`RowAction`].

use crate::Symbol;
use crate::core::DatabaseSchema;
use crate::sync::log_row::{LogRow, PG_SQL, SQLITE_SQL};
use futures::StreamExt;
use futures::stream::BoxStream;
use sqlx::postgres::PgRow;
use sqlx::sqlite::SqliteRow;
use sqlx::{PgPool, SqlitePool};
use std::cmp::Ordering;
use std::fmt;


/// Kind of change recorded in a log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogOpType {
    Insert,
    Update,
    Delete,
}

/// Error returned when a stored op type is not one of `I`, `U` or `D`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidOpType(pub String);

impl fmt::Display for InvalidOpType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid op type: {}", self.0)
    }
}

impl std::error::Error for InvalidOpType {}

impl TryFrom<&str> for LogOpType {
    type Error = InvalidOpType;

    #[inline]
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "I" => Ok(Self::Insert),
            "U" => Ok(Self::Update),
            "D" => Ok(Self::Delete),
            other => Err(InvalidOpType(other.to_string())),
        }
    }
}

/// Lookahead for one stream: the undecoded row plus its cheap, allocation-free fields.
/// `record_id` stays borrowed from the row until the entry is known to be needed.
struct Head<R> {
    row: R,
    table: Symbol,
    op: LogOpType,
    version: i64,
    updated_at: i64,
}

impl<R: LogRow> Head<R> {
    fn new(
        row: R,
        schema: &DatabaseSchema,
        cache: &mut TableCache,
        db_err: fn(sqlx::Error) -> TruthTableError,
    ) -> Result<Self, TruthTableError> {
        let table = cache.symbol_for(schema, row.table().map_err(db_err)?)?;
        let op = LogOpType::try_from(row.op().map_err(db_err)?).map_err(|e| {
            db_err(sqlx::Error::ColumnDecode {
                index: "op_type".to_string(),
                source: Box::new(e),
            })
        })?;
        let version = row.version().map_err(db_err)?;
        let updated_at = row.updated_at().map_err(db_err)?;
        row.record_id().map_err(db_err)?;

        Ok(Self {
            row,
            table,
            op,
            version,
            updated_at,
        })
    }

    #[inline]
    fn record_id(&self) -> &str {
        self.row.record_id().expect("validated in Head::new")
    }

    /// The only place `record_id` is copied to the heap.
    fn into_entry(self) -> SyncLogEntry {
        SyncLogEntry {
            table_name: self.table,
            record_id: self.record_id().to_owned(),
            op_type: self.op,
            version: self.version,
            updated_at: self.updated_at,
        }
    }
}

/// Merge-key comparison across any two heads. Equal symbols skip the string resolve.
#[inline]
fn cmp_heads<A: LogRow, B: LogRow>(a: &Head<A>, b: &Head<B>, schema: &DatabaseSchema) -> Ordering {
    if a.table == b.table {
        a.record_id().cmp(b.record_id())
    } else {
        let name = |s| {
            schema
                .resolve_symbol(s)
                .expect("symbol not from this schema")
        };
        name(a.table).cmp(name(b.table))
    }
}

/// A single row of a sync log.
///
/// `(table_name, record_id)` uniquely identifies a record. `version` is incremented on every
/// change to that record, and `updated_at` is the change time in milliseconds since the Unix
/// epoch.
#[derive(Debug, Clone)]
pub struct SyncLogEntry {
    pub table_name: Symbol,
    pub record_id: String,
    /// The operation this entry records.
    pub op_type: LogOpType,
    pub version: i64,
    pub updated_at: i64,
}

/// Outcome of comparing the two sides for one record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    /// The Postgres entry wins; apply it to SQLite.
    ApplyFromPostgres,
    /// The SQLite entry wins; apply it to Postgres.
    ApplyFromSQLite,
    /// `version` and `updated_at` are equal but the operations differ.
    /// This needs an explicit resolution policy.
    Conflict,
}

/// Per-stream memo of the last table name seen and its schema [`Symbol`].
///
/// Both logs are ordered by table name, so consecutive rows almost always belong to the same
/// table. A hit costs a single `&str` comparison — no allocation and no interner hash. The
/// owned buffer is rewritten (reusing its capacity) only when the stream crosses into the
/// next table, i.e. once per table per stream.
#[derive(Default)]
struct TableCache {
    name: String,
    symbol: Option<Symbol>,
}

impl TableCache {
    /// Returns the schema symbol for `table`, hitting the memo when possible.
    ///
    /// A lookup miss in the schema means the log references a table unknown to the schema,
    /// which the guarantee forbids — reported as [`TruthTableError::UnknownTable`].
    #[inline]
    fn symbol_for(
        &mut self,
        schema: &DatabaseSchema,
        table: &str,
    ) -> Result<Symbol, TruthTableError> {
        if let Some(symbol) = self.symbol
            && self.name == table
        {
            return Ok(symbol);
        }

        let symbol = schema
            .get_symbol(table)
            .ok_or_else(|| TruthTableError::UnknownTable(table.to_string()))?;

        self.name.clear();
        self.name.push_str(table);
        self.symbol = Some(symbol);
        Ok(symbol)
    }
}

/// The entries found for one record on each side, and the resulting action.
#[derive(Debug)]
pub struct SyncTruthEntry {
    /// Postgres entry, or `None` if the record is absent from the Postgres log.
    pub postgres: Option<SyncLogEntry>,
    /// SQLite entry, or `None` if the record is absent from the SQLite log.
    pub sqlite: Option<SyncLogEntry>,
    pub action: RowAction,
}

impl SyncTruthEntry {
    /// Returns the entry that should be applied, or `None` when the outcome is undecided
    /// ([`RowAction::Conflict`]).
    pub fn winning_entry(&self) -> Option<&SyncLogEntry> {
        match self.action {
            RowAction::Conflict => None,
            RowAction::ApplyFromPostgres => self.postgres.as_ref(),
            RowAction::ApplyFromSQLite => self.sqlite.as_ref(),
        }
    }
}

/// Error raised while reading either log stream.
#[derive(Debug)]
pub enum TruthTableError {
    Postgres(sqlx::Error),
    Sqlite(sqlx::Error),
    /// A log row referenced a table that is not interned in the schema. The schema and the
    /// sync log are out of sync; the merge cannot continue.
    UnknownTable(String),
}

impl fmt::Display for TruthTableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Postgres(e) => write!(f, "postgres stream error: {e}"),
            Self::Sqlite(e) => write!(f, "sqlite stream error: {e}"),
            Self::UnknownTable(t) => write!(f, "sync log references unknown table: {t}"),
        }
    }
}

impl std::error::Error for TruthTableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Postgres(e) | Self::Sqlite(e) => Some(e),
            Self::UnknownTable(_) => None,
        }
    }
}

/// Merge-join over the Postgres and SQLite sync logs.
///
/// The table owns both queries, so the byte-wise ordering the merge depends on cannot be
/// violated by callers.
///
/// # Error handling
///
/// Errors are terminal. After the first `Err`, the table must not be used further.
///
/// # Resources
///
/// Each stream holds one pooled connection until it is exhausted or dropped. Avoid running
/// other queries on a single-connection pool while iterating.
pub struct TruthTable<'a> {
    schema: &'a DatabaseSchema,
    pg_stream: BoxStream<'a, Result<PgRow, sqlx::Error>>,
    sq_stream: BoxStream<'a, Result<SqliteRow, sqlx::Error>>,
    /// Last-table memo of each stream; see [`TableCache`].
    pg_cache: TableCache,
    sq_cache: TableCache,
    /// Lookahead: the next unconsumed row of each stream.
    pg_head: Option<Head<PgRow>>,
    sq_head: Option<Head<SqliteRow>>,
}

/// Reads the next entry from a stream, flattening `Option<Result<_>>` into `Result<Option<_>>`.
#[inline]
async fn pull<R: LogRow>(
    s: &mut BoxStream<'_, Result<R, sqlx::Error>>,
    schema: &DatabaseSchema,
    cache: &mut TableCache,
    db_err: fn(sqlx::Error) -> TruthTableError,
) -> Result<Option<Head<R>>, TruthTableError> {
    match s.next().await.transpose().map_err(db_err)? {
        Some(row) => Head::new(row, schema, cache, db_err).map(Some),
        None => Ok(None),
    }
}

/// Installs `next` as the lookahead entry and returns the previous one.
///
/// In debug builds, asserts that the stream is strictly ascending by key. A violation means
/// the ordering contract is broken and the merge results would be wrong.
#[inline]
fn advance<R: LogRow>(
    cur: &mut Option<Head<R>>,
    next: Option<Head<R>>,
    schema: &DatabaseSchema,
) -> Option<Head<R>> {
    if let (Some(old), Some(new)) = (cur.as_ref(), next.as_ref()) {
        debug_assert!(
            cmp_heads(new, old, schema) == Ordering::Greater,
            "stream not strictly ascending: {:?} -> {:?}",
            old.record_id(),
            new.record_id()
        );
    }
    std::mem::replace(cur, next)
}

impl<'a> TruthTable<'a> {
    /// Opens both log streams and primes the lookahead with the first entry of each.
    pub async fn new(
        pg: &'a PgPool,
        sqlite: &'a SqlitePool,
        schema: &'a DatabaseSchema,
    ) -> Result<Self, TruthTableError> {
        let mut pg_stream = sqlx::query(PG_SQL).fetch(pg);
        let mut sq_stream = sqlx::query(SQLITE_SQL).fetch(sqlite);
        let mut pg_cache = TableCache::default();
        let mut sq_cache = TableCache::default();

        // Prime both lookaheads concurrently.
        let (pg_head, sq_head) = futures::try_join!(
            pull(
                &mut pg_stream,
                schema,
                &mut pg_cache,
                TruthTableError::Postgres
            ),
            pull(
                &mut sq_stream,
                schema,
                &mut sq_cache,
                TruthTableError::Sqlite
            ),
        )?;

        Ok(Self {
            schema,
            pg_stream,
            sq_stream,
            pg_cache,
            sq_cache,
            pg_head,
            sq_head,
        })
    }

    /// Returns the next *divergent* record in key order, or `None` when both logs are
    /// exhausted. Records on which both sides already agree are skipped internally without
    /// allocating.
    ///
    /// The side with the smaller key is consumed. If the keys are equal, both sides are
    /// consumed and compared.
    /// # Cancellation
    ///
    /// `next_entry` is not cancellation-safe: dropping its future mid-poll can lose a fetched
    /// row from one stream. Do not race it inside `select!`; poll it to completion.
    pub async fn next_entry(&mut self) -> Result<Option<SyncTruthEntry>, TruthTableError> {
        let Self {
            schema,
            pg_stream,
            sq_stream,
            pg_cache,
            sq_cache,
            pg_head,
            sq_head,
        } = self;
        let schema: &DatabaseSchema = schema;

        loop {
            let ord = match (&*pg_head, &*sq_head) {
                (None, None) => return Ok(None),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(p), Some(s)) => cmp_heads(p, s, schema),
            };

            // Both fetches are in flight together when the keys are equal.
            let (next_pg, next_sq) = futures::try_join!(
                async {
                    if ord != Ordering::Greater {
                        pull(pg_stream, schema, pg_cache, TruthTableError::Postgres)
                            .await
                            .map(Some)
                    } else {
                        Ok(None)
                    }
                },
                async {
                    if ord != Ordering::Less {
                        pull(sq_stream, schema, sq_cache, TruthTableError::Sqlite)
                            .await
                            .map(Some)
                    } else {
                        Ok(None)
                    }
                },
            )?;

            let pg = next_pg.and_then(|n| advance(pg_head, n, schema));
            let sq = next_sq.and_then(|n| advance(sq_head, n, schema));

            if let Some(entry) = Self::resolve(pg, sq) {
                return Ok(Some(entry));
            }
            // Records that were already in sync are skipped without allocating.
        }
    }

    /// Chooses the winning side for one record.
    ///
    /// - A record present on one side only is applied from that side.
    /// - Otherwise the higher `version` wins, with ties broken by the later `updated_at`.
    /// - If both are equal, the sides agree when their operations match, and are in
    ///   [`RowAction::Conflict`] when they do not.
    ///
    /// `version` is a per-side change counter, not a global clock, so this is a
    /// last-writer-wins policy based on change count first and time second.
    fn resolve(pg: Option<Head<PgRow>>, sq: Option<Head<SqliteRow>>) -> Option<SyncTruthEntry> {
        let action = match (&pg, &sq) {
            (Some(_), None) => RowAction::ApplyFromPostgres,
            (None, Some(_)) => RowAction::ApplyFromSQLite,
            (Some(p), Some(s)) => match (p.version, p.updated_at).cmp(&(s.version, s.updated_at)) {
                Ordering::Greater => RowAction::ApplyFromPostgres,
                Ordering::Less => RowAction::ApplyFromSQLite,
                Ordering::Equal if p.op == s.op => return None, // in sync: nothing allocated
                Ordering::Equal => RowAction::Conflict,
            },
            (None, None) => unreachable!("next_entry returns early when both sides are exhausted"),
        };

        Some(SyncTruthEntry {
            postgres: pg.map(Head::into_entry),
            sqlite: sq.map(Head::into_entry),
            action,
        })
    }
}
