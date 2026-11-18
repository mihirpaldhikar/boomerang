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

use alloc::string::String;
use core::fmt::{self, Write};
use thiserror::Error;

/// Exact source position of a YAML error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLocation {
    /// 0-based byte offset in the input YAML.
    pub offset: usize,
    /// 1-based line number (`0` if source context has not been attached).
    pub line: usize,
    /// 1-based UTF-8 character column (`0` if source context has not been attached).
    pub column: usize,
}

impl fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line > 0 {
            write!(
                f,
                "line {}, column {} (byte {})",
                self.line, self.column, self.offset
            )
        } else {
            write!(f, "byte {}", self.offset)
        }
    }
}

/// Structured, programmatically matchable YAML error categories.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum YamlErrorKind {
    #[error("syntax error: {0}")]
    Syntax(String),

    #[error("unexpected end of YAML input")]
    UnexpectedEof,

    #[error("maximum YAML nesting depth ({0}) exceeded")]
    MaxDepthExceeded(usize),

    #[error("unknown alias `*{0}`")]
    UnknownAlias(String),

    #[error("missing required field `{0}`")]
    MissingField(String),

    #[error("unknown field `{0}`")]
    UnknownField(String),

    #[error("type mismatch: expected {expected}, found {found}")]
    TypeMismatch {
        expected: &'static str,
        found: &'static str,
    },

    #[error("invalid {ty} value `{value}`")]
    InvalidValue { ty: &'static str, value: String },

    #[error("unknown enum variant `{0}`")]
    UnknownVariant(String),

    #[error("cannot borrow `&str` from escaped or block scalar; use `String` or `Cow<str>`")]
    BorrowedEscapedScalar,
}

/// User-friendly YAML parse/deserialization error with exact line, column, offset, and snippet.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{kind} at {location}{snippet}")]
pub struct YamlError {
    pub kind: YamlErrorKind,
    pub location: SourceLocation,
    snippet: String,
}

impl YamlError {
    pub fn from_kind(offset: usize, kind: YamlErrorKind) -> Self {
        Self {
            kind,
            location: SourceLocation {
                offset,
                line: 0,
                column: 0,
            },
            snippet: String::new(),
        }
    }

    pub fn new(offset: usize, msg: impl Into<String>) -> Self {
        Self::from_kind(offset, YamlErrorKind::Syntax(msg.into()))
    }

    pub fn missing_field(offset: usize, field: impl Into<String>) -> Self {
        Self::from_kind(offset, YamlErrorKind::MissingField(field.into()))
    }

    pub fn unknown_field(offset: usize, field: impl Into<String>) -> Self {
        Self::from_kind(offset, YamlErrorKind::UnknownField(field.into()))
    }

    pub fn unknown_variant(offset: usize, variant: impl Into<String>) -> Self {
        Self::from_kind(offset, YamlErrorKind::UnknownVariant(variant.into()))
    }

    pub fn type_mismatch(offset: usize, expected: &'static str, found: &'static str) -> Self {
        Self::from_kind(offset, YamlErrorKind::TypeMismatch { expected, found })
    }

    pub fn invalid_value(offset: usize, ty: &'static str, value: impl Into<String>) -> Self {
        Self::from_kind(
            offset,
            YamlErrorKind::InvalidValue {
                ty,
                value: value.into(),
            },
        )
    }

    /// Attaches exact 1-based line, UTF-8 column, and a formatted source snippet.
    /// Idempotent if source context has already been attached.
    pub fn with_source(mut self, src: &str) -> Self {
        if self.location.line > 0 {
            return self;
        }

        let mut offset = self.location.offset.min(src.len());
        while offset > 0 && !src.is_char_boundary(offset) {
            offset -= 1;
        }

        let prefix = &src[..offset];
        let line_start = prefix.rfind('\n').map_or(0, |i| i + 1);
        let line_num = prefix.as_bytes().iter().filter(|&&b| b == b'\n').count() + 1;
        let col_num = src[line_start..offset].chars().count() + 1;

        let line_end = src[offset..].find('\n').map_or(src.len(), |i| offset + i);
        let raw_line = &src[line_start..line_end];
        let current_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        let prev_line = if line_start > 0 {
            let prev_end = line_start - 1;
            let prev_start = src[..prev_end].rfind('\n').map_or(0, |i| i + 1);
            let prev_raw = &src[prev_start..prev_end];
            Some((
                line_num - 1,
                prev_raw.strip_suffix('\r').unwrap_or(prev_raw),
            ))
        } else {
            None
        };

        let mut caret_pad = String::new();
        for ch in src[line_start..offset].chars() {
            if ch == '\t' {
                caret_pad.push('\t');
            } else {
                caret_pad.push(' ');
            }
        }

        let width = digit_count(line_num).max(2);
        let mut snippet = String::new();
        let _ = write!(snippet, "\n{:>width$} |", "");
        if let Some((prev_n, prev_txt)) = prev_line {
            let _ = write!(snippet, "\n{prev_n:>width$} | {prev_txt}");
        }
        let _ = write!(snippet, "\n{line_num:>width$} | {current_line}");
        let _ = write!(snippet, "\n{:>width$} | {caret_pad}^", "");

        self.location = SourceLocation {
            offset,
            line: line_num,
            column: col_num,
        };
        self.snippet = snippet;
        self
    }
}

fn digit_count(mut n: usize) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}
