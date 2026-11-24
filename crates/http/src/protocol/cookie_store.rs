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

use bytes::{BufMut, Bytes, BytesMut};
use hashbrown::HashMap;
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl SameSite {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            SameSite::Strict => "Strict",
            SameSite::Lax => "Lax",
            SameSite::None => "None",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum CookieExpiration {
    Seconds(u64),
    Minutes(u64),
    Hours(u64),
    Days(u64),
    Months(u64),
    Years(u64),
    At(u64),
}

impl CookieExpiration {
    pub(crate) fn as_secs_duration(&self) -> u64 {
        match self {
            CookieExpiration::Seconds(s) => *s,
            CookieExpiration::Minutes(m) => m * 60,
            CookieExpiration::Hours(h) => h * 3_600,
            CookieExpiration::Days(d) => d * 86_400,
            CookieExpiration::Months(m) => m * 2_592_000,
            CookieExpiration::Years(y) => y * 31_536_000,
            CookieExpiration::At(ts) => {
                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                ts.saturating_sub(now)
            }
        }
    }

    pub(crate) fn timestamp(&self) -> u64 {
        if let CookieExpiration::At(ts) = self {
            return *ts;
        }

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        now + self.as_secs_duration()
    }
}

pub trait ExpireExt {
    fn seconds(self) -> CookieExpiration;
    fn minutes(self) -> CookieExpiration;
    fn hours(self) -> CookieExpiration;
    fn days(self) -> CookieExpiration;
    fn months(self) -> CookieExpiration;
    fn years(self) -> CookieExpiration;
}

impl ExpireExt for u64 {
    fn seconds(self) -> CookieExpiration {
        CookieExpiration::Seconds(self)
    }
    fn minutes(self) -> CookieExpiration {
        CookieExpiration::Minutes(self)
    }
    fn hours(self) -> CookieExpiration {
        CookieExpiration::Hours(self)
    }
    fn days(self) -> CookieExpiration {
        CookieExpiration::Days(self)
    }
    fn months(self) -> CookieExpiration {
        CookieExpiration::Months(self)
    }
    fn years(self) -> CookieExpiration {
        CookieExpiration::Years(self)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CookieOptions {
    pub domain: Option<String>,
    pub path: Option<String>,
    pub expires: Option<CookieExpiration>,
    pub max_age: Option<CookieExpiration>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<SameSite>,
}

#[derive(Debug, Clone)]
pub(crate) struct Cookie {
    pub value: String,
    pub options: Option<CookieOptions>,
}

pub(crate) struct CookieStore {
    map: HashMap<String, Cookie>,
}

impl CookieStore {
    pub(crate) fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub(crate) fn set<K, V>(&mut self, key: K, value: V, options: Option<CookieOptions>)
    where
        K: Into<String>,
        V: Into<String>,
    {
        let mut k: String = key.into();
        let trimmed = k.trim();
        if trimmed.len() != k.len() {
            k = trimmed.to_owned();
        }
        let v: String = value.into();

        if !crate::protocol::field_lines::is_valid_header_name(k.as_bytes())
            || has_invalid_cookie_bytes(v.as_bytes())
        {
            return;
        }

        self.map.insert(k, Cookie { value: v, options });
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(|c| c.value.as_str())
    }

    pub(crate) fn remove(&mut self, key: &str) -> Option<String> {
        self.map.remove(key).map(|c| c.value)
    }

    pub(crate) fn to_bytes(&self) -> Bytes {
        if self.map.is_empty() {
            return Bytes::new();
        }

        let mut buf = BytesMut::with_capacity(self.map.len() * 64);

        for (cookie_name, cookie) in self.map.iter() {
            buf.put_slice(b"Set-Cookie: ");
            buf.put_slice(cookie_name.as_bytes());
            buf.put_slice(b"=");
            buf.put_slice(cookie.value.as_bytes());

            if let Some(options) = &cookie.options {
                if let Some(domain) = &options.domain {
                    if !has_invalid_cookie_bytes(domain.as_bytes()) {
                        buf.put_slice(b"; Domain=");
                        buf.put_slice(domain.as_bytes());
                    }
                }

                if let Some(path) = &options.path {
                    if !has_invalid_cookie_bytes(path.as_bytes()) {
                        buf.put_slice(b"; Path=");
                        buf.put_slice(path.as_bytes());
                    }
                }

                if let Some(expires) = &options.expires {
                    buf.put_slice(b"; Expires=");
                    let mut date_buf = [0u8; 29];
                    format_http_date_bytes(expires.timestamp(), &mut date_buf);
                    buf.put_slice(&date_buf);
                }

                if let Some(max_age) = &options.max_age {
                    buf.put_slice(b"; Max-Age=");
                    let mut num_buf = [0u8; 20];
                    buf.put_slice(format_u64(max_age.as_secs_duration(), &mut num_buf));
                }

                if options.secure {
                    buf.put_slice(b"; Secure");
                }

                if options.http_only {
                    buf.put_slice(b"; HttpOnly");
                }

                if let Some(same_site) = &options.same_site {
                    buf.put_slice(b"; SameSite=");
                    buf.put_slice(same_site.as_str().as_bytes());
                }
            }

            buf.put_slice(b"\r\n");
        }

        buf.freeze()
    }
}

impl From<&[u8]> for CookieStore {
    fn from(bytes: &[u8]) -> Self {
        #[inline(always)]
        fn trim_ascii(mut s: &[u8]) -> &[u8] {
            while !s.is_empty() && (s[0] == b' ' || s[0] == b'\t') {
                s = &s[1..];
            }
            while !s.is_empty() && (s[s.len() - 1] == b' ' || s[s.len() - 1] == b'\t') {
                s = &s[..s.len() - 1];
            }
            s
        }

        let end = bytes.len();
        let mut map: HashMap<String, Cookie> = HashMap::with_capacity(8);

        let mut i: usize = 0;
        while i < end {
            let pair_end = match memchr::memchr(b';', &bytes[i..end]) {
                Some(pos) => i + pos,
                None => end,
            };

            let pair = trim_ascii(&bytes[i..pair_end]);
            if !pair.is_empty() {
                let (name_bytes, value_bytes) = match memchr::memchr(b'=', pair) {
                    Some(pos) => (trim_ascii(&pair[..pos]), trim_ascii(&pair[pos + 1..])),
                    None => (pair, &[][..]),
                };

                if !name_bytes.is_empty() {
                    if let (Ok(cookie_name), Ok(cookie_value)) = (
                        std::str::from_utf8(name_bytes),
                        std::str::from_utf8(value_bytes),
                    ) {
                        map.insert(
                            cookie_name.to_owned(),
                            Cookie {
                                value: cookie_value.to_owned(),
                                options: None,
                            },
                        );
                    }
                }
            }

            i = pair_end + 1;
        }

        Self { map }
    }
}

#[inline]
fn format_u64(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
    if n == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = 20;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    &buf[i..]
}

#[inline]
fn write_two_digits(val: u32, dst: &mut [u8]) {
    dst[0] = b'0' + (val / 10) as u8;
    dst[1] = b'0' + (val % 10) as u8;
}

#[inline]
pub(crate) fn format_http_date_bytes(secs: u64, out: &mut [u8; 29]) {
    const DAYS: [&[u8; 3]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
    const MONTHS: [&[u8; 3]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];

    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;

    let wday = ((days + 4).rem_euclid(7)) as usize;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + if month <= 2 { 1 } else { 0 }) as u32;

    out[0..3].copy_from_slice(DAYS[wday]);
    out[3..5].copy_from_slice(b", ");
    write_two_digits(day, &mut out[5..7]);
    out[7] = b' ';
    out[8..11].copy_from_slice(MONTHS[(month - 1) as usize]);
    out[11] = b' ';
    write_two_digits(year / 100, &mut out[12..14]);
    write_two_digits(year % 100, &mut out[14..16]);
    out[16] = b' ';
    write_two_digits(hour, &mut out[17..19]);
    out[19] = b':';
    write_two_digits(min, &mut out[20..22]);
    out[22] = b':';
    write_two_digits(sec, &mut out[23..25]);
    out[25..29].copy_from_slice(b" GMT");
}

#[inline]
fn has_invalid_cookie_bytes(bytes: &[u8]) -> bool {
    bytes.iter().any(|&b| b < 0x20 || b == 0x7F || b == b';')
}
