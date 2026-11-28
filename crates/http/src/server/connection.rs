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
    Body, FieldLines, HttpRequest, HttpRequestError, HttpStatus, ResponseHook, ResponseParts,
    StreamPoll,
};
use crate::server::encode::{Framing, HeadCtx};
use crate::server::transport::Transport;
use crate::server::{WorkerLocalState, encode};
use bytes::{Buf, BytesMut};
use memchr::memmem::Finder;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::io::IoSlice;
use std::ops::Range;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, LazyLock};
use std::task::Poll;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const DATE_HEADER_LEN: usize = 37;
const DATE_TEMPLATE: &[u8; DATE_HEADER_LEN] = b"date: Thu, 01 Jan 1970 00:00:00 GMT\r\n";

const MAX_POOLED_READ_BUFS: usize = 32;
const MAX_POOLED_STREAM_BUFS: usize = 8;
const STREAM_BUF_LEN: usize = 64 * 1024;

const OUT_INITIAL_CAP: usize = 512;
const OUT_IDLE_CAP: usize = 4 * 1024;
const COALESCE_MAX: usize = 16 * 1024;
const MAX_BATCH: u8 = 16;
const WRITE_SLICE: usize = 64 * 1024;
const MAX_HEAD: usize = 64 * 1024;

const CRLF: &[u8] = b"\r\n";
const LAST_CHUNK: &[u8] = b"0\r\n\r\n";
const CRLF_LAST_CHUNK: &[u8] = b"\r\n0\r\n\r\n";
const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";
const SMALL_WRITE: usize = 4 * 1024;

const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

static UNIX_SECS: AtomicU64 = AtomicU64::new(0);

static CLOCK_TICKER: LazyLock<bool> = LazyLock::new(|| {
    UNIX_SECS.store(wall_secs(), Relaxed);
    std::thread::Builder::new()
        .name("http-date-clock".into())
        .spawn(|| {
            loop {
                std::thread::sleep(Duration::from_millis(250));
                UNIX_SECS.store(wall_secs(), Relaxed);
            }
        })
        .is_ok()
});

static MONOTONIC_EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn wall_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[inline]
fn cached_unix_secs() -> u64 {
    if *CLOCK_TICKER {
        UNIX_SECS.load(Relaxed)
    } else {
        wall_secs()
    }
}

#[inline]
pub(crate) fn coarse_elapsed_secs() -> u64 {
    MONOTONIC_EPOCH.elapsed().as_secs()
}

type PathParams = Vec<(Arc<str>, Range<usize>)>;

pub(crate) struct WorkerScratch {
    body_buf: Vec<u8>,
    stream_pool: Vec<Vec<u8>>,
    req_fields: FieldLines,
    resp_fields: FieldLines,
    route_buf: String,
    path_params_buf: PathParams,
    extras_buf: HashMap<String, String>,
    read_pool: Vec<BytesMut>,
    cached_date: (u64, [u8; DATE_HEADER_LEN]),
}

thread_local! {
    static WORKER_SCRATCH: RefCell<WorkerScratch> = RefCell::new(WorkerScratch {
        body_buf: Vec::with_capacity(4096),
        stream_pool: Vec::new(),
        req_fields: FieldLines::with_capacity(16, 1024),
        resp_fields: FieldLines::with_capacity(8, 256),
        route_buf: String::with_capacity(128),
        path_params_buf: Vec::with_capacity(4),
        extras_buf: HashMap::new(),
        read_pool: Vec::with_capacity(MAX_POOLED_READ_BUFS),
        cached_date: (u64::MAX, *DATE_TEMPLATE),
    });
}

impl WorkerScratch {
    #[inline]
    pub(crate) fn take_body_buf() -> Vec<u8> {
        WORKER_SCRATCH.with_borrow_mut(|s| std::mem::take(&mut s.body_buf))
    }

    #[inline]
    pub(crate) fn take_resp_fields() -> FieldLines {
        WORKER_SCRATCH.with_borrow_mut(|s| std::mem::take(&mut s.resp_fields))
    }

