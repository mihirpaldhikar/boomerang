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
use crate::protocol::HttpRequestError;

const SLOT_HOST: usize = 0;
const SLOT_CONNECTION: usize = 1;
const SLOT_CONTENT_LENGTH: usize = 2;
const SLOT_TRANSFER_ENCODING: usize = 3;
const SLOT_CONTENT_TYPE: usize = 4;
const SLOT_ACCEPT_ENCODING: usize = 5;
const SLOT_ORIGIN: usize = 6;
const SLOT_COOKIE: usize = 7;
const SLOT_EXPECT: usize = 8;
const SLOT_AC_REQ_METHOD: usize = 9;
const SLOT_FORWARDED: usize = 10;
const SLOT_X_REAL_IP: usize = 11;
const SLOT_X_FORWARDED_FOR: usize = 12;
const SLOT_CF_CONNECTING_IP: usize = 13;
const SLOT_COUNT: usize = 14;

const NO_FIELD: u16 = u16::MAX;
const MAX_HEADER_LINES: usize = 100;

const TCHAR_TABLE: [bool; 256] = {
    let mut t = [false; 256];
    let mut i = 0usize;
    while i < 256 {
        let b = i as u8;
        t[i] = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            );
        i += 1;
    }
    t
};

#[inline]
fn known_slot(name: &[u8]) -> Option<usize> {
    let is = |s: &[u8]| name.eq_ignore_ascii_case(s);
    match name.len() {
        4 if is(b"host") => Some(SLOT_HOST),
        6 if is(b"origin") => Some(SLOT_ORIGIN),
        6 if is(b"cookie") => Some(SLOT_COOKIE),
        6 if is(b"expect") => Some(SLOT_EXPECT),
        9 if is(b"forwarded") => Some(SLOT_FORWARDED),
        9 if is(b"x-real-ip") => Some(SLOT_X_REAL_IP),
        10 if is(b"connection") => Some(SLOT_CONNECTION),
        12 if is(b"content-type") => Some(SLOT_CONTENT_TYPE),
        14 if is(b"content-length") => Some(SLOT_CONTENT_LENGTH),
        15 if is(b"accept-encoding") => Some(SLOT_ACCEPT_ENCODING),
        15 if is(b"x-forwarded-for") => Some(SLOT_X_FORWARDED_FOR),
        16 if is(b"cf-connecting-ip") => Some(SLOT_CF_CONNECTING_IP),
        17 if is(b"transfer-encoding") => Some(SLOT_TRANSFER_ENCODING),
        29 if is(b"access-control-request-method") => Some(SLOT_AC_REQ_METHOD),
        _ => None,
    }
}

#[inline(always)]
fn header_bloom_bit(name: &[u8]) -> u64 {
    let h = name.len().wrapping_mul(31) ^ usize::from(name[0].to_ascii_lowercase());
    1u64 << (h & 63)
}

