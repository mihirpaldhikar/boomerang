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
mod definition;
mod small_future;

pub use definition::{
    __register_route, MiddlewareHandler, RouteDefinition, RouteEntry, RouteHandler,
};
pub use small_future::SmallFuture;

use crate::router::definition::iter_registered_routes;
use hashbrown::HashMap;
use std::sync::{Arc, LazyLock};
use crate::protocol::HttpMethod;

static CATCH_ALL_KEY: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("__path__"));

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Static(Box<str>),
    Param(Arc<str>),
    CatchAll,
}

impl Segment {
    fn from_str(s: &str) -> Self {
        if s == "{__path__}" {
            Segment::CatchAll
        } else if s.starts_with('{') && s.ends_with('}') {
            Segment::Param(Arc::from(&s[1..s.len() - 1]))
        } else {
            Segment::Static(Box::from(s))
        }
    }
}

struct MethodTable {
    slots: [Option<RouteDefinition>; HttpMethod::COUNT],
    allow_mask: u16,
}

impl MethodTable {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            allow_mask: 0,
        }
    }

    fn insert(&mut self, route: RouteDefinition) {
        let idx = route.method.as_index();
        self.allow_mask |= 1u16 << idx;
        self.slots[idx] = Some(route);
    }

    #[inline(always)]
    fn get(&self, method: &HttpMethod) -> Option<&RouteDefinition> {
        self.slots[method.as_index()].as_ref()
    }
}

struct StaticChild {
    key: Box<str>,
    node: Node,
}

struct Node {
    handlers: Option<Box<MethodTable>>,
    static_children: Vec<StaticChild>,
    param_child: Option<(Arc<str>, Box<Node>)>,
    catch_all: Option<Box<MethodTable>>,
}

impl Node {
    fn new() -> Self {
        Self {
            handlers: None,
            static_children: Vec::new(),
            param_child: None,
            catch_all: None,
        }
    }

    fn insert(&mut self, segments: &[Segment], route: RouteDefinition) {
        if segments.is_empty() {
            self.handlers
                .get_or_insert_with(|| Box::new(MethodTable::new()))
                .insert(route);
            return;
        }

        match &segments[0] {
            Segment::Static(s) => {
                match self
                    .static_children
                    .binary_search_by(|c| c.key.as_ref().cmp(s.as_ref()))
                {
                    Ok(idx) => {
                        self.static_children[idx].node.insert(&segments[1..], route);
                    }
                    Err(idx) => {
                        let mut child = Node::new();
                        child.insert(&segments[1..], route);
                        self.static_children.insert(
                            idx,
                            StaticChild {
                                key: s.clone(),
                                node: child,
                            },
                        );
                    }
                }
            }

            Segment::Param(name) => {
                if let Some((_, child)) = &mut self.param_child {
                    child.insert(&segments[1..], route);
                } else {
                    let mut child = Box::new(Node::new());
                    child.insert(&segments[1..], route);
                    self.param_child = Some((name.clone(), child));
                }
            }

            Segment::CatchAll => {
                self.catch_all
                    .get_or_insert_with(|| Box::new(MethodTable::new()))
                    .insert(route);
            }
        }
    }

    #[inline]
    fn find_static_child(&self, seg: &str) -> Option<&Node> {
        let children = &self.static_children;
        let seg_bytes = seg.as_bytes();
        if children.len() <= 8 {
            for c in children {
                if c.key.as_bytes() == seg_bytes {
                    return Some(&c.node);
                }
            }
            None
        } else {
            children
                .binary_search_by(|c| c.key.as_ref().cmp(seg))
                .ok()
                .map(|idx| &children[idx].node)
        }
    }

