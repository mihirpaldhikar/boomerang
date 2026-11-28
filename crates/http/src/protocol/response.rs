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
use crate::compression::Compress;
use crate::protocol::body::Body;
use crate::protocol::cookie_store::{CookieOptions, CookieStore};
use crate::protocol::field_lines::FieldLines;
use crate::protocol::status::HttpStatus;
use crate::server::RequestId;
use crate::server::{Connection, WorkerScratch};
use bytes::Bytes;
use std::borrow::Cow;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::io::{AsyncRead, BufReader};

const ENCODINGS: [&str; 3] = ["zstd", "br", "gzip"]; // priority order

#[inline]
fn pick_encoding(header: &str) -> Option<&'static str> {
    header
        .split(',')
        .filter_map(|part| {
            let token = part.split(';').next()?.trim();
            ENCODINGS.iter().position(|e| token.eq_ignore_ascii_case(e))
        })
        .min()
        .map(|i| ENCODINGS[i])
}

pub struct Uninit;
pub struct Ready;

pub(crate) type ResponseHook = Arc<dyn Fn(HttpStatus) + Send + Sync>;

pub(crate) struct ResponseParts {
    pub(crate) http_version: Bytes,
    pub(crate) request_id: RequestId,
    pub(crate) status_code: HttpStatus,
    pub(crate) content_type: Option<Cow<'static, str>>,
    pub(crate) content_length: Option<u64>,
    pub(crate) is_chunked: bool,
    pub(crate) body: Option<Body>,
    pub(crate) field_lines: FieldLines,
    pub(crate) cookies: Option<CookieStore>,
    pub(crate) suppress_body: bool,
    pub(crate) on_sent: Option<ResponseHook>,
    pub(crate) accept_encoding: Option<&'static str>,
}

impl ResponseParts {
    async fn compress(&mut self) {
        let fallback = self.field_lines.remove("accept-encoding");
        let Some(encoding) = self
            .accept_encoding
            .or_else(|| fallback.as_deref().and_then(pick_encoding))
        else {
            return;
        };

        let compressible = self
            .content_type
            .as_deref()
            .filter(|ct| !ct.eq_ignore_ascii_case("text/event-stream"))
            .is_some_and(Compress::is_compressible);
        if !compressible || self.field_lines.get("content-encoding").is_some() {
            return;
        }

        let Some(body) = self.body.take() else { return };

        if body.buffered_len().is_some_and(|n| n < 1024) {
            self.body = Some(body);
            return;
        }

        self.body = Some(match body {
            Body::Bytes {
                bytes,
                content_type,
            } => self.compress_buffered(bytes, content_type, encoding).await,
            Body::PooledVec {
                bytes,
                content_type,
            } => {
                self.compress_buffered(Bytes::from(bytes), content_type, encoding)
                    .await
            }
            Body::Stream {
                reader,
                content_type,
                ..
            } => self.compressed_stream(reader, content_type, encoding),
            Body::ChunkStream { source, .. } => {
                let reader = Box::new(crate::protocol::stream::ChunkSourceReader::new(source));
                self.compressed_stream(reader, None, encoding)
            }
        });
    }

    fn set_encoding_headers(&mut self, encoding: &'static str) {
        self.field_lines.set("content-encoding", encoding);
        self.field_lines.set("vary", "Accept-Encoding");
    }

    async fn compress_buffered(
        &mut self,
        bytes: Bytes,
        content_type: Option<String>,
        encoding: &'static str,
    ) -> Body {
        let compressed = Compress::compress_bytes(bytes.clone(), encoding).await;
        if compressed.len() >= bytes.len() {
            return Body::Bytes {
                bytes,
                content_type,
            };
        }
        self.content_length = Some(compressed.len() as u64);
        self.set_encoding_headers(encoding);
        Body::PooledVec {
            bytes: compressed,
            content_type,
        }
    }

    fn compressed_stream(
        &mut self,
        reader: Box<dyn AsyncRead + Unpin + Send>,
        content_type: Option<String>,
        encoding: &'static str,
    ) -> Body {
        self.field_lines.remove("content-length");
        self.content_length = None;
        self.is_chunked = true;
        self.set_encoding_headers(encoding);

        let buffered = BufReader::with_capacity(64 * 1024, reader);
        let reader: Box<dyn AsyncRead + Unpin + Send> = match encoding {
            "zstd" => Box::new(
                async_compression::tokio::bufread::ZstdEncoder::with_quality(
                    buffered,
                    async_compression::Level::Fastest,
                ),
            ),
            "br" => Box::new(
                async_compression::tokio::bufread::BrotliEncoder::with_quality(
                    buffered,
                    async_compression::Level::Precise(4),
                ),
            ),
            _ => Box::new(
                async_compression::tokio::bufread::GzipEncoder::with_quality(
                    buffered,
                    async_compression::Level::Fastest,
                ),
            ),
        };
        Body::Stream {
            reader,
            content_length: 0,
            content_type,
        }
    }
}

