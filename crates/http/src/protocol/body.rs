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
pub use json::{JsonValue, RawJson, ToJson};

use crate::protocol::body::multipart::Multipart;
use crate::server::Connection;
use bytes::Bytes;
use tokio::io::AsyncRead;

static OCTET_STREAM: &[u8] = b"application/octet-stream";

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

impl From<json::JsonValue> for Body {
    fn from(json: json::JsonValue) -> Self {
        Self::Bytes {
            bytes: Bytes::from(json.to_vec()),
            content_type: Some("application/json".into()),
        }
    }
}

impl From<json::RawJson> for Body {
    fn from(json: json::RawJson) -> Self {
        Self::Bytes {
            bytes: json.0,
            content_type: Some("application/json".into()),
        }
    }
}

impl Body {
    pub(crate) fn from(bytes: &Bytes, content_type: Option<String>) -> Self {
        Self::Bytes {
            bytes: bytes.clone(),
            content_type,
        }
    }

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

    pub(crate) async fn read_exact(
        conn: &mut Connection,
        content_len: usize,
        max_body_size: usize,
        content_type: Option<String>,
    ) -> Result<Self, String> {
        use bytes::Buf;

        if content_len > max_body_size {
            return Err("body too large".to_string());
        }

        if conn.reader.len() >= content_len {
            let bytes = conn.reader.split_to(content_len).freeze();
            return Ok(Self::Bytes {
                bytes,
                content_type,
            });
        }

        // Grow incrementally so slow/stalled clients cannot
        // force multi-MB allocations before sending payload bytes.
        let mut out = Vec::with_capacity(content_len.min(64 * 1024));
        if !conn.reader.is_empty() {
            out.extend_from_slice(&conn.reader);
            conn.reader.clear();
        }

        while out.len() < content_len {
            let n = conn.read_more().await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("stream closed unexpectedly".into());
            }
            let remaining = content_len - out.len();
            let take = remaining.min(conn.reader.len());
            out.extend_from_slice(&conn.reader[..take]);
            conn.reader.advance(take);
        }

        Ok(Self::Bytes {
            bytes: out.into(),
            content_type,
        })
    }

    pub(crate) async fn read_chunked(
        conn: &mut Connection,
        max_body_size: usize,
        content_type: Option<String>,
    ) -> Result<Body, String> {
        use bytes::Buf;

        let mut body = Vec::new();

        loop {
            let pos = loop {
                if let Some(pos) = memchr::memchr(b'\n', &conn.reader) {
                    break pos;
                }
                if conn.reader.len() > 4096 {
                    return Err("chunk size line too long".to_string());
                }
                let n = conn.read_more().await.map_err(|e| e.to_string())?;
                if n == 0 {
                    return Err("stream closed unexpectedly".into());
                }
            };

            if pos == 0 || conn.reader[pos - 1] != b'\r' {
                return Err("invalid chunk size line ending".to_string());
            }
            if pos > 4096 {
                return Err("chunk size line too long".to_string());
            }

            let chunk_size = {
                let line = std::str::from_utf8(&conn.reader[..pos - 1])
                    .map_err(|_| "invalid chunk size line")?;
                let size_str = line.split(';').next().unwrap_or("").trim();

                if size_str.is_empty() || !size_str.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("invalid chunk size: '{}'", size_str));
                }

                usize::from_str_radix(size_str, 16)
                    .map_err(|_| format!("invalid chunk size: '{}'", size_str))?
            };
            conn.reader.advance(pos + 1);

            if chunk_size == 0 {
                let mut trailer_bytes = 0usize;
                loop {
                    let t_pos = loop {
                        if let Some(pos) = memchr::memchr(b'\n', &conn.reader) {
                            break pos;
                        }
                        if conn.reader.len() > 16 * 1024 {
                            return Err("chunk trailers too large".to_string());
                        }
                        let n = conn.read_more().await.map_err(|e| e.to_string())?;
                        if n == 0 {
                            return Err("stream closed before chunk trailers finished".to_string());
                        }
                    };
                    if t_pos == 0 || conn.reader[t_pos - 1] != b'\r' {
                        return Err("invalid chunk trailer line ending".to_string());
                    }
                    let line_len = t_pos + 1;
                    trailer_bytes += line_len;
                    if trailer_bytes > 16 * 1024 {
                        return Err("chunk trailers too large".to_string());
                    }
                    let is_empty_line = t_pos == 1;
                    conn.reader.advance(line_len);
                    if is_empty_line {
                        break;
                    }
                }
                break;
            }

            let new_len = body
                .len()
                .checked_add(chunk_size)
                .filter(|&l| l <= max_body_size)
                .ok_or_else(|| "body too large".to_string())?;
            body.reserve((new_len - body.len()).min(64 * 1024));

            while conn.reader.len() < chunk_size {
                let n = conn.read_more().await.map_err(|e| e.to_string())?;
                if n == 0 {
                    return Err("stream closed unexpectedly".into());
                }
            }
            body.extend_from_slice(&conn.reader[..chunk_size]);
            conn.reader.advance(chunk_size);

            while conn.reader.len() < 2 {
                let n = conn.read_more().await.map_err(|e| e.to_string())?;
                if n == 0 {
                    return Err("stream closed unexpectedly".into());
                }
            }
            if &conn.reader[..2] != b"\r\n" {
                return Err("missing CRLF after chunk data".to_string());
            }
            conn.reader.advance(2);
        }

        Ok(Self::Bytes {
            bytes: body.into(),
            content_type,
        })
    }

    pub fn as_json(&self) -> Option<JsonValue> {
        let (slice, content_type): (&[u8], &Option<String>) = match self {
            Self::Bytes {
                bytes,
                content_type,
            } => (bytes, content_type),
            Self::PooledVec {
                bytes,
                content_type,
            } => (bytes, content_type),
            Self::Stream { .. } | Self::ChunkStream { .. } => return None,
        };
        match content_type {
            Some(ct)
                if ct
                    .get(..16)
                    .map_or(false, |p| p.eq_ignore_ascii_case("application/json")) =>
            {
                json::JsonValue::parse(slice).ok()
            }
            _ => None,
        }
    }

    pub fn as_multipart(&self) -> Option<Multipart> {
        let (slice_bytes, content_type): (Bytes, &Option<String>) = match self {
            Self::Bytes {
                bytes,
                content_type,
            } => (bytes.clone(), content_type),
            Self::PooledVec { .. } | Self::Stream { .. } | Self::ChunkStream { .. } => {
                return None;
            }
        };

        match content_type {
            Some(ct)
                if ct
                    .get(..19)
                    .map_or(false, |p| p.eq_ignore_ascii_case("multipart/form-data")) =>
            {
                let boundary = ct.split(';').find_map(|param| {
                    let p = param.trim();
                    if p.len() >= 9 && p[..9].eq_ignore_ascii_case("boundary=") {
                        Some(p[9..].trim().trim_matches('"'))
                    } else {
                        None
                    }
                })?;

                Multipart::parse(&slice_bytes, boundary).ok()
            }
            _ => None,
        }
    }

    pub fn to_bytes(&self) -> Bytes {
        match self {
            Body::Bytes { bytes, .. } => bytes.clone(),
            Body::PooledVec { bytes, .. } => Bytes::copy_from_slice(bytes),
            Body::Stream { .. } | Body::ChunkStream { .. } => {
                panic!("cannot convert stream body to bytes")
            }
        }
    }

    pub fn is_stream(&self) -> bool {
        matches!(self, Body::Stream { .. } | Body::ChunkStream { .. })
    }
}
