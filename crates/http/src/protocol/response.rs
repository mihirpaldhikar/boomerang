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
use std::sync::Arc;
use tokio::io::{AsyncRead, BufReader};

const ENC_ZSTD: u8 = 1 << 0;
const ENC_BR: u8 = 1 << 1;
const ENC_GZIP: u8 = 1 << 2;

#[inline]
fn parse_accept_encoding_mask(header: &str) -> u8 {
    let mut mask = 0u8;
    for part in header.split(',') {
        let token = part.split(';').next().unwrap_or("").trim();
        if token.eq_ignore_ascii_case("zstd") {
            mask |= ENC_ZSTD;
        } else if token.eq_ignore_ascii_case("br") {
            mask |= ENC_BR;
        } else if token.eq_ignore_ascii_case("gzip") {
            mask |= ENC_GZIP;
        }
    }
    mask
}

pub(crate) type ResponseHook = Arc<dyn Fn(HttpStatus) + Send + Sync>;

pub struct HttpResponseBodyUnInitialized;
pub struct HttpResponseBodyInitialized;

pub struct HttpResponse<State> {
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
    pub(crate) accept_encoding_mask: u8,
    pub(crate) _state: std::marker::PhantomData<State>,
}

impl HttpResponse<HttpResponseBodyUnInitialized> {
    pub(crate) fn new<B: Into<Bytes>>(http_version: B, request_id: RequestId) -> Self {
        Self::new_with_fields(http_version, request_id, FieldLines::with_capacity(6, 128))
    }

    pub(crate) fn new_with_fields<B: Into<Bytes>>(
        http_version: B,
        request_id: RequestId,
        field_lines: FieldLines,
    ) -> Self {
        Self {
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
            accept_encoding_mask: 0,
            _state: std::marker::PhantomData,
        }
    }

    #[inline]
    pub(crate) fn into_pooled_body(
        mut self,
        buf: Vec<u8>,
        content_type: &'static str,
    ) -> HttpResponse<HttpResponseBodyInitialized> {
        self.content_type = Some(Cow::Borrowed(content_type));
        self.content_length = Some(buf.len() as u64);
        self.is_chunked = false;
        self.into_initialized(Some(Body::PooledVec {
            bytes: buf,
            content_type: None,
        }))
    }

    #[inline]
    pub(crate) fn with_accept_encoding(mut self, header: Option<&str>) -> Self {
        if let Some(h) = header {
            self.accept_encoding_mask = parse_accept_encoding_mask(h);
        }
        self
    }

    #[inline]
    fn into_initialized(self, body: Option<Body>) -> HttpResponse<HttpResponseBodyInitialized> {
        HttpResponse {
            http_version: self.http_version,
            request_id: self.request_id,
            status_code: self.status_code,
            content_type: self.content_type,
            content_length: self.content_length,
            is_chunked: self.is_chunked,
            body,
            field_lines: self.field_lines,
            cookies: self.cookies,
            suppress_body: self.suppress_body,
            on_sent: self.on_sent,
            accept_encoding_mask: self.accept_encoding_mask,
            _state: std::marker::PhantomData,
        }
    }

    pub fn json<V: crate::protocol::body::ToJson>(
        self,
        value: V,
    ) -> HttpResponse<HttpResponseBodyInitialized> {
        let mut buf = Vec::with_capacity(value.size_hint());
        value.write_json(&mut buf);
        self.into_pooled_body(buf, "application/json")
    }

    pub fn text<V: AsRef<str>>(self, val: V) -> HttpResponse<HttpResponseBodyInitialized> {
        self.into_pooled_body(val.as_ref().as_bytes().to_vec(), "text/plain")
    }