    #[inline]
    pub(crate) fn take_extras_buf() -> HashMap<String, String> {
        WORKER_SCRATCH.with_borrow_mut(|s| std::mem::take(&mut s.extras_buf))
    }

    #[inline]
    fn take_req_buffers() -> (FieldLines, String, PathParams) {
        WORKER_SCRATCH.with_borrow_mut(|s| {
            (
                std::mem::take(&mut s.req_fields),
                std::mem::take(&mut s.route_buf),
                std::mem::take(&mut s.path_params_buf),
            )
        })
    }

    #[inline]
    pub(crate) fn take_stream_buf() -> Vec<u8> {
        WORKER_SCRATCH
            .with_borrow_mut(|s| s.stream_pool.pop())
            .unwrap_or_else(|| vec![0; STREAM_BUF_LEN])
    }

    #[inline]
    pub(crate) fn recycle_stream_buf(buf: Vec<u8>) {
        if buf.len() == STREAM_BUF_LEN {
            WORKER_SCRATCH.with_borrow_mut(|s| {
                if s.stream_pool.len() < MAX_POOLED_STREAM_BUFS {
                    s.stream_pool.push(buf);
                }
            });
        }
    }

    #[inline]
    pub(crate) fn recycle_request(
        mut req_fields: FieldLines,
        mut route_buf: String,
        mut path_params: PathParams,
        mut extras: HashMap<String, String>,
    ) {
        req_fields.clear();
        route_buf.clear();
        path_params.clear();
        extras.clear();

        WORKER_SCRATCH.with_borrow_mut(|s| {
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

    pub(crate) fn recycle_resp_fields(mut f: FieldLines) {
        f.clear();
        WORKER_SCRATCH.with_borrow_mut(|s| {
            if s.resp_fields.arena_capacity() == 0 && f.arena_capacity() <= 16 * 1024 {
                s.resp_fields = f;
            }
        });
    }

    pub(crate) fn recycle_body_buf(mut b: Vec<u8>) {
        b.clear();
        WORKER_SCRATCH.with_borrow_mut(|s| {
            if s.body_buf.capacity() == 0 && b.capacity() <= 64 * 1024 {
                s.body_buf = b;
            }
        });
    }

    #[inline]
    fn take_read_buf() -> BytesMut {
        WORKER_SCRATCH
            .with_borrow_mut(|s| s.read_pool.pop())
            .unwrap_or_else(|| BytesMut::with_capacity(8 * 1024))
    }

    #[inline]
    fn recycle_read_buf(buf: BytesMut) {
        debug_assert!(buf.is_empty());
        if (4096..=32 * 1024).contains(&buf.capacity()) {
            WORKER_SCRATCH.with_borrow_mut(|s| {
                if s.read_pool.len() < MAX_POOLED_READ_BUFS {
                    s.read_pool.push(buf);
                }
            });
        }
    }

    pub(crate) fn date() -> [u8; DATE_HEADER_LEN] {
        let now = cached_unix_secs();
        WORKER_SCRATCH.with_borrow_mut(|s| {
            if s.cached_date.0 != now {
                s.cached_date.0 = now;
                format_http_date_header(now, &mut s.cached_date.1);
            }
            s.cached_date.1
        })
    }
}

#[inline]
fn write_two_digits(val: u64, dst: &mut [u8]) {
    dst[0] = b'0' + (val / 10) as u8;
    dst[1] = b'0' + (val % 10) as u8;
}

fn format_http_date_header(unix_secs: u64, out: &mut [u8; DATE_HEADER_LEN]) {
    const DAYS: [&[u8; 3]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
    const MONTHS: [&[u8; 3]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];

    let days = unix_secs / 86_400;
    let rem = unix_secs % 86_400;

    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);

    *out = *DATE_TEMPLATE;
    out[6..9].copy_from_slice(DAYS[((days + 4) % 7) as usize]);
    write_two_digits(day, &mut out[11..13]);
    out[14..17].copy_from_slice(MONTHS[(month - 1) as usize]);
    write_two_digits(year / 100, &mut out[18..20]);
    write_two_digits(year % 100, &mut out[20..22]);
    write_two_digits(rem / 3600, &mut out[23..25]);
    write_two_digits(rem % 3600 / 60, &mut out[26..28]);
    write_two_digits(rem % 60, &mut out[29..31]);
}

#[inline]
pub(crate) fn format_u64(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &buf[i..];
        }
    }
}

#[inline]
fn chunk_size_line(mut n: usize, buf: &mut [u8; 18]) -> &[u8] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    buf[16..].copy_from_slice(CRLF);
    let mut i = 16;
    loop {
        i -= 1;
        buf[i] = HEX[n & 0xf];
        n >>= 4;
        if n == 0 {
            return &buf[i..];
        }
    }
}

