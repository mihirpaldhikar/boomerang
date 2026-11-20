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

use crate::protocol::HttpMethod;
use crate::router::small_future::SmallFuture;
use crate::server::HttpCall;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

pub type RouteHandler = for<'r> fn(&'r mut HttpCall) -> SmallFuture<'r, ()>;
pub type MiddlewareHandler = RouteHandler;

#[derive(Clone, Copy)]
pub struct RouteDefinition {
    pub pattern: &'static str,
    pub method: HttpMethod,
    pub handler: RouteHandler,
    pub middleware: &'static [MiddlewareHandler],
}

impl RouteDefinition {
    #[doc(hidden)]
    pub const fn __from_macro(
        method: HttpMethod,
        pattern: &'static str,
        handler: RouteHandler,
        middleware: &'static [MiddlewareHandler],
    ) -> Self {
        Self {
            pattern,
            method,
            handler,
            middleware,
        }
    }
}

#[repr(C)]
pub struct RouteEntry {
    pub route: RouteDefinition,
    next: AtomicPtr<RouteEntry>,
}

// SAFETY: `next` is only mutated atomically via `__register_route` before `main`.
unsafe impl Sync for RouteEntry {}

impl RouteEntry {
    pub const fn new(route: RouteDefinition) -> Self {
        Self {
            route,
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

static ROUTE_HEAD: AtomicPtr<RouteEntry> = AtomicPtr::new(ptr::null_mut());

#[doc(hidden)]
pub fn __register_route(entry: &'static RouteEntry) {
    let ptr = entry as *const RouteEntry as *mut RouteEntry;
    let mut head = ROUTE_HEAD.load(Ordering::Relaxed);
    loop {
        entry.next.store(head, Ordering::Relaxed);
        match ROUTE_HEAD.compare_exchange_weak(head, ptr, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => head = actual,
        }
    }
}

pub(crate) fn iter_registered_routes() -> impl Iterator<Item = RouteDefinition> {
    struct Iter(*const RouteEntry);

    impl Iterator for Iter {
        type Item = RouteDefinition;

        fn next(&mut self) -> Option<Self::Item> {
            if self.0.is_null() {
                return None;
            }
            let entry = unsafe { &*self.0 };
            self.0 = entry.next.load(Ordering::Acquire);
            Some(entry.route)
        }
    }

    Iter(ROUTE_HEAD.load(Ordering::Acquire))
}
