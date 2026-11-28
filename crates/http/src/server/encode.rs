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
use crate::protocol::ResponseParts;
use crate::server::connection::{DATE_HEADER_LEN, format_u64};

const FORBIDDEN_TRAILERS: [&str; 7] = [
    "connection",
    "content-length",
    "content-type",
    "host",
    "keep-alive",
    "set-cookie",
    "transfer-encoding",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    None,
    Length,
    Chunked,
    Close,
}

pub(crate) fn framing(r: &ResponseParts) -> Framing {
    if matches!(
        r.status_code,
        HttpStatus::NoContent | HttpStatus::NotModified
    ) {
        Framing::None
    } else if r.is_chunked {
        Framing::Chunked
    } else if r.content_length.is_some() || r.body.is_none() {
        Framing::Length
    } else {
        Framing::Close
    }
}

pub(crate) struct HeadCtx<'a> {
    pub keep_alive: bool,
    pub ka_header: &'a [u8],
    pub date: &'a [u8; DATE_HEADER_LEN],
}

#[inline]
fn push_field(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    out.extend_from_slice(name);
    out.extend_from_slice(b": ");
    out.extend_from_slice(value);
    out.extend_from_slice(b"\r\n");
}

pub(crate) fn encode_head(out: &mut Vec<u8>, r: &ResponseParts, f: Framing, ctx: &HeadCtx<'_>) {
    out.extend_from_slice(&r.http_version);
    out.push(b' ');
    out.extend_from_slice(r.status_code.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(ctx.date);

    let mut id = [0u8; 33];
    push_field(
        out,
        b"x-request-id",
        r.request_id.encode(&mut id).as_bytes(),
    );

    out.extend_from_slice(if ctx.keep_alive {
        ctx.ka_header
    } else {
        b"connection: close\r\n"
    });

    if let Some(ct) = &r.content_type {
        push_field(out, b"content-type", ct.as_bytes());
    }

    let keep_user_length = match f {
        Framing::Chunked => {
            out.extend_from_slice(b"transfer-encoding: chunked\r\n");
            false
        }
        Framing::Length => match r.content_length {
            Some(len) => {
                let mut n = [0u8; 20];
                push_field(out, b"content-length", format_u64(len, &mut n));
                false
            }
            None if r.suppress_body && r.field_lines.get("content-length").is_some() => true,
            None => {
                out.extend_from_slice(b"content-length: 0\r\n");
                false
            }
        },
        Framing::None => matches!(r.status_code, HttpStatus::NotModified),
        Framing::Close => false,
    };

    for (name, value) in r.field_lines.iter() {
        let skip = match name {
            "date" | "connection" | "keep-alive" | "x-request-id" | "transfer-encoding" => true,
            "content-length" => !keep_user_length,
            "content-type" => r.content_type.is_some(),
            _ => false,
        };
        if !skip {
            push_field(out, name.as_bytes(), value.as_bytes());
        }
    }

    if let Some(store) = &r.cookies {
        out.extend_from_slice(&store.to_bytes());
    }
    out.extend_from_slice(b"\r\n");
}

pub(crate) fn encode_trailers<K: AsRef<str>, V: AsRef<str>>(
    out: &mut Vec<u8>,
    trailers: impl IntoIterator<Item = (K, V)>,
) {
    out.extend_from_slice(b"0\r\n");
    for (name, value) in trailers {
        let (name, value) = (name.as_ref(), value.as_ref());
        if is_valid_trailer_name(name) && is_valid_trailer_value(value) {
            push_field(out, name.as_bytes(), value.as_bytes());
        }
    }
    out.extend_from_slice(b"\r\n");
}

fn is_valid_trailer_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !FORBIDDEN_TRAILERS
            .iter()
            .any(|f| name.eq_ignore_ascii_case(f))
}

fn is_valid_trailer_value(value: &str) -> bool {
    !value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7F)
}
