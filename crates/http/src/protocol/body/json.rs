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

use bytes::Bytes;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::{Index, IndexMut};

const MAX_PARSE_DEPTH: usize = 128;

#[inline]
fn find_json_escape(bytes: &[u8]) -> Option<usize> {
    let len = bytes.len();
    let mut i = 0usize;

    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::aarch64::*;
        let ctrl_limit = vdupq_n_u8(0x20);
        let quote = vdupq_n_u8(b'"');
        let slash = vdupq_n_u8(b'\\');

        while i + 16 <= len {
            let v = vld1q_u8(bytes.as_ptr().add(i));
            let is_ctrl = vcltq_u8(v, ctrl_limit);
            let is_quote = vceqq_u8(v, quote);
            let is_slash = vceqq_u8(v, slash);
            let hit = vorrq_u8(is_ctrl, vorrq_u8(is_quote, is_slash));

            if vmaxvq_u8(hit) != 0 {
                for off in 0..16 {
                    let b = *bytes.get_unchecked(i + off);
                    if b < 0x20 || b == b'"' || b == b'\\' {
                        return Some(i + off);
                    }
                }
            }
            i += 16;
        }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::*;
        let zero = _mm_setzero_si128();
        let ctrl_sub = _mm_set1_epi8(0x1f);
        let quote = _mm_set1_epi8(b'"' as i8);
        let slash = _mm_set1_epi8(b'\\' as i8);

        while i + 16 <= len {
            let v = _mm_loadu_si128(bytes.as_ptr().add(i) as *const __m128i);
            let is_ctrl = _mm_cmpeq_epi8(_mm_subs_epu8(v, ctrl_sub), zero);
            let is_quote = _mm_cmpeq_epi8(v, quote);
            let is_slash = _mm_cmpeq_epi8(v, slash);
            let hit = _mm_or_si128(is_ctrl, _mm_or_si128(is_quote, is_slash));
            let mask = _mm_movemask_epi8(hit) as u32;
            if mask != 0 {
                return Some(i + mask.trailing_zeros() as usize);
            }
            i += 16;
        }
    }

    while i < len {
        let b = bytes[i];
        if b < 0x20 || b == b'"' || b == b'\\' {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[inline]
pub fn write_json_str(s: &str, out: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    out.push(b'"');

    let mut i = 0usize;
    while let Some(rel) = find_json_escape(&bytes[i..]) {
        if rel > 0 {
            out.extend_from_slice(&bytes[i..i + rel]);
        }
        let pos = i + rel;
        match bytes[pos] {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\x08' => out.extend_from_slice(b"\\b"),
            b'\x0c' => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            ctrl => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.extend_from_slice(&[
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[(ctrl >> 4) as usize],
                    HEX[(ctrl & 0xF) as usize],
                ]);
            }
        }
        i = pos + 1;
    }

    if i < bytes.len() {
        out.extend_from_slice(&bytes[i..]);
    }
    out.push(b'"');
}

#[inline]
fn write_u64(mut n: u64, out: &mut Vec<u8>) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 20;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.extend_from_slice(&buf[i..]);
}

#[inline]
fn write_i64(n: i64, out: &mut Vec<u8>) {
    if n < 0 {
        out.push(b'-');
        write_u64(n.wrapping_neg() as u64, out);
    } else {
        write_u64(n as u64, out);
    }
}

pub trait ToJson {
    fn write_json(&self, out: &mut Vec<u8>);