pub struct HttpResponse<'a, S> {
    parts: ResponseParts,
    conn: Option<&'a RefCell<Connection>>,
    _state: PhantomData<S>,
}

impl<'a, S> HttpResponse<'a, S> {
    fn into_state<T>(self) -> HttpResponse<'a, T> {
        HttpResponse {
            parts: self.parts,
            conn: self.conn,
            _state: PhantomData,
        }
    }

    pub(crate) fn into_parts(self) -> ResponseParts {
        self.parts
    }

    pub(crate) fn attach(mut self, conn: &'a RefCell<Connection>) -> Self {
        self.conn = Some(conn);
        self
    }

    pub(crate) fn with_accept_encoding(mut self, header: Option<&str>) -> Self {
        self.parts.accept_encoding = header.and_then(pick_encoding);
        self
    }

    pub(crate) fn set_on_set(mut self, hook: ResponseHook) -> Self {
        self.parts.on_sent = Some(hook);
        self
    }

    pub(crate) fn suppress_body(mut self) -> Self {
        self.parts.suppress_body = true;
        self
    }

    pub fn status_code(mut self, code: HttpStatus) -> Self {
        self.parts.status_code = code;
        self
    }

    pub fn add_header<K: AsRef<str>, V: AsRef<str>>(self, k: K, v: V) -> Self {
        if FieldLines::is_reserved(k.as_ref()) {
            return self;
        }
        self.__add_header_internal(k, v)
    }

    pub(crate) fn __add_header_internal<K: AsRef<str>, V: AsRef<str>>(
        mut self,
        k: K,
        v: V,
    ) -> Self {
        self.parts.field_lines.set(k.as_ref(), v.as_ref());
        self
    }

    pub fn remove_header<K: AsRef<str>>(self, k: K) -> Self {
        if FieldLines::is_reserved(k.as_ref()) {
            return self;
        }
        self.__remove_header_internal(k)
    }

    pub(crate) fn __remove_header_internal<K: AsRef<str>>(mut self, k: K) -> Self {
        self.parts.field_lines.remove(k.as_ref());
        self
    }

    pub fn add_cookie<K: Into<String>, V: Into<String>>(
        mut self,
        name: K,
        value: V,
        options: Option<CookieOptions>,
    ) -> Self {
        self.parts.cookies.get_or_insert_with(CookieStore::new).set(
            name.into(),
            value.into(),
            options,
        );
        self
    }

    pub fn remove_cookie<K: Into<String>>(mut self, name: K) -> Self {
        if let Some(store) = self.parts.cookies.as_mut() {
            store.remove(&name.into());
        }
        self
    }
}

impl<'a> HttpResponse<'a, Uninit> {
    pub(crate) fn new<B: Into<Bytes>>(http_version: B, request_id: RequestId) -> Self {
        Self::new_with_fields(http_version, request_id, FieldLines::with_capacity(6, 128))
    }

    pub(crate) fn new_with_fields<B: Into<Bytes>>(
        http_version: B,
        request_id: RequestId,
        field_lines: FieldLines,
    ) -> Self {
        Self {
            parts: ResponseParts {
                http_version: http_version.into(),
                request_id,
                status_code: HttpStatus::Ok,
                content_type: None,
                content_length: None,
                is_chunked: false,
                body: None,
                field_lines,
                cookies: None,
                suppress_body: false,
                on_sent: None,
                accept_encoding: None,
            },
            conn: None,
            _state: PhantomData,
        }
    }

    pub fn declare_trailers<S: AsRef<str>>(self, names: S) -> Self {
        self.__add_header_internal("trailer", names)
    }

