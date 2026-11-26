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

use crate::protocol::body::Body;
use crate::protocol::field_lines::FieldLines;
use crate::protocol::method::HttpMethod;
use bytes::Bytes;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock, OnceLock};

static EMPTY_PARAM: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from(""));

#[derive(thiserror::Error, Debug)]
pub enum HttpRequestError {
    #[error("{0}")]
    RequestLineParsingFailed(String),
    #[error("{0}")]
    HeaderParsingFailed(String),
    #[error("{0}")]
    BodyParsingFailed(String),
    #[error("{0}")]
    RequestParsingFailed(String),
    #[error("invalid http method")]
    InvalidHttpMethod(),
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("request line too long")]
    RequestLineTooLong,
    #[error("headers too large")]
    HeadersTooLarge,
    #[error("io error: {0}")]
    Io(tokio::io::Error),
    #[error("request time out")]
    Timeout,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum HttpParam {
    Int(i64),
    Float(f64),
    Hex(u64),
    Bool(bool),
    IPv4(Ipv4Addr),
    IPv6(Ipv6Addr),
    Url(Bytes),
    Uuid([u8; 16]),
    Date {
        year: i32,
        month: u8,
        day: u8,
    },
    Time {
        hour: u8,
        minute: u8,
        second: u8,
        nanosecond: u32,
    },
    DateTime {
        unix_secs: i64,
        nanosecond: u32,
        offset_secs: i32,
    },
    Email(String),
    Text(String),
}

#[inline(always)]
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[inline]
fn decode_form_component(raw: &[u8], scratch: &mut Vec<u8>) -> Arc<str> {
    if raw.is_empty() {
        return EMPTY_PARAM.clone();
    }

    let Some(first) = memchr::memchr2(b'%', b'+', raw) else {
        // SAFETY: `raw` is sliced from a valid `&str` only on ASCII (`?`, `&`, `=`)
        // boundaries and contains neither `%` nor `+`.
        return Arc::from(unsafe { std::str::from_utf8_unchecked(raw) });
    };

    scratch.clear();
    scratch.reserve(raw.len());
    scratch.extend_from_slice(&raw[..first]);

    let mut i = first;
    while i < raw.len() {
        match raw[i] {
            b'+' => {
                scratch.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 < raw.len() {
                    if let (Some(hi), Some(lo)) = (hex_nibble(raw[i + 1]), hex_nibble(raw[i + 2])) {
                        scratch.push((hi << 4) | lo);
                        i += 3;
                        continue;
                    }
                }
                scratch.push(b'%');
                i += 1;
            }
            _ => {
                let next = memchr::memchr2(b'%', b'+', &raw[i..])
                    .map(|off| i + off)
                    .unwrap_or(raw.len());
                scratch.extend_from_slice(&raw[i..next]);
                i = next;
            }
        }
    }

    match std::str::from_utf8(scratch) {
        Ok(s) => Arc::from(s),
        Err(_) => Arc::from(String::from_utf8_lossy(scratch).as_ref()),
    }
}

impl HttpParam {
    pub(crate) fn parse_query_params(url: &str) -> Vec<(Arc<str>, Arc<str>)> {
        let bytes = url.as_bytes();
        let Some(q_pos) = memchr::memchr(b'?', bytes) else {
            return Vec::new();
        };

        let query = &bytes[q_pos + 1..];
        if query.is_empty() {
            return Vec::new();
        }

        let mut params = Vec::with_capacity(4);
        let mut scratch = Vec::new();
        let mut i = 0usize;

        while i < query.len() {
            let pair_end = memchr::memchr(b'&', &query[i..])
                .map(|off| i + off)
                .unwrap_or(query.len());

            let pair = &query[i..pair_end];
            if !pair.is_empty() {
                let (k_raw, v_raw) = match memchr::memchr(b'=', pair) {
                    Some(eq) => (&pair[..eq], &pair[eq + 1..]),
                    None => (pair, &[][..]),
                };
                let k = decode_form_component(k_raw, &mut scratch);
                let v = decode_form_component(v_raw, &mut scratch);
                params.push((k, v));
            }

            i = pair_end + 1;
        }

        params
    }
}

