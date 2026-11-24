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

use crate::HttpStatus;
use crate::protocol::stream_channel;
use crate::server::HttpCall;
use bytes::{Bytes, BytesMut};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const SMALL_FILE_THRESHOLD: u64 = 64 * 1024;

pub struct StaticFileOptions {
    pub dir: &'static str,
    pub index: Option<&'static str>,
}

pub async fn serve_static(call: &mut HttpCall, opts: StaticFileOptions) {
    let tail = call
        .path_param("__path__")
        .unwrap_or("")
        .trim_start_matches('/');

    let base = Path::new(opts.dir);

    if tail.is_empty() {
        serve_index(call, base, opts.index).await;
        return;
    }

    let requested = base.join(tail);

    let Ok(canonical_base) = tokio::fs::canonicalize(base).await else {
        call.response()
            .status_code(HttpStatus::InternalServerError)
            .empty()
            .send()
            .await;
        return;
    };

    let Ok(canonical_req) = tokio::fs::canonicalize(&requested).await else {
        serve_index(call, base, opts.index).await;
        return;
    };

    if !canonical_req.starts_with(&canonical_base) {
        call.response()
            .status_code(HttpStatus::Forbidden)
            .empty()
            .send()
            .await;
        return;
    }

    match tokio::fs::metadata(&canonical_req).await {
        Ok(meta) if meta.is_dir() => {
            serve_index(call, base, opts.index).await;
        }
        Ok(meta) if meta.is_file() => {
            serve_file(call, &canonical_req).await;
        }
        _ => {
            serve_index(call, base, opts.index).await;
        }
    }
}

async fn serve_index(call: &mut HttpCall, base: &Path, index: Option<&'static str>) {
    let Some(idx) = index else {
        call.response()
            .status_code(HttpStatus::Forbidden)
            .empty()
            .send()
            .await;
        return;
    };

    let path = base.join(idx);
    match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.is_file() => serve_file(call, &path).await,
        _ => {
            call.response()
                .status_code(HttpStatus::NotFound)
                .empty()
                .send()
                .await;
        }
    }
}

#[inline]
fn parse_byte_range(header: &str, file_len: u64) -> Option<(u64, u64)> {
    if file_len == 0 {
        return None;
    }
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start_s, end_s) = spec.split_once('-')?;
    let start_s = start_s.trim();
    let end_s = end_s.trim();

    let (start, end) = if start_s.is_empty() {
        let suffix: u64 = end_s.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        (file_len.saturating_sub(suffix), file_len - 1)
    } else {
        let start: u64 = start_s.parse().ok()?;
        let end: u64 = if end_s.is_empty() {
            file_len - 1
        } else {
            end_s.parse::<u64>().ok()?.min(file_len - 1)
        };
        (start, end)
    };

    if start <= end && start < file_len {
        Some((start, end))
    } else {
        None
    }
}

async fn serve_file(call: &mut HttpCall, path: &Path) {
    let mime = mime_from_path(path);

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => {
            call.response()
                .status_code(HttpStatus::NotFound)
                .empty()
                .send()
                .await;
            return;
        }
    };

    let Ok(metadata) = file.metadata().await else {
        call.response()
            .status_code(HttpStatus::InternalServerError)
            .empty()
            .send()
            .await;
        return;
    };

    let file_len = metadata.len();

    let range_span = call
        .header("range")
        .and_then(|hdr| parse_byte_range(hdr, file_len));

    let content_length = if let Some((start, end)) = range_span {
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            call.response()
                .status_code(HttpStatus::InternalServerError)
                .empty()
                .send()
                .await;
            return;
        }
        end - start + 1
    } else {
        file_len
    };

    if content_length <= SMALL_FILE_THRESHOLD {
        let mut buf = Vec::with_capacity(content_length as usize);
        let mut limited = file.take(content_length);
        if limited.read_to_end(&mut buf).await.is_err() {
            call.response()
                .status_code(HttpStatus::InternalServerError)
                .empty()
                .send()
                .await;
            return;
        }

        let mut resp = call.response();
        if let Some((start, end)) = range_span {
            resp = resp
                .status_code(HttpStatus::PartialContent)
                .__add_header_internal("content-range", format!("bytes {start}-{end}/{file_len}"));
        }
        resp.__add_header_internal("accept-ranges", "bytes")
            .bytes(Bytes::from(buf), mime)
            .send()
            .await;
        return;
    }

    let (tx, rx) = stream_channel(8);
    tokio::spawn(async move {
        let mut limited = file.take(content_length);
        let mut buf = BytesMut::with_capacity(64 * 1024);

        loop {
            if buf.capacity() < 64 * 1024 {
                buf.reserve(64 * 1024);
            }
            let read_res = tokio::select! {
                biased;
                _ = tx.closed() => return,
                res = limited.read_buf(&mut buf) => res,
            };
            match read_res {
                Ok(0) => {
                    if limited.limit() > 0 {
                        tx.abort().await;
                    }
                    break;
                }
                Ok(_) => {
                    let chunk = buf.split().freeze();
                    if tx.send(chunk).await.is_err() {
                        return;
                    }
                }
                Err(_) => {
                    tx.abort().await;
                    return;
                }
            }
        }
    });

    let mut resp = call.response();
    if let Some((start, end)) = range_span {
        resp = resp
            .status_code(HttpStatus::PartialContent)
            .__add_header_internal("content-range", format!("bytes {start}-{end}/{file_len}"));
    }
    resp.__add_header_internal("accept-ranges", "bytes")
        .stream_from_len(rx, content_length, mime)
        .send()
        .await;
}

fn mime_from_path(path: &Path) -> &'static str {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return "application/octet-stream";
    };
    if ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm") {
        "text/html; charset=utf-8"
    } else if ext.eq_ignore_ascii_case("css") {
        "text/css; charset=utf-8"
    } else if ext.eq_ignore_ascii_case("js") || ext.eq_ignore_ascii_case("mjs") {
        "application/javascript"
    } else if ext.eq_ignore_ascii_case("json") {
        "application/json"
    } else if ext.eq_ignore_ascii_case("png") {
        "image/png"
    } else if ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg") {
        "image/jpeg"
    } else if ext.eq_ignore_ascii_case("gif") {
        "image/gif"
    } else if ext.eq_ignore_ascii_case("webp") {
        "image/webp"
    } else if ext.eq_ignore_ascii_case("svg") {
        "image/svg+xml"
    } else if ext.eq_ignore_ascii_case("ico") {
        "image/x-icon"
    } else if ext.eq_ignore_ascii_case("woff") {
        "font/woff"
    } else if ext.eq_ignore_ascii_case("woff2") {
        "font/woff2"
    } else if ext.eq_ignore_ascii_case("ttf") {
        "font/ttf"
    } else if ext.eq_ignore_ascii_case("pdf") {
        "application/pdf"
    } else if ext.eq_ignore_ascii_case("txt") {
        "text/plain; charset=utf-8"
    } else if ext.eq_ignore_ascii_case("xml") {
        "application/xml"
    } else if ext.eq_ignore_ascii_case("wasm") {
        "application/wasm"
    } else {
        "application/octet-stream"
    }
}
