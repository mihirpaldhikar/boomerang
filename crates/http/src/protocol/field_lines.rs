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

#[inline]
fn known_slot(name: &[u8]) -> Option<usize> {
    match name.len() {
        4 if name.eq_ignore_ascii_case(b"host") => Some(SLOT_HOST),
        6 => {
            if name.eq_ignore_ascii_case(b"origin") {
                Some(SLOT_ORIGIN)
            } else if name.eq_ignore_ascii_case(b"cookie") {
                Some(SLOT_COOKIE)
            } else if name.eq_ignore_ascii_case(b"expect") {
                Some(SLOT_EXPECT)
            } else {
                None
            }
        }
        10 if name.eq_ignore_ascii_case(b"connection") => Some(SLOT_CONNECTION),
        12 if name.eq_ignore_ascii_case(b"content-type") => Some(SLOT_CONTENT_TYPE),
        14 if name.eq_ignore_ascii_case(b"content-length") => Some(SLOT_CONTENT_LENGTH),
        15 if name.eq_ignore_ascii_case(b"accept-encoding") => Some(SLOT_ACCEPT_ENCODING),
        17 if name.eq_ignore_ascii_case(b"transfer-encoding") => Some(SLOT_TRANSFER_ENCODING),
        29 if name.eq_ignore_ascii_case(b"access-control-request-method") => {
            Some(SLOT_AC_REQ_METHOD)
        }
        _ => None,
    }
}

#[inline(always)]
fn header_bloom_bit(name: &[u8]) -> u64 {
    let h = ((name.len()).wrapping_mul(31)) ^ (name[0].to_ascii_lowercase() as usize);
    1u64 << (h & 63)
}

pub(crate) struct FieldLines {
    arena: Vec<u8>,
    headers: Vec<(u32, u32, u32, u32)>,
    known: [u16; 10],
}

impl Default for FieldLines {
    fn default() -> Self {
        Self {
            arena: Vec::new(),
            headers: Vec::new(),
            known: [u16::MAX; 10],
        }
    }
}

impl From<&[u8]> for FieldLines {
    fn from(bytes: &[u8]) -> Self {
        let mut fl = Self::with_capacity(16, bytes.len());
        fl.parse_into(bytes);
        fl
    }
}

impl FieldLines {
    #[inline]
    pub(crate) fn is_reserved(name: &str) -> bool {
        match name.len() {
            4 => name.eq_ignore_ascii_case("date"),
            6 => name.eq_ignore_ascii_case("cookie"),
            10 => {
                name.eq_ignore_ascii_case("set-cookie")
                    || name.eq_ignore_ascii_case("connection")
                    || name.eq_ignore_ascii_case("keep-alive")
            }
            12 => {
                name.eq_ignore_ascii_case("content-type")
                    || name.eq_ignore_ascii_case("x-request-id")
            }
            13 => {
                name.eq_ignore_ascii_case("content-range")
                    || name.eq_ignore_ascii_case("accept-ranges")
            }
            14 => name.eq_ignore_ascii_case("content-length"),
            15 => name.eq_ignore_ascii_case("accept-encoding"),
            17 => name.eq_ignore_ascii_case("transfer-encoding"),
            22 => name.eq_ignore_ascii_case("access-control-max-age"),
            27 => name.eq_ignore_ascii_case("access-control-allow-origin"),
            28 => name.eq_ignore_ascii_case("access-control-allow-methods"),
            29 => name.eq_ignore_ascii_case("access-control-expose-headers"),
            30 => name.eq_ignore_ascii_case("access-control-request-headers"),
            32 => name.eq_ignore_ascii_case("access-control-allow-credentials"),
            _ => false,
        }
    }
}

impl FieldLines {
    pub(crate) fn with_capacity(headers_cap: usize, arena_cap: usize) -> Self {
        Self {
            arena: Vec::with_capacity(arena_cap),
            headers: Vec::with_capacity(headers_cap),
            known: [u16::MAX; 10],
        }
    }

    #[inline]
    pub(crate) fn clear(&mut self) {
        self.arena.clear();
        self.headers.clear();
        self.known = [u16::MAX; 10];
    }

    #[inline]
    pub(crate) fn arena_capacity(&self) -> usize {
        self.arena.capacity()
    }

