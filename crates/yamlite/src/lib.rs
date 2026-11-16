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

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate self as yamlite;
#[cfg(feature = "std")]
extern crate std;

mod parser;
pub mod simd;

#[cfg(feature = "derive")]
pub use yamlite_macros::FromYaml;

use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

pub mod __private {
    pub use alloc::borrow::Cow;
    pub use alloc::format;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlError {
    pub offset: usize,
    pub message: String,
}

impl YamlError {
    pub fn new(offset: usize, msg: impl Into<String>) -> Self {
        Self {
            offset,
            message: msg.into(),
        }
    }
}

impl fmt::Display for YamlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "YAML error at byte {}: {}", self.offset, self.message)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for YamlError {}

#[derive(Debug, Clone)]
pub enum TapeKind {
    Null,
    PlainStr { start: u32, end: u32 },
    QuotedStr { start: u32, end: u32 },
    OwnedStr(u32),
    Tag { start: u32, end: u32 },
    MapStart { end_idx: u32 },
    MapEnd,
    SeqStart { end_idx: u32 },
    SeqEnd,
    Alias { target_idx: u32 },
}

#[derive(Debug, Clone)]
pub struct TapeNode {
    pub kind: TapeKind,
    pub offset: u32,
}

pub struct Tape<'a> {
    pub src: &'a str,
    pub nodes: Vec<TapeNode>,
    pub owned_strings: Vec<String>,
}

pub struct TapeCursor<'t, 'a> {
    pub tape: &'t Tape<'a>,
    pub idx: usize,
}

impl<'t, 'a> TapeCursor<'t, 'a> {
    pub fn new(tape: &'t Tape<'a>) -> Self {
        Self { tape, idx: 0 }
    }

    #[inline]
    pub fn peek(&self) -> Result<&'t TapeNode, YamlError> {
        self.tape
            .nodes
            .get(self.idx)
            .ok_or_else(|| YamlError::new(self.tape.src.len(), "unexpected end of YAML"))
    }

    #[inline]
    pub fn next_node(&mut self) -> Result<&'t TapeNode, YamlError> {
        let n = self.peek()?;
        self.idx += 1;
        Ok(n)
    }

    #[inline]
    pub fn take_tag(&mut self) -> Option<&'a str> {
        if let Ok(node) = self.peek() {
            if let TapeKind::Tag { start, end } = node.kind {
                self.idx += 1;
                return Some(&self.tape.src[start as usize..end as usize]);
            }
        }
        None
    }

    #[inline]
    pub fn skip_tags(&mut self) {
        while let Ok(node) = self.peek() {
            if matches!(node.kind, TapeKind::Tag { .. }) {
                self.idx += 1;
            } else {
                break;
            }
        }
    }

    #[inline]
    pub fn deref_alias(&mut self) -> Result<Option<TapeCursor<'t, 'a>>, YamlError> {
        if let TapeKind::Alias { target_idx } = self.peek()?.kind {
            self.idx += 1;
            return Ok(Some(TapeCursor {
                tape: self.tape,
                idx: target_idx as usize,
            }));
        }
        Ok(None)
    }

    fn peek_past_tags(&self) -> Option<&'t TapeNode> {
        let mut i = self.idx;
        while let Some(n) = self.tape.nodes.get(i) {
            match n.kind {
                TapeKind::Tag { .. } => i += 1,
                TapeKind::Alias { target_idx } => i = target_idx as usize,
                _ => return Some(n),
            }
        }
        None
    }

    #[inline]
    pub fn is_map(&self) -> bool {
        matches!(
            self.peek_past_tags().map(|n| &n.kind),
            Some(TapeKind::MapStart { .. })
        )
    }

    #[inline]
    pub fn is_seq(&self) -> bool {
        matches!(
            self.peek_past_tags().map(|n| &n.kind),
            Some(TapeKind::SeqStart { .. })
        )
    }

    #[inline]
    pub fn is_null(&self) -> bool {
        match self.peek_past_tags().map(|n| &n.kind) {
            Some(TapeKind::Null) => true,
            Some(TapeKind::PlainStr { start, end }) => matches!(
                &self.tape.src[*start as usize..*end as usize],
                "null" | "Null" | "NULL" | "~" | ""
            ),
            _ => false,
        }
    }

    pub fn expect_unit(&mut self) -> Result<(), YamlError> {
        self.skip_tags();
        if let Some(mut aliased) = self.deref_alias()? {
            return aliased.expect_unit();
        }
        if self.is_null() {
            self.next_node()?;
            return Ok(());
        }
        let n = self.peek()?;
        Err(YamlError::new(n.offset as usize, "expected null/unit"))
    }

    pub fn read_cow_str(&mut self) -> Result<Cow<'a, str>, YamlError> {
        self.skip_tags();
        if let Some(mut aliased) = self.deref_alias()? {
            return aliased.read_cow_str();
        }
        let n = self.next_node()?;
        match n.kind {
            TapeKind::PlainStr { start, end } | TapeKind::QuotedStr { start, end } => {
                Ok(Cow::Borrowed(&self.tape.src[start as usize..end as usize]))
            }
            TapeKind::OwnedStr(id) => Ok(Cow::Owned(self.tape.owned_strings[id as usize].clone())),
            TapeKind::Null => Ok(Cow::Borrowed("")),
            _ => Err(YamlError::new(n.offset as usize, "expected string scalar")),
        }
    }

    pub fn skip_value(&mut self) -> Result<(), YamlError> {
        self.skip_tags();
        let n = self.next_node()?;
        match n.kind {
            TapeKind::MapStart { end_idx } | TapeKind::SeqStart { end_idx } => {
                self.idx = end_idx as usize;
            }
            _ => {}
        }
        Ok(())
    }
}

