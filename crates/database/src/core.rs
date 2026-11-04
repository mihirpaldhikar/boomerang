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
mod schema;
mod schema_builder;
mod string_interner;
mod value;

pub use schema::{Action, DatabaseSchema};
pub use schema_builder::{DatabaseSchemaBuilder, RawForeignKey};
use std::fmt;
use std::fmt::{Display, Write};
pub use string_interner::{StringInterner, Symbol};
pub(crate) use value::ValueType;
pub(crate) use schema::{TableRef, ColumnRef};

/// Quoted SQL identifier. Escapes on the fly, no intermediate String.
pub(crate) struct Ident<'a>(pub &'a str);

impl Display for Ident<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('"')?;
        for c in self.0.chars() {
            if c == '"' {
                f.write_char('"')?;
            }
            f.write_char(c)?;
        }
        f.write_char('"')
    }
}

pub(crate) struct Lit<'a>(pub &'a str);

impl Display for Lit<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('\'')?;
        self.0.chars().try_for_each(|c| {
            if c == '\'' {
                f.write_char('\'')?;
            }
            f.write_char(c)
        })?;
        f.write_char('\'')
    }
}