impl HttpParam {
    #[inline]
    pub(crate) fn parse_uuid_bytes(s: &str) -> Option<[u8; 16]> {
        let b = s.as_bytes();
        let mut out = [0u8; 16];

        match b.len() {
            32 => {
                for (i, slot) in out.iter_mut().enumerate() {
                    let hi = hex_nibble(b[i * 2])?;
                    let lo = hex_nibble(b[i * 2 + 1])?;
                    *slot = (hi << 4) | lo;
                }
                Some(out)
            }
            36 => {
                if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
                    return None;
                }
                let mut src = 0usize;
                for (i, slot) in out.iter_mut().enumerate() {
                    if i == 4 || i == 6 || i == 8 || i == 10 {
                        src += 1;
                    }
                    let hi = hex_nibble(b[src])?;
                    let lo = hex_nibble(b[src + 1])?;
                    *slot = (hi << 4) | lo;
                    src += 2;
                }
                Some(out)
            }
            _ => None,
        }
    }

    #[inline]
    pub(crate) fn parse_url_bytes(s: &str) -> Option<Bytes> {
        let bytes = s.as_bytes();
        let colon = memchr::memchr(b':', bytes)?;
        if colon == 0 {
            return None;
        }
        let scheme = &bytes[..colon];
        if !scheme[0].is_ascii_alphabetic()
            || !scheme[1..]
                .iter()
                .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
        {
            return None;
        }
        if bytes.iter().any(|&c| c <= 0x20 || c == 0x7F) {
            return None;
        }
        Some(Bytes::copy_from_slice(bytes))
    }
}

pub(crate) enum BodyState {
    Unread {
        content_length: usize,
        is_chunked: bool,
    },
    Reading,
    Read(Body),
}

#[inline]
pub(crate) fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpCidr {
    V4 { net: u32, mask: u32 },
    V6 { net: u128, mask: u128 },
}

impl IpCidr {
    pub fn from_v4(addr: Ipv4Addr, prefix: u8) -> Option<Self> {
        if prefix > 32 {
            return None;
        }
        let mask = if prefix == 0 {
            0
        } else {
            !0u32 << (32 - prefix)
        };
        Some(Self::V4 {
            net: u32::from(addr) & mask,
            mask,
        })
    }

    pub fn from_v6(addr: Ipv6Addr, prefix: u8) -> Option<Self> {
        if prefix > 128 {
            return None;
        }
        let mask = if prefix == 0 {
            0
        } else {
            !0u128 << (128 - prefix)
        };
        Some(Self::V6 {
            net: u128::from(addr) & mask,
            mask,
        })
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some((ip_str, prefix_str)) = s.split_once('/') {
            let prefix: u8 = prefix_str.parse().ok()?;
            match normalize_ip(ip_str.parse().ok()?) {
                IpAddr::V4(v4) => Self::from_v4(v4, prefix),
                IpAddr::V6(v6) => Self::from_v6(v6, prefix),
            }
        } else {
            match normalize_ip(s.parse().ok()?) {
                IpAddr::V4(v4) => Self::from_v4(v4, 32),
                IpAddr::V6(v6) => Self::from_v6(v6, 128),
            }
        }
    }