    #[inline]
    fn size_hint(&self) -> usize {
        128
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawJson(pub Bytes);

impl ToJson for RawJson {
    #[inline]
    fn write_json(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0);
    }

    #[inline]
    fn size_hint(&self) -> usize {
        self.0.len()
    }
}

impl ToJson for bool {
    #[inline]
    fn write_json(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(if *self { b"true" } else { b"false" });
    }
}

impl ToJson for str {
    #[inline]
    fn write_json(&self, out: &mut Vec<u8>) {
        write_json_str(self, out);
    }
}

impl ToJson for String {
    #[inline]
    fn write_json(&self, out: &mut Vec<u8>) {
        write_json_str(self, out);
    }
}

impl ToJson for std::sync::Arc<str> {
    #[inline]
    fn write_json(&self, out: &mut Vec<u8>) {
        write_json_str(self.as_ref(), out);
    }
}

macro_rules! impl_int_to_json {
    ($($signed:ty),* ; $($unsigned:ty),*) => {
        $(
            impl ToJson for $signed {
                #[inline]
                fn write_json(&self, out: &mut Vec<u8>) {
                    write_i64(*self as i64, out);
                }
            }
        )*
        $(
            impl ToJson for $unsigned {
                #[inline]
                fn write_json(&self, out: &mut Vec<u8>) {
                    write_u64(*self as u64, out);
                }
            }
        )*
    };
}

impl_int_to_json!(i8, i16, i32, i64, isize ; u8, u16, u32, u64, usize);

impl ToJson for f64 {
    #[inline]
    fn write_json(&self, out: &mut Vec<u8>) {
        use std::io::Write;
        if self.is_finite() {
            let _ = write!(out, "{}", self);
        } else {
            out.extend_from_slice(b"null");
        }
    }
}

impl ToJson for f32 {
    #[inline]
    fn write_json(&self, out: &mut Vec<u8>) {
        use std::io::Write;
        if self.is_finite() {
            let _ = write!(out, "{}", self);
        } else {
            out.extend_from_slice(b"null");
        }
    }
}

impl<T: ToJson + ?Sized> ToJson for &T {
    #[inline]
    fn write_json(&self, out: &mut Vec<u8>) {
        (*self).write_json(out);
    }
}

impl<T: ToJson> ToJson for Option<T> {
    #[inline]
    fn write_json(&self, out: &mut Vec<u8>) {
        match self {
            Some(v) => v.write_json(out),
            None => out.extend_from_slice(b"null"),
        }
    }
}

impl<T: ToJson> ToJson for [T] {
    fn write_json(&self, out: &mut Vec<u8>) {
        out.push(b'[');
        for (idx, item) in self.iter().enumerate() {
            if idx > 0 {
                out.push(b',');
            }
            item.write_json(out);
        }
        out.push(b']');
    }
}

impl<T: ToJson> ToJson for Vec<T> {
    #[inline]
    fn write_json(&self, out: &mut Vec<u8>) {
        self.as_slice().write_json(out);
    }
}

impl<K: AsRef<str>, V: ToJson> ToJson for hashbrown::HashMap<K, V> {
    fn write_json(&self, out: &mut Vec<u8>) {
        out.push(b'{');
        for (idx, (k, v)) in self.iter().enumerate() {
            if idx > 0 {
                out.push(b',');
            }
            write_json_str(k.as_ref(), out);
            out.push(b':');
            v.write_json(out);
        }
        out.push(b'}');
    }
}

impl<K: AsRef<str>, V: ToJson> ToJson for BTreeMap<K, V> {
    fn write_json(&self, out: &mut Vec<u8>) {
        out.push(b'{');
        for (idx, (k, v)) in self.iter().enumerate() {
            if idx > 0 {
                out.push(b',');
            }
            write_json_str(k.as_ref(), out);
            out.push(b':');
            v.write_json(out);
        }
        out.push(b'}');
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

static NULL_VALUE: JsonValue = JsonValue::Null;

impl JsonValue {
    #[inline]
    pub fn is_null(&self) -> bool {
        matches!(self, JsonValue::Null)
    }

    #[inline]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    #[inline]
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Int(n) => Some(*n),
            JsonValue::Uint(n) if *n <= i64::MAX as u64 => Some(*n as i64),
            _ => None,
        }
    }

