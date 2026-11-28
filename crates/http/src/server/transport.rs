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

use std::io::IoSlice;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

trait DynStream: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static {}
impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> DynStream for S {}

enum StreamInner {
    Tcp(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
    Custom(Box<dyn DynStream>),
}

pub struct Transport {
    stream: StreamInner,
    peer_addr: SocketAddr,
}

impl Transport {
    pub fn from_tcp(stream: TcpStream, peer_addr: SocketAddr) -> Self {
        Self {
            stream: StreamInner::Tcp(stream),
            peer_addr,
        }
    }

    pub fn from_tls(stream: TlsStream<TcpStream>, peer_addr: SocketAddr) -> Self {
        Self {
            stream: StreamInner::Tls(Box::new(stream)),
            peer_addr,
        }
    }

    pub fn from_custom<S>(stream: S, peer_addr: SocketAddr) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    {
        Self {
            stream: StreamInner::Custom(Box::new(stream)),
            peer_addr,
        }
    }

    #[inline]
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    #[inline]
    pub fn as_tcp(&self) -> Option<&TcpStream> {
        match &self.stream {
            StreamInner::Tcp(s) => Some(s),
            _ => None,
        }
    }
}

impl AsyncRead for Transport {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut self.stream {
            StreamInner::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            StreamInner::Tls(s) => Pin::new(&mut **s).poll_read(cx, buf),
            StreamInner::Custom(s) => Pin::new(&mut **s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut self.stream {
            StreamInner::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            StreamInner::Tls(s) => Pin::new(&mut **s).poll_write(cx, buf),
            StreamInner::Custom(s) => Pin::new(&mut **s).poll_write(cx, buf),
        }
    }

    #[inline]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        match &mut self.stream {
            StreamInner::Tcp(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            StreamInner::Tls(s) => Pin::new(&mut **s).poll_write_vectored(cx, bufs),
            StreamInner::Custom(s) => Pin::new(&mut **s).poll_write_vectored(cx, bufs),
        }
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        match &self.stream {
            StreamInner::Tcp(s) => s.is_write_vectored(),
            StreamInner::Tls(s) => s.is_write_vectored(),
            StreamInner::Custom(s) => s.is_write_vectored(),
        }
    }

    #[inline]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.stream {
            StreamInner::Tcp(s) => Pin::new(s).poll_flush(cx),
            StreamInner::Tls(s) => Pin::new(&mut **s).poll_flush(cx),
            StreamInner::Custom(s) => Pin::new(&mut **s).poll_flush(cx),
        }
    }

    #[inline]
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.stream {
            StreamInner::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            StreamInner::Tls(s) => Pin::new(&mut **s).poll_shutdown(cx),
            StreamInner::Custom(s) => Pin::new(&mut **s).poll_shutdown(cx),
        }
    }
}

impl Unpin for Transport {}