    #[inline]
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (*self, ip) {
            (Self::V4 { net, mask }, IpAddr::V4(v4)) => (u32::from(v4) & mask) == net,
            (Self::V6 { net, mask }, IpAddr::V6(v6)) => (u128::from(v6) & mask) == net,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyHeaderMode {
    /// Checks `X-Forwarded-For` -> RFC 7239 `Forwarded` -> `X-Real-IP`
    #[default]
    Auto,
    XForwardedFor,
    Forwarded,
    XRealIp,
    CfConnectingIp,
}

#[derive(Debug, Clone, Default)]
pub struct TrustedProxies {
    cidrs: Vec<IpCidr>,
    mode: ProxyHeaderMode,
}

impl TrustedProxies {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn add_private_networks(&mut self) {
        self.cidrs.extend([
            IpCidr::from_v4(Ipv4Addr::new(127, 0, 0, 0), 8).unwrap(),
            IpCidr::from_v4(Ipv4Addr::new(10, 0, 0, 0), 8).unwrap(),
            IpCidr::from_v4(Ipv4Addr::new(172, 16, 0, 0), 12).unwrap(),
            IpCidr::from_v4(Ipv4Addr::new(192, 168, 0, 0), 16).unwrap(),
            IpCidr::from_v4(Ipv4Addr::new(100, 64, 0, 0), 10).unwrap(),
            IpCidr::from_v6(Ipv6Addr::LOCALHOST, 128).unwrap(),
            IpCidr::from_v6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7).unwrap(),
        ]);
    }

    pub fn push_cidr(&mut self, cidr: IpCidr) {
        self.cidrs.push(cidr);
    }

    pub fn set_mode(&mut self, mode: ProxyHeaderMode) {
        self.mode = mode;
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cidrs.is_empty()
    }

    #[inline]
    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        self.cidrs.iter().any(|c| c.contains(ip))
    }
}

#[inline]
fn parse_forwarded_ip(raw: &str) -> Option<IpAddr> {
    let s = raw.trim().trim_matches('"').trim();
    if s.is_empty() || s.eq_ignore_ascii_case("unknown") {
        return None;
    }

    if let Some(after_open) = s.strip_prefix('[') {
        let (inside, _) = after_open.split_once(']')?;
        return inside
            .parse::<Ipv6Addr>()
            .ok()
            .map(|v6| normalize_ip(IpAddr::V6(v6)));
    }

    if let Ok(ip) = s.parse::<IpAddr>() {
        return Some(normalize_ip(ip));
    }

    if let Some((left, _port)) = s.rsplit_once(':') {
        if !left.contains(':') {
            return left.parse::<Ipv4Addr>().ok().map(IpAddr::V4);
        }
    }

    None
}

#[inline]
fn peel_xff(xff: &str, trusted: &TrustedProxies, fallback: IpAddr) -> Option<IpAddr> {
    let mut last_trusted: Option<IpAddr> = None;
    let mut saw_any = false;
    for part in xff.rsplit(',').take(16) {
        let Some(ip) = parse_forwarded_ip(part) else {
            break;
        };
        saw_any = true;
        if !trusted.is_trusted(ip) {
            return Some(ip);
        }
        last_trusted = Some(ip);
    }
    if saw_any {
        Some(last_trusted.unwrap_or(fallback))
    } else {
        None
    }
}

#[inline]
fn peel_rfc7239_forwarded(hdr: &str, trusted: &TrustedProxies, fallback: IpAddr) -> Option<IpAddr> {
    let mut last_trusted: Option<IpAddr> = None;
    let mut saw_any = false;

    for hop in hdr.rsplit(',').take(16) {
        let for_val = hop.split(';').find_map(|param| {
            let p = param.trim();
            if p.len() >= 4 && p[..4].eq_ignore_ascii_case("for=") {
                Some(&p[4..])
            } else {
                None
            }
        });
        let Some(raw_ip) = for_val else {
            break;
        };
        let Some(ip) = parse_forwarded_ip(raw_ip) else {
            break;
        };
        saw_any = true;
        if !trusted.is_trusted(ip) {
            return Some(ip);
        }
        last_trusted = Some(ip);
    }

    if saw_any {
        Some(last_trusted.unwrap_or(fallback))
    } else {
        None
    }
}

