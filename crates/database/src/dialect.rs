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
mod postgres;

use std::fmt;
use std::fmt::Formatter;
pub use postgres::{PostgresDatabase};

#[derive(Debug)]
pub enum DatabaseError {
    ConnectionPoolFailed,
    FailedToAcquireTransaction,
    FailedToCommitTransaction,
    SetupFailed { step: &'static str, reason: String },
    SchemaHashComputationFailed,
    SqlxError(sqlx::Error),
    BackfillFailed { table: String, reason: &'static str },
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            DatabaseError::ConnectionPoolFailed => {
                write!(f, "failed to create connection pool")
            }
            DatabaseError::FailedToAcquireTransaction => {
                write!(f, "failed to acquire transaction")
            }
            DatabaseError::FailedToCommitTransaction => {
                write!(f, "failed to commit transaction")
            }
            DatabaseError::SetupFailed { step, reason } => {
                write!(f, "setup failed while {step}: {reason}")
            }
            DatabaseError::SchemaHashComputationFailed => {
                write!(f, "failed to compute schema hash")
            }
            DatabaseError::SqlxError(err) => {
                write!(f, "sqlx error: {}", err)
            }
            DatabaseError::BackfillFailed { table, reason } => {
                write!(f, "backfill failed for table {table}: {reason}")
            }
        }
    }
}

impl std::error::Error for DatabaseError {}