pub trait FromYaml<'a>: Sized {
    fn from_yaml(input: &'a str) -> Result<Self, YamlError> {
        let tape = parser::parse_to_tape(input)?;
        let mut cursor = TapeCursor::new(&tape);
        Self::from_cursor(&mut cursor)
    }

    fn from_cursor(cursor: &mut TapeCursor<'_, 'a>) -> Result<Self, YamlError>;
}

pub fn from_yaml_stream<'a, T: FromYaml<'a>>(input: &'a str) -> Result<Vec<T>, YamlError> {
    let tapes = parser::parse_stream_to_tapes(input)?;
    let mut out = Vec::with_capacity(tapes.len());
    for tape in &tapes {
        let mut cursor = TapeCursor::new(tape);
        out.push(T::from_cursor(&mut cursor)?);
    }
    Ok(out)
}

impl<'src: 'out, 'out> FromYaml<'src> for &'out str {
    fn from_cursor(cursor: &mut TapeCursor<'_, 'src>) -> Result<Self, YamlError> {
        cursor.skip_tags();
        if let Some(mut aliased) = cursor.deref_alias()? {
            return Self::from_cursor(&mut aliased);
        }
        let n = cursor.next_node()?;
        match n.kind {
            TapeKind::PlainStr { start, end } | TapeKind::QuotedStr { start, end } => {
                Ok(&cursor.tape.src[start as usize..end as usize])
            }
            TapeKind::OwnedStr(_) => Err(YamlError::new(
                n.offset as usize,
                "cannot borrow `&str` from escaped or block scalar; use `String` or `Cow<str>`",
            )),
            _ => Err(YamlError::new(n.offset as usize, "expected `&str` scalar")),
        }
    }
}

impl<'a> FromYaml<'a> for String {
    fn from_cursor(cursor: &mut TapeCursor<'_, 'a>) -> Result<Self, YamlError> {
        Ok(cursor.read_cow_str()?.into_owned())
    }
}

impl<'src: 'out, 'out> FromYaml<'src> for Cow<'out, str> {
    fn from_cursor(cursor: &mut TapeCursor<'_, 'src>) -> Result<Self, YamlError> {
        cursor.read_cow_str()
    }
}

impl<'a> FromYaml<'a> for bool {
    fn from_cursor(cursor: &mut TapeCursor<'_, 'a>) -> Result<Self, YamlError> {
        cursor.skip_tags();
        if let Some(mut aliased) = cursor.deref_alias()? {
            return Self::from_cursor(&mut aliased);
        }
        let n = cursor.next_node()?;
        match n.kind {
            TapeKind::PlainStr { start, end } | TapeKind::QuotedStr { start, end } => {
                match &cursor.tape.src[start as usize..end as usize] {
                    "true" | "True" | "TRUE" => Ok(true),
                    "false" | "False" | "FALSE" => Ok(false),
                    other => Err(YamlError::new(
                        n.offset as usize,
                        alloc::format!("invalid bool `{other}`"),
                    )),
                }
            }
            _ => Err(YamlError::new(n.offset as usize, "expected bool")),
        }
    }
}