    /// `len == None` means chunked. Every body setter goes through here.
    fn with_body<C: Into<Cow<'static, str>>>(
        mut self,
        body: Body,
        content_type: C,
        len: Option<u64>,
    ) -> HttpResponse<'a, Ready> {
        let p = &mut self.parts;
        p.content_type = Some(content_type.into());
        p.content_length = len;
        p.is_chunked = len.is_none();
        p.body = Some(body);
        self.into_state()
    }

    fn pooled(self, buf: Vec<u8>, content_type: &'static str) -> HttpResponse<'a, Ready> {
        let len = buf.len() as u64;
        self.with_body(
            Body::PooledVec {
                bytes: buf,
                content_type: None,
            },
            content_type,
            Some(len),
        )
    }

    pub fn json<V: crate::protocol::body::ToJson>(self, value: V) -> HttpResponse<'a, Ready> {
        let mut buf = WorkerScratch::take_body_buf();
        buf.clear();
        buf.reserve(value.size_hint());
        value.write_json(&mut buf);
        self.pooled(buf, "application/json")
    }

    pub fn text<V: AsRef<str>>(self, v: V) -> HttpResponse<'a, Ready> {
        let mut buf = WorkerScratch::take_body_buf();
        buf.clear();
        buf.extend_from_slice(v.as_ref().as_bytes());
        self.pooled(buf, "text/plain")
    }

    pub fn bytes<V: Into<Bytes>, C: Into<Cow<'static, str>>>(
        self,
        bytes: V,
        ct: C,
    ) -> HttpResponse<'a, Ready> {
        let bytes = bytes.into();
        let len = bytes.len() as u64;
        self.with_body(
            Body::Bytes {
                bytes,
                content_type: None,
            },
            ct,
            Some(len),
        )
    }

    pub fn download<V: Into<Bytes>, C: Into<Cow<'static, str>>>(
        self,
        bytes: V,
        filename: &str,
        content_type: C,
    ) -> HttpResponse<'a, Ready> {
        let cd = format_content_disposition(filename);
        self.add_header("content-disposition", cd)
            .add_header("x-content-type-options", "nosniff")
            .bytes(bytes, content_type)
    }

    pub fn empty(mut self) -> HttpResponse<'a, Ready> {
        self.parts.body = None;
        self.into_state()
    }

    pub fn stream_from<S, C>(self, source: S, ct: C) -> HttpResponse<'a, Ready>
    where
        S: crate::protocol::stream::IntoBodyStream + 'static,
        C: Into<Cow<'static, str>>,
    {
        self.chunk_stream(source, ct, None)
    }

    pub fn stream_from_len<S, C>(self, source: S, len: u64, ct: C) -> HttpResponse<'a, Ready>
    where
        S: crate::protocol::stream::IntoBodyStream + 'static,
        C: Into<Cow<'static, str>>,
    {
        self.chunk_stream(source, ct, Some(len))
    }

    fn chunk_stream<S, C>(self, source: S, ct: C, len: Option<u64>) -> HttpResponse<'a, Ready>
    where
        S: crate::protocol::stream::IntoBodyStream + 'static,
        C: Into<Cow<'static, str>>,
    {
        let body = Body::ChunkStream {
            source: Box::new(source.into_body_stream()),
            content_length: len,
        };
        self.with_body(body, ct, len)
    }

    pub fn stream_with<F, Fut, E, C>(self, content_type: C, producer: F) -> HttpResponse<'a, Ready>
    where
        F: FnOnce(crate::protocol::stream::StreamSender) -> Fut + 'static,
        Fut: Future<Output = Result<(), E>> + 'static,
        E: crate::protocol::stream::StreamError,
        C: Into<Cow<'static, str>>,
    {
        let (tx, rx) = crate::protocol::stream::stream_channel(16);
        let handle = tokio::task::spawn_local(async move {
            let err_tx = tx.clone();
            if let Err(err) = producer(tx).await {
                err_tx.apply_error(err).await;
            }
        });
        let body = Body::ChunkStream {
            source: Box::new(crate::protocol::stream::ManagedStreamSource {
                inner: rx,
                handle: Some(handle),
            }),
            content_length: None,
        };
        self.with_body(body, content_type, None)
    }

    pub fn sse<F, Fut, E>(self, producer: F) -> HttpResponse<'a, Ready>
    where
        F: FnOnce(crate::protocol::stream::StreamSender) -> Fut + 'static,
        Fut: Future<Output = Result<(), E>> + 'static,
        E: crate::protocol::stream::StreamError,
    {
        self.__add_header_internal("cache-control", "no-cache")
            .stream_with("text/event-stream", producer)
    }
}

impl<'a> HttpResponse<'a, Ready> {
    pub async fn compress(mut self) -> Self {
        self.parts.compress().await;
        self
    }

    pub async fn failable_send(self) -> Result<(), std::io::Error> {
        let cell = self
            .conn
            .ok_or_else(|| std::io::Error::other("response is not attached to a connection"))?;
        let mut conn = cell.try_borrow_mut().map_err(|_| {
            std::io::Error::other(
                "connection is already in use (concurrent response on the same HttpCall)",
            )
        })?;
        conn.write_response(self.parts).await
    }

    pub async fn send(self) {
        let _ = self.failable_send().await;
    }
}

fn format_content_disposition(filename: &str) -> String {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or("download");
    let mut ascii_safe = String::with_capacity(base.len());
    let mut has_non_ascii = false;

    for ch in base.chars() {
        if ch.is_control() || ch == '\0' {
            continue;
        }
        if ch.is_ascii() {
            ascii_safe.push(if ch == '"' { '_' } else { ch });
        } else {
            has_non_ascii = true;
            ascii_safe.push('_');
        }
    }
    if ascii_safe.is_empty() {
        ascii_safe.push_str("download");
    }

    if !has_non_ascii {
        return format!(r#"attachment; filename="{ascii_safe}""#);
    }

    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut utf8_pct = String::with_capacity(base.len() * 3);
    for &b in base.as_bytes() {
        if b < 0x20 || b == 0x7F {
            continue;
        }
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            utf8_pct.push(b as char);
        } else {
            utf8_pct.push('%');
            utf8_pct.push(HEX[(b >> 4) as usize] as char);
            utf8_pct.push(HEX[(b & 0xF) as usize] as char);
        }
    }

    format!(r#"attachment; filename="{ascii_safe}"; filename*=UTF-8''{utf8_pct}"#)
}