    #[inline]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            JsonValue::Uint(n) => Some(*n),
            JsonValue::Int(n) if *n >= 0 => Some(*n as u64),
            _ => None,
        }
    }

    #[inline]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Float(f) => Some(*f),
            JsonValue::Int(n) => Some(*n as f64),
            JsonValue::Uint(n) => Some(*n as f64),
            _ => None,
        }
    }

    #[inline]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    #[inline]
    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(arr) => Some(arr.as_slice()),
            _ => None,
        }
    }

    #[inline]
    pub fn as_object(&self) -> Option<&[(String, JsonValue)]> {
        match self {
            JsonValue::Object(obj) => Some(obj.as_slice()),
            _ => None,
        }
    }

    #[inline]
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(entries) => {
                entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }

    #[inline]
    pub fn get_mut(&mut self, key: &str) -> Option<&mut JsonValue> {
        match self {
            JsonValue::Object(entries) => entries
                .iter_mut()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128);
        self.write_json(&mut buf);
        buf
    }

    pub fn parse(input: &[u8]) -> Result<Self, JsonError> {
        let mut parser = JsonParser { input, pos: 0 };
        let val = parser.parse_value(0)?;
        parser.skip_ws();
        if parser.pos != parser.input.len() {
            return Err(JsonError::TrailingCharacters);
        }
        Ok(val)
    }
}

impl ToJson for JsonValue {
    fn write_json(&self, out: &mut Vec<u8>) {
        match self {
            JsonValue::Null => out.extend_from_slice(b"null"),
            JsonValue::Bool(b) => b.write_json(out),
            JsonValue::Int(n) => write_i64(*n, out),
            JsonValue::Uint(n) => write_u64(*n, out),
            JsonValue::Float(f) => f.write_json(out),
            JsonValue::String(s) => write_json_str(s, out),
            JsonValue::Array(arr) => arr.write_json(out),
            JsonValue::Object(entries) => {
                out.push(b'{');
                for (idx, (k, v)) in entries.iter().enumerate() {
                    if idx > 0 {
                        out.push(b',');
                    }
                    write_json_str(k, out);
                    out.push(b':');
                    v.write_json(out);
                }
                out.push(b'}');
            }
        }
    }
}

impl fmt::Display for JsonValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.to_vec();
        f.write_str(unsafe { std::str::from_utf8_unchecked(&bytes) })
    }
}

impl Index<&str> for JsonValue {
    type Output = JsonValue;

    #[inline]
    fn index(&self, key: &str) -> &Self::Output {
        self.get(key).unwrap_or(&NULL_VALUE)
    }
}

impl IndexMut<&str> for JsonValue {
    fn index_mut(&mut self, key: &str) -> &mut Self::Output {
        if !matches!(self, JsonValue::Object(_)) {
            *self = JsonValue::Object(Vec::new());
        }
        let JsonValue::Object(entries) = self else {
            unreachable!()
        };
        if let Some(idx) = entries.iter().rposition(|(k, _)| k == key) {
            return &mut entries[idx].1;
        }
        entries.push((key.to_owned(), JsonValue::Null));
        let last = entries.len() - 1;
        &mut entries[last].1
    }
}

impl Index<usize> for JsonValue {
    type Output = JsonValue;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        match self {
            JsonValue::Array(arr) => arr.get(index).unwrap_or(&NULL_VALUE),
            _ => &NULL_VALUE,
        }
    }
}

pub trait IntoJsonValue {
    fn into_json_value(self) -> JsonValue;
}

