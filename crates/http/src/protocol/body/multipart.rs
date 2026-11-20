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
use crate::protocol::body::OCTET_STREAM;
use bytes::Bytes;

#[derive(thiserror::Error, Debug)]
pub enum MultipartError {
    #[error("boundary not found")]
    MissingBoundary,
    #[error("{0}")]
    Malformed(&'static str),
    #[error("{0}")]
    Utf8(#[from] std::str::Utf8Error),
}

#[derive(Debug)]
pub struct MultiPartField {
    name: Bytes,
    content_type: Option<Bytes>,
    content: Bytes,
}

impl MultiPartField {
    #[inline]
    pub fn name(&self) -> &str {
        // SAFETY: validated as UTF-8 in `Multipart::parse_header_line`.
        unsafe { std::str::from_utf8_unchecked(&self.name) }
    }

    #[inline]
    pub fn content_type(&self) -> Option<&str> {
        // SAFETY: validated as UTF-8 (or static ASCII) in `Multipart::parse_headers_memchr`.
        self.content_type
            .as_ref()
            .map(|b| unsafe { std::str::from_utf8_unchecked(b) })
    }

    #[inline]
    pub fn content(&self) -> &Bytes {
        &self.content
    }
}

pub struct Multipart {
    fields: Vec<MultiPartField>,
}

impl Multipart {
    pub(crate) fn parse(body: &Bytes, boundary: &str) -> Result<Self, MultipartError> {
        if boundary.is_empty() {
            return Err(MultipartError::MissingBoundary);
        }

        let buf = body.as_ref();
        let b_bytes = boundary.as_bytes();
        let marker_len = 2 + 2 + b_bytes.len() + 2;

        let mut stack_marker = [0u8; 76];
        let mut heap_marker = Vec::new();
        let marker: &mut [u8] = if marker_len <= stack_marker.len() {
            &mut stack_marker[..marker_len]
        } else {
            heap_marker.resize(marker_len, 0);
            &mut heap_marker
        };

        marker[0..4].copy_from_slice(b"\r\n--");
        marker[4..4 + b_bytes.len()].copy_from_slice(b_bytes);
        marker[4 + b_bytes.len()..].copy_from_slice(b"\r\n");

        let open = &marker[2..];
        let mid = &marker[..marker_len - 2];

        let first = memchr::memmem::find(buf, open).ok_or(MultipartError::MissingBoundary)?;
        let mut pos = first + open.len();

        let mid_finder = memchr::memmem::Finder::new(mid);
        let hdr_end_finder = memchr::memmem::Finder::new(b"\r\n\r\n");
        let base_ptr = buf.as_ptr() as usize;

        let mut fields = Vec::with_capacity(4);

        loop {
            let hdr_rel = hdr_end_finder
                .find(&buf[pos..])
                .ok_or(MultipartError::Malformed("headers missing CRLFCRLF"))?;
            let hdr_end = pos + hdr_rel;
            let headers = &buf[pos..hdr_end];
            let (name, content_type) = Self::parse_headers_memchr(body, base_ptr, headers)?;

            let data_start = hdr_end + 4;
            let rel = mid_finder
                .find(&buf[data_start..])
                .ok_or(MultipartError::Malformed("next boundary not found"))?;
            let delim_at = data_start + rel;

            let data = body.slice(data_start..delim_at);
            fields.push(MultiPartField {
                name,
                content_type,
                content: data,
            });

            let after = delim_at + mid.len();
            if buf.get(after..after + 2) == Some(b"--") {
                break;
            }
            if buf.get(after..after + 2) != Some(b"\r\n") {
                return Err(MultipartError::Malformed("boundary line missing CRLF"));
            }
            pos = after + 2;
            if pos >= buf.len() {
                break;
            }
        }

        Ok(Self { fields })
    }

    #[inline]
    pub fn get(&self, name: &str) -> Option<&MultiPartField> {
        self.fields.iter().rfind(|f| f.name() == name)
    }

    #[inline]
    pub fn fields(&self) -> &[MultiPartField] {
        &self.fields
    }

    #[inline]
    fn slice_str_as_bytes(body: &Bytes, base_ptr: usize, s: &str) -> Bytes {
        let start = s.as_ptr() as usize - base_ptr;
        body.slice(start..start + s.len())
    }

    #[inline]
    fn parse_headers_memchr(
        body: &Bytes,
        base_ptr: usize,
        hdr: &[u8],
    ) -> Result<(Bytes, Option<Bytes>), MultipartError> {
        let mut name: Option<Bytes> = None;
        let mut has_filename = false;
        let mut ctype: Option<Bytes> = None;

        let mut start = 0;
        while start <= hdr.len() {
            let next = match memchr::memchr(b'\n', &hdr[start..]) {
                Some(off) => start + off,
                None => {
                    if start < hdr.len() {
                        Self::parse_header_line(
                            body,
                            base_ptr,
                            &hdr[start..],
                            &mut name,
                            &mut has_filename,
                            &mut ctype,
                        )?;
                    }
                    break;
                }
            };

            let line = if next > start && hdr[next - 1] == b'\r' {
                &hdr[start..next - 1]
            } else {
                &hdr[start..next]
            };

            if !line.is_empty() {
                Self::parse_header_line(
                    body,
                    base_ptr,
                    line,
                    &mut name,
                    &mut has_filename,
                    &mut ctype,
                )?;
            }

            start = next + 1;
        }

        let name = name.ok_or(MultipartError::Malformed(
            "missing Content-Disposition name",
        ))?;

        if ctype.is_none() && has_filename {
            ctype = Some(Bytes::from_static(OCTET_STREAM));
        }
        Ok((name, ctype))
    }

    #[inline]
    fn parse_header_line(
        body: &Bytes,
        base_ptr: usize,
        line: &[u8],
        name: &mut Option<Bytes>,
        has_filename: &mut bool,
        ctype: &mut Option<Bytes>,
    ) -> Result<(), MultipartError> {
        let s = std::str::from_utf8(line)?;

        let Some(colon) = memchr::memchr(b':', line) else {
            return Ok(());
        };
        let key = s[..colon].trim();
        let val = s[colon + 1..].trim();

        if key.eq_ignore_ascii_case("Content-Type") {
            if !val.is_empty() {
                *ctype = Some(Self::slice_str_as_bytes(body, base_ptr, val));
            }
        } else if key.eq_ignore_ascii_case("Content-Disposition") {
            for seg in val.split(';') {
                let seg = seg.trim();
                if seg.len() >= 5 && seg[..5].eq_ignore_ascii_case("name=") {
                    let unquoted = Self::unquote(&seg[5..]);
                    *name = Some(Self::slice_str_as_bytes(body, base_ptr, unquoted));
                } else if seg.len() >= 9 && seg[..9].eq_ignore_ascii_case("filename=") {
                    *has_filename = true;
                }
            }
        }
        Ok(())
    }

    #[inline]
    fn unquote(s: &str) -> &str {
        let s = s.trim();
        if s.len() >= 2 && s.as_bytes()[0] == b'"' && s.as_bytes()[s.len() - 1] == b'"' {
            &s[1..s.len() - 1]
        } else {
            s
        }
    }
}