    pub(crate) fn parse_into(&mut self, bytes: &[u8]) {
        self.clear();

        #[inline(always)]
        fn trim_ascii(mut s: &[u8]) -> &[u8] {
            while !s.is_empty() && (s[0] == b' ' || s[0] == b'\t') {
                s = &s[1..];
            }
            while !s.is_empty()
                && (s[s.len() - 1] == b' ' || s[s.len() - 1] == b'\t' || s[s.len() - 1] == b'\r')
            {
                s = &s[..s.len() - 1];
            }
            s
        }

        let mut end = bytes.len();
        if end >= 4 && &bytes[end - 4..end] == b"\r\n\r\n" {
            end -= 4;
        } else if end >= 2 && (&bytes[end - 2..end] == b"\n\n" || &bytes[end - 2..end] == b"\r\n") {
            end -= 2;
        } else if end >= 1 && &bytes[end - 1..end] == b"\n" {
            end -= 1;
        }

        let all_ascii = bytes[..end].is_ascii();
        self.arena.reserve(end);

        let mut seen_mask: u64 = 0;
        let mut i = 0;
        while i < end {
            let line_end = match memchr::memchr(b'\n', &bytes[i..end]) {
                Some(pos) => i + pos,
                None => end,
            };

            let line = &bytes[i..line_end];
            if line.is_empty() || line == b"\r" {
                i = line_end + 1;
                continue;
            }

            if let Some(colon_pos) = memchr::memchr(b':', line) {
                let name_bytes = trim_ascii(&line[..colon_pos]);
                let val_bytes = trim_ascii(&line[colon_pos + 1..]);

                if !name_bytes.is_empty()
                    && (all_ascii
                        || (std::str::from_utf8(name_bytes).is_ok()
                            && std::str::from_utf8(val_bytes).is_ok()))
                {
                    let name_len = name_bytes.len() as u32;
                    let slot = known_slot(name_bytes);

                    let existing_idx = if let Some(s) = slot {
                        let idx = self.known[s];
                        if idx != u16::MAX {
                            Some(idx as usize)
                        } else {
                            None
                        }
                    } else {
                        let bit = header_bloom_bit(name_bytes);
                        if (seen_mask & bit) != 0 {
                            self.headers.iter().position(|&(np, nl, _, _)| {
                                nl == name_len
                                    && self.arena[np as usize..(np + nl) as usize]
                                        .eq_ignore_ascii_case(name_bytes)
                            })
                        } else {
                            seen_mask |= bit;
                            None
                        }
                    };

                    if let Some(idx) = existing_idx {
                        let (_, _, vp, vl) = self.headers[idx];
                        let old_vp = vp as usize;
                        let old_vl = vl as usize;

                        let new_vp = self.arena.len() as u32;
                        self.arena.reserve(old_vl + 2 + val_bytes.len());
                        self.arena.extend_from_within(old_vp..old_vp + old_vl);
                        self.arena.extend_from_slice(b", ");
                        self.arena.extend_from_slice(val_bytes);

                        self.headers[idx].2 = new_vp;
                        self.headers[idx].3 = self.arena.len() as u32 - new_vp;
                    } else {
                        let new_idx = self.headers.len();
                        let np = self.arena.len() as u32;
                        self.arena.extend_from_slice(name_bytes);
                        self.arena[np as usize..].make_ascii_lowercase();

                        let vp = self.arena.len() as u32;
                        self.arena.extend_from_slice(val_bytes);

                        self.headers
                            .push((np, name_len, vp, self.arena.len() as u32 - vp));

                        if let Some(s) = slot {
                            if new_idx < u16::MAX as usize {
                                self.known[s] = new_idx as u16;
                            }
                        }
                    }
                }
            }

            i = line_end + 1;
        }
    }