#[inline]
fn resolve_client_ip(
    peer_ip: IpAddr,
    field_lines: &FieldLines,
    trusted: &TrustedProxies,
) -> IpAddr {
    if trusted.is_empty() || !trusted.is_trusted(peer_ip) {
        return peer_ip;
    }

    match trusted.mode {
        ProxyHeaderMode::Auto => {
            if let Some(xff) = field_lines.get("x-forwarded-for") {
                if let Some(ip) = peel_xff(xff, trusted, peer_ip) {
                    return ip;
                }
            }
            if let Some(fwd) = field_lines.get("forwarded") {
                if let Some(ip) = peel_rfc7239_forwarded(fwd, trusted, peer_ip) {
                    return ip;
                }
            }
            if let Some(real_ip) = field_lines.get("x-real-ip") {
                if let Some(ip) = parse_forwarded_ip(real_ip) {
                    return ip;
                }
            }
            peer_ip
        }
        ProxyHeaderMode::XForwardedFor => field_lines
            .get("x-forwarded-for")
            .and_then(|v| peel_xff(v, trusted, peer_ip))
            .unwrap_or(peer_ip),
        ProxyHeaderMode::Forwarded => field_lines
            .get("forwarded")
            .and_then(|v| peel_rfc7239_forwarded(v, trusted, peer_ip))
            .unwrap_or(peer_ip),
        ProxyHeaderMode::XRealIp => field_lines
            .get("x-real-ip")
            .and_then(parse_forwarded_ip)
            .unwrap_or(peer_ip),
        ProxyHeaderMode::CfConnectingIp => field_lines
            .get("cf-connecting-ip")
            .and_then(parse_forwarded_ip)
            .unwrap_or(peer_ip),
    }
}

pub(crate) struct HttpRequest {
    peer_ip: IpAddr,
    client_ip: IpAddr,
    http_version: Bytes,
    method: HttpMethod,
    pub(crate) route: String,
    pub(crate) field_lines: FieldLines,
    pub(crate) max_body_size: usize,
    pub(crate) request_timeout: std::time::Duration,
    pub(crate) body_state: BodyState,

    pub(crate) path_params: Vec<(Arc<str>, std::ops::Range<usize>)>,
    pub(crate) query_params: OnceLock<Vec<(Arc<str>, Arc<str>)>>,
}

#[inline]
fn parse_content_length_strict(raw: &str) -> Result<usize, HttpRequestError> {
    let mut expected: Option<usize> = None;
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
            return Err(HttpRequestError::HeaderParsingFailed(
                "Invalid Content-Length".into(),
            ));
        }
        let val = trimmed
            .parse::<usize>()
            .map_err(|_| HttpRequestError::HeaderParsingFailed("Invalid Content-Length".into()))?;
        match expected {
            Some(prev) if prev != val => {
                return Err(HttpRequestError::HeaderParsingFailed(
                    "Conflicting Content-Length headers".into(),
                ));
            }
            None => expected = Some(val),
            _ => {}
        }
    }
    expected.ok_or_else(|| HttpRequestError::HeaderParsingFailed("Invalid Content-Length".into()))
}

#[inline]
fn contains_chunked_encoding(raw: &str) -> bool {
    raw.split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
}

#[inline]
fn trim_ascii_str(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && (bytes[start] == b' ' || bytes[start] == b'\t') {
        start += 1;
    }
    while end > start && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    &s[start..end]
}

