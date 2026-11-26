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
use crate::protocol::{
    Body, BodyState, HttpMethod, HttpParam, HttpRequest, HttpRequestError, HttpResponse,
    HttpResponseInit, ResponseHook,
};
use crate::server::connection::{Connection, WorkerScratch};
use crate::server::{RequestId, RestartContext, RestartHandle};
use std::collections::HashMap;
use std::net::IpAddr;

pub struct HttpCall {
    pub(crate) connection: Connection,
    pub(crate) request: HttpRequest,
    pub(crate) extras: HashMap<String, String>,
    pub(crate) request_id: RequestId,
    pub(crate) response_hook: Option<ResponseHook>,
    pub(crate) suppress_body: bool,
}

impl HttpCall {
    pub(crate) async fn parse(
        mut connection: Connection,
        request_id: RequestId,
        max_body_size: usize,
        keep_alive_timeout: std::time::Duration,
        request_timeout: std::time::Duration,
        idle_sleep: std::pin::Pin<&mut tokio::time::Sleep>,
        last_timer_sec: &mut u64,
    ) -> Result<Option<Self>, (Connection, HttpRequestError)> {
        let request = match connection
            .read_request(
                max_body_size,
                keep_alive_timeout,
                request_timeout,
                idle_sleep,
                last_timer_sec,
            )
            .await
        {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(None),
            Err(e) => return Err((connection, e)),
        };

        Ok(Some(Self {
            connection,
            request,
            extras: WorkerScratch::take_extras_buf(),
            request_id,
            response_hook: None,
            suppress_body: false,
        }))
    }

    pub(crate) fn method(&self) -> &HttpMethod {
        self.request.method()
    }

