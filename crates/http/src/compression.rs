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
use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder, ZstdEncoder};
use bytes::Bytes;
use tokio::io::AsyncReadExt;

pub(crate) struct Compress {}

#[inline]
fn contains_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

impl Compress {
    #[inline]
    pub(crate) fn is_compressible(content_type: &str) -> bool {
        let bytes = content_type.as_bytes();
        bytes
            .get(..5)
            .map_or(false, |p| p.eq_ignore_ascii_case(b"text/"))
            || contains_ascii_ci(bytes, b"json")
            || contains_ascii_ci(bytes, b"javascript")
            || contains_ascii_ci(bytes, b"xml")
            || contains_ascii_ci(bytes, b"svg")
    }

    pub(crate) async fn compress_bytes(data: Bytes, encoding: &'static str) -> Vec<u8> {
        if data.len() >= 16 * 1024 || encoding == "br" {
            return tokio::task::spawn_blocking(move || match encoding {
                "zstd" => Self::sync_zstd(&data),
                "br" => Self::sync_brotli(&data),
                _ => Self::sync_gzip(&data),
            })
            .await
            .unwrap_or_default();
        }

        match encoding {
            "zstd" => Self::with_zstd(&data).await,
            "br" => Self::with_brotli(&data).await,
            _ => Self::with_gzip(&data).await,
        }
    }

    fn sync_gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = GzipEncoder::with_quality(
            std::io::Cursor::new(data),
            async_compression::Level::Fastest,
        );
        let mut out = Vec::with_capacity(data.len() / 2);
        let _ = enc.read_to_end(&mut out);
        out
    }

    fn sync_brotli(data: &[u8]) -> Vec<u8> {
        let mut enc = BrotliEncoder::with_quality(
            std::io::Cursor::new(data),
            async_compression::Level::Precise(4),
        );
        let mut out = Vec::with_capacity(data.len() / 2);
        let _ = enc.read_to_end(&mut out);
        out
    }

    fn sync_zstd(data: &[u8]) -> Vec<u8> {
        let mut enc = ZstdEncoder::with_quality(
            std::io::Cursor::new(data),
            async_compression::Level::Fastest,
        );
        let mut out = Vec::with_capacity(data.len() / 2);
        let _ = enc.read_to_end(&mut out);
        out
    }

    pub(crate) async fn with_gzip(data: &[u8]) -> Vec<u8> {
        use async_compression::tokio::bufread::GzipEncoder;
        use tokio::io::AsyncReadExt;
        let mut enc = GzipEncoder::with_quality(
            std::io::Cursor::new(data),
            async_compression::Level::Fastest,
        );
        let mut out = Vec::with_capacity(data.len() / 2);
        let _ = enc.read_to_end(&mut out).await;
        out
    }

    pub(crate) async fn with_brotli(data: &[u8]) -> Vec<u8> {
        use async_compression::tokio::bufread::BrotliEncoder;
        use tokio::io::AsyncReadExt;
        let mut enc = BrotliEncoder::with_quality(
            std::io::Cursor::new(data),
            async_compression::Level::Precise(4),
        );
        let mut out = Vec::with_capacity(data.len() / 2);
        let _ = enc.read_to_end(&mut out).await;
        out
    }

    pub(crate) async fn with_zstd(data: &[u8]) -> Vec<u8> {
        use async_compression::tokio::bufread::ZstdEncoder;
        use tokio::io::AsyncReadExt;
        let mut enc = ZstdEncoder::with_quality(
            std::io::Cursor::new(data),
            async_compression::Level::Fastest,
        );
        let mut out = Vec::with_capacity(data.len() / 2);
        let _ = enc.read_to_end(&mut out).await;
        out
    }
}
