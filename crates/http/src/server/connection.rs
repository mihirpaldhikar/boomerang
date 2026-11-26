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

use crate::protocol::{
    Body, FieldLines, HttpRequest, HttpRequestError, HttpResponse, HttpResponseBodyInitialized,
    HttpStatus, TrustedProxies,
};
use crate::server::RestartHandle;
use crate::server::transport::Transport;
use bytes::BytesMut;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::IoSlice;
use std::ops::Range;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const DATE_HEADER_LEN: usize = 37;
const MAX_POOLED_READ_BUFS: usize = 32;

pub(crate) struct WorkerScratch {
    header_buf: Vec<u8>,
    body_buf: Vec<u8>,
    req_fields: FieldLines,
    resp_fields: FieldLines,
    route_buf: String,
    path_params_buf: Vec<(Arc<str>, Range<usize>)>,
    extras_buf: HashMap<String, String>,
    read_pool: Vec<BytesMut>,
    cached_date: (u64, [u8; DATE_HEADER_LEN]),
}

static MONOTONIC_EPOCH: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

thread_local! {
    static WORKER_SCRATCH: RefCell<WorkerScratch> = RefCell::new(WorkerScratch {
        header_buf: Vec::with_capacity(1024),
        body_buf: Vec::with_capacity(4096),
        req_fields: FieldLines::with_capacity(16, 1024),
        resp_fields: FieldLines::with_capacity(8, 256),
        route_buf: String::with_capacity(128),
        path_params_buf: Vec::with_capacity(4),
        extras_buf: HashMap::new(),
        read_pool: Vec::with_capacity(MAX_POOLED_READ_BUFS),
        cached_date: (u64::MAX, *b"date: Thu, 01 Jan 1970 00:00:00 GMT\r\n"),
    });
}

#[inline]
pub(crate) fn coarse_elapsed_secs() -> u64 {
    MONOTONIC_EPOCH.elapsed().as_secs()
}

impl WorkerScratch {
    #[inline]
    pub(crate) fn take_body_buf() -> Vec<u8> {
        WORKER_SCRATCH.with(|cell| {
            let mut s = cell.borrow_mut();
            let mut buf = std::mem::take(&mut s.body_buf);
            buf.clear();
            buf
        })
    }

    #[inline]
    pub(crate) fn take_resp_fields() -> FieldLines {
        WORKER_SCRATCH.with(|cell| {
            let mut s = cell.borrow_mut();
            let mut fl = std::mem::take(&mut s.resp_fields);
            fl.clear();
            fl
        })
    }

    #[inline]
    pub(crate) fn take_extras_buf() -> HashMap<String, String> {
        WORKER_SCRATCH.with(|cell| {
            let mut s = cell.borrow_mut();
            let mut map = std::mem::take(&mut s.extras_buf);
            map.clear();
            map
        })
    }

    #[inline]
    fn take_req_buffers() -> (FieldLines, String, Vec<(Arc<str>, Range<usize>)>) {
        WORKER_SCRATCH.with(|cell| {
            let mut s = cell.borrow_mut();
            (
                std::mem::take(&mut s.req_fields),
                std::mem::take(&mut s.route_buf),
                std::mem::take(&mut s.path_params_buf),
            )
        })
    }

    #[inline]
    pub(crate) fn recycle_request(
        mut req_fields: FieldLines,
        mut route_buf: String,
        mut path_params: Vec<(Arc<str>, Range<usize>)>,
        mut extras: HashMap<String, String>,
    ) {
        req_fields.clear();
        route_buf.clear();
        path_params.clear();
        extras.clear();

        WORKER_SCRATCH.with(|cell| {
            let mut s = cell.borrow_mut();
            if s.req_fields.arena_capacity() == 0 && req_fields.arena_capacity() <= 32 * 1024 {
                s.req_fields = req_fields;
            }
            if s.route_buf.capacity() == 0 && route_buf.capacity() <= 4096 {
                s.route_buf = route_buf;
            }
            if s.path_params_buf.capacity() == 0 && path_params.capacity() <= 32 {
                s.path_params_buf = path_params;
            }
            if s.extras_buf.capacity() == 0 && extras.capacity() <= 32 {
                s.extras_buf = extras;
            }
        });
    }

    #[inline]
    fn take_read_buf() -> BytesMut {
        WORKER_SCRATCH.with(|cell| {
            cell.borrow_mut()
                .read_pool
                .pop()
                .unwrap_or_else(|| BytesMut::with_capacity(8 * 1024))
        })
    }