impl IntoJsonValue for JsonValue {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        self
    }
}
impl IntoJsonValue for bool {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        JsonValue::Bool(self)
    }
}
impl IntoJsonValue for &str {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        JsonValue::String(self.to_owned())
    }
}
impl IntoJsonValue for String {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        JsonValue::String(self)
    }
}
impl IntoJsonValue for i32 {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        JsonValue::Int(self as i64)
    }
}
impl IntoJsonValue for i64 {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        JsonValue::Int(self)
    }
}
impl IntoJsonValue for isize {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        JsonValue::Int(self as i64)
    }
}
impl IntoJsonValue for u32 {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        JsonValue::Uint(self as u64)
    }
}
impl IntoJsonValue for u64 {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        JsonValue::Uint(self)
    }
}
impl IntoJsonValue for usize {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        JsonValue::Uint(self as u64)
    }
}
impl IntoJsonValue for f32 {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        let f = self as f64;
        if f.is_finite() {
            JsonValue::Float(f)
        } else {
            JsonValue::Null
        }
    }
}
impl IntoJsonValue for f64 {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        if self.is_finite() {
            JsonValue::Float(self)
        } else {
            JsonValue::Null
        }
    }
}
impl<T: IntoJsonValue> IntoJsonValue for Option<T> {
    #[inline]
    fn into_json_value(self) -> JsonValue {
        match self {
            Some(v) => v.into_json_value(),
            None => JsonValue::Null,
        }
    }
}
impl<T: IntoJsonValue> IntoJsonValue for Vec<T> {
    fn into_json_value(self) -> JsonValue {
        JsonValue::Array(
            self.into_iter()
                .map(IntoJsonValue::into_json_value)
                .collect(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonError {
    UnexpectedEof,
    UnexpectedToken,
    InvalidNumber,
    InvalidString,
    InvalidUnicodeEscape,
    TrailingCharacters,
    DepthLimitExceeded,
}

struct JsonParser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    #[inline]
    fn skip_ws(&mut self) {
        while self.pos < self.input.len()
            && matches!(self.input[self.pos], b' ' | b'\t' | b'\r' | b'\n')
        {
            self.pos += 1;
        }
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        if depth > MAX_PARSE_DEPTH {
            return Err(JsonError::DepthLimitExceeded);
        }
        self.skip_ws();
        let b = self.peek().ok_or(JsonError::UnexpectedEof)?;
        match b {
            b'n' => {
                if self.input[self.pos..].starts_with(b"null") {
                    self.pos += 4;
                    Ok(JsonValue::Null)
                } else {
                    Err(JsonError::UnexpectedToken)
                }
            }
            b't' => {
                if self.input[self.pos..].starts_with(b"true") {
                    self.pos += 4;
                    Ok(JsonValue::Bool(true))
                } else {
                    Err(JsonError::UnexpectedToken)
                }
            }
            b'f' => {
                if self.input[self.pos..].starts_with(b"false") {
                    self.pos += 5;
                    Ok(JsonValue::Bool(false))
                } else {
                    Err(JsonError::UnexpectedToken)
                }
            }
            b'"' => self.parse_string().map(JsonValue::String),
            b'[' => self.parse_array(depth + 1),
            b'{' => self.parse_object(depth + 1),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(JsonError::UnexpectedToken),
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.pos += 1;
        let start = self.pos;

        loop {
            let rel = memchr::memchr2(b'"', b'\\', &self.input[self.pos..])
                .ok_or(JsonError::UnexpectedEof)?;
            let idx = self.pos + rel;

            if self.input[self.pos..idx].iter().any(|&c| c < 0x20) {
                return Err(JsonError::InvalidString);
            }

            if self.input[idx] == b'"' {
                let s = std::str::from_utf8(&self.input[start..idx])
                    .map_err(|_| JsonError::InvalidString)?
                    .to_owned();
                self.pos = idx + 1;
                return Ok(s);
            } else {
                break;
            }
        }

        let mut out = String::new();
        self.pos = start;

        loop {
            let rel = memchr::memchr2(b'"', b'\\', &self.input[self.pos..])
                .ok_or(JsonError::UnexpectedEof)?;
            let idx = self.pos + rel;

            let chunk = &self.input[self.pos..idx];
            if chunk.iter().any(|&c| c < 0x20) {
                return Err(JsonError::InvalidString);
            }
            let s = std::str::from_utf8(chunk).map_err(|_| JsonError::InvalidString)?;
            out.push_str(s);

            if self.input[idx] == b'"' {
                self.pos = idx + 1;
                return Ok(out);
            }

            self.pos = idx + 1;
            let esc = self.peek().ok_or(JsonError::UnexpectedEof)?;
            self.pos += 1;
            match esc {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'b' => out.push('\x08'),
                b'f' => out.push('\x0c'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    let code = self.parse_hex4()?;
                    if (0xD800..=0xDBFF).contains(&code) {
                        if !self.input[self.pos..].starts_with(b"\\u") {
                            return Err(JsonError::InvalidUnicodeEscape);
                        }
                        self.pos += 2;
                        let low = self.parse_hex4()?;
                        if !(0xDC00..=0xDFFF).contains(&low) {
                            return Err(JsonError::InvalidUnicodeEscape);
                        }
                        let cp = 0x10000 + (((code as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                        let ch = char::from_u32(cp).ok_or(JsonError::InvalidUnicodeEscape)?;
                        out.push(ch);
                    } else if (0xDC00..=0xDFFF).contains(&code) {
                        return Err(JsonError::InvalidUnicodeEscape);
                    } else {
                        let ch =
                            char::from_u32(code as u32).ok_or(JsonError::InvalidUnicodeEscape)?;
                        out.push(ch);
                    }
                }
                _ => return Err(JsonError::InvalidString),
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u16, JsonError> {
        if self.pos + 4 > self.input.len() {
            return Err(JsonError::InvalidUnicodeEscape);
        }
        let mut val = 0u16;
        for &b in &self.input[self.pos..self.pos + 4] {
            let digit = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(JsonError::InvalidUnicodeEscape),
            };
            val = (val << 4) | (digit as u16);
        }
        self.pos += 4;
        Ok(val)
    }

    fn parse_number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(JsonError::InvalidNumber);
                }
            }
            Some(b'1'..=b'9') => {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(JsonError::InvalidNumber),
        }

        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JsonError::InvalidNumber);
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JsonError::InvalidNumber);
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        let num_str = unsafe { std::str::from_utf8_unchecked(&self.input[start..self.pos]) };
        if is_float {
            let f: f64 = num_str.parse().map_err(|_| JsonError::InvalidNumber)?;
            if !f.is_finite() {
                return Err(JsonError::InvalidNumber);
            }
            Ok(JsonValue::Float(f))
        } else if num_str.starts_with('-') {
            if let Ok(n) = num_str.parse::<i64>() {
                Ok(JsonValue::Int(n))
            } else {
                let f: f64 = num_str.parse().map_err(|_| JsonError::InvalidNumber)?;
                Ok(JsonValue::Float(f))
            }
        } else if let Ok(n) = num_str.parse::<u64>() {
            Ok(JsonValue::Uint(n))
        } else {
            let f: f64 = num_str.parse().map_err(|_| JsonError::InvalidNumber)?;
            Ok(JsonValue::Float(f))
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        self.pos += 1;
        self.skip_ws();
        let mut arr = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(arr));
        }

        loop {
            arr.push(self.parse_value(depth)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JsonValue::Array(arr));
                }
                _ => return Err(JsonError::UnexpectedToken),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        self.pos += 1;
        self.skip_ws();
        let mut obj = Vec::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(obj));
        }

        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(JsonError::UnexpectedToken);
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(JsonError::UnexpectedToken);
            }
            self.pos += 1;
            let val = self.parse_value(depth)?;
            obj.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsonValue::Object(obj));
                }
                _ => return Err(JsonError::UnexpectedToken),
            }
        }
    }
}