    pub fn bytes<V: Into<Bytes>, C: Into<Cow<'static, str>>>(
        mut self,
        bytes: V,
        content_type: C,
    ) -> HttpResponse<HttpResponseBodyInitialized> {
        let bytes = bytes.into();
        self.content_type = Some(content_type.into());
        self.content_length = Some(bytes.len() as u64);
        self.is_chunked = false;
        self.into_initialized(Some(Body::Bytes {
            bytes,
            content_type: None,
        }))
    }

    pub fn stream_from<S, C>(
        mut self,
        source: S,
        content_type: C,
    ) -> HttpResponse<HttpResponseBodyInitialized>
    where
        S: crate::protocol::stream::IntoBodyStream + 'static,
        C: Into<Cow<'static, str>>,
    {
        self.content_type = Some(content_type.into());
        self.is_chunked = true;
        self.content_length = None;
        self.into_initialized(Some(Body::ChunkStream {
            source: Box::new(source.into_body_stream()),
            content_length: None,
        }))
    }

    pub fn stream_from_len<S, C>(
        mut self,
        source: S,
        content_len: u64,
        content_type: C,
    ) -> HttpResponse<HttpResponseBodyInitialized>
    where
        S: crate::protocol::stream::IntoBodyStream + 'static,
        C: Into<Cow<'static, str>>,
    {
        self.content_type = Some(content_type.into());
        self.is_chunked = false;
        self.content_length = Some(content_len);
        self.into_initialized(Some(Body::ChunkStream {
            source: Box::new(source.into_body_stream()),
            content_length: Some(content_len),
        }))
    }

    pub fn stream_with<F, Fut, E, C>(
        mut self,
        content_type: C,
        producer: F,
    ) -> HttpResponse<HttpResponseBodyInitialized>
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

        self.content_type = Some(content_type.into());
        self.is_chunked = true;
        self.content_length = None;
        self.into_initialized(Some(Body::ChunkStream {
            source: Box::new(crate::protocol::stream::ManagedStreamSource {
                inner: rx,
                handle: Some(handle),
            }),
            content_length: None,
        }))
    }

    pub fn sse<F, Fut, E>(self, producer: F) -> HttpResponse<HttpResponseBodyInitialized>
    where
        F: FnOnce(crate::protocol::stream::StreamSender) -> Fut + 'static,
        Fut: Future<Output = Result<(), E>> + 'static,
        E: crate::protocol::stream::StreamError,
    {
        self.__add_header_internal("cache-control", "no-cache")
            .stream_with(Cow::Borrowed("text/event-stream"), producer)
    }

    pub fn declare_trailers<S: AsRef<str>>(self, header_names: S) -> Self {
        self.__add_header_internal("trailer", header_names.as_ref())
    }

    pub fn empty(self) -> HttpResponse<HttpResponseBodyInitialized> {
        self.into_initialized(None)
    }
}

impl HttpResponse<HttpResponseBodyInitialized> {
    pub async fn compress(mut self) -> Self {
        let fallback_ae = self.field_lines.remove("accept-encoding");
        let mask = if self.accept_encoding_mask != 0 {
            self.accept_encoding_mask
        } else if let Some(ref ae) = fallback_ae {
            parse_accept_encoding_mask(ae)
        } else {
            0
        };

        if mask == 0 || self.field_lines.get("content-encoding").is_some() {
            return self;
        }

        let Some(body) = self.body.take() else {
            return self;
        };

        let is_compressible = self
            .content_type
            .as_deref()
            .filter(|ct| !ct.eq_ignore_ascii_case("text/event-stream"))
            .map(Compress::is_compressible)
            .unwrap_or(false);

        if !is_compressible {
            self.body = Some(body);
            return self;
        }

        let encoding = if (mask & ENC_ZSTD) != 0 {
            "zstd"
        } else if (mask & ENC_BR) != 0 {
            "br"
        } else if (mask & ENC_GZIP) != 0 {
            "gzip"
        } else {
            self.body = Some(body);
            return self;
        };

        match body {
            Body::Bytes {
                bytes,
                content_type,
            } => {
                if bytes.len() < 1024 {
                    self.body = Some(Body::Bytes {
                        bytes,
                        content_type,
                    });
                    return self;
                }

                let orig_len = bytes.len();
                let compressed = Compress::compress_bytes(bytes.clone(), encoding).await;

                if compressed.len() >= orig_len {
                    self.body = Some(Body::Bytes {
                        bytes,
                        content_type,
                    });
                    return self;
                }

                self.content_length = Some(compressed.len() as u64);
                self.field_lines.set("content-encoding", encoding);
                self.field_lines.set("vary", "Accept-Encoding");

                self.body = Some(Body::PooledVec {
                    bytes: compressed,
                    content_type,
                });
            }
            Body::PooledVec {
                bytes,
                content_type,
            } => {
                if bytes.len() < 1024 {
                    self.body = Some(Body::PooledVec {
                        bytes,
                        content_type,
                    });
                    return self;
                }

                let orig_len = bytes.len();
                let owned_bytes = Bytes::from(bytes);
                let compressed = Compress::compress_bytes(owned_bytes.clone(), encoding).await;

                if compressed.len() >= orig_len {
                    self.body = Some(Body::Bytes {
                        bytes: owned_bytes,
                        content_type,
                    });
                    return self;
                }

                self.content_length = Some(compressed.len() as u64);
                self.field_lines.set("content-encoding", encoding);
                self.field_lines.set("vary", "Accept-Encoding");

                self.body = Some(Body::PooledVec {
                    bytes: compressed,
                    content_type,
                });
            }
            Body::Stream {
                reader,
                content_type,
                ..
            } => {
                self.body = Some(Self::wrap_compressed_stream(
                    reader,
                    content_type,
                    encoding,
                    &mut self.content_length,
                    &mut self.is_chunked,
                    &mut self.field_lines,
                ));
            }
            Body::ChunkStream { source, .. } => {
                let reader: Box<dyn AsyncRead + Unpin + Send> =
                    Box::new(crate::protocol::stream::ChunkSourceReader::new(source));
                self.body = Some(Self::wrap_compressed_stream(
                    reader,
                    None,
                    encoding,
                    &mut self.content_length,
                    &mut self.is_chunked,
                    &mut self.field_lines,
                ));
            }
        }

        self
    }