macro_rules! impl_int {
    ($($t:ty),*) => {
        $(
            impl<'a> FromYaml<'a> for $t {
                fn from_cursor(cursor: &mut TapeCursor<'_, 'a>) -> Result<Self, YamlError> {
                    cursor.skip_tags();
                    if let Some(mut aliased) = cursor.deref_alias()? {
                        return Self::from_cursor(&mut aliased);
                    }
                    let offset = cursor.peek()?.offset as usize;
                    let s = cursor.read_cow_str()?;
                    let res = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                        <$t>::from_str_radix(hex, 16)
                    } else if let Some(oct) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
                        <$t>::from_str_radix(oct, 8)
                    } else {
                        s.parse::<$t>()
                    };
                    res.map_err(|_| YamlError::new(offset, alloc::format!("invalid integer `{s}`")))
                }
            }
        )*
    };
}
impl_int!(
    i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize
);

macro_rules! impl_float {
    ($($t:ty),*) => {
        $(
            impl<'a> FromYaml<'a> for $t {
                fn from_cursor(cursor: &mut TapeCursor<'_, 'a>) -> Result<Self, YamlError> {
                    cursor.skip_tags();
                    if let Some(mut aliased) = cursor.deref_alias()? {
                        return Self::from_cursor(&mut aliased);
                    }
                    let offset = cursor.peek()?.offset as usize;
                    let s = cursor.read_cow_str()?;
                    match s.as_ref() {
                        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => Ok(<$t>::INFINITY),
                        "-.inf" | "-.Inf" | "-.INF" => Ok(<$t>::NEG_INFINITY),
                        ".nan" | ".NaN" | ".NAN" => Ok(<$t>::NAN),
                        _ => s
                            .parse::<$t>()
                            .map_err(|_| YamlError::new(offset, alloc::format!("invalid float `{s}`"))),
                    }
                }
            }
        )*
    };
}
impl_float!(f32, f64);

impl<'a, T: FromYaml<'a>> FromYaml<'a> for Option<T> {
    fn from_cursor(cursor: &mut TapeCursor<'_, 'a>) -> Result<Self, YamlError> {
        if cursor.is_null() {
            cursor.skip_tags();
            if let Some(mut aliased) = cursor.deref_alias()? {
                let _ = aliased.next_node()?;
                return Ok(None);
            }
            cursor.next_node()?;
            return Ok(None);
        }
        Ok(Some(T::from_cursor(cursor)?))
    }
}

impl<'a, T: FromYaml<'a>> FromYaml<'a> for Vec<T> {
    fn from_cursor(cursor: &mut TapeCursor<'_, 'a>) -> Result<Self, YamlError> {
        cursor.skip_tags();
        if let Some(mut aliased) = cursor.deref_alias()? {
            return Self::from_cursor(&mut aliased);
        }
        if cursor.is_null() {
            cursor.next_node()?;
            return Ok(Vec::new());
        }
        let start = cursor.next_node()?;
        let end_idx = match start.kind {
            TapeKind::SeqStart { end_idx } => end_idx as usize,
            _ => return Err(YamlError::new(start.offset as usize, "expected sequence")),
        };
        let mut out = Vec::new();
        while cursor.idx + 1 < end_idx {
            out.push(T::from_cursor(cursor)?);
        }
        cursor.next_node()?;
        Ok(out)
    }
}

impl<'a, K, V> FromYaml<'a> for BTreeMap<K, V>
where
    K: FromYaml<'a> + Ord,
    V: FromYaml<'a>,
{
    fn from_cursor(cursor: &mut TapeCursor<'_, 'a>) -> Result<Self, YamlError> {
        cursor.skip_tags();
        if let Some(mut aliased) = cursor.deref_alias()? {
            return Self::from_cursor(&mut aliased);
        }
        if cursor.is_null() {
            cursor.next_node()?;
            return Ok(BTreeMap::new());
        }
        let start = cursor.next_node()?;
        let end_idx = match start.kind {
            TapeKind::MapStart { end_idx } => end_idx as usize,
            _ => return Err(YamlError::new(start.offset as usize, "expected mapping")),
        };
        let mut map = BTreeMap::new();
        while cursor.idx + 1 < end_idx {
            let k = K::from_cursor(cursor)?;
            let v = V::from_cursor(cursor)?;
            map.insert(k, v);
        }
        cursor.next_node()?;
        Ok(map)
    }
}