#[inline]
fn find_head_end(buf: &[u8], from: usize) -> Option<usize> {
    static HEAD_END: LazyLock<Finder<'static>> = LazyLock::new(|| Finder::new(b"\r\n\r\n"));
    HEAD_END.find(&buf[from..]).map(|i| from + i + 4)
}

fn check(declared: Option<u64>, sent: u64, n: usize) -> io::Result<()> {
    match declared {
        Some(d) if sent + n as u64 > d => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "body exceeds declared Content-Length",
        )),
        _ => Ok(()),
    }
}

fn check_complete(declared: Option<u64>, sent: u64) -> io::Result<()> {
    match declared {
        Some(d) if sent != d => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "body shorter than declared Content-Length",
        )),
        _ => Ok(()),
    }
}

async fn timed<F>(fut: F) -> io::Result<()>
where
    F: Future<Output = io::Result<()>>,
{
    let mut fut = std::pin::pin!(fut);

    if let Poll::Ready(res) = std::future::poll_fn(|cx| Poll::Ready(fut.as_mut().poll(cx))).await {
        return res;
    }

    tokio::time::timeout(WRITE_TIMEOUT, fut)
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "client write timed out",
            ))
        })
}

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub max_body_size: usize,
    pub keep_alive: Duration,
    pub request_timeout: Duration,
}

struct PendingHook(Option<(ResponseHook, HttpStatus)>);

impl PendingHook {
    fn fire(&mut self, delivered: bool) {
        if let Some((hook, status)) = self.0.take() {
            hook(status, delivered);
        }
    }
}

impl Drop for PendingHook {
    fn drop(&mut self) {
        self.fire(false);
    }
}

pub(crate) struct Connection {
    pub(crate) transport: Transport,
    pub(crate) reader: BytesMut,
    out: Vec<u8>,
    batched: u8,
    hooks: Vec<PendingHook>,
    pub(crate) state: Rc<WorkerLocalState>,
    has_written_response: bool,
    keep_alive: bool,
}

impl Connection {
    pub(crate) fn with_worker_state(transport: Transport, state: Rc<WorkerLocalState>) -> Self {
        Self {
            transport,
            reader: BytesMut::new(),
            out: Vec::new(),
            batched: 0,
            hooks: Vec::new(),
            state,
            has_written_response: false,
            keep_alive: true,
        }
    }

    pub(crate) fn set_keep_alive(&mut self, v: bool) {
        self.keep_alive = v;
    }
    pub(crate) fn keep_alive(&self) -> bool {
        self.keep_alive
    }
    pub(crate) fn has_written_response(&self) -> bool {
        self.has_written_response
    }
    pub(crate) fn into_transport(self) -> Transport {
        self.transport
    }

    pub(crate) fn recycle_reader_if_empty(&mut self) {
        if self.reader.is_empty() && self.reader.capacity() > 0 {
            WorkerScratch::recycle_read_buf(std::mem::take(&mut self.reader));
        }
    }