    #[inline]
    fn recycle_read_buf(buf: BytesMut) {
        if buf.capacity() >= 4096 && buf.capacity() <= 32 * 1024 {
            WORKER_SCRATCH.with(|cell| {
                let mut s = cell.borrow_mut();
                if s.read_pool.len() < MAX_POOLED_READ_BUFS {
                    s.read_pool.push(buf);
                }
            });
        }
    }

    #[inline]
    fn take_header_and_date() -> (Vec<u8>, [u8; DATE_HEADER_LEN]) {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        WORKER_SCRATCH.with(|cell| {
            let mut s = cell.borrow_mut();
            let mut buf = std::mem::take(&mut s.header_buf);
            buf.clear();
            if s.cached_date.0 != now_secs {
                s.cached_date.0 = now_secs;
                format_http_date_header(now_secs, &mut s.cached_date.1);
            }
            (buf, s.cached_date.1)
        })
    }
}

#[inline]
fn write_two_digits(val: u32, dst: &mut [u8]) {
    dst[0] = b'0' + (val / 10) as u8;
    dst[1] = b'0' + (val % 10) as u8;
}

#[inline]
fn format_http_date_header(unix_secs: u64, out: &mut [u8; DATE_HEADER_LEN]) {
    const DAYS: [&[u8; 3]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
    const MONTHS: [&[u8; 3]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];

    let days = (unix_secs / 86_400) as i64;
    let rem = (unix_secs % 86_400) as u32;
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

    out[0..6].copy_from_slice(b"date: ");
    out[6..9].copy_from_slice(DAYS[wday]);
    out[9..11].copy_from_slice(b", ");
    write_two_digits(day, &mut out[11..13]);
    out[13] = b' ';
    out[14..17].copy_from_slice(MONTHS[(month - 1) as usize]);
    out[17] = b' ';
    write_two_digits(year / 100, &mut out[18..20]);
    write_two_digits(year % 100, &mut out[20..22]);
    out[22] = b' ';
    write_two_digits(hour, &mut out[23..25]);
    out[25] = b':';
    write_two_digits(min, &mut out[26..28]);
    out[28] = b':';
    write_two_digits(sec, &mut out[29..31]);
    out[31..37].copy_from_slice(b" GMT\r\n");
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

async fn write_all_vectored_pair<W: AsyncWrite + Unpin>(
    writer: &mut W,
    mut a: &[u8],
    mut b: &[u8],
) -> std::io::Result<()> {
    while !a.is_empty() || !b.is_empty() {
        let n = if a.is_empty() {
            writer.write(b).await?
        } else if b.is_empty() {
            writer.write(a).await?
        } else {
            let bufs = [IoSlice::new(a), IoSlice::new(b)];
            writer.write_vectored(&bufs).await?
        };
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write whole vectored buffer",
            ));
        }
        if n >= a.len() {
            b = &b[n - a.len()..];
            a = &[];
        } else {
            a = &a[n..];
        }
    }
    Ok(())
}

async fn write_all_vectored_triple<W: AsyncWrite + Unpin>(
    writer: &mut W,
    a: &[u8],
    b: &[u8],
    c: &[u8],
) -> std::io::Result<()> {
    let mut slices: [&[u8]; 3] = [a, b, c];
    let mut idx = 0usize;
    while idx < 3 {
        while idx < 3 && slices[idx].is_empty() {
            idx += 1;
        }
        if idx == 3 {
            break;
        }
        let mut iov = [IoSlice::new(&[]), IoSlice::new(&[]), IoSlice::new(&[])];
        let mut count = 0usize;
        for s in &slices[idx..] {
            if !s.is_empty() {
                iov[count] = IoSlice::new(s);
                count += 1;
            }
        }
        let mut n = writer.write_vectored(&iov[..count]).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write whole vectored chunk",
            ));
        }
        while idx < 3 && n > 0 {
            let len = slices[idx].len();
            if n >= len {
                n -= len;
                slices[idx] = &[];
                idx += 1;
            } else {
                slices[idx] = &slices[idx][n..];
                n = 0;
            }
        }
    }
    Ok(())
}

pub(crate) struct Connection {
    pub(crate) transport: Transport,
    pub(crate) reader: BytesMut,
    pipeline_buf: Vec<u8>,
    keep_alive_header: Arc<[u8]>,
    pub(crate) restart_handle: RestartHandle,
    has_written_response: bool,
    keep_alive: bool,
    trusted_proxies: Arc<TrustedProxies>,
}

