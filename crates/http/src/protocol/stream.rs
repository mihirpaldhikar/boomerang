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

use crate::protocol::body::ToJson;
use crate::protocol::write_json_str;
use bytes::Bytes;
use std::convert::Infallible;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamClosed;

impl fmt::Display for StreamClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stream closed by client")
    }
}

impl std::error::Error for StreamClosed {}

pub enum StreamErrorAction {
    /// Sends custom bytes as the final chunk and terminates cleanly (`0\r\n\r\n`).
    Send(Bytes),
    /// Sends custom bytes and keeps the stream open for remaining chunks (`mpsc::Receiver<Result<_, E>>`).
    EmitAndContinue(Bytes),
    /// Ends a chunked stream with HTTP/1.1 trailers (`0\r\nKey: Val\r\n\r\n`).
    Trailers(Vec<(String, String)>),
    /// Aborts the TCP stream without `0\r\n\r\n` so the client detects truncation.
    Abort,
}

/// Implement on any error type returned by `stream_with`, `sse`, or `mpsc::Receiver<Result<_, E>>`.
pub trait StreamError: Send + 'static {
    fn into_stream_error(self) -> StreamErrorAction;
}

impl StreamError for Infallible {
    #[inline]
    fn into_stream_error(self) -> StreamErrorAction {
        match self {}
    }
}

impl StreamError for StreamClosed {
    #[inline]
    fn into_stream_error(self) -> StreamErrorAction {
        StreamErrorAction::Abort
    }
}

impl StreamError for std::io::Error {
    #[inline]
    fn into_stream_error(self) -> StreamErrorAction {
        StreamErrorAction::Abort
    }
}

impl StreamError for String {
    #[inline]
    fn into_stream_error(self) -> StreamErrorAction {
        StreamErrorAction::Abort
    }
}

impl StreamError for &'static str {
    #[inline]
    fn into_stream_error(self) -> StreamErrorAction {
        StreamErrorAction::Abort
    }
}

pub struct StreamChunk(pub Bytes);

impl From<Bytes> for StreamChunk {
    #[inline]
    fn from(b: Bytes) -> Self {
        Self(b)
    }
}
impl From<Vec<u8>> for StreamChunk {
    #[inline]
    fn from(v: Vec<u8>) -> Self {
        Self(Bytes::from(v))
    }
}
impl From<String> for StreamChunk {
    #[inline]
    fn from(s: String) -> Self {
        Self(Bytes::from(s))
    }
}
impl From<&'static [u8]> for StreamChunk {
    #[inline]
    fn from(b: &'static [u8]) -> Self {
        Self(Bytes::from_static(b))
    }
}
impl From<&'static str> for StreamChunk {
    #[inline]
    fn from(s: &'static str) -> Self {
        Self(Bytes::from_static(s.as_bytes()))
    }
}

pub struct BinWriter<'a> {
    buf: &'a mut Vec<u8>,
}

impl<'a> BinWriter<'a> {
    #[inline]
    pub fn new(buf: &'a mut Vec<u8>) -> Self {
        Self { buf }
    }