    fn wrap_compressed_stream(
        reader: Box<dyn AsyncRead + Unpin + Send>,
        content_type: Option<String>,
        encoding: &'static str,
        content_length: &mut Option<u64>,
        is_chunked: &mut bool,
        field_lines: &mut FieldLines,
    ) -> Body {
        field_lines.remove("content-length");
        *content_length = None;
        *is_chunked = true;
        field_lines.set("content-encoding", encoding);
        field_lines.set("vary", "Accept-Encoding");

        let buffered = BufReader::with_capacity(64 * 1024, reader);
        let compressed_reader: Box<dyn AsyncRead + Unpin + Send> = match encoding {
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
            reader: compressed_reader,
            content_length: 0,
            content_type,
        }
    }
}

impl<State> HttpResponse<State> {
    pub fn status_code(mut self, status_code: HttpStatus) -> Self {
        self.status_code = status_code;
        self
    }

    pub(crate) fn set_on_set(mut self, hook: ResponseHook) -> Self {
        self.on_sent = Some(hook);
        self
    }

    pub fn add_header<K: AsRef<str>, V: AsRef<str>>(
        mut self,
        field_name: K,
        field_value: V,
    ) -> Self {
        let field_name = field_name.as_ref();
        if !FieldLines::is_reserved(field_name) {
            self.field_lines.set(field_name, field_value.as_ref());
        }
        self
    }

    pub(crate) fn __add_header_internal<K: AsRef<str>, V: AsRef<str>>(
        mut self,
        field_name: K,
        field_value: V,
    ) -> Self {
        self.field_lines
            .set(field_name.as_ref(), field_value.as_ref());
        self
    }

    pub fn remove_header<S: AsRef<str>>(mut self, field_name: S) -> Self {
        let field_name = field_name.as_ref();
        if !FieldLines::is_reserved(field_name) {
            self.field_lines.remove(field_name);
        }
        self
    }

    pub(crate) fn __remove_header_internal<S: AsRef<str>>(mut self, field_name: S) -> Self {
        self.field_lines.remove(field_name.as_ref());
        self
    }

    pub fn add_cookie<K: Into<String>, V: Into<String>>(
        mut self,
        cookie_name: K,
        cookie_value: V,
        options: Option<CookieOptions>,
    ) -> Self {
        let cookie_store = self.cookies.get_or_insert_with(CookieStore::new);
        cookie_store.set(cookie_name.into(), cookie_value.into(), options);
        self
    }

    pub fn remove_cookie<K: Into<String>>(mut self, cookie_name: K) -> Self {
        if let Some(cookie_store) = self.cookies.as_mut() {
            cookie_store.remove(&cookie_name.into());
        }
        self
    }

    pub(crate) fn suppress_body(mut self) -> Self {
        self.suppress_body = true;
        self
    }
}

pub struct HttpResponseInit<'a> {
    pub(crate) connection: &'a mut Connection,
    pub(crate) response: HttpResponse<HttpResponseBodyUnInitialized>,
}

impl<'a> HttpResponseInit<'a> {
    pub fn status_code(mut self, code: HttpStatus) -> Self {
        self.response = self.response.status_code(code);
        self
    }

    pub fn add_header<K: AsRef<str>, V: AsRef<str>>(mut self, k: K, v: V) -> Self {
        self.response = self.response.add_header(k, v);
        self
    }

    pub(crate) fn __add_header_internal<K: AsRef<str>, V: AsRef<str>>(
        mut self,
        k: K,
        v: V,
    ) -> Self {
        self.response = self.response.__add_header_internal(k, v);
        self
    }

