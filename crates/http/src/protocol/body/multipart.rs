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
use memchr::memmem;

const OCTET_STREAM: &str = "application/octet-stream";

#[derive(thiserror::Error, Debug)]
pub enum MultipartError {
    #[error("boundary not found")]
    MissingBoundary,
    #[error("{0}")]
    Malformed(&'static str),
    #[error("{0}")]
    Utf8(#[from] std::str::Utf8Error),
}

#[derive(Debug, Clone, Copy)]
pub struct MultiPartField<'a> {
    name: &'a str,
    filename: Option<&'a str>,
    content_type: Option<&'a str>,
    content: &'a [u8],
}

impl<'a> MultiPartField<'a> {
    #[inline]
    pub fn name(&self) -> &'a str {
        self.name
    }

    #[inline]
    pub fn filename(&self) -> Option<&'a str> {
        self.filename
    }
    #[inline]
    pub fn content_type(&self) -> Option<&'a str> {
        self.content_type
    }
    #[inline]
    pub fn content(&self) -> &'a [u8] {
        self.content
    }
    #[inline]
    pub fn text(&self) -> Result<&'a str, std::str::Utf8Error> {
        std::str::from_utf8(self.content)
    }
}

pub struct Multipart<'a> {
    fields: Vec<MultiPartField<'a>>,
}

impl<'a> Multipart<'a> {
    pub(crate) fn parse(body: &'a [u8], boundary: &str) -> Result<Self, MultipartError> {
        let b = boundary.as_bytes();
        if b.is_empty() {
            return Err(MultipartError::MissingBoundary);
        }
        if b.len() > 70 {
            return Err(MultipartError::Malformed("boundary too long"));
        }

        let mut scratch = [0u8; 76];
        let n = 6 + b.len();
        scratch[..4].copy_from_slice(b"\r\n--");
        scratch[4..4 + b.len()].copy_from_slice(b);
        scratch[4 + b.len()..n].copy_from_slice(b"\r\n");
        let marker = &scratch[..n];
        let open = &marker[2..];
        let mid = &marker[..n - 2];

        let first = memmem::find(body, open).ok_or(MultipartError::MissingBoundary)?;
        let mut pos = first + open.len();

        let mid_finder = memmem::Finder::new(mid);
        let hdr_finder = memmem::Finder::new(b"\r\n\r\n");
        let mut fields = Vec::with_capacity(4);

        loop {
            let hdr_end = pos
                + hdr_finder
                    .find(&body[pos..])
                    .ok_or(MultipartError::Malformed("headers missing CRLFCRLF"))?;

            let hdr = std::str::from_utf8(&body[pos..hdr_end])?;
            let (name, filename, content_type) = parse_headers(hdr)?;

            let data_start = hdr_end + 4;
            let delim = data_start
                + mid_finder
                    .find(&body[data_start..])
                    .ok_or(MultipartError::Malformed("next boundary not found"))?;

            fields.push(MultiPartField {
                name,
                filename,
                content_type,
                content: &body[data_start..delim],
            });

            let after = delim + mid.len();
            match body.get(after..after + 2) {
                Some(b"--") => break,
                Some(b"\r\n") => pos = after + 2,
                _ => return Err(MultipartError::Malformed("boundary line missing CRLF")),
            }
        }

        Ok(Self { fields })
    }

    #[inline]
    pub fn get(&self, name: &str) -> Option<MultiPartField<'a>> {
        self.fields.iter().rfind(|f| f.name == name).copied()
    }

    #[inline]
    pub fn fields(&self) -> &[MultiPartField<'a>] {
        &self.fields
    }
}

type PartHeaders<'a> = (&'a str, Option<&'a str>, Option<&'a str>);

fn parse_headers(hdr: &str) -> Result<PartHeaders<'_>, MultipartError> {
    let (mut name, mut filename, mut ctype) = (None, None, None);

    for line in hdr.split('\n') {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let (key, val) = (key.trim(), val.trim());

        if key.eq_ignore_ascii_case("content-type") {
            if !val.is_empty() {
                ctype = Some(val);
            }
        } else if key.eq_ignore_ascii_case("content-disposition") {
            for (k, v) in params(val) {
                if k.eq_ignore_ascii_case("name") {
                    name = Some(v);
                } else if k.eq_ignore_ascii_case("filename") {
                    filename = Some(v);
                }
            }
        }
    }

    let name = name.ok_or(MultipartError::Malformed(
        "missing Content-Disposition name",
    ))?;
    if ctype.is_none() && filename.is_some() {
        ctype = Some(OCTET_STREAM);
    }
    Ok((name, filename, ctype))
}

fn params(s: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut rest = s.split_once(';').map_or("", |(_, r)| r);

    std::iter::from_fn(move || {
        loop {
            rest = rest.trim_start_matches(|c: char| c == ';' || c.is_ascii_whitespace());
            if rest.is_empty() {
                return None;
            }
            let eq = rest.find('=');
            let semi = rest.find(';');

            match (eq, semi) {
                (Some(e), sc) if sc.map_or(true, |sc| e < sc) => {
                    let key = rest[..e].trim_end();
                    let after = rest[e + 1..].trim_start();
                    let (val, tail) = if let Some(q) = after.strip_prefix('"') {
                        match closing_quote(q) {
                            Some(end) => (&q[..end], &q[end + 1..]),
                            None => (q, ""),
                        }
                    } else {
                        let end = after.find(';').unwrap_or(after.len());
                        (after[..end].trim_end(), &after[end..])
                    };
                    rest = tail;
                    return Some((key, val));
                }
                (_, Some(sc)) => rest = &rest[sc + 1..],
                _ => return None,
            }
        }
    })
}

fn closing_quote(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while let Some(off) = b.get(i..).and_then(|t| memchr::memchr2(b'"', b'\\', t)) {
        let p = i + off;
        if b[p] == b'"' {
            return Some(p);
        }
        i = p + 2;
    }
    None
}
