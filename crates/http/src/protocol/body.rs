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
mod json;
mod multipart;

pub(crate) use json::write_json_str;
pub use json::{IntoJsonValue, JsonValue, ToJson};

use crate::HttpStatus;
use crate::protocol::body::json::{JsonRef, RawJson};
use crate::protocol::body::multipart::Multipart;
use crate::server::Connection;
use bytes::Bytes;
use tokio::io::AsyncRead;

static OCTET_STREAM: &[u8] = b"application/octet-stream";

#[derive(Clone, Copy)]
pub struct BodyRef<'a> {
    bytes: &'a Bytes,
    content_type: Option<&'a str>,
}

impl<'a> BodyRef<'a> {
    pub(crate) fn new(bytes: &'a Bytes, content_type: Option<&'a str>) -> Self {
        Self {
            bytes,
            content_type,
        }
    }

    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }
    pub fn as_str(&self) -> Result<&'a str, std::str::Utf8Error> {
        std::str::from_utf8(self.bytes)
    }
    pub fn content_type(&self) -> Option<&'a str> {
        self.content_type
    }
    pub fn len(&self) -> usize {
        self.bytes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn to_bytes(&self) -> Bytes {
        self.bytes.clone()
    }

    fn is_type(&self, prefix: &str) -> bool {
        self.content_type
            .and_then(|ct| ct.get(..prefix.len()))
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
    }

    fn media_type(&self) -> Option<&'a str> {
        self.content_type
            .map(|ct| ct.split(';').next().unwrap_or("").trim())
    }

    fn is_json(&self) -> bool {
        self.media_type().is_some_and(|m| {
            let b = m.as_bytes();
            m.eq_ignore_ascii_case("application/json")
                || (b.len() > 5 && b[b.len() - 5..].eq_ignore_ascii_case(b"+json"))
        })
    }

    pub fn as_json(&self) -> Option<JsonRef<'a>> {
        self.json().ok()
    }

    pub fn json(&self) -> Result<JsonRef<'a>, BodyError> {
        if !self.is_type("application/json") {
            return Err(BodyError::WrongContentType);
        }
        JsonRef::parse(self.bytes).map_err(|_| BodyError::Malformed("invalid JSON"))
    }

    pub fn as_multipart(&self) -> Option<Multipart> {
        if !self.is_type("multipart/form-data") {
            return None;
        }
        let boundary = self.content_type?.split(';').find_map(|param| {
            let p = param.trim();
            p.get(..9)
                .filter(|k| k.eq_ignore_ascii_case("boundary="))
                .map(|_| p[9..].trim().trim_matches('"'))
        })?;
        Multipart::parse(self.bytes, boundary).ok()
    }
}

impl std::ops::Deref for BodyRef<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.bytes
    }
}
impl AsRef<[u8]> for BodyRef<'_> {
    fn as_ref(&self) -> &[u8] {
        self.bytes
    }
}