    pub fn remove_header<K: AsRef<str>>(mut self, name: K) -> Self {
        self.response = self.response.remove_header(name);
        self
    }

    pub(crate) fn __remove_header_internal<K: AsRef<str>>(mut self, name: K) -> Self {
        self.response = self.response.__remove_header_internal(name);
        self
    }

    pub fn add_cookie<K: Into<String>, V: Into<String>>(
        mut self,
        name: K,
        value: V,
        opts: Option<CookieOptions>,
    ) -> Self {
        self.response = self.response.add_cookie(name, value, opts);
        self
    }

    pub fn remove_cookie<K: Into<String>>(mut self, name: K) -> Self {
        self.response = self.response.remove_cookie(name);
        self
    }

    pub fn json<V: crate::protocol::body::ToJson>(self, value: V) -> HttpResponseReady<'a> {
        let mut buf = WorkerScratch::take_body_buf();
        buf.clear();
        buf.reserve(value.size_hint());
        value.write_json(&mut buf);
        HttpResponseReady {
            connection: self.connection,
            response: self.response.into_pooled_body(buf, "application/json"),
        }
    }

    pub fn bytes<V: Into<Bytes>, C: Into<Cow<'static, str>>>(
        self,
        b: V,
        ct: C,
    ) -> HttpResponseReady<'a> {
        HttpResponseReady {
            connection: self.connection,
            response: self.response.bytes(b, ct),
        }
    }

    pub fn text<V: AsRef<str>>(self, v: V) -> HttpResponseReady<'a> {
        let mut buf = WorkerScratch::take_body_buf();
        buf.clear();
        buf.extend_from_slice(v.as_ref().as_bytes());
        HttpResponseReady {
            connection: self.connection,
            response: self.response.into_pooled_body(buf, "text/plain"),
        }
    }

    pub fn stream_from<S, C>(self, source: S, content_type: C) -> HttpResponseReady<'a>
    where
        S: crate::protocol::stream::IntoBodyStream + 'static,
        C: Into<Cow<'static, str>>,
    {
        HttpResponseReady {
            connection: self.connection,
            response: self.response.stream_from(source, content_type),
        }
    }

    pub fn stream_from_len<S, C>(
        self,
        source: S,
        content_len: u64,
        content_type: C,
    ) -> HttpResponseReady<'a>
    where
        S: crate::protocol::stream::IntoBodyStream + 'static,
        C: Into<Cow<'static, str>>,
    {
        HttpResponseReady {
            connection: self.connection,
            response: self
                .response
                .stream_from_len(source, content_len, content_type),
        }
    }

    pub fn stream_with<F, Fut, E, C>(self, content_type: C, producer: F) -> HttpResponseReady<'a>
    where
        F: FnOnce(crate::protocol::stream::StreamSender) -> Fut + 'static,
        Fut: Future<Output = Result<(), E>> + 'static,
        E: crate::protocol::stream::StreamError,
        C: Into<Cow<'static, str>>,
    {
        HttpResponseReady {
            connection: self.connection,
            response: self.response.stream_with(content_type, producer),
        }
    }

    pub fn sse<F, Fut, E>(self, producer: F) -> HttpResponseReady<'a>
    where
        F: FnOnce(crate::protocol::stream::StreamSender) -> Fut + 'static,
        Fut: Future<Output = Result<(), E>> + 'static,
        E: crate::protocol::stream::StreamError,
    {
        HttpResponseReady {
            connection: self.connection,
            response: self.response.sse(producer),
        }
    }
    pub fn empty(self) -> HttpResponseReady<'a> {
        HttpResponseReady {
            connection: self.connection,
            response: self.response.empty(),
        }
    }
}

pub struct HttpResponseReady<'a> {
    pub(crate) connection: &'a mut Connection,
    pub(crate) response: HttpResponse<HttpResponseBodyInitialized>,
}

impl<'a> HttpResponseReady<'a> {
    pub fn status_code(mut self, code: HttpStatus) -> Self {
        self.response = self.response.status_code(code);
        self
    }

    pub fn add_header<K: AsRef<str>, V: AsRef<str>>(mut self, k: K, v: V) -> Self {
        self.response = self.response.add_header(k, v);
        self
    }

    pub async fn compress(mut self) -> Self {
        self.response = self.response.compress().await;
        self
    }

    pub async fn failable_send(self) -> Result<(), tokio::io::Error> {
        self.connection.write_response(self.response).await
    }

    pub async fn send(self) {
        let _ = self.failable_send().await;
    }
}