#[inline]
fn trim_ows(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

fn parse_error(msg: &'static str) -> HttpRequestError {
    HttpRequestError::HeaderParsingFailed(msg.into())
}

#[derive(Clone, Copy)]
struct Field {
    name_start: u32,
    name_len: u32,
    value_start: u32,
    value_len: u32,
}

pub(crate) struct FieldLines {
    arena: Vec<u8>,
    headers: Vec<Field>,
    known: [u16; SLOT_COUNT],
}

impl Default for FieldLines {
    fn default() -> Self {
        Self::with_capacity(0, 0)
    }
}

impl From<&[u8]> for FieldLines {
    fn from(bytes: &[u8]) -> Self {
        let mut fl = Self::with_capacity(16, bytes.len());
        if fl.parse_into(bytes).is_err() {
            fl.clear();
        }
        fl
    }
}

impl FieldLines {
    #[inline]
    pub(crate) fn is_reserved(name: &str) -> bool {
        let is = |s: &str| name.eq_ignore_ascii_case(s);
        match name.len() {
            4 => is("date"),
            6 => is("cookie"),
            10 => is("set-cookie") || is("connection") || is("keep-alive"),
            12 => is("content-type") || is("x-request-id"),
            13 => is("content-range") || is("accept-ranges"),
            14 => is("content-length"),
            15 => is("accept-encoding"),
            17 => is("transfer-encoding"),
            22 => is("access-control-max-age"),
            27 => is("access-control-allow-origin"),
            28 => is("access-control-allow-methods") || is("access-control-allow-headers"),
            29 => is("access-control-expose-headers"),
            30 => is("access-control-request-headers"),
            32 => is("access-control-allow-credentials"),
            _ => false,
        }
    }

    pub(crate) fn with_capacity(headers_cap: usize, arena_cap: usize) -> Self {
        Self {
            arena: Vec::with_capacity(arena_cap),
            headers: Vec::with_capacity(headers_cap),
            known: [NO_FIELD; SLOT_COUNT],
        }
    }

    #[inline]
    pub(crate) fn clear(&mut self) {
        self.arena.clear();
        self.headers.clear();
        self.known = [NO_FIELD; SLOT_COUNT];
    }

    #[inline]
    pub(crate) fn arena_capacity(&self) -> usize {
        self.arena.capacity()
    }

    pub(crate) fn parse_into(&mut self, bytes: &[u8]) -> Result<(), HttpRequestError> {
        self.clear();

        if !bytes.ends_with(b"\r\n") {
            return Err(parse_error("headers missing CRLF terminator"));
        }
        let end = bytes.len() - 2;
        if end == 0 {
            return Ok(());
        }

        let all_ascii = bytes[..end].is_ascii();
        self.arena.reserve(end);

        let mut seen_mask = 0u64;
        let mut lines = 0usize;
        let mut i = 0;
        while i < end {
            let line_end = memchr::memchr(b'\n', &bytes[i..end])
                .map(|off| i + off)
                .ok_or_else(|| parse_error("header line missing LF"))?;

            if line_end == i || bytes[line_end - 1] != b'\r' {
                return Err(parse_error("bare LF in headers"));
            }

            let line = &bytes[i..line_end - 1];
            i = line_end + 1;
            if line.is_empty() {
                return Err(parse_error("unexpected empty header line"));
            }

            lines += 1;
            if lines > MAX_HEADER_LINES {
                return Err(HttpRequestError::HeadersTooLarge);
            }

            let colon =
                memchr::memchr(b':', line).ok_or_else(|| parse_error("header missing colon"))?;

            let name = &line[..colon];
            if !is_valid_header_name(name) {
                return Err(parse_error("invalid header name"));
            }

            let value = trim_ows(&line[colon + 1..]);
            if has_invalid_header_bytes(value) {
                return Err(parse_error("invalid control byte in header value"));
            }
            if !all_ascii && std::str::from_utf8(value).is_err() {
                return Err(parse_error("invalid UTF-8 in header value"));
            }

            let slot = known_slot(name);
            let existing = match slot {
                Some(s) => self.known_index(s),
                None => {
                    let bit = header_bloom_bit(name);
                    let maybe_seen = seen_mask & bit != 0;
                    seen_mask |= bit;
                    if maybe_seen { self.scan(name) } else { None }
                }
            };

            match existing {
                None => self.insert(slot, name, value),
                Some(idx) => match slot {
                    Some(SLOT_HOST | SLOT_CONTENT_LENGTH) => {
                        return Err(parse_error("duplicate Host or Content-Length header"));
                    }
                    Some(SLOT_COOKIE) => self.append_value(idx, b"; ", value),
                    _ => self.append_value(idx, b", ", value),
                },
            }
        }

        Ok(())
    }

    pub(crate) fn set<K: AsRef<str>, V: AsRef<str>>(&mut self, key: K, value: V) {
        let name = key.as_ref().as_bytes();
        let value = value.as_ref().as_bytes();

        if !is_valid_header_name(name) || has_invalid_header_bytes(value) {
            return;
        }

        let slot = known_slot(name);
        let existing = match slot {
            Some(s) => self.known_index(s),
            None => self.scan(name),
        };

        match existing {
            Some(idx) => self.append_value(idx, b", ", value),
            None => self.insert(slot, name, value),
        }
    }

    #[inline]
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.find(key.as_bytes())
            .map(|idx| self.value(self.headers[idx]))
    }

    pub(crate) fn remove(&mut self, key: &str) -> Option<String> {
        let idx = self.find(key.as_bytes())?;
        let field = self.headers.remove(idx);

        for slot in &mut self.known {
            if *slot == NO_FIELD {
                continue;
            }
            match usize::from(*slot) {
                i if i == idx => *slot = NO_FIELD,
                i if i > idx => *slot -= 1,
                _ => {}
            }
        }

        Some(self.value(field).to_owned())
    }

    pub(crate) fn to_bytes(&self) -> bytes::Bytes {
        let total: usize = self
            .headers
            .iter()
            .map(|f| f.name_len as usize + f.value_len as usize + 4)
            .sum();

        let mut buf = bytes::BytesMut::with_capacity(total);
        for (name, value) in self.iter() {
            buf.extend_from_slice(name.as_bytes());
            buf.extend_from_slice(b": ");
            buf.extend_from_slice(value.as_bytes());
            buf.extend_from_slice(b"\r\n");
        }
        buf.freeze()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers
            .iter()
            .map(move |&f| (self.name(f), self.value(f)))
    }

    #[inline]
    fn name_bytes(&self, f: Field) -> &[u8] {
        &self.arena[f.name_start as usize..(f.name_start + f.name_len) as usize]
    }

    #[inline]
    fn name(&self, f: Field) -> &str {
        // SAFETY: `insert` only receives validated token names, which are ASCII.
        unsafe { std::str::from_utf8_unchecked(self.name_bytes(f)) }
    }

    #[inline]
    fn value(&self, f: Field) -> &str {
        let range = f.value_start as usize..(f.value_start + f.value_len) as usize;
        // SAFETY: every value comes from a `&str` or was UTF-8 checked by the parser,
        // and joining values with ASCII separators keeps it valid.
        unsafe { std::str::from_utf8_unchecked(&self.arena[range]) }
    }

    #[inline]
    fn known_index(&self, slot: usize) -> Option<usize> {
        let idx = self.known[slot];
        (idx != NO_FIELD).then_some(usize::from(idx))
    }

    #[inline]
    fn scan(&self, name: &[u8]) -> Option<usize> {
        self.headers.iter().position(|&f| {
            f.name_len as usize == name.len() && self.name_bytes(f).eq_ignore_ascii_case(name)
        })
    }

    #[inline]
    fn find(&self, name: &[u8]) -> Option<usize> {
        match known_slot(name) {
            Some(slot) => self.known_index(slot),
            None => self.scan(name),
        }
    }

    fn insert(&mut self, slot: Option<usize>, name: &[u8], value: &[u8]) {
        self.arena.reserve(name.len() + value.len());
        let idx = self.headers.len();

        let name_start = self.arena.len();
        self.arena.extend_from_slice(name);
        self.arena[name_start..].make_ascii_lowercase();

        let value_start = self.arena.len();
        self.arena.extend_from_slice(value);

        self.headers.push(Field {
            name_start: name_start as u32,
            name_len: name.len() as u32,
            value_start: value_start as u32,
            value_len: value.len() as u32,
        });

        if let Some(s) = slot
            && idx < usize::from(NO_FIELD)
        {
            self.known[s] = idx as u16;
        }
    }

    fn append_value(&mut self, idx: usize, sep: &[u8], value: &[u8]) {
        let field = self.headers[idx];
        let len = field.value_len as usize;
        let mut start = field.value_start as usize;

        let extra = if start + len != self.arena.len() {
            len + sep.len() + value.len()
        } else {
            sep.len() + value.len()
        };
        self.arena.reserve(extra);

        if start + len != self.arena.len() {
            let new_start = self.arena.len();
            self.arena.extend_from_within(start..start + len);
            start = new_start;
        }
        self.arena.extend_from_slice(sep);
        self.arena.extend_from_slice(value);

        self.headers[idx].value_start = start as u32;
        self.headers[idx].value_len = (self.arena.len() - start) as u32;
    }
}

#[inline]
pub(crate) fn is_valid_header_name(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|&b| TCHAR_TABLE[usize::from(b)])
}

#[inline]
pub(crate) fn has_invalid_header_bytes(bytes: &[u8]) -> bool {
    bytes.iter().any(|&b| (b < 0x20 && b != b'\t') || b == 0x7F)
}