impl HttpRequest {
    pub(crate) fn parse(
        header_bytes: &[u8],
        peer_addr: SocketAddr,
        max_body_size: usize,
        request_timeout: std::time::Duration,
        trusted_proxies: &TrustedProxies,
        mut field_lines: FieldLines,
        mut route: String,
        mut path_params: Vec<(Arc<str>, std::ops::Range<usize>)>,
    ) -> Result<Self, HttpRequestError> {
        let req_line_end = memchr::memchr(b'\n', &header_bytes).ok_or_else(|| {
            HttpRequestError::RequestLineParsingFailed("Missing request line".into())
        })?;

        let (method, http_version) = {
            let mut line = &header_bytes[..req_line_end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }

            let first_space = memchr::memchr(b' ', line).ok_or_else(|| {
                HttpRequestError::RequestLineParsingFailed("Missing HTTP method".into())
            })?;
            let after_method = first_space + 1;

            let second_space_rel =
                memchr::memchr(b' ', &line[after_method..]).ok_or_else(|| {
                    HttpRequestError::RequestLineParsingFailed("Missing route".into())
                })?;
            let second_space = after_method + second_space_rel;
            let version_start = second_space + 1;

            let method = HttpMethod::from_bytes(&line[..first_space])?;

            let r_bytes = &line[after_method..second_space];
            if r_bytes.is_empty() {
                return Err(HttpRequestError::RequestLineParsingFailed(
                    "Empty route".into(),
                ));
            }
            let r_str = std::str::from_utf8(r_bytes).map_err(|_| {
                HttpRequestError::RequestLineParsingFailed("Invalid UTF-8 in route".into())
            })?;
            route.clear();
            route.push_str(r_str);

            let ver_bytes = &line[version_start..];
            let version = match ver_bytes {
                b"HTTP/1.1" => Bytes::from_static(b"HTTP/1.1"),
                b"HTTP/1.0" => Bytes::from_static(b"HTTP/1.0"),
                _ => {
                    if !ver_bytes.starts_with(b"HTTP/1.") || ver_bytes.len() != 8 {
                        return Err(HttpRequestError::RequestLineParsingFailed(
                            "Unsupported HTTP version".into(),
                        ));
                    }
                    Bytes::copy_from_slice(ver_bytes)
                }
            };

            (method, version)
        };

        let peer_ip = normalize_ip(peer_addr.ip());

        let _ = field_lines.parse_into(&header_bytes[req_line_end + 1..]);

        let has_te = field_lines.get("transfer-encoding");
        let has_cl = field_lines.get("content-length");

        let is_chunked = has_te.map(contains_chunked_encoding).unwrap_or(false);

        if is_chunked && has_cl.is_some() {
            return Err(HttpRequestError::HeaderParsingFailed(
                "Both Content-Length and Transfer-Encoding present".into(),
            ));
        }

        let content_length = match has_cl {
            Some(cl_raw) => parse_content_length_strict(cl_raw)?,
            None => 0,
        };

        if content_length > max_body_size {
            return Err(HttpRequestError::PayloadTooLarge);
        }

        path_params.clear();

        let client_ip = resolve_client_ip(peer_ip, &field_lines, trusted_proxies);

        Ok(Self {
            client_ip,
            peer_ip,
            method,
            route,
            http_version,
            field_lines,
            max_body_size,
            body_state: BodyState::Unread {
                content_length,
                is_chunked,
            },
            path_params,
            query_params: OnceLock::new(),
            request_timeout,
        })
    }

    #[inline]
    pub(crate) fn client_ip(&self) -> IpAddr {
        self.client_ip
    }

    #[inline]
    pub(crate) fn peer_ip(&self) -> IpAddr {
        self.peer_ip
    }

    #[inline]
    pub(crate) fn host(&self) -> Option<&str> {
        self.field_lines.get("host")
    }

    #[inline]
    pub(crate) fn http_version(&self) -> &Bytes {
        &self.http_version
    }

    #[inline]
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.field_lines.get(name)
    }

    pub(crate) fn cookie(&self, name: &str) -> Option<&str> {
        let raw = self.field_lines.get("cookie")?;
        let bytes = raw.as_bytes();
        let target = name.as_bytes();
        let mut found: Option<&str> = None;

        let mut i = 0usize;
        while i < bytes.len() {
            let pair_end = match memchr::memchr(b';', &bytes[i..]) {
                Some(off) => i + off,
                None => bytes.len(),
            };
            let pair = trim_ascii_str(&raw[i..pair_end]);
            if !pair.is_empty() {
                if let Some(eq) = memchr::memchr(b'=', pair.as_bytes()) {
                    let k = trim_ascii_str(&pair[..eq]);
                    if k.as_bytes() == target {
                        found = Some(trim_ascii_str(&pair[eq + 1..]));
                    }
                } else if pair.as_bytes() == target {
                    found = Some("");
                }
            }
            i = pair_end + 1;
        }

        found
    }

    #[inline]
    pub(crate) fn route(&self) -> &str {
        &self.route
    }

    #[inline]
    pub(crate) fn method(&self) -> &HttpMethod {
        &self.method
    }
}