    pub fn restart<F, Fut>(&self, force: bool, pre_hook: F)
    where
        F: FnOnce(RestartContext) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.connection.restart_handle.restart(force, pre_hook);
    }

    pub fn restart_handle(&self) -> RestartHandle {
        self.connection.restart_handle.clone()
    }

    pub fn client_ip(&self) -> IpAddr {
        self.request.client_ip()
    }

    pub fn peer_ip(&self) -> IpAddr {
        self.request.peer_ip()
    }

    pub fn host(&self) -> Option<&str> {
        self.request.host()
    }

    pub fn route(&self) -> &str {
        self.request.route()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.request.header(name)
    }

    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.request.cookie(name)
    }

    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    pub async fn body(&mut self) -> Result<&Body, String> {
        if let BodyState::Read(ref b) = self.request.body_state {
            return Ok(b);
        }

        let state = std::mem::replace(&mut self.request.body_state, BodyState::Reading);

        let (content_length, is_chunked) = match state {
            BodyState::Unread {
                content_length,
                is_chunked,
            } => (content_length, is_chunked),
            _ => return Err("Body already read or error occurred".into()),
        };

        if self
            .header("expect")
            .map(|v| v.eq_ignore_ascii_case("100-continue"))
            .unwrap_or(false)
        {
            let _ = self
                .connection
                .write_all_raw(b"HTTP/1.1 100 Continue\r\n\r\n")
                .await;
        }

        let content_type = self.header("content-type").map(|s| s.to_string());

        let body_fut = async {
            if is_chunked {
                Body::read_chunked(
                    &mut self.connection,
                    self.request.max_body_size,
                    content_type,
                )
                .await
            } else {
                Body::read_exact(
                    &mut self.connection,
                    content_length,
                    self.request.max_body_size,
                    content_type,
                )
                .await
            }
        };

        let body = match tokio::time::timeout(self.request.request_timeout, body_fut).await {
            Ok(res) => res?,
            Err(_) => return Err("Timed out reading request body".into()),
        };

        self.request.body_state = BodyState::Read(body);

        if let BodyState::Read(ref b) = self.request.body_state {
            Ok(b)
        } else {
            unreachable!()
        }
    }

    #[inline]
    pub fn path_param(&self, param_name: &str) -> Option<&str> {
        self.request
            .path_params
            .iter()
            .find(|(k, _)| k.as_ref() == param_name)
            .and_then(|(_, range)| self.request.route.get(range.clone()))
    }

    pub fn query_param(&self, param_name: &str) -> Option<&str> {
        let params = self
            .request
            .query_params
            .get_or_init(|| HttpParam::parse_query_params(&self.request.route));

        params
            .iter()
            .find(|(k, _)| k.as_ref() == param_name)
            .map(|(_, v)| v.as_ref())
    }

    pub fn set_extras<K: Into<String>, V: Into<String>>(&mut self, key: K, value: V) {
        let mut normalized: String = key.into();
        let trimmed = normalized.trim();
        if trimmed.len() != normalized.len() {
            normalized = trimmed.to_ascii_lowercase();
        } else {
            normalized.make_ascii_lowercase();
        }

        match self.extras.entry(normalized) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(value.into());
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().push_str(", ");
                entry.get_mut().push_str(&value.into());
            }
        }
    }

    pub fn get_extras(&self, key: &str) -> Option<&str> {
        self.extras.get(key).map(|s| s.as_str())
    }

    pub fn remove_extras(&mut self, key: &str) -> Option<String> {
        self.extras.remove(key)
    }

    pub(crate) fn response_sent(&self) -> bool {
        self.connection.has_written_response()
    }

    pub(crate) fn set_response_hook(&mut self, hook: ResponseHook) {
        self.response_hook = Some(hook);
    }

    pub(crate) fn set_suppress_body(&mut self) {
        self.suppress_body = true;
    }

    pub fn response(&mut self) -> HttpResponseInit<'_> {
        let mut field_lines = WorkerScratch::take_resp_fields();
        field_lines.clear();

        let mut response = HttpResponse::new_with_fields(
            self.request.http_version().clone(),
            self.request_id,
            field_lines,
        )
        .with_accept_encoding(self.request.header("accept-encoding"));

        if let Some(hook) = self.response_hook.take() {
            response = response.set_on_set(hook);
        }
        if self.suppress_body {
            response = response.suppress_body();
        }

        HttpResponseInit {
            connection: &mut self.connection,
            response,
        }
    }

    #[inline]
    pub(crate) fn response_with_request(&mut self) -> (&HttpRequest, HttpResponseInit<'_>) {
        let mut field_lines = WorkerScratch::take_resp_fields();
        field_lines.clear();

        let mut response = HttpResponse::new_with_fields(
            self.request.http_version().clone(),
            self.request_id,
            field_lines,
        )
        .with_accept_encoding(self.request.header("accept-encoding"));

        if let Some(hook) = self.response_hook.take() {
            response = response.set_on_set(hook);
        }
        if self.suppress_body {
            response = response.suppress_body();
        }

        (
            &self.request,
            HttpResponseInit {
                connection: &mut self.connection,
                response,
            },
        )
    }
}

impl HttpCall {
    pub(crate) async fn discard_unread_body(&mut self) -> Result<(), std::io::Error> {
        use bytes::Buf;

        let state = std::mem::replace(&mut self.request.body_state, BodyState::Reading);

        let (content_length, is_chunked) = match state {
            BodyState::Read(_) => return Ok(()),
            BodyState::Reading => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Incomplete body read forces connection close",
                ));
            }
            BodyState::Unread {
                content_length,
                is_chunked,
            } => (content_length, is_chunked),
        };

        if content_length == 0 && !is_chunked {
            return Ok(());
        }

        if is_chunked || content_length > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Large or chunked unread body forces connection close",
            ));
        }

        let mut remaining = content_length;

        let avail = self.connection.reader.len();
        if avail > 0 {
            let take = remaining.min(avail);
            self.connection.reader.advance(take);
            remaining -= take;
        }

        if remaining == 0 {
            return Ok(());
        }

        self.connection.flush_pipeline().await?;

        let drain_fut = async {
            while remaining > 0 {
                let n = self.connection.read_more().await?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "EOF while draining unread body",
                    ));
                }
                let take = remaining.min(self.connection.reader.len());
                self.connection.reader.advance(take);
                remaining -= take;
            }
            Ok(())
        };

        tokio::time::timeout(std::time::Duration::from_secs(2), drain_fut)
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Timeout while draining unread body",
                )
            })?
    }
}
