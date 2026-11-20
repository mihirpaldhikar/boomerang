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

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum HttpStatus {
    Ok,
    Created,
    Accepted,
    NoContent,
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    InternalServerError,
    NotImplemented,
    BadGateWay,
    ServiceUnavailable,
    ContentTooLarge,
    UriTooLong,
    RequestHeaderFieldsTooLarge,
    NotModified,
    MethodNotAllowed,
    PartialContent,
    RequestTimeout,
}

impl HttpStatus {
    #[inline]
    pub(crate) fn as_bytes(&self) -> &'static [u8] {
        match self {
            HttpStatus::Ok => b"200 OK",
            HttpStatus::Created => b"201 Created",
            HttpStatus::Accepted => b"202 Accepted",
            HttpStatus::NoContent => b"204 No Content",
            HttpStatus::PartialContent => b"206 Partial Content",
            HttpStatus::NotModified => b"304 Not Modified",
            HttpStatus::BadRequest => b"400 Bad Request",
            HttpStatus::Unauthorized => b"401 Unauthorized",
            HttpStatus::Forbidden => b"403 Forbidden",
            HttpStatus::NotFound => b"404 Not Found",
            HttpStatus::MethodNotAllowed => b"405 Method Not Allowed",
            HttpStatus::RequestTimeout => b"408 Request Timeout",
            HttpStatus::ContentTooLarge => b"413 Content Too Large",
            HttpStatus::UriTooLong => b"414 URI Too Long",
            HttpStatus::RequestHeaderFieldsTooLarge => b"431 Request Header Fields Too Large",
            HttpStatus::InternalServerError => b"500 Internal Server Error",
            HttpStatus::NotImplemented => b"501 Not Implemented",
            HttpStatus::BadGateWay => b"502 Bad Gateway",
            HttpStatus::ServiceUnavailable => b"503 Service Unavailable",
        }
    }
}

impl From<HttpStatus> for u16 {
    fn from(value: HttpStatus) -> Self {
        match value {
            HttpStatus::Ok => 200,
            HttpStatus::Created => 201,
            HttpStatus::Accepted => 202,
            HttpStatus::NoContent => 204,
            HttpStatus::PartialContent => 206,
            HttpStatus::NotModified => 304,
            HttpStatus::BadRequest => 400,
            HttpStatus::Unauthorized => 401,
            HttpStatus::Forbidden => 403,
            HttpStatus::NotFound => 404,
            HttpStatus::MethodNotAllowed => 405,
            HttpStatus::RequestTimeout => 408,
            HttpStatus::ContentTooLarge => 413,
            HttpStatus::UriTooLong => 414,
            HttpStatus::RequestHeaderFieldsTooLarge => 431,
            HttpStatus::InternalServerError => 500,
            HttpStatus::NotImplemented => 501,
            HttpStatus::BadGateWay => 502,
            HttpStatus::ServiceUnavailable => 503,
        }
    }
}
