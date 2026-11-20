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

use crate::protocol::{HttpMethod, HttpResponse, HttpResponseBodyUnInitialized, HttpStatus};
use crate::server::HttpCall;
use std::marker::PhantomData;

#[derive(Clone, Debug)]
pub enum AllowedOrigin {
    Any,
    Only(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct Cors {
    pub(crate) allowed_origin: AllowedOrigin,
    pub(crate) allowed_methods_mask: u16,
    pub(crate) allowed_methods_str: Box<str>,
    pub(crate) allow_any_header: bool,
    pub(crate) allowed_headers_str: Option<Box<str>>,
    pub(crate) exposed_headers_str: Option<Box<str>>,
    pub(crate) allow_credentials: bool,
    pub(crate) max_age_str: Option<Box<str>>,
}

impl Cors {
    #[inline]
    fn resolve_allow_origin<'a>(&self, origin: Option<&'a str>) -> Option<&'a str> {
        match (&self.allowed_origin, origin) {
            (AllowedOrigin::Any, Some(o)) if self.allow_credentials => Some(o),
            (AllowedOrigin::Any, _) => Some("*"),
            (AllowedOrigin::Only(list), Some(o))
                if list.iter().any(|allowed| allowed.eq_ignore_ascii_case(o)) =>
            {
                Some(o)
            }
            _ => None,
        }
    }

    #[inline]
    fn select_vary(vary_origin: bool, vary_req_headers: bool) -> Option<&'static str> {
        match (vary_origin, vary_req_headers) {
            (false, false) => None,
            (true, false) => Some("Origin"),
            (false, true) => Some("Access-Control-Request-Headers"),
            (true, true) => Some("Origin, Access-Control-Request-Headers"),
        }
    }

    pub(crate) async fn handle_preflight(&self, call: &mut HttpCall) {
        self.apply_cors(call, true).await;
    }

    pub fn add_cors_headers(
        &self,
        call: &HttpCall,
        resp: HttpResponse<HttpResponseBodyUnInitialized>,
    ) -> HttpResponse<HttpResponseBodyUnInitialized> {
        let origin = if matches!(self.allowed_origin, AllowedOrigin::Any) && !self.allow_credentials
        {
            None
        } else {
            call.header("origin")
        };

        let Some(allow_origin) = self.resolve_allow_origin(origin) else {
            return resp;
        };

        let mut response = resp.__add_header_internal("access-control-allow-origin", allow_origin);

        let vary_origin =
            self.allow_credentials || !matches!(self.allowed_origin, AllowedOrigin::Any);
        if self.allow_credentials {
            response = response.__add_header_internal("access-control-allow-credentials", "true");
        }

        if !self.allowed_methods_str.is_empty() {
            response = response
                .__add_header_internal("access-control-allow-methods", &self.allowed_methods_str);
        }

        if let Some(exposed) = &self.exposed_headers_str {
            response = response.__add_header_internal("access-control-expose-headers", exposed);
        }

        if vary_origin {
            response = response.__add_header_internal("vary", "Origin");
        }

        response
    }

    async fn apply_cors(&self, call: &mut HttpCall, is_preflight: bool) {
        let (req, resp_init) = call.response_with_request();

        let origin = if matches!(self.allowed_origin, AllowedOrigin::Any) && !self.allow_credentials
        {
            None
        } else {
            req.header("origin")
        };

        let Some(allow_origin) = self.resolve_allow_origin(origin) else {
            if is_preflight {
                resp_init
                    .status_code(HttpStatus::NoContent)
                    .empty()
                    .send()
                    .await;
            }
            return;
        };

        if is_preflight {
            let requested_method = match req.header("access-control-request-method") {
                Some(val) => match HttpMethod::from_bytes(val.as_bytes()) {
                    Ok(m) => m,
                    Err(_) => {
                        resp_init
                            .status_code(HttpStatus::NoContent)
                            .empty()
                            .send()
                            .await;
                        return;
                    }
                },
                None => {
                    resp_init
                        .status_code(HttpStatus::NoContent)
                        .empty()
                        .send()
                        .await;
                    return;
                }
            };

            if (self.allowed_methods_mask & (1u16 << requested_method.as_index())) == 0 {
                resp_init
                    .status_code(HttpStatus::NoContent)
                    .empty()
                    .send()
                    .await;
                return;
            }
        }

        let mut resp = if is_preflight {
            resp_init.status_code(HttpStatus::NoContent)
        } else {
            resp_init
        };

        resp = resp.__add_header_internal("access-control-allow-origin", allow_origin);

        let vary_origin =
            self.allow_credentials || !matches!(self.allowed_origin, AllowedOrigin::Any);
        if self.allow_credentials {
            resp = resp.__add_header_internal("access-control-allow-credentials", "true");
        }

        if !self.allowed_methods_str.is_empty() {
            resp = resp
                .__add_header_internal("access-control-allow-methods", &self.allowed_methods_str);
        }

        let mut vary_req_headers = false;
        if is_preflight {
            let allow_headers = if self.allow_any_header {
                vary_req_headers = true;
                req.header("access-control-request-headers").unwrap_or("")
            } else {
                self.allowed_headers_str.as_deref().unwrap_or("")
            };

            if !allow_headers.is_empty() {
                resp = resp.__add_header_internal("access-control-allow-headers", allow_headers);
            } else {
                vary_req_headers = false;
            }

            if let Some(max_age) = &self.max_age_str {
                resp = resp.__add_header_internal("access-control-max-age", max_age);
            }
        }

        if let Some(vary) = Self::select_vary(vary_origin, vary_req_headers) {
            resp = resp.__add_header_internal("vary", vary);
        }

        if let Some(exposed) = &self.exposed_headers_str {
            resp = resp.__add_header_internal("access-control-expose-headers", exposed);
        }

        resp.empty().send().await;
    }
}