#[macro_export]
macro_rules! json_raw {
    ($($tt:tt)+) => {{
        let mut __buf = ::std::vec::Vec::with_capacity(128);
        $crate::__json_raw_write!(__buf, $($tt)+);
        $crate::protocol::body::json::RawJson($crate::__bytes::Bytes::from(__buf))
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __json_raw_write {
    ($buf:ident, null) => {
        $buf.extend_from_slice(b"null");
    };
    ($buf:ident, true) => {
        $buf.extend_from_slice(b"true");
    };
    ($buf:ident, false) => {
        $buf.extend_from_slice(b"false");
    };
    ($buf:ident, []) => {
        $buf.extend_from_slice(b"[]");
    };
    ($buf:ident, [ $($tt:tt)+ ]) => {
        $buf.push(b'[');
        $crate::__json_raw_array!($buf false () $($tt)+);
        $buf.push(b']');
    };
    ($buf:ident, {}) => {
        $buf.extend_from_slice(b"{}");
    };
    ($buf:ident, { $($tt:tt)+ }) => {
        $buf.push(b'{');
        $crate::__json_raw_object!($buf false () $($tt)+);
        $buf.push(b'}');
    };
    ($buf:ident, $other:expr) => {
        $crate::protocol::body::json::ToJson::write_json(&($other), &mut $buf);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __json_raw_array {
    ($buf:ident $has_prev:ident ()) => {};
    ($buf:ident $has_prev:ident ($($cur:tt)+)) => {
        if $has_prev {
            $buf.push(b',');
        }
        $crate::__json_raw_write!($buf, $($cur)+);
    };
    ($buf:ident $has_prev:ident ($($cur:tt)+) , $($rest:tt)*) => {
        if $has_prev {
            $buf.push(b',');
        }
        $crate::__json_raw_write!($buf, $($cur)+);
        $crate::__json_raw_array!($buf true () $($rest)*);
    };
    ($buf:ident $has_prev:ident ($($cur:tt)*) $next:tt $($rest:tt)*) => {
        $crate::__json_raw_array!($buf $has_prev ($($cur)* $next) $($rest)*);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __json_raw_object {
    ($buf:ident $has_prev:ident ()) => {};
    ($buf:ident $has_prev:ident () $key:literal : $($rest:tt)*) => {
        if $has_prev {
            $buf.push(b',');
        }
        $crate::protocol::body::json::write_json_str($key, &mut $buf);
        $buf.push(b':');
        $crate::__json_raw_object!(@val $buf () $($rest)*);
    };
    ($buf:ident $has_prev:ident () ($key:expr) : $($rest:tt)*) => {
        if $has_prev {
            $buf.push(b',');
        }
        $crate::protocol::body::json::write_json_str(&($key), &mut $buf);
        $buf.push(b':');
        $crate::__json_raw_object!(@val $buf () $($rest)*);
    };
    (@val $buf:ident ($($val:tt)+)) => {
        $crate::__json_raw_write!($buf, $($val)+);
    };
    (@val $buf:ident ($($val:tt)+) , $($rest:tt)*) => {
        $crate::__json_raw_write!($buf, $($val)+);
        $crate::__json_raw_object!($buf true () $($rest)*);
    };
    (@val $buf:ident ($($val:tt)*) $next:tt $($rest:tt)*) => {
        $crate::__json_raw_object!(@val $buf ($($val)* $next) $($rest)*);
    };
}

#[macro_export]
macro_rules! impl_to_json {
    ($ty:ty { $($field:ident),* $(,)? }) => {
        impl $crate::protocol::body::json::ToJson for $ty {
            fn write_json(&self, out: &mut ::std::vec::Vec<u8>) {
                out.push(b'{');
                let mut __first = true;
                $(
                    if !__first {
                        out.push(b',');
                    }
                    __first = false;
                    $crate::protocol::body::json::write_json_str(stringify!($field), out);
                    out.push(b':');
                    $crate::protocol::body::json::ToJson::write_json(&self.$field, out);
                )*
                out.push(b'}');
            }
        }
    };
}
