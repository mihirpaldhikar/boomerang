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
use crate::protocol::request::HttpRequestError;
use std::fmt::{Display, Formatter};
use std::str::FromStr;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum HttpMethod {
    GET,
    POST,
    PATCH,
    PUT,
    DELETE,
    OPTIONS,
    HEAD,
    CONNECT,
    TRACE,
}

impl HttpMethod {
    #[inline]
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, HttpRequestError> {
        match bytes {
            b"GET" => Ok(HttpMethod::GET),
            b"POST" => Ok(HttpMethod::POST),
            b"PUT" => Ok(HttpMethod::PUT),
            b"DELETE" => Ok(HttpMethod::DELETE),
            b"PATCH" => Ok(HttpMethod::PATCH),
            b"OPTIONS" => Ok(HttpMethod::OPTIONS),
            b"HEAD" => Ok(HttpMethod::HEAD),
            b"CONNECT" => Ok(HttpMethod::CONNECT),
            b"TRACE" => Ok(HttpMethod::TRACE),
            _ => {
                if bytes.eq_ignore_ascii_case(b"GET") {
                    Ok(HttpMethod::GET)
                } else if bytes.eq_ignore_ascii_case(b"POST") {
                    Ok(HttpMethod::POST)
                } else if bytes.eq_ignore_ascii_case(b"PUT") {
                    Ok(HttpMethod::PUT)
                } else if bytes.eq_ignore_ascii_case(b"DELETE") {
                    Ok(HttpMethod::DELETE)
                } else if bytes.eq_ignore_ascii_case(b"PATCH") {
                    Ok(HttpMethod::PATCH)
                } else if bytes.eq_ignore_ascii_case(b"OPTIONS") {
                    Ok(HttpMethod::OPTIONS)
                } else if bytes.eq_ignore_ascii_case(b"HEAD") {
                    Ok(HttpMethod::HEAD)
                } else if bytes.eq_ignore_ascii_case(b"CONNECT") {
                    Ok(HttpMethod::CONNECT)
                } else if bytes.eq_ignore_ascii_case(b"TRACE") {
                    Ok(HttpMethod::TRACE)
                } else {
                    Err(HttpRequestError::InvalidHttpMethod())
                }
            }
        }
    }
}

impl HttpMethod {
    pub(crate) const COUNT: usize = 9;

    #[inline(always)]
    pub(crate) const fn as_index(&self) -> usize {
        match self {
            HttpMethod::GET => 0,
            HttpMethod::POST => 1,
            HttpMethod::PUT => 2,
            HttpMethod::DELETE => 3,
            HttpMethod::PATCH => 4,
            HttpMethod::OPTIONS => 5,
            HttpMethod::HEAD => 6,
            HttpMethod::CONNECT => 7,
            HttpMethod::TRACE => 8,
        }
    }
}

impl FromStr for HttpMethod {
    type Err = HttpRequestError;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_bytes(s.as_bytes())
    }
}

impl Display for HttpMethod {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let method = match self {
            HttpMethod::GET => "GET",
            HttpMethod::POST => "POST",
            HttpMethod::PATCH => "PATCH",
            HttpMethod::PUT => "PUT",
            HttpMethod::DELETE => "DELETE",
            HttpMethod::OPTIONS => "OPTIONS",
            HttpMethod::HEAD => "HEAD",
            HttpMethod::CONNECT => "CONNECT",
            HttpMethod::TRACE => "TRACE",
        };

        f.write_str(method)
    }
}