pub struct Any;
pub struct Restricted<T>(PhantomData<T>);

pub struct CorsBuilder<OriginState, MethodState, HeaderState> {
    allowed_origin: AllowedOrigin,
    allowed_methods: Vec<HttpMethod>,
    allowed_headers: Vec<String>,
    exposed_headers: Vec<String>,
    allow_credentials: bool,
    max_age: Option<u64>,
    _origin: PhantomData<OriginState>,
    _method: PhantomData<MethodState>,
    _header: PhantomData<HeaderState>,
}

impl CorsBuilder<Any, Restricted<HttpMethod>, Restricted<String>> {
    pub fn new() -> Self {
        Self {
            allowed_origin: AllowedOrigin::Any,
            allowed_methods: Vec::new(),
            allowed_headers: Vec::new(),
            exposed_headers: Vec::new(),
            allow_credentials: false,
            max_age: None,
            _origin: PhantomData,
            _method: PhantomData,
            _header: PhantomData,
        }
    }
}

impl Default for CorsBuilder<Any, Restricted<HttpMethod>, Restricted<String>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<MethodState, HeaderState> CorsBuilder<Any, MethodState, HeaderState> {
    pub fn allow_any_origin(self) -> Self {
        self
    }

    pub fn only_origins(self) -> CorsBuilder<Restricted<String>, MethodState, HeaderState> {
        CorsBuilder {
            allowed_origin: AllowedOrigin::Only(Vec::new()),
            allowed_methods: self.allowed_methods,
            allowed_headers: self.allowed_headers,
            exposed_headers: self.exposed_headers,
            allow_credentials: self.allow_credentials,
            max_age: self.max_age,
            _origin: PhantomData,
            _method: PhantomData,
            _header: PhantomData,
        }
    }
}

impl<MethodState, HeaderState> CorsBuilder<Restricted<String>, MethodState, HeaderState> {
    pub fn add_origin<S: Into<String>>(mut self, origin: S) -> Self {
        if let AllowedOrigin::Only(origins) = &mut self.allowed_origin {
            origins.push(origin.into());
        }
        self
    }
}

impl<OriginState, HeaderState> CorsBuilder<OriginState, Restricted<HttpMethod>, HeaderState> {
    pub fn add_method(mut self, method: HttpMethod) -> Self {
        self.allowed_methods.push(method);
        self
    }
}

impl<OriginState, MethodState> CorsBuilder<OriginState, MethodState, Restricted<String>> {
    pub fn add_header<S: Into<String>>(mut self, header: S) -> Self {
        self.allowed_headers.push(header.into());
        self
    }
}

impl<OriginState, MethodState, HeaderState> CorsBuilder<OriginState, MethodState, HeaderState> {
    pub fn add_exposed_header<S: Into<String>>(mut self, header: S) -> Self {
        self.exposed_headers.push(header.into());
        self
    }

    pub fn allow_credentials(mut self, value: bool) -> Self {
        self.allow_credentials = value;
        self
    }

    pub fn max_age(mut self, value: u64) -> Self {
        self.max_age = Some(value);
        self
    }

    pub fn build(self) -> Cors {
        let mut allowed_methods_mask = 0u16;
        let mut methods_str = String::with_capacity(self.allowed_methods.len() * 7);
        for m in &self.allowed_methods {
            let bit = 1u16 << m.as_index();
            if (allowed_methods_mask & bit) != 0 {
                continue;
            }
            allowed_methods_mask |= bit;
            if !methods_str.is_empty() {
                methods_str.push(',');
            }
            use std::fmt::Write;
            let _ = write!(methods_str, "{m}");
        }

        let allow_any_header =
            self.allowed_headers.is_empty() || self.allowed_headers.iter().any(|h| h == "*");
        let allowed_headers_str = if allow_any_header {
            None
        } else {
            Some(self.allowed_headers.join(",").into_boxed_str())
        };

        let exposed_headers_str = if self.exposed_headers.is_empty() {
            None
        } else {
            Some(self.exposed_headers.join(",").into_boxed_str())
        };

        let max_age_str = self.max_age.map(|v| v.to_string().into_boxed_str());

        Cors {
            allowed_origin: self.allowed_origin,
            allowed_methods_mask,
            allowed_methods_str: methods_str.into_boxed_str(),
            allow_any_header,
            allowed_headers_str,
            exposed_headers_str,
            allow_credentials: self.allow_credentials,
            max_age_str,
        }
    }
}