    fn find_node<'a>(
        &'a self,
        base_ptr: usize,
        path: &str,
        method: &HttpMethod,
        params: &mut Vec<(Arc<str>, std::ops::Range<usize>)>,
        allowed_mask: &mut u16,
    ) -> Option<&'a RouteDefinition> {
        let (seg, rest) = match next_segment(path) {
            Some(t) => t,
            None => {
                if let Some(table) = &self.handlers {
                    *allowed_mask |= table.allow_mask;
                    if let Some(found) = table.get(method) {
                        return Some(found);
                    }
                }

                if let Some(table) = &self.catch_all {
                    *allowed_mask |= table.allow_mask;
                    if let Some(route) = table.get(method) {
                        params.push((CATCH_ALL_KEY.clone(), 0..0));
                        return Some(route);
                    }
                }
                return None;
            }
        };

        if let Some(child) = self.find_static_child(seg) {
            if let Some(found) = child.find_node(base_ptr, rest, method, params, allowed_mask) {
                return Some(found);
            }
        }

        if let Some((param_name, child)) = &self.param_child {
            let saved_len = params.len();
            let start = seg.as_ptr() as usize - base_ptr;
            params.push((param_name.clone(), start..start + seg.len()));

            if let Some(found) = child.find_node(base_ptr, rest, method, params, allowed_mask) {
                return Some(found);
            }

            params.truncate(saved_len);
        }

        if let Some(table) = &self.catch_all {
            *allowed_mask |= table.allow_mask;
            if let Some(route) = table.get(method) {
                let tail = trim_leading_slashes(path);
                let start = tail.as_ptr() as usize - base_ptr;
                params.push((CATCH_ALL_KEY.clone(), start..start + tail.len()));
                return Some(route);
            }
        }

        None
    }
}

#[inline]
fn trim_leading_slashes(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b'/' {
        i += 1;
    }
    &s[i..]
}

#[inline]
fn next_segment(path: &str) -> Option<(&str, &str)> {
    let trimmed = trim_leading_slashes(path);
    if trimmed.is_empty() {
        return None;
    }
    match memchr::memchr(b'/', trimmed.as_bytes()) {
        Some(idx) => Some((&trimmed[..idx], &trimmed[idx..])),
        None => Some((trimmed, "")),
    }
}

pub(crate) struct Router {
    static_routes: HashMap<Box<str>, MethodTable>,
    root: Node,
    has_dynamic: bool,
}

impl Router {
    pub fn build() -> Self {
        let mut static_routes: HashMap<Box<str>, MethodTable> = HashMap::new();
        let mut root = Node::new();
        let mut has_dynamic = false;

        for route in iter_registered_routes() {
            let segments = Self::parse_segments(&route.pattern);
            let is_static = segments.iter().all(|s| matches!(s, Segment::Static(_)));

            if is_static {
                let normalized = if segments.is_empty() {
                    Box::from("/")
                } else {
                    let mut s = String::with_capacity(route.pattern.len());
                    for seg in &segments {
                        if let Segment::Static(part) = seg {
                            s.push('/');
                            s.push_str(part);
                        }
                    }
                    s.into_boxed_str()
                };

                match static_routes.get_mut(normalized.as_ref()) {
                    Some(table) => table.insert(route.clone()),
                    None => {
                        let mut table = MethodTable::new();
                        table.insert(route.clone());
                        static_routes.insert(normalized, table);
                    }
                }
            } else {
                has_dynamic = true;
            }

            root.insert(&segments, route);
        }

        Self {
            static_routes,
            root,
            has_dynamic,
        }
    }

    #[inline]
    pub fn find_or_allowed_into<'a>(
        &'a self,
        method: &HttpMethod,
        path: &str,
        params_out: &mut Vec<(Arc<str>, std::ops::Range<usize>)>,
    ) -> Result<&'a RouteDefinition, u16> {
        params_out.clear();
        let clean_path = match memchr::memchr(b'?', path.as_bytes()) {
            Some(i) => &path[..i],
            None => path,
        };

        let mut allowed_mask = 0u16;

        if let Some(table) = self.static_routes.get(clean_path) {
            allowed_mask |= table.allow_mask;
            if let Some(route) = table.get(method) {
                return Ok(route);
            }
        }

        if !self.has_dynamic && clean_path.len() <= 1
            || (!self.has_dynamic
                && !clean_path.ends_with('/')
                && memchr::memmem::find(clean_path.as_bytes(), b"//").is_none())
        {
            return Err(allowed_mask);
        }

        let base_ptr = path.as_ptr() as usize;
        match self
            .root
            .find_node(base_ptr, clean_path, method, params_out, &mut allowed_mask)
        {
            Some(route) => Ok(route),
            None => {
                params_out.clear();
                Err(allowed_mask)
            }
        }
    }

    fn parse_segments(pattern: &str) -> Vec<Segment> {
        pattern
            .split('/')
            .filter(|s| !s.is_empty())
            .map(Segment::from_str)
            .collect()
    }
}