impl Connection {
    #[inline]
    pub(crate) fn with_server_state(
        transport: Transport,
        keep_alive_header: Arc<[u8]>,
        restart_handle: RestartHandle,
        trusted_proxies: Arc<TrustedProxies>,
    ) -> Self {
        Self {
            transport,
            reader: BytesMut::new(),
            pipeline_buf: Vec::new(),
            keep_alive_header,
            restart_handle,
            has_written_response: false,
            keep_alive: true,
            trusted_proxies,
        }
    }

    #[inline]
    pub(crate) fn set_keep_alive(&mut self, keep_alive: bool) {
        self.keep_alive = keep_alive;
    }

    #[inline]
    pub(crate) fn has_written_response(&self) -> bool {
        self.has_written_response
    }

    #[inline]
    pub(crate) async fn read_more(&mut self) -> std::io::Result<usize> {
        if self.reader.capacity() == 0 {
            self.reader = WorkerScratch::take_read_buf();
        } else if self.reader.capacity() - self.reader.len() < 4096 {
            self.reader.reserve(8192);
        }
        self.transport.read_buf(&mut self.reader).await
    }

    #[inline]
    pub(crate) async fn write_all_raw(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.flush_pipeline().await?;
        self.transport.write_all(buf).await
    }

    #[inline]
    pub(crate) async fn flush_pipeline(&mut self) -> std::io::Result<()> {
        if !self.pipeline_buf.is_empty() {
            let res = self.transport.write_all(&self.pipeline_buf).await;
            self.pipeline_buf.clear();
            res?;
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn recycle_reader_if_empty(&mut self) {
        if self.reader.is_empty() && self.reader.capacity() > 0 {
            let buf = std::mem::take(&mut self.reader);
            WorkerScratch::recycle_read_buf(buf);
        }
    }

    pub(crate) async fn read_request(
        &mut self,
        max_body_size: usize,
        keep_alive_timeout: std::time::Duration,
        request_timeout: std::time::Duration,
        mut idle_sleep: std::pin::Pin<&mut tokio::time::Sleep>,
        last_timer_sec: &mut u64,
    ) -> Result<Option<HttpRequest>, HttpRequestError> {
        self.has_written_response = false;

        let peer_addr = self.transport.peer_addr();

        if self.reader.is_empty() {
            if let Err(e) = self.flush_pipeline().await {
                return Err(HttpRequestError::Io(e));
            }

            self.recycle_reader_if_empty();

            let now_sec = coarse_elapsed_secs();
            if keep_alive_timeout < std::time::Duration::from_secs(2)
                || now_sec != *last_timer_sec
                || idle_sleep.is_elapsed()
            {
                *last_timer_sec = now_sec;
                idle_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + keep_alive_timeout);
            }

            let idle_read = async {
                if self.transport.is_plain_tcp() {
                    loop {
                        self.transport.wait_readable().await?;
                        let mut buf = WorkerScratch::take_read_buf();
                        match self.transport.try_read_buf(&mut buf) {
                            Ok(n) => {
                                if n > 0 {
                                    self.reader = buf;
                                } else {
                                    WorkerScratch::recycle_read_buf(buf);
                                }
                                return Ok(n);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                WorkerScratch::recycle_read_buf(buf);
                                continue;
                            }
                            Err(e) => {
                                WorkerScratch::recycle_read_buf(buf);
                                return Err(e);
                            }
                        }
                    }
                } else {
                    self.read_more().await
                }
            };

            let idle_res = tokio::select! {
                biased;
                res = idle_read => Some(res),
                _ = idle_sleep.as_mut() => None,
            };

            match idle_res {
                Some(Ok(0)) | None => {
                    self.recycle_reader_if_empty();
                    return Ok(None);
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    self.recycle_reader_if_empty();
                    return Err(HttpRequestError::Io(e));
                }
            }
        }

        if let Some(pos) = memchr::memmem::find(&self.reader, b"\r\n\r\n") {
            let header_len = pos + 4;
            let (req_fields, route_buf, path_params) = WorkerScratch::take_req_buffers();
            let req = HttpRequest::parse(
                &self.reader[..header_len],
                peer_addr,
                max_body_size,
                request_timeout,
                &self.trusted_proxies,
                req_fields,
                route_buf,
                path_params,
            )?;
            if self.reader.len() == header_len {
                self.reader.clear();
            } else {
                use bytes::Buf;
                self.reader.advance(header_len);
            }
            return Ok(Some(req));
        }

        if self.reader.len() > 64 * 1024 {
            return Err(HttpRequestError::HeadersTooLarge);
        }

        let mut scan_from = self.reader.len().saturating_sub(3);
        let parse_future = async {
            loop {
                self.flush_pipeline().await.map_err(HttpRequestError::Io)?;

                let n = self.read_more().await.map_err(HttpRequestError::Io)?;
                if n == 0 {
                    return Err(HttpRequestError::HeaderParsingFailed(
                        "EOF before headers finished".into(),
                    ));
                }

                if let Some(rel) = memchr::memmem::find(&self.reader[scan_from..], b"\r\n\r\n") {
                    let header_len = scan_from + rel + 4;
                    let (req_fields, route_buf, path_params) = WorkerScratch::take_req_buffers();
                    let req = HttpRequest::parse(
                        &self.reader[..header_len],
                        peer_addr,
                        max_body_size,
                        request_timeout,
                        &self.trusted_proxies,
                        req_fields,
                        route_buf,
                        path_params,
                    )?;
                    if self.reader.len() == header_len {
                        self.reader.clear();
                    } else {
                        use bytes::Buf;
                        self.reader.advance(header_len);
                    }
                    return Ok(req);
                }

                if self.reader.len() > 64 * 1024 {
                    return Err(HttpRequestError::HeadersTooLarge);
                }

                scan_from = self.reader.len().saturating_sub(3);
            }
        };

        match tokio::time::timeout(request_timeout, parse_future).await {
            Ok(Ok(req)) => Ok(Some(req)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(HttpRequestError::Timeout),
        }
    }

    pub(crate) async fn write_response(
        &mut self,
        response: HttpResponse<HttpResponseBodyInitialized>,
    ) -> Result<(), tokio::io::Error> {
        self.has_written_response = true;

        let status_forbids_body = matches!(
            response.status_code,
            HttpStatus::NoContent | HttpStatus::NotModified
        );

        let (mut header_buf, date_bytes) = WorkerScratch::take_header_and_date();

        header_buf.extend_from_slice(&response.http_version);
        header_buf.push(b' ');
        header_buf.extend_from_slice(response.status_code.as_bytes());
        header_buf.extend_from_slice(b"\r\n");

        header_buf.extend_from_slice(&date_bytes);

        header_buf.extend_from_slice(b"x-request-id: ");
        let mut id_buf = [0u8; 33];
        header_buf.extend_from_slice(response.request_id.encode(&mut id_buf).as_bytes());
        header_buf.extend_from_slice(b"\r\n");

        if self.keep_alive {
            header_buf.extend_from_slice(&self.keep_alive_header);
        } else {
            header_buf.extend_from_slice(b"connection: close\r\n");
        }

        if let Some(ct) = &response.content_type {
            header_buf.extend_from_slice(b"content-type: ");
            header_buf.extend_from_slice(ct.as_bytes());
            header_buf.extend_from_slice(b"\r\n");
        }

        if !status_forbids_body {
            if response.is_chunked {
                header_buf.extend_from_slice(b"transfer-encoding: chunked\r\n");
            } else if let Some(len) = response.content_length {
                header_buf.extend_from_slice(b"content-length: ");
                let mut num_buf = [0u8; 20];
                header_buf.extend_from_slice(format_u64(len, &mut num_buf));
                header_buf.extend_from_slice(b"\r\n");
            } else if response.body.is_none()
                && response.field_lines.get("content-length").is_none()
            {
                header_buf.extend_from_slice(b"content-length: 0\r\n");
            }
        }

        for (field_name, field_value) in response.field_lines.iter() {
            if matches!(
                field_name,
                "date" | "connection" | "keep-alive" | "x-request-id"
            ) {
                continue;
            }
            header_buf.extend_from_slice(field_name.as_bytes());
            header_buf.extend_from_slice(b": ");
            header_buf.extend_from_slice(field_value.as_bytes());
            header_buf.extend_from_slice(b"\r\n");
        }

        if let Some(store) = &response.cookies {
            let bytes = store.to_bytes();
            if !bytes.is_empty() {
                header_buf.extend_from_slice(&bytes);
            }
        }

        header_buf.extend_from_slice(b"\r\n");

        let write_body = !response.suppress_body && !status_forbids_body;
        let is_chunked = response.is_chunked;

        let pipelining = !self.reader.is_empty();
        let mut header_buf_opt = Some(header_buf);
        let mut resp_fields_opt = Some(response.field_lines);
        let mut recycled_body: Option<Vec<u8>> = None;

        let write_res: Result<(), tokio::io::Error> = async {
            let hb = header_buf_opt.as_ref().unwrap();
            match (write_body, is_chunked, response.body) {
                (true, false, Some(Body::Bytes { bytes, .. })) => {
                    if pipelining && self.pipeline_buf.len() + hb.len() + bytes.len() <= 8 * 1024 {
                        self.pipeline_buf.extend_from_slice(hb);
                        self.pipeline_buf.extend_from_slice(&bytes);
                    } else {
                        self.flush_pipeline().await?;
                        write_all_vectored_pair(&mut self.transport, hb, &bytes).await?;
                    }
                }
                (true, false, Some(Body::PooledVec { bytes, .. })) => {
                    if pipelining && self.pipeline_buf.len() + hb.len() + bytes.len() <= 8 * 1024 {
                        self.pipeline_buf.extend_from_slice(hb);
                        self.pipeline_buf.extend_from_slice(&bytes);
                    } else {
                        self.flush_pipeline().await?;
                        write_all_vectored_pair(&mut self.transport, hb, &bytes).await?;
                    }
                    recycled_body = Some(bytes);
                }
                (false, _, _) | (true, false, None) => {
                    if pipelining && self.pipeline_buf.len() + hb.len() <= 8 * 1024 {
                        self.pipeline_buf.extend_from_slice(hb);
                    } else {
                        self.flush_pipeline().await?;
                        self.transport.write_all(hb).await?;
                    }
                }
                (true, _, body) => {
                    self.flush_pipeline().await?;
                    self.transport.write_all(hb).await?;

                    let mut hb_owned = header_buf_opt.take().unwrap();
                    let mut rf_owned = resp_fields_opt.take().unwrap();
                    hb_owned.clear();
                    rf_owned.clear();
                    WORKER_SCRATCH.with(|cell| {
                        let mut s = cell.borrow_mut();
                        if s.header_buf.capacity() == 0 && hb_owned.capacity() <= 16 * 1024 {
                            s.header_buf = hb_owned;
                        }
                        if s.resp_fields.arena_capacity() == 0
                            && rf_owned.arena_capacity() <= 16 * 1024
                        {
                            s.resp_fields = rf_owned;
                        }
                    });

                    recycled_body = self.write_body_slow(body, is_chunked).await?;
                }
            }
            Ok(())
        }
        .await;

        WORKER_SCRATCH.with(|cell| {
            let mut s = cell.borrow_mut();
            if let Some(mut hb) = header_buf_opt {
                hb.clear();
                if s.header_buf.capacity() == 0 && hb.capacity() <= 16 * 1024 {
                    s.header_buf = hb;
                }
            }
            if let Some(mut rf) = resp_fields_opt {
                rf.clear();
                if s.resp_fields.arena_capacity() == 0 && rf.arena_capacity() <= 16 * 1024 {
                    s.resp_fields = rf;
                }
            }
            if let Some(mut body_buf) = recycled_body {
                body_buf.clear();
                if s.body_buf.capacity() == 0 && body_buf.capacity() <= 64 * 1024 {
                    s.body_buf = body_buf;
                }
            }
        });

        write_res?;

        if let Some(hook) = response.on_sent {
            hook(response.status_code);
        }

        Ok(())
    }

    async fn write_body_slow(
        &mut self,
        body: Option<Body>,
        is_chunked: bool,
    ) -> Result<Option<Vec<u8>>, tokio::io::Error> {
        match body {
            Some(Body::Bytes { bytes, .. }) => {
                if is_chunked {
                    self.write_chunk(&bytes).await?;
                    self.transport.write_all(b"0\r\n\r\n").await?;
                } else {
                    self.transport.write_all(&bytes).await?;
                }
                Ok(None)
            }
            Some(Body::PooledVec { bytes, .. }) => {
                if is_chunked {
                    self.write_chunk(&bytes).await?;
                    self.transport.write_all(b"0\r\n\r\n").await?;
                } else {
                    self.transport.write_all(&bytes).await?;
                }
                Ok(Some(bytes))
            }
            Some(Body::Stream { mut reader, .. }) => {
                if is_chunked {
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        let n = reader.read(&mut buf).await?;
                        if n == 0 {
                            break;
                        }
                        let mut hex_buf = [0u8; 18];
                        let hex_str = format_hex(n, &mut hex_buf);
                        write_all_vectored_triple(&mut self.transport, hex_str, &buf[..n], b"\r\n")
                            .await?;
                    }
                    self.transport.write_all(b"0\r\n\r\n").await?;
                } else {
                    let mut buffered = BufReader::with_capacity(64 * 1024, reader);
                    tokio::io::copy_buf(&mut buffered, &mut self.transport).await?;
                }
                Ok(None)
            }
            Some(Body::ChunkStream {
                mut source,
                content_length,
            }) => {
                let mut written: u64 = 0;
                let mut sent_terminator = false;
                loop {
                    let next = std::future::poll_fn(|cx| source.poll_next(cx)).await;
                    match next {
                        Some(crate::protocol::StreamPoll::Data(chunk)) => {
                            if chunk.is_empty() {
                                continue;
                            }
                            if !is_chunked
                                && let Some(expected) = content_length
                                && written + (chunk.len() as u64) > expected
                            {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "stream exceeded declared Content-Length",
                                ));
                            }

                            let write_fut = async {
                                if is_chunked {
                                    let mut hex_buf = [0u8; 18];
                                    let hex_str = format_hex(chunk.len(), &mut hex_buf);
                                    write_all_vectored_triple(
                                        &mut self.transport,
                                        hex_str,
                                        &chunk,
                                        b"\r\n",
                                    )
                                    .await
                                } else {
                                    self.transport.write_all(&chunk).await
                                }
                            };

                            match tokio::time::timeout(
                                std::time::Duration::from_secs(30),
                                write_fut,
                            )
                            .await
                            {
                                Ok(res) => res?,
                                Err(_) => {
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::TimedOut,
                                        "stream write timed out",
                                    ));
                                }
                            }
                            written += chunk.len() as u64;
                        }
                        Some(crate::protocol::StreamPoll::Trailers(trailers)) => {
                            if is_chunked {
                                let mut tbuf = Vec::with_capacity(128);
                                tbuf.extend_from_slice(b"0\r\n");
                                for (k, v) in trailers {
                                    let valid_k = !k.is_empty()
                                        && k.bytes().all(|b| {
                                            b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
                                        });
                                    let valid_v =
                                        !v.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7F);
                                    if valid_k && valid_v {
                                        tbuf.extend_from_slice(k.as_bytes());
                                        tbuf.extend_from_slice(b": ");
                                        tbuf.extend_from_slice(v.as_bytes());
                                        tbuf.extend_from_slice(b"\r\n");
                                    }
                                }
                                tbuf.extend_from_slice(b"\r\n");
                                self.transport.write_all(&tbuf).await?;
                                sent_terminator = true;
                            }
                            break;
                        }
                        Some(crate::protocol::StreamPoll::Abort) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "stream aborted by producer",
                            ));
                        }
                        None => break,
                    }
                }

                if is_chunked && !sent_terminator {
                    self.transport.write_all(b"0\r\n\r\n").await?;
                } else if !is_chunked && let Some(expected) = content_length {
                    if written != expected {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "stream ended before declared Content-Length",
                        ));
                    }
                }
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn write_chunk(&mut self, data: &[u8]) -> Result<(), tokio::io::Error> {
        let mut hex_buf = [0u8; 18];
        let hex_str = format_hex(data.len(), &mut hex_buf);
        write_all_vectored_triple(&mut self.transport, hex_str, data, b"\r\n").await
    }

    pub async fn shutdown(&mut self) -> Result<(), tokio::io::Error> {
        self.flush_pipeline().await?;
        self.recycle_reader_if_empty();
        self.transport.shutdown().await
    }

    pub(crate) fn into_transport(self) -> Transport {
        self.transport
    }
}

#[inline]
fn format_hex(n: usize, buf: &mut [u8; 18]) -> &[u8] {
    if n == 0 {
        buf[0] = b'0';
        buf[1] = b'\r';
        buf[2] = b'\n';
        return &buf[..3];
    }

    let mut i = 15usize;
    let mut val = n;
    while val > 0 {
        let digit = (val & 0xF) as u8;
        buf[i] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        i -= 1;
        val >>= 4;
    }
    i += 1;

    let end = 16;
    buf[end] = b'\r';
    buf[end + 1] = b'\n';

    &buf[i..end + 2]
}