#[derive(thiserror::Error, Debug)]
pub enum BodyError {
    #[error("body too large")]
    TooLarge,
    #[error("closed")]
    Closed,
    #[error("timeout")]
    Timeout,
    #[error("busy")]
    Busy,
    #[error("consumed")]
    Consumed,
    #[error("malformed: {0}")]
    Malformed(&'static str),
    #[error("IO err: {0}")]
    Io(#[from] std::io::Error),
    #[error("wrong content type")]
    WrongContentType,
}

impl BodyError {
    pub fn status(&self) -> HttpStatus {
        match self {
            Self::TooLarge => HttpStatus::ContentTooLarge,
            Self::Timeout => HttpStatus::RequestTimeout,
            Self::WrongContentType => HttpStatus::UnsupportedMediaType,
            Self::Busy | Self::Consumed => HttpStatus::InternalServerError,
            _ => HttpStatus::BadRequest,
        }
    }
}

pub enum Body {
    Bytes {
        bytes: Bytes,
        content_type: Option<String>,
    },
    PooledVec {
        bytes: Vec<u8>,
        content_type: Option<String>,
    },
    Stream {
        reader: Box<dyn AsyncRead + Unpin + Send>,
        content_length: u64,
        content_type: Option<String>,
    },
    ChunkStream {
        source: Box<dyn crate::protocol::stream::ChunkSource>,
        content_length: Option<u64>,
    },
}

impl From<JsonValue> for Body {
    fn from(json: JsonValue) -> Self {
        Self::Bytes {
            bytes: Bytes::from(json.to_vec()),
            content_type: Some("application/json".into()),
        }
    }
}

impl From<RawJson> for Body {
    fn from(json: RawJson) -> Self {
        Self::Bytes {
            bytes: json.0,
            content_type: Some("application/json".into()),
        }
    }
}

impl Body {
    pub(crate) fn from_stream<R>(
        reader: R,
        content_length: u64,
        content_type: Option<String>,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        Self::Stream {
            reader: Box::new(reader),
            content_length,
            content_type,
        }
    }

    async fn ensure(conn: &mut Connection, n: usize) -> Result<(), BodyError> {
        while conn.reader.len() < n {
            if conn.read_more().await? == 0 {
                return Err(BodyError::Closed);
            }
        }
        Ok(())
    }

    async fn find_line(
        conn: &mut Connection,
        limit: usize,
        msg: &'static str,
    ) -> Result<usize, BodyError> {
        loop {
            if let Some(pos) = memchr::memchr(b'\n', &conn.reader) {
                return Ok(pos);
            }
            if conn.reader.len() > limit {
                return Err(BodyError::Malformed(msg));
            }
            if conn.read_more().await? == 0 {
                return Err(BodyError::Closed);
            }
        }
    }

    pub(crate) async fn read_exact(
        conn: &mut Connection,
        len: usize,
        max: usize,
    ) -> Result<Bytes, BodyError> {
        use bytes::Buf;
        if len > max {
            return Err(BodyError::TooLarge);
        }
        if conn.reader.len() >= len {
            return Ok(conn.reader.split_to(len).freeze());
        }

        let mut out = Vec::with_capacity(len.min(64 * 1024));
        out.extend_from_slice(&conn.reader);
        conn.reader.clear();

        while out.len() < len {
            if conn.read_more().await? == 0 {
                return Err(BodyError::Closed);
            }
            let take = (len - out.len()).min(conn.reader.len());
            out.extend_from_slice(&conn.reader[..take]);
            conn.reader.advance(take);
        }
        Ok(out.into())
    }

    pub(crate) async fn read_chunked(
        conn: &mut Connection,
        max: usize,
    ) -> Result<Bytes, BodyError> {
        use bytes::Buf;
        let mut body = Vec::new();

        loop {
            let pos = Self::find_line(conn, 4096, "chunk size line too long").await?;
            if pos == 0 || conn.reader[pos - 1] != b'\r' || pos > 4096 {
                return Err(BodyError::Malformed("invalid chunk size line"));
            }
            let size = {
                let line = std::str::from_utf8(&conn.reader[..pos - 1])
                    .map_err(|_| BodyError::Malformed("invalid chunk size line"))?;
                let s = line.split(';').next().unwrap_or("").trim();
                if s.is_empty() || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(BodyError::Malformed("invalid chunk size"));
                }
                usize::from_str_radix(s, 16)
                    .map_err(|_| BodyError::Malformed("invalid chunk size"))?
            };
            conn.reader.advance(pos + 1);

            if size == 0 {
                let mut trailer_bytes = 0usize;
                loop {
                    let t = Self::find_line(conn, 16 * 1024, "chunk trailers too large").await?;
                    if t == 0 || conn.reader[t - 1] != b'\r' {
                        return Err(BodyError::Malformed("invalid chunk trailer line ending"));
                    }
                    trailer_bytes += t + 1;
                    if trailer_bytes > 16 * 1024 {
                        return Err(BodyError::Malformed("chunk trailers too large"));
                    }
                    conn.reader.advance(t + 1);
                    if t == 1 {
                        break;
                    }
                }
                break;
            }

            let new_len = body
                .len()
                .checked_add(size)
                .filter(|&l| l <= max)
                .ok_or(BodyError::TooLarge)?;
            body.reserve((new_len - body.len()).min(64 * 1024));

            Self::ensure(conn, size).await?;
            body.extend_from_slice(&conn.reader[..size]);
            conn.reader.advance(size);

            Self::ensure(conn, 2).await?;
            if &conn.reader[..2] != b"\r\n" {
                return Err(BodyError::Malformed("missing CRLF after chunk data"));
            }
            conn.reader.advance(2);
        }
        Ok(body.into())
    }

    pub(crate) fn buffered_len(&self) -> Option<usize> {
        match self {
            Body::Bytes { bytes, .. } => Some(bytes.len()),
            Body::PooledVec { bytes, .. } => Some(bytes.len()),
            _ => None,
        }
    }
}