    pub(crate) fn set<K: AsRef<str>, V: AsRef<str>>(&mut self, key: K, value: V) {
        let key_bytes = key.as_ref().as_bytes();
        let val_bytes = value.as_ref().as_bytes();
        if key_bytes.is_empty() {
            return;
        }
        let key_len = key_bytes.len() as u32;
        let slot = known_slot(key_bytes);

        let existing_idx = if let Some(s) = slot {
            let idx = self.known[s];
            if idx != u16::MAX {
                Some(idx as usize)
            } else {
                None
            }
        } else {
            self.headers.iter().position(|&(np, nl, _, _)| {
                nl == key_len
                    && self.arena[np as usize..(np + nl) as usize].eq_ignore_ascii_case(key_bytes)
            })
        };

        if let Some(idx) = existing_idx {
            let (_, _, vp, vl) = self.headers[idx];
            let old_vp = vp as usize;
            let old_vl = vl as usize;

            let new_vp = self.arena.len() as u32;
            self.arena.reserve(old_vl + 2 + val_bytes.len());
            self.arena.extend_from_within(old_vp..old_vp + old_vl);
            self.arena.extend_from_slice(b", ");
            self.arena.extend_from_slice(val_bytes);

            let new_vl = self.arena.len() as u32 - new_vp;
            self.headers[idx].2 = new_vp;
            self.headers[idx].3 = new_vl;
        } else {
            let new_idx = self.headers.len();
            let np = self.arena.len() as u32;
            self.arena.reserve(key_bytes.len() + val_bytes.len());
            self.arena.extend_from_slice(key_bytes);
            self.arena[np as usize..].make_ascii_lowercase();

            let vp = self.arena.len() as u32;
            self.arena.extend_from_slice(val_bytes);
            let vl = val_bytes.len() as u32;

            self.headers.push((np, key_len, vp, vl));

            if let Some(s) = slot {
                if new_idx < u16::MAX as usize {
                    self.known[s] = new_idx as u16;
                }
            }
        }
    }

    #[inline]
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        let key_bytes = key.as_bytes();
        if let Some(slot) = known_slot(key_bytes) {
            let idx = self.known[slot];
            if idx == u16::MAX {
                return None;
            }
            let (_, _, vp, vl) = self.headers[idx as usize];
            return Some(unsafe {
                std::str::from_utf8_unchecked(&self.arena[vp as usize..(vp + vl) as usize])
            });
        }

        let key_len = key_bytes.len() as u32;
        for &(np, nl, vp, vl) in &self.headers {
            if nl == key_len {
                let name = &self.arena[np as usize..(np + nl) as usize];
                if name.eq_ignore_ascii_case(key_bytes) {
                    return Some(unsafe {
                        std::str::from_utf8_unchecked(&self.arena[vp as usize..(vp + vl) as usize])
                    });
                }
            }
        }
        None
    }

    pub(crate) fn remove(&mut self, key: &str) -> Option<String> {
        let key_bytes = key.as_bytes();
        let idx = if let Some(s) = known_slot(key_bytes) {
            let i = self.known[s];
            if i == u16::MAX {
                return None;
            }
            i as usize
        } else {
            let key_len = key_bytes.len() as u32;
            self.headers.iter().position(|&(np, nl, _, _)| {
                nl == key_len
                    && self.arena[np as usize..(np + nl) as usize].eq_ignore_ascii_case(key_bytes)
            })?
        };

        let (_, _, vp, vl) = self.headers.remove(idx);
        for slot in &mut self.known {
            if *slot == idx as u16 {
                *slot = u16::MAX;
            } else if *slot != u16::MAX && (*slot as usize) > idx {
                *slot -= 1;
            }
        }

        String::from_utf8(self.arena[vp as usize..(vp + vl) as usize].to_vec()).ok()
    }

    pub(crate) fn to_bytes(&self) -> bytes::Bytes {
        let mut total_size = 0;
        for &(_, nl, _, vl) in &self.headers {
            total_size += nl as usize + vl as usize + 4;
        }

        let mut buf = bytes::BytesMut::with_capacity(total_size);
        for &(np, nl, vp, vl) in &self.headers {
            buf.extend_from_slice(&self.arena[np as usize..(np + nl) as usize]);
            buf.extend_from_slice(b": ");
            buf.extend_from_slice(&self.arena[vp as usize..(vp + vl) as usize]);
            buf.extend_from_slice(b"\r\n");
        }
        buf.freeze()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers.iter().map(move |&(np, nl, vp, vl)| {
            let k = unsafe {
                std::str::from_utf8_unchecked(&self.arena[np as usize..(np + nl) as usize])
            };
            let v = unsafe {
                std::str::from_utf8_unchecked(&self.arena[vp as usize..(vp + vl) as usize])
            };
            (k, v)
        })
    }
}
