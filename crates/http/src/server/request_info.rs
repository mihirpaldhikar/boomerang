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
use crate::protocol::{HttpMethod, HttpStatus};
use crate::server::RequestId;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct RequestInfo {
    id: RequestId,
    route: Arc<str>,
    method: HttpMethod,
    response_status: Option<HttpStatus>,
    started_at: std::time::SystemTime,
    ended_at: Option<std::time::SystemTime>,
}

impl RequestInfo {
    pub(crate) fn new(id: RequestId, route: Arc<str>, method: HttpMethod) -> Self {
        Self {
            id,
            route,
            method,
            response_status: None,
            started_at: std::time::SystemTime::now(),
            ended_at: None,
        }
    }

    pub fn id(&self) -> &RequestId {
        &self.id
    }
    pub fn method(&self) -> &HttpMethod {
        &self.method
    }
    pub fn route(&self) -> &str {
        self.route.as_ref()
    }
    pub fn response_status(&self) -> Option<HttpStatus> {
        self.response_status.clone()
    }
    pub fn started_at(&self) -> std::time::SystemTime {
        self.started_at
    }
    pub fn ended_at(&self) -> Option<std::time::SystemTime> {
        self.ended_at
    }

    pub(crate) fn set_response_status(&mut self, status: HttpStatus) {
        self.response_status = Some(status);
    }

    pub(crate) fn mark_as_end(&mut self) {
        self.ended_at = Some(std::time::SystemTime::now());
    }
}