    #[inline]
    fn pipelined(&self) -> bool {
        !self.reader.is_empty()
    }

    fn settle_hooks(&mut self, delivered: bool) {
        for mut hook in self.hooks.drain(..) {
            hook.fire(delivered);
        }
    }

    async fn write_out(&mut self, parts: &[&[u8]]) -> io::Result<()> {
        debug_assert!(parts.len() <= 3);
        let mut iov = [IoSlice::new(&[]); 4];
        iov[0] = IoSlice::new(&self.out);
        for (slot, p) in iov[1..].iter_mut().zip(parts) {
            *slot = IoSlice::new(p);
        }
        let n = parts.len() + 1;

        let res = timed(async {
            Self::write_all_vectored(&mut self.transport, &mut iov[..n]).await?;
            self.transport.flush().await
        })
        .await;

        self.out.clear();
        self.batched = 0;
        self.settle_hooks(res.is_ok());
        res
    }

    async fn emit(&mut self, parts: &[&[u8]], coalesce: bool) -> io::Result<()> {
        let n: usize = parts.iter().map(|p| p.len()).sum();
        if coalesce && self.out.len() + n <= COALESCE_MAX && (self.pipelined() || n <= SMALL_WRITE)
        {
            for p in parts {
                self.out.extend_from_slice(p);
            }
            return Ok(());
        }
        self.write_out(parts).await
    }

    pub(crate) async fn flush(&mut self) -> io::Result<()> {
        if self.out.is_empty() {
            Ok(())
        } else {
            self.write_out(&[]).await
        }
    }

    async fn put(
        &mut self,
        data: &[u8],
        chunked: bool,
        coalesce: bool,
        finish: bool,
    ) -> io::Result<()> {
        let mut pieces = data.chunks(WRITE_SLICE).peekable();
        while let Some(piece) = pieces.next() {
            if chunked {
                let mut line = [0u8; 18];
                let size = chunk_size_line(piece.len(), &mut line);
                let tail = if finish && pieces.peek().is_none() {
                    CRLF_LAST_CHUNK
                } else {
                    CRLF
                };
                self.emit(&[size, piece, tail], coalesce).await?;
            } else {
                self.emit(&[piece], coalesce).await?;
            }
        }
        if chunked && finish && data.is_empty() {
            self.emit(&[LAST_CHUNK], coalesce).await?;
        }
        Ok(())
    }

    pub(crate) async fn write_response(&mut self, mut r: ResponseParts) -> io::Result<()> {
        self.has_written_response = true;
        let hook = r
            .on_sent
            .take()
            .map(|h| PendingHook(Some((h, r.status_code.clone()))));

        let res = self.send_response(&mut r).await;

        self.hooks.extend(hook);
        if res.is_err() {
            self.keep_alive = false;
            self.out.clear();
            self.settle_hooks(false);
        } else if self.out.is_empty() {
            self.settle_hooks(true);
        }
        res
    }

    async fn send_response(&mut self, r: &mut ResponseParts) -> io::Result<()> {
        let framing = encode::framing(r);
        if framing == Framing::Close || crate::server::is_draining() {
            self.keep_alive = false;
        }

        let date = WorkerScratch::date();
        self.out.reserve(OUT_INITIAL_CAP);
        encode::encode_head(
            &mut self.out,
            r,
            framing,
            &HeadCtx {
                keep_alive: self.keep_alive,
                ka_header: &self.state.keep_alive_header,
                date: &date,
            },
        );
        WorkerScratch::recycle_resp_fields(std::mem::take(&mut r.field_lines));

        let body = r.body.take();
        if r.suppress_body || framing == Framing::None {
            if let Some(Body::PooledVec { bytes, .. }) = body {
                WorkerScratch::recycle_body_buf(bytes);
            }
        } else {
            self.send_body(body, framing, r.content_length).await?;
        }

        if self.keep_alive && self.pipelined() && self.batched + 1 < MAX_BATCH {
            self.batched += 1;
            Ok(())
        } else {
            self.flush().await
        }
    }