    #[inline]
    pub fn put_u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    #[inline]
    pub fn put_u16_be(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    #[inline]
    pub fn put_u16_le(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    pub fn put_u32_be(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    #[inline]
    pub fn put_u32_le(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    pub fn put_u64_be(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    #[inline]
    pub fn put_u64_le(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    pub fn put_i32_be(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    #[inline]
    pub fn put_i64_be(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    #[inline]
    pub fn put_f32_be(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    #[inline]
    pub fn put_f64_be(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    #[inline]
    pub fn put_slice(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }
    #[inline]
    pub fn put_len_bytes(&mut self, bytes: &[u8]) {
        let len = u32::try_from(bytes.len()).expect("frame payload exceeds 4 GiB u32 limit");
        self.put_u32_be(len);
        self.buf.extend_from_slice(bytes);
    }
    #[inline]
    pub fn put_len_str(&mut self, s: &str) {
        self.put_len_bytes(s.as_bytes());
    }
    #[inline]
    pub fn as_mut_vec(&mut self) -> &mut Vec<u8> {
        self.buf
    }
}

pub trait BinaryEncode {
    fn encode(&self, out: &mut BinWriter<'_>);

    #[inline]
    fn encoded_size_hint(&self) -> usize {
        128
    }
}

impl BinaryEncode for [u8] {
    #[inline]
    fn encode(&self, out: &mut BinWriter<'_>) {
        out.put_slice(self);
    }
    #[inline]
    fn encoded_size_hint(&self) -> usize {
        self.len()
    }
}
impl BinaryEncode for Vec<u8> {
    #[inline]
    fn encode(&self, out: &mut BinWriter<'_>) {
        out.put_slice(self);
    }
    #[inline]
    fn encoded_size_hint(&self) -> usize {
        self.len()
    }
}
impl BinaryEncode for Bytes {
    #[inline]
    fn encode(&self, out: &mut BinWriter<'_>) {
        out.put_slice(self);
    }
    #[inline]
    fn encoded_size_hint(&self) -> usize {
        self.len()
    }
}
impl BinaryEncode for str {
    #[inline]
    fn encode(&self, out: &mut BinWriter<'_>) {
        out.put_slice(self.as_bytes());
    }
    #[inline]
    fn encoded_size_hint(&self) -> usize {
        self.len()
    }
}
impl<T: BinaryEncode + ?Sized> BinaryEncode for &T {
    #[inline]
    fn encode(&self, out: &mut BinWriter<'_>) {
        (*self).encode(out);
    }
    #[inline]
    fn encoded_size_hint(&self) -> usize {
        (*self).encoded_size_hint()
    }
}

/// Standalone encoder helpers for standard `tokio::sync::mpsc` producers.
#[inline]
pub fn encode_bin<T: BinaryEncode + ?Sized>(value: &T) -> Bytes {
    let mut buf = Vec::with_capacity(value.encoded_size_hint());
    value.encode(&mut BinWriter::new(&mut buf));
    Bytes::from(buf)
}

#[inline]
pub fn encode_frame<T: BinaryEncode + ?Sized>(value: &T) -> Bytes {
    let mut buf = Vec::with_capacity(4usize.saturating_add(value.encoded_size_hint()));
    buf.extend_from_slice(&[0u8; 4]);
    value.encode(&mut BinWriter::new(&mut buf));
    let len = u32::try_from(buf.len() - 4).expect("frame payload exceeds 4 GiB u32 limit");
    buf[0..4].copy_from_slice(&len.to_be_bytes());
    Bytes::from(buf)
}

#[inline]
pub fn encode_tagged_frame<T: BinaryEncode + ?Sized>(tag: u8, value: &T) -> Bytes {
    let mut buf = Vec::with_capacity(5usize.saturating_add(value.encoded_size_hint()));
    buf.push(tag);
    buf.extend_from_slice(&[0u8; 4]);
    value.encode(&mut BinWriter::new(&mut buf));
    let len = u32::try_from(buf.len() - 5).expect("frame payload exceeds 4 GiB u32 limit");
    buf[1..5].copy_from_slice(&len.to_be_bytes());
    Bytes::from(buf)
}

#[inline]
pub fn encode_tagged_error(tag: u8, code: u32, message: &str) -> Bytes {
    let payload_len = u32::try_from(8usize.saturating_add(message.len()))
        .expect("error message exceeds 4 GiB u32 limit");
    let mut buf = Vec::with_capacity(5 + payload_len as usize);
    let mut w = BinWriter::new(&mut buf);
    w.put_u8(tag);
    w.put_u32_be(payload_len);
    w.put_u32_be(code);
    w.put_len_str(message);
    Bytes::from(buf)
}

pub(crate) enum StreamItem {
    Data(Bytes),
    Trailers(Vec<(String, String)>),
    Abort,
}

#[derive(Clone)]
pub struct StreamSender {
    tx: mpsc::Sender<StreamItem>,
    aborted: Arc<AtomicBool>,
}

pub struct StreamReceiver {
    rx: mpsc::Receiver<StreamItem>,
    aborted: Arc<AtomicBool>,
}

#[inline]
pub fn stream_channel(capacity: usize) -> (StreamSender, StreamReceiver) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    let aborted = Arc::new(AtomicBool::new(false));
    (
        StreamSender {
            tx,
            aborted: aborted.clone(),
        },
        StreamReceiver { rx, aborted },
    )
}

impl StreamSender {
    #[inline]
    pub async fn send<B: Into<StreamChunk>>(&self, chunk: B) -> Result<(), StreamClosed> {
        let bytes = chunk.into().0;
        if bytes.is_empty() {
            return if self.tx.is_closed() {
                Err(StreamClosed)
            } else {
                Ok(())
            };
        }
        self.tx
            .send(StreamItem::Data(bytes))
            .await
            .map_err(|_| StreamClosed)
    }

    #[inline]
    pub fn try_send<B: Into<StreamChunk>>(
        &self,
        chunk: B,
    ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        let bytes = chunk.into().0;
        if bytes.is_empty() {
            return if self.tx.is_closed() {
                Err(mpsc::error::TrySendError::Closed(bytes))
            } else {
                Ok(())
            };
        }
        self.tx
            .try_send(StreamItem::Data(bytes))
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(StreamItem::Data(b)) => {
                    mpsc::error::TrySendError::Full(b)
                }
                mpsc::error::TrySendError::Closed(StreamItem::Data(b)) => {
                    mpsc::error::TrySendError::Closed(b)
                }
                _ => unreachable!(),
            })
    }

    #[inline]
    pub fn blocking_send<B: Into<StreamChunk>>(&self, chunk: B) -> Result<(), StreamClosed> {
        let bytes = chunk.into().0;
        if bytes.is_empty() {
            return if self.tx.is_closed() {
                Err(StreamClosed)
            } else {
                Ok(())
            };
        }
        self.tx
            .blocking_send(StreamItem::Data(bytes))
            .map_err(|_| StreamClosed)
    }

    #[inline]
    pub async fn send_bin<T: BinaryEncode + ?Sized>(&self, value: &T) -> Result<(), StreamClosed> {
        self.send(encode_bin(value)).await
    }

    #[inline]
    pub async fn send_bin_with<F>(&self, capacity_hint: usize, f: F) -> Result<(), StreamClosed>
    where
        F: FnOnce(&mut BinWriter<'_>),
    {
        let mut buf = Vec::with_capacity(capacity_hint);
        f(&mut BinWriter::new(&mut buf));
        self.send(buf).await
    }

    #[inline]
    pub async fn send_frame<T: BinaryEncode + ?Sized>(
        &self,
        value: &T,
    ) -> Result<(), StreamClosed> {
        self.send(encode_frame(value)).await
    }

    #[inline]
    pub async fn send_tagged_frame<T: BinaryEncode + ?Sized>(
        &self,
        tag: u8,
        value: &T,
    ) -> Result<(), StreamClosed> {
        self.send(encode_tagged_frame(tag, value)).await
    }

    #[inline]
    pub fn blocking_send_tagged_frame<T: BinaryEncode + ?Sized>(
        &self,
        tag: u8,
        value: &T,
    ) -> Result<(), StreamClosed> {
        self.blocking_send(encode_tagged_frame(tag, value))
    }

    #[inline]
    pub async fn send_tagged_error(
        &self,
        tag: u8,
        code: u32,
        message: &str,
    ) -> Result<(), StreamClosed> {
        self.send(encode_tagged_error(tag, code, message)).await
    }

    pub async fn send_json<T: ToJson>(&self, value: &T) -> Result<(), StreamClosed> {
        let mut buf = Vec::with_capacity(value.size_hint() + 1);
        value.write_json(&mut buf);
        buf.push(b'\n');
        self.send(buf).await
    }

    pub async fn send_json_error(&self, code: &str, message: &str) -> Result<(), StreamClosed> {
        let mut buf = Vec::with_capacity(32 + code.len() + message.len());
        buf.extend_from_slice(br#"{"error":{"code":"#);
        write_json_str(code, &mut buf);
        buf.extend_from_slice(br#","message":"#);
        write_json_str(message, &mut buf);
        buf.extend_from_slice(b"}}\n");
        self.send(buf).await
    }

    pub async fn send_sse(&self, event: Option<&str>, data: &str) -> Result<(), StreamClosed> {
        let mut buf = Vec::with_capacity(data.len() + 32);
        if let Some(ev) = event {
            if !ev.is_empty() && !ev.bytes().any(|b| b < 0x20 || b == 0x7F) {
                buf.extend_from_slice(b"event: ");
                buf.extend_from_slice(ev.as_bytes());
                buf.push(b'\n');
            }
        }

        let bytes = data.as_bytes();
        let mut i = 0usize;
        while i <= bytes.len() {
            let mut end = i;
            while end < bytes.len() && bytes[end] != b'\n' && bytes[end] != b'\r' {
                end += 1;
            }
            buf.extend_from_slice(b"data: ");
            buf.extend_from_slice(&bytes[i..end]);
            buf.push(b'\n');
            if end >= bytes.len() {
                break;
            }
            if bytes[end] == b'\r' && bytes.get(end + 1) == Some(&b'\n') {
                i = end + 2;
            } else {
                i = end + 1;
            }
        }
        buf.push(b'\n');
        self.send(buf).await
    }

    pub async fn send_sse_error(&self, code: &str, message: &str) -> Result<(), StreamClosed> {
        let mut data = Vec::with_capacity(28 + code.len() + message.len());
        data.extend_from_slice(br#"{"code":"#);
        write_json_str(code, &mut data);
        data.extend_from_slice(br#","message":"#);
        write_json_str(message, &mut data);
        data.push(b'}');
        let s = unsafe { std::str::from_utf8_unchecked(&data) };
        self.send_sse(Some("error"), s).await
    }

    pub async fn finish_with_trailers(self, trailers: Vec<(String, String)>) {
        let _ = self.tx.send(StreamItem::Trailers(trailers)).await;
    }

    pub async fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        let _ = self.tx.try_send(StreamItem::Abort);
    }

    pub(crate) async fn apply_error<E: StreamError>(self, err: E) {
        match err.into_stream_error() {
            StreamErrorAction::Send(b) | StreamErrorAction::EmitAndContinue(b) => {
                let _ = self.send(b).await;
            }
            StreamErrorAction::Trailers(t) => {
                self.finish_with_trailers(t).await;
            }
            StreamErrorAction::Abort => {
                self.abort().await;
            }
        }
    }

    #[inline]
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    #[inline]
    pub async fn closed(&self) {
        self.tx.closed().await
    }
}

pub enum StreamPoll {
    Data(Bytes),
    Trailers(Vec<(String, String)>),
    Abort,
}

pub trait ChunkSource: Send {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<StreamPoll>>;
}

pub trait IntoBodyStream {
    type Source: ChunkSource + 'static;
    fn into_body_stream(self) -> Self::Source;
}

impl ChunkSource for StreamReceiver {
    #[inline]
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<StreamPoll>> {
        if self.aborted.load(Ordering::Acquire) {
            return Poll::Ready(Some(StreamPoll::Abort));
        }

        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(StreamItem::Data(b))) => Poll::Ready(Some(StreamPoll::Data(b))),
            Poll::Ready(Some(StreamItem::Trailers(t))) => {
                Poll::Ready(Some(StreamPoll::Trailers(t)))
            }
            Poll::Ready(Some(StreamItem::Abort)) => Poll::Ready(Some(StreamPoll::Abort)),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl IntoBodyStream for StreamReceiver {
    type Source = Self;
    fn into_body_stream(self) -> Self::Source {
        self
    }
}

pub struct ReceiverSource<R> {
    rx: R,
    finished: bool,
}

impl<R> ReceiverSource<R> {
    #[inline]
    fn new(rx: R) -> Self {
        Self {
            rx,
            finished: false,
        }
    }
}

macro_rules! impl_mpsc_source {
    ($item:ty, $map:expr) => {
        impl ChunkSource for ReceiverSource<mpsc::Receiver<$item>> {
            #[inline]
            fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<StreamPoll>> {
                self.rx.poll_recv(cx).map(|opt| opt.map($map))
            }
        }
        impl IntoBodyStream for mpsc::Receiver<$item> {
            type Source = ReceiverSource<Self>;
            fn into_body_stream(self) -> Self::Source {
                ReceiverSource::new(self)
            }
        }
        impl ChunkSource for ReceiverSource<mpsc::UnboundedReceiver<$item>> {
            #[inline]
            fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<StreamPoll>> {
                self.rx.poll_recv(cx).map(|opt| opt.map($map))
            }
        }
        impl IntoBodyStream for mpsc::UnboundedReceiver<$item> {
            type Source = ReceiverSource<Self>;
            fn into_body_stream(self) -> Self::Source {
                ReceiverSource::new(self)
            }
        }
    };
}

impl_mpsc_source!(Bytes, |b| StreamPoll::Data(b));
impl_mpsc_source!(Vec<u8>, |v| StreamPoll::Data(Bytes::from(v)));
impl_mpsc_source!(String, |s| StreamPoll::Data(Bytes::from(s)));
impl_mpsc_source!(&'static [u8], |b| StreamPoll::Data(Bytes::from_static(b)));
impl_mpsc_source!(&'static str, |s| StreamPoll::Data(Bytes::from_static(
    s.as_bytes()
)));

macro_rules! impl_mpsc_result_source {
    ($item:ty, $to_bytes:expr) => {
        impl<E: StreamError> ChunkSource for ReceiverSource<mpsc::Receiver<Result<$item, E>>> {
            #[inline]
            fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<StreamPoll>> {
                if self.finished {
                    return Poll::Ready(None);
                }
                match self.rx.poll_recv(cx) {
                    Poll::Ready(Some(Ok(val))) => {
                        Poll::Ready(Some(StreamPoll::Data(($to_bytes)(val))))
                    }
                    Poll::Ready(Some(Err(err))) => match err.into_stream_error() {
                        StreamErrorAction::EmitAndContinue(b) => {
                            Poll::Ready(Some(StreamPoll::Data(b)))
                        }
                        StreamErrorAction::Send(b) => {
                            self.finished = true;
                            Poll::Ready(Some(StreamPoll::Data(b)))
                        }
                        StreamErrorAction::Trailers(t) => {
                            self.finished = true;
                            Poll::Ready(Some(StreamPoll::Trailers(t)))
                        }
                        StreamErrorAction::Abort => {
                            self.finished = true;
                            Poll::Ready(Some(StreamPoll::Abort))
                        }
                    },
                    Poll::Ready(None) => Poll::Ready(None),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
        impl<E: StreamError> IntoBodyStream for mpsc::Receiver<Result<$item, E>> {
            type Source = ReceiverSource<Self>;
            fn into_body_stream(self) -> Self::Source {
                ReceiverSource::new(self)
            }
        }

        impl<E: StreamError> ChunkSource
            for ReceiverSource<mpsc::UnboundedReceiver<Result<$item, E>>>
        {
            #[inline]
            fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<StreamPoll>> {
                if self.finished {
                    return Poll::Ready(None);
                }
                match self.rx.poll_recv(cx) {
                    Poll::Ready(Some(Ok(val))) => {
                        Poll::Ready(Some(StreamPoll::Data(($to_bytes)(val))))
                    }
                    Poll::Ready(Some(Err(err))) => match err.into_stream_error() {
                        StreamErrorAction::EmitAndContinue(b) => {
                            Poll::Ready(Some(StreamPoll::Data(b)))
                        }
                        StreamErrorAction::Send(b) => {
                            self.finished = true;
                            Poll::Ready(Some(StreamPoll::Data(b)))
                        }
                        StreamErrorAction::Trailers(t) => {
                            self.finished = true;
                            Poll::Ready(Some(StreamPoll::Trailers(t)))
                        }
                        StreamErrorAction::Abort => {
                            self.finished = true;
                            Poll::Ready(Some(StreamPoll::Abort))
                        }
                    },
                    Poll::Ready(None) => Poll::Ready(None),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
        impl<E: StreamError> IntoBodyStream for mpsc::UnboundedReceiver<Result<$item, E>> {
            type Source = ReceiverSource<Self>;
            fn into_body_stream(self) -> Self::Source {
                ReceiverSource::new(self)
            }
        }
    };
}

impl_mpsc_result_source!(Bytes, |b: Bytes| b);
impl_mpsc_result_source!(Vec<u8>, |v: Vec<u8>| Bytes::from(v));
impl_mpsc_result_source!(String, |s: String| Bytes::from(s));

pub(crate) struct ManagedStreamSource<S> {
    pub(crate) inner: S,
    pub(crate) handle: Option<tokio::task::JoinHandle<()>>,
}

impl<S: ChunkSource> ChunkSource for ManagedStreamSource<S> {
    #[inline]
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<StreamPoll>> {
        self.inner.poll_next(cx)
    }
}

impl<S> Drop for ManagedStreamSource<S> {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

pub(crate) struct ChunkSourceReader {
    source: Box<dyn ChunkSource>,
    current: Bytes,
}

impl ChunkSourceReader {
    pub(crate) fn new(source: Box<dyn ChunkSource>) -> Self {
        Self {
            source,
            current: Bytes::new(),
        }
    }
}

impl AsyncRead for ChunkSourceReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        while self.current.is_empty() {
            match self.source.poll_next(cx) {
                Poll::Ready(Some(StreamPoll::Data(chunk))) => {
                    if !chunk.is_empty() {
                        self.current = chunk;
                    }
                }
                Poll::Ready(Some(StreamPoll::Trailers(_))) | Poll::Ready(None) => {
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(StreamPoll::Abort)) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "stream aborted by producer",
                    )));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        let take = self.current.len().min(buf.remaining());
        buf.put_slice(&self.current[..take]);
        let _ = self.current.split_to(take);
        Poll::Ready(Ok(()))
    }
}
