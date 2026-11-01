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

use futures::StreamExt;
use futures::stream::BoxStream;
use sqlx::{PgPool, SqlitePool};
use std::cmp::Ordering;
use std::fmt;

/// Postgres log query.
///
/// The merge requires byte-wise ordering identical to Rust's `str::cmp`. The explicit
/// `COLLATE "C"` overrides the database and column collation, which could otherwise order
/// keys differently from the SQLite side. `op_type` is cast to `text` because the column is
/// `char(1)`, which does not decode into `String` directly.
const PG_SQL: &str = r#"
    SELECT table_name, record_id, op_type::text AS op_type, version, updated_at
    FROM boomerang.sync_log
    ORDER BY table_name COLLATE "C", record_id COLLATE "C"
"#;

/// SQLite log query, ordered byte-wise to match [`PG_SQL`] and Rust's `str::cmp`.
const SQLITE_SQL: &str = r#"
    SELECT table_name, record_id, op_type, version, updated_at
    FROM __sync_log__
    ORDER BY table_name COLLATE BINARY, record_id COLLATE BINARY
"#;

/// Kind of change recorded in a log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogOpType {
    Insert,
    Update,
    Delete,
}

impl TryFrom<String> for LogOpType {
    type Error = InvalidOpType;
    fn try_from(s: String) -> Result<Self, Self::Error> { Self::try_from(s.as_str()) }
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

/// A single row of a sync log.
///
/// `(table_name, record_id)` uniquely identifies a record. `version` is incremented on every
/// change to that record, and `updated_at` is the change time in milliseconds since the Unix
/// epoch.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SyncLogEntry {
    pub table_name: String,
    pub record_id: String,
    /// Raw operation code (`I`, `U` or `D`).
    #[sqlx(try_from = "String")]
    pub op_type: LogOpType,
    pub version: i64,
    pub updated_at: i64,
}

impl SyncLogEntry {
    /// Merge key. Both streams are ordered by this key.
    #[inline]
    fn key(&self) -> (&str, &str) {
        (&self.table_name, &self.record_id)
    }
}

/// Outcome of comparing the two sides for one record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    /// Both sides agree; nothing to apply.
    None,
    /// The Postgres entry wins; apply it to SQLite.
    ApplyFromPostgres,
    /// The SQLite entry wins; apply it to Postgres.
    ApplyFromSQLite,
    /// `version` and `updated_at` are equal but the operations differ.
    /// This needs an explicit resolution policy.
    Conflict,
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
    /// Returns the entry that should be applied, or `None` when there is nothing to apply
    /// (`None`) or the outcome is undecided (`Conflict`).
    pub fn winning_entry(&self) -> Option<&SyncLogEntry> {
        match self.action {
            RowAction::None | RowAction::Conflict => None,
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
}

impl fmt::Display for TruthTableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Postgres(e) => write!(f, "postgres stream error: {e}"),
            Self::Sqlite(e) => write!(f, "sqlite stream error: {e}"),
        }
    }
}

impl std::error::Error for TruthTableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Postgres(e) | Self::Sqlite(e) => Some(e),
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
    pg_stream: BoxStream<'a, Result<SyncLogEntry, sqlx::Error>>,
    sq_stream: BoxStream<'a, Result<SyncLogEntry, sqlx::Error>>,
    /// Lookahead: the next unconsumed entry of each stream.
    pg_current: Option<SyncLogEntry>,
    sq_current: Option<SyncLogEntry>,
}

/// Reads the next entry from a stream, flattening `Option<Result<_>>` into `Result<Option<_>>`.
#[inline]
async fn pull(
    s: &mut BoxStream<'_, Result<SyncLogEntry, sqlx::Error>>,
) -> Result<Option<SyncLogEntry>, sqlx::Error> {
    s.next().await.transpose()
}

/// Installs `next` as the lookahead entry and returns the previous one.
///
/// In debug builds, asserts that the stream is strictly ascending by key. A violation means
/// the ordering contract is broken and the merge results would be wrong.
#[inline]
fn advance(cur: &mut Option<SyncLogEntry>, next: Option<SyncLogEntry>) -> Option<SyncLogEntry> {
    if let (Some(old), Some(new)) = (cur.as_ref(), next.as_ref()) {
        debug_assert!(
            new.key() > old.key(),
            "stream not strictly ascending: {:?} -> {:?}",
            old.key(),
            new.key()
        );
    }
    std::mem::replace(cur, next)
}

impl<'a> TruthTable<'a> {
    /// Opens both log streams and primes the lookahead with the first entry of each.
    pub async fn new(pg: &'a PgPool, sqlite: &'a SqlitePool) -> Result<Self, TruthTableError> {
        let mut pg_stream = sqlx::query_as::<_, SyncLogEntry>(PG_SQL).fetch(pg);
        let mut sq_stream = sqlx::query_as::<_, SyncLogEntry>(SQLITE_SQL).fetch(sqlite);

        let pg_current = pull(&mut pg_stream)
            .await
            .map_err(TruthTableError::Postgres)?;
        let sq_current = pull(&mut sq_stream)
            .await
            .map_err(TruthTableError::Sqlite)?;

        Ok(Self {
            pg_stream,
            sq_stream,
            pg_current,
            sq_current,
        })
    }

    /// Returns the next record in key order, or `None` when both logs are exhausted.
    ///
    /// The side with the smaller key is consumed. If the keys are equal, both sides are
    /// consumed and compared.
    pub async fn next_entry(&mut self) -> Result<Option<SyncTruthEntry>, TruthTableError> {
        // Decide which side(s) to consume. A side that is exhausted always loses the
        // comparison, so the other side drains.
        let ord = match (&self.pg_current, &self.sq_current) {
            (None, None) => return Ok(None),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(p), Some(s)) => p.key().cmp(&s.key()),
        };

        // Fetch before swapping, so lookahead entries are consumed only if every fetch
        // succeeded.
        let next_pg = if ord != Ordering::Greater {
            Some(
                pull(&mut self.pg_stream)
                    .await
                    .map_err(TruthTableError::Postgres)?,
            )
        } else {
            None
        };
        let next_sq = if ord != Ordering::Less {
            Some(
                pull(&mut self.sq_stream)
                    .await
                    .map_err(TruthTableError::Sqlite)?,
            )
        } else {
            None
        };

        let pg = next_pg.and_then(|n| advance(&mut self.pg_current, n));
        let sq = next_sq.and_then(|n| advance(&mut self.sq_current, n));

        Ok(Some(Self::resolve(pg, sq)))
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
    fn resolve(pg: Option<SyncLogEntry>, sq: Option<SyncLogEntry>) -> SyncTruthEntry {
        let action = match (&pg, &sq) {
            (Some(_), None) => RowAction::ApplyFromPostgres,
            (None, Some(_)) => RowAction::ApplyFromSQLite,
            (Some(p), Some(s)) => match (p.version, p.updated_at).cmp(&(s.version, s.updated_at)) {
                Ordering::Greater => RowAction::ApplyFromPostgres,
                Ordering::Less => RowAction::ApplyFromSQLite,
                Ordering::Equal if p.op_type == s.op_type => RowAction::None,
                Ordering::Equal => RowAction::Conflict,
            },
            (None, None) => unreachable!("next_entry returns early when both sides are exhausted"),
        };

        SyncTruthEntry {
            postgres: pg,
            sqlite: sq,
            action,
        }
    }
}