    async fn send_body(
        &mut self,
        body: Option<Body>,
        framing: Framing,
        len: Option<u64>,
    ) -> io::Result<()> {
        let chunked = framing == Framing::Chunked;
        let declared = len.filter(|_| framing == Framing::Length);

        match body {
            None => self.send_buffered(&[], chunked, declared).await,
            Some(Body::Bytes { bytes, .. }) => self.send_buffered(&bytes, chunked, declared).await,
            Some(Body::PooledVec { bytes, .. }) => {
                let res = self.send_buffered(&bytes, chunked, declared).await;
                WorkerScratch::recycle_body_buf(bytes);
                res
            }
            Some(Body::Stream { reader, .. }) => {
                self.flush().await?;
                let mut buf = WorkerScratch::take_stream_buf();
                let res = self.send_reader(reader, &mut buf, chunked, declared).await;
                WorkerScratch::recycle_stream_buf(buf);
                res
            }
            Some(Body::ChunkStream { mut source, .. }) => {
                self.flush().await?;
                let mut sent = 0u64;
                let trailers = loop {
                    match std::future::poll_fn(|cx| source.poll_next(cx)).await {
                        Some(StreamPoll::Data(c)) if c.is_empty() => {}
                        Some(StreamPoll::Data(c)) => {
                            check(declared, sent, c.len())?;
                            self.put(&c, chunked, false, false).await?;
                            sent += c.len() as u64;
                        }
                        Some(StreamPoll::Trailers(t)) => break Some(t),
                        Some(StreamPoll::Abort) => {
                            return Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "stream aborted by producer",
                            ));
                        }
                        None => break None,
                    }
                };
                check_complete(declared, sent)?;
                match trailers {
                    Some(t) if chunked => {
                        let mut tb = Vec::with_capacity(128);
                        encode::encode_trailers(&mut tb, t);
                        self.emit(&[tb.as_slice()], true).await
                    }
                    _ => self.end_stream(chunked).await,
                }
            }
        }
    }

    async fn send_buffered(
        &mut self,
        data: &[u8],
        chunked: bool,
        declared: Option<u64>,
    ) -> io::Result<()> {
        check(declared, 0, data.len())?;
        check_complete(declared, data.len() as u64)?;
        self.put(data, chunked, true, true).await
    }

    async fn send_reader<R: AsyncRead + Unpin>(
        &mut self,
        mut reader: R,
        buf: &mut [u8],
        chunked: bool,
        declared: Option<u64>,
    ) -> io::Result<()> {
        let mut sent = 0u64;
        loop {
            let n = reader.read(buf).await?;
            if n == 0 {
                break;
            }
            check(declared, sent, n)?;
            self.put(&buf[..n], chunked, false, false).await?;
            sent += n as u64;
        }
        check_complete(declared, sent)?;
        self.end_stream(chunked).await
    }

    async fn end_stream(&mut self, chunked: bool) -> io::Result<()> {
        if chunked {
            self.emit(&[LAST_CHUNK], true).await
        } else {
            Ok(())
        }
    }

    pub(crate) async fn send_continue(&mut self) -> io::Result<()> {
        self.emit(&[CONTINUE], true).await
    }

    pub(crate) async fn shutdown(&mut self) -> io::Result<()> {
        self.flush().await?;
        self.recycle_reader_if_empty();
        timed(self.transport.shutdown()).await
    }

    pub(crate) async fn read_more(&mut self) -> io::Result<usize> {
        self.flush().await?;
        if self.reader.capacity() == 0 {
            self.reader = WorkerScratch::take_read_buf();
        } else if self.reader.capacity() - self.reader.len() < 4096 {
            self.reader.reserve(8192);
        }
        self.transport.read_buf(&mut self.reader).await
    }

    pub(crate) async fn read_request(
        &mut self,
        lim: &Limits,
        mut idle: Pin<&mut tokio::time::Sleep>,
        tick: &mut u64,
    ) -> Result<Option<HttpRequest>, HttpRequestError> {
        let reused = self.has_written_response;
        self.has_written_response = false;
        let (mut scanned, mut deadline) = (0usize, None);

        loop {
            match find_head_end(&self.reader, scanned) {
                Some(end) if end <= MAX_HEAD => return self.take_request(end, lim).map(Some),
                Some(_) => return Err(HttpRequestError::HeadersTooLarge),
                None if self.reader.len() > MAX_HEAD => {
                    return Err(HttpRequestError::HeadersTooLarge);
                }
                None => {}
            }
            scanned = self.reader.len().saturating_sub(3);

            if self.reader.is_empty() {
                if !self
                    .wait_idle(lim.keep_alive, idle.as_mut(), tick, reused)
                    .await
                    .map_err(HttpRequestError::Io)?
                {
                    return Ok(None);
                }
            } else {
                self.flush().await.map_err(HttpRequestError::Io)?;
                let d = *deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + lim.request_timeout);
                match tokio::time::timeout_at(d, self.read_more()).await {
                    Err(_) => return Err(HttpRequestError::Timeout),
                    Ok(Err(e)) => return Err(HttpRequestError::Io(e)),
                    Ok(Ok(0)) => {
                        return Err(HttpRequestError::HeaderParsingFailed(
                            "EOF before headers finished".into(),
                        ));
                    }
                    Ok(Ok(_)) => {}
                }
            }
        }
    }

    fn take_request(&mut self, end: usize, lim: &Limits) -> Result<HttpRequest, HttpRequestError> {
        let (fields, route, params) = WorkerScratch::take_req_buffers();
        let req = HttpRequest::parse(
            &self.reader[..end],
            self.transport.peer_addr(),
            lim.max_body_size,
            lim.request_timeout,
            &self.state.trusted_proxies,
            fields,
            route,
            params,
        )?;
        if self.reader.len() == end {
            self.reader.clear();
        } else {
            self.reader.advance(end);
        }
        Ok(req)
    }

    async fn wait_idle(
        &mut self,
        ka: Duration,
        mut sleep: Pin<&mut tokio::time::Sleep>,
        tick: &mut u64,
        reused: bool,
    ) -> io::Result<bool> {
        self.flush().await?;
        self.out.shrink_to(OUT_IDLE_CAP);
        self.recycle_reader_if_empty();

        let now = coarse_elapsed_secs();
        if ka < Duration::from_secs(2) || now != *tick || sleep.is_elapsed() {
            *tick = now;
            sleep.as_mut().reset(tokio::time::Instant::now() + ka);
        }

        let res = tokio::select! {
            biased;
            r = self.first_byte() => r,
            _ = sleep.as_mut() => Ok(0),
            _ = crate::server::draining(), if reused => Ok(0),
        };
        if !matches!(res, Ok(n) if n > 0) {
            self.recycle_reader_if_empty();
        }
        res.map(|n| n > 0)
    }

    async fn first_byte(&mut self) -> io::Result<usize> {
        let Some(tcp) = self.transport.as_tcp() else {
            return self.read_more().await;
        };
        loop {
            tcp.readable().await?;
            let mut buf = WorkerScratch::take_read_buf();
            let res = tcp.try_read_buf(&mut buf);
            if buf.is_empty() {
                WorkerScratch::recycle_read_buf(buf);
            } else {
                self.reader = buf;
            }
            match res {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                other => return other,
            }
        }
    }

    async fn write_all_vectored<W: AsyncWrite + Unpin>(
        w: &mut W,
        mut bufs: &mut [IoSlice<'_>],
    ) -> io::Result<()> {
        IoSlice::advance_slices(&mut bufs, 0);
        while !bufs.is_empty() {
            if let [only] = &*bufs {
                return w.write_all(only).await;
            }
            let n = w.write_vectored(bufs).await?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            IoSlice::advance_slices(&mut bufs, n);
        }
        Ok(())
    }
}
