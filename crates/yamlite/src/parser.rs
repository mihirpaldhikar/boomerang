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

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::{Tape, TapeKind, TapeNode, YamlError};
use crate::{YamlErrorKind, simd};

const MAX_DEPTH: usize = 128;

pub fn parse_to_tape(src: &str) -> Result<Tape, YamlError> {
    let mut p = Parser::new(src);
    p.parse_single_document()?;
    Ok(Tape {
        src,
        nodes: p.nodes,
        owned_strings: p.owned_strings,
    })
}

pub fn parse_stream_to_tapes(src: &str) -> Result<Vec<Tape<'_>>, YamlError> {
    let mut p = Parser::new(src);
    let mut tapes = Vec::new();
    p.skip_bom();

    loop {
        p.skip_ws_and_comments();
        while p.pos < p.bytes.len() && p.bytes[p.pos] == b'%' && p.current_column() == 0 {
            p.skip_to_eol_simd();
            p.skip_ws_and_comments();
        }
        if p.pos >= p.bytes.len() {
            break;
        }
        if p.is_doc_end() {
            p.pos += 3;
            p.skip_ws_and_comments();
            continue;
        }
        p.nodes.clear();
        p.anchors.clear();
        p.owned_strings.clear();
        p.parse_single_document_body()?;
        tapes.push(Tape {
            src,
            nodes: core::mem::take(&mut p.nodes),
            owned_strings: core::mem::take(&mut p.owned_strings),
        });
        p.skip_ws_and_comments();
        if p.pos < p.bytes.len() && p.is_doc_end() {
            p.pos += 3;
        }
    }
    if tapes.is_empty() {
        tapes.push(Tape {
            src,
            nodes: alloc::vec![TapeNode {
                kind: TapeKind::Null,
                offset: 0
            }],
            owned_strings: Vec::new(),
        });
    }
    Ok(tapes)
}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    nodes: Vec<TapeNode>,
    owned_strings: Vec<String>,
    anchors: BTreeMap<&'a str, u32>,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            nodes: Vec::with_capacity((src.len() / 8).max(8)),
            owned_strings: Vec::new(),
            anchors: BTreeMap::new(),
            depth: 0,
        }
    }

    fn skip_bom(&mut self) {
        if self.bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            self.pos += 3;
        }
    }

    #[inline]
    fn enter_depth(&mut self) -> Result<(), YamlError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(YamlError::from_kind(
                self.pos,
                YamlErrorKind::MaxDepthExceeded(MAX_DEPTH),
            ));
        }
        Ok(())
    }

    #[inline]
    fn exit_depth(&mut self) {
        self.depth -= 1;
    }

    fn skip_to_eol_simd(&mut self) {
        while self.pos < self.bytes.len() {
            let (masks, consumed) = simd::scan_chunk(self.bytes, self.pos);
            if masks.newlines != 0 {
                self.pos += masks.newlines.trailing_zeros() as usize;
                return;
            }
            self.pos += consumed;
        }
    }

    fn consume_newline(&mut self) -> bool {
        if self.pos < self.bytes.len() && self.bytes[self.pos] == b'\r' {
            self.pos += 1;
            if self.pos < self.bytes.len() && self.bytes[self.pos] == b'\n' {
                self.pos += 1;
            }
            return true;
        }
        if self.pos < self.bytes.len() && self.bytes[self.pos] == b'\n' {
            self.pos += 1;
            return true;
        }
        false
    }

    fn skip_horiz_ws(&mut self) {
        loop {
            self.pos += simd::count_spaces(self.bytes, self.pos);
            if self.pos < self.bytes.len() && self.bytes[self.pos] == b'\t' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            self.skip_horiz_ws();
            if self.pos >= self.bytes.len() {
                break;
            }
            match self.bytes[self.pos] {
                b'\n' | b'\r' => {
                    self.consume_newline();
                }
                b'#' => self.skip_to_eol_simd(),
                _ => break,
            }
        }
    }

    #[inline]
    fn line_start_of(&self, mut pos: usize) -> usize {
        while pos > 0 && self.bytes[pos - 1] != b'\n' {
            pos -= 1;
        }
        pos
    }

    #[inline]
    fn current_column(&self) -> usize {
        self.pos - self.line_start_of(self.pos)
    }

    #[inline]
    fn line_indent_at(&self, pos: usize) -> usize {
        simd::count_spaces(self.bytes, self.line_start_of(pos))
    }

    #[inline]
    fn is_doc_start(&self) -> bool {
        self.current_column() == 0
            && self.bytes[self.pos..].starts_with(b"---")
            && self
                .bytes
                .get(self.pos + 3)
                .map_or(true, |&b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
    }

    #[inline]
    fn is_doc_end(&self) -> bool {
        self.current_column() == 0
            && self.bytes[self.pos..].starts_with(b"...")
            && self
                .bytes
                .get(self.pos + 3)
                .map_or(true, |&b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
    }

    #[inline]
    fn is_doc_boundary(&self) -> bool {
        self.is_doc_start() || self.is_doc_end()
    }

    fn parse_single_document(&mut self) -> Result<(), YamlError> {
        self.skip_bom();
        self.skip_ws_and_comments();
        while self.pos < self.bytes.len()
            && self.bytes[self.pos] == b'%'
            && self.current_column() == 0
        {
            self.skip_to_eol_simd();
            self.skip_ws_and_comments();
        }
        self.parse_single_document_body()
    }

    fn parse_single_document_body(&mut self) -> Result<(), YamlError> {
        if self.pos < self.bytes.len() && self.is_doc_start() {
            self.pos += 3;
            self.skip_ws_and_comments();
        }
        if self.pos >= self.bytes.len() || self.is_doc_boundary() {
            self.nodes.push(TapeNode {
                kind: TapeKind::Null,
                offset: self.pos as u32,
            });
            return Ok(());
        }
        self.parse_block_node(-1)
    }

    fn parse_node_properties(&mut self) -> Result<Option<&'a str>, YamlError> {
        let mut anchor: Option<&'a str> = None;
        loop {
            self.skip_horiz_ws();
            if self.pos >= self.bytes.len() {
                break;
            }
            match self.bytes[self.pos] {
                b'&' if anchor.is_none() => {
                    self.pos += 1;
                    let start = self.pos;
                    while self.pos < self.bytes.len()
                        && !matches!(
                            self.bytes[self.pos],
                            b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}'
                        )
                    {
                        self.pos += 1;
                    }
                    if start == self.pos {
                        return Err(YamlError::new(start, "empty anchor name"));
                    }
                    anchor = Some(&self.src[start..self.pos]);
                }
                b'!' => {
                    let start = self.pos;
                    self.pos += 1;
                    while self.pos < self.bytes.len()
                        && !matches!(
                            self.bytes[self.pos],
                            b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}'
                        )
                    {
                        self.pos += 1;
                    }
                    self.nodes.push(TapeNode {
                        kind: TapeKind::Tag {
                            start: start as u32,
                            end: self.pos as u32,
                        },
                        offset: start as u32,
                    });
                }
                _ => break,
            }
        }
        Ok(anchor)
    }

    fn parse_block_node_inner(&mut self, parent_indent: isize) -> Result<(), YamlError> {
        self.skip_ws_and_comments();
        if self.pos >= self.bytes.len() || self.is_doc_boundary() {
            self.nodes.push(TapeNode {
                kind: TapeKind::Null,
                offset: self.pos as u32,
            });
            return Ok(());
        }

        let anchor = self.parse_node_properties()?;
        let value_start_idx = self.nodes.len() as u32;
        if let Some(name) = anchor {
            self.anchors.insert(name, value_start_idx);
        }

        if self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b'\n' | b'\r' | b'#') {
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() || self.is_doc_boundary() {
                self.nodes.push(TapeNode {
                    kind: TapeKind::Null,
                    offset: self.pos as u32,
                });
                return Ok(());
            }
            let next_indent = self.line_indent_at(self.pos) as isize;
            if next_indent <= parent_indent && !self.is_seq_dash_at(self.pos) {
                self.nodes.push(TapeNode {
                    kind: TapeKind::Null,
                    offset: self.pos as u32,
                });
                return Ok(());
            }
        }

        if self.pos >= self.bytes.len() || self.is_doc_boundary() {
            self.nodes.push(TapeNode {
                kind: TapeKind::Null,
                offset: self.pos as u32,
            });
            return Ok(());
        }

        match self.bytes[self.pos] {
            b'*' => self.parse_alias(),
            b'[' => self.parse_flow_seq(),
            b'{' => self.parse_flow_map(),
            b'|' | b'>' => self.parse_block_scalar(parent_indent),
            b'-' if self.is_seq_dash_at(self.pos) => self.parse_block_seq(self.current_column()),
            _ if self.looks_like_block_map_entry() => self.parse_block_map(self.current_column()),
            _ => self.parse_plain_or_quoted_scalar(false),
        }
    }

    fn parse_block_node(&mut self, parent_indent: isize) -> Result<(), YamlError> {
        self.enter_depth()?;
        let res = self.parse_block_node_inner(parent_indent);
        self.exit_depth();
        res
    }

    #[inline]
    fn is_seq_dash_at(&self, pos: usize) -> bool {
        self.bytes.get(pos) == Some(&b'-')
            && self
                .bytes
                .get(pos + 1)
                .map_or(true, |&c| matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
    }

    fn parse_alias(&mut self) -> Result<(), YamlError> {
        let offset = self.pos;
        self.pos += 1;
        let start = self.pos;
        while self.pos < self.bytes.len()
            && !matches!(
                self.bytes[self.pos],
                b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}'
            )
        {
            self.pos += 1;
        }
        let name = &self.src[start..self.pos];
        let target_idx = *self.anchors.get(name).ok_or_else(|| {
            YamlError::from_kind(offset, YamlErrorKind::UnknownAlias(String::from(name)))
        })?;
        self.nodes.push(TapeNode {
            kind: TapeKind::Alias { target_idx },
            offset: offset as u32,
        });
        Ok(())
    }

    fn looks_like_block_map_entry(&self) -> bool {
        let mut i = self.pos;
        let mut quote = None;
        while i < self.bytes.len() {
            let b = self.bytes[i];
            if let Some(q) = quote {
                if q == b'"' && b == b'\\' {
                    i += 2;
                    continue;
                }
                if q == b'\'' && b == b'\'' && self.bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                if b == q {
                    quote = None;
                }
            } else {
                match b {
                    b'"' | b'\'' => quote = Some(b),
                    b'\n' | b'\r' => return false,
                    b'#' if i > self.pos && matches!(self.bytes[i - 1], b' ' | b'\t') => {
                        return false;
                    }
                    b':' if self
                        .bytes
                        .get(i + 1)
                        .map_or(true, |&n| matches!(n, b' ' | b'\t' | b'\n' | b'\r')) =>
                    {
                        return true;
                    }
                    b'[' | b'{' => return false,
                    _ => {}
                }
            }
            i += 1;
        }
        false
    }

    fn parse_block_map(&mut self, map_indent: usize) -> Result<(), YamlError> {
        let start_node_idx = self.nodes.len();
        self.nodes.push(TapeNode {
            kind: TapeKind::MapStart { end_idx: 0 },
            offset: self.pos as u32,
        });

        let mut explicit_keys: Vec<String> = Vec::new();
        let mut pending_merges: Vec<u32> = Vec::new();

        loop {
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() || self.is_doc_boundary() {
                break;
            }
            let indent = self.line_indent_at(self.pos);
            if indent != map_indent || !self.looks_like_block_map_entry() {
                break;
            }

            if self.bytes[self.pos..].starts_with(b"<<") {
                let after = self.pos + 2 + simd::count_spaces(self.bytes, self.pos + 2);
                if self.bytes.get(after) == Some(&b':')
                    && self
                        .bytes
                        .get(after + 1)
                        .map_or(true, |&n| matches!(n, b' ' | b'\t' | b'\n' | b'\r'))
                {
                    self.pos = after + 1;
                    self.skip_horiz_ws();
                    self.collect_merge_targets(&mut pending_merges)?;
                    continue;
                }
            }

            let key_node_idx = self.nodes.len();
            self.parse_map_key()?;
            let key_str = self.extract_key_text(key_node_idx);
            explicit_keys.push(key_str);

            if self.bytes.get(self.pos) != Some(&b':') {
                return Err(YamlError::new(self.pos, "expected `:` after mapping key"));
            }
            self.pos += 1;
            self.skip_horiz_ws();

            if self.pos >= self.bytes.len() || matches!(self.bytes[self.pos], b'\n' | b'\r' | b'#')
            {
                self.skip_ws_and_comments();
                if self.pos >= self.bytes.len() || self.is_doc_boundary() {
                    self.nodes.push(TapeNode {
                        kind: TapeKind::Null,
                        offset: self.pos as u32,
                    });
                } else {
                    let next_indent = self.line_indent_at(self.pos);
                    if next_indent == map_indent && self.is_seq_dash_at(self.pos) {
                        self.parse_block_seq(next_indent)?;
                    } else if next_indent <= map_indent {
                        self.nodes.push(TapeNode {
                            kind: TapeKind::Null,
                            offset: self.pos as u32,
                        });
                    } else {
                        self.parse_block_node(map_indent as isize)?;
                    }
                }
            } else {
                self.parse_block_node(map_indent as isize)?;
            }
        }

        for merge_idx in pending_merges {
            self.apply_merge_map(merge_idx, &mut explicit_keys)?;
        }

        self.nodes.push(TapeNode {
            kind: TapeKind::MapEnd,
            offset: self.pos as u32,
        });
        let end_idx = self.nodes.len() as u32;
        if let TapeKind::MapStart {
            end_idx: ref mut slot,
        } = self.nodes[start_node_idx].kind
        {
            *slot = end_idx;
        }
        Ok(())
    }

    fn collect_merge_targets(&mut self, out: &mut Vec<u32>) -> Result<(), YamlError> {
        let resolve_one = |this: &mut Self| -> Result<u32, YamlError> {
            this.skip_horiz_ws();
            let offset = this.pos;
            if this.bytes.get(this.pos) != Some(&b'*') {
                return Err(YamlError::new(offset, "`<<` merge expects `*alias`"));
            }
            this.pos += 1;
            let start = this.pos;
            while this.pos < this.bytes.len()
                && !matches!(
                    this.bytes[this.pos],
                    b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}'
                )
            {
                this.pos += 1;
            }
            if start == this.pos {
                return Err(YamlError::new(offset, "empty merge alias"));
            }
            let name = &this.src[start..this.pos];
            this.anchors.get(name).copied().ok_or_else(|| {
                YamlError::from_kind(offset, YamlErrorKind::UnknownAlias(String::from(name)))
            })
        };

        if self.bytes.get(self.pos) == Some(&b'[') {
            self.pos += 1;
            loop {
                self.skip_ws_and_comments();
                if self.bytes.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    break;
                }
                out.push(resolve_one(self)?);
                self.skip_ws_and_comments();
                if self.bytes.get(self.pos) == Some(&b',') {
                    self.pos += 1;
                } else if self.bytes.get(self.pos) != Some(&b']') {
                    return Err(YamlError::new(
                        self.pos,
                        "expected `,` or `]` in `<<` merge list",
                    ));
                }
            }
        } else {
            out.push(resolve_one(self)?);
        }
        Ok(())
    }

    fn apply_merge_map(
        &mut self,
        target_idx: u32,
        seen_keys: &mut Vec<String>,
    ) -> Result<(), YamlError> {
        let start = target_idx as usize;
        let end = match self.nodes.get(start).map(|n| &n.kind) {
            Some(TapeKind::MapStart { end_idx }) => *end_idx as usize,
            _ => return Err(YamlError::new(self.pos, "merge target must be a mapping")),
        };

        let mut i = start + 1;
        while i + 1 < end {
            let key_node_idx = i;
            let key_str = self.extract_key_text(key_node_idx);
            let mut val_idx = i + 1;
            while matches!(
                self.nodes.get(val_idx).map(|n| &n.kind),
                Some(TapeKind::Tag { .. })
            ) {
                val_idx += 1;
            }
            let next_i = match self.nodes[val_idx].kind {
                TapeKind::MapStart { end_idx } | TapeKind::SeqStart { end_idx } => end_idx as usize,
                _ => val_idx + 1,
            };

            if !seen_keys.iter().any(|k| k == &key_str) {
                seen_keys.push(key_str);
                let key_node = self.nodes[key_node_idx].clone();
                self.nodes.push(key_node);
                self.nodes.push(TapeNode {
                    kind: TapeKind::Alias {
                        target_idx: (i + 1) as u32,
                    },
                    offset: self.nodes[i + 1].offset,
                });
            }
            i = next_i;
        }
        Ok(())
    }

    fn extract_key_text(&self, node_idx: usize) -> String {
        match self.nodes[node_idx].kind {
            TapeKind::PlainStr { start, end } | TapeKind::QuotedStr { start, end } => {
                String::from(&self.src[start as usize..end as usize])
            }
            TapeKind::OwnedStr(id) => self.owned_strings[id as usize].clone(),
            _ => String::new(),
        }
    }

    fn parse_block_seq(&mut self, seq_indent: usize) -> Result<(), YamlError> {
        let start_node_idx = self.nodes.len();
        self.nodes.push(TapeNode {
            kind: TapeKind::SeqStart { end_idx: 0 },
            offset: self.pos as u32,
        });

        loop {
            self.skip_ws_and_comments();
            if self.pos >= self.bytes.len() || self.is_doc_boundary() {
                break;
            }
            let indent = self.line_indent_at(self.pos);
            if indent != seq_indent || !self.is_seq_dash_at(self.pos) {
                break;
            }
            self.pos += 1;
            self.skip_horiz_ws();

            if self.pos >= self.bytes.len() || matches!(self.bytes[self.pos], b'\n' | b'\r' | b'#')
            {
                self.skip_ws_and_comments();
                if self.pos >= self.bytes.len() || self.is_doc_boundary() {
                    self.nodes.push(TapeNode {
                        kind: TapeKind::Null,
                        offset: self.pos as u32,
                    });
                } else {
                    let next_indent = self.line_indent_at(self.pos);
                    if next_indent <= seq_indent {
                        self.nodes.push(TapeNode {
                            kind: TapeKind::Null,
                            offset: self.pos as u32,
                        });
                    } else {
                        self.parse_block_node(seq_indent as isize)?;
                    }
                }
            } else {
                self.parse_block_node(seq_indent as isize)?;
            }
        }

        self.nodes.push(TapeNode {
            kind: TapeKind::SeqEnd,
            offset: self.pos as u32,
        });
        let end_idx = self.nodes.len() as u32;
        if let TapeKind::SeqStart {
            end_idx: ref mut slot,
        } = self.nodes[start_node_idx].kind
        {
            *slot = end_idx;
        }
        Ok(())
    }

    fn parse_flow_seq(&mut self) -> Result<(), YamlError> {
        self.enter_depth()?;
        let start_node_idx = self.nodes.len();
        self.nodes.push(TapeNode {
            kind: TapeKind::SeqStart { end_idx: 0 },
            offset: self.pos as u32,
        });
        self.pos += 1;

        loop {
            self.skip_ws_and_comments();
            if self.bytes.get(self.pos) == Some(&b']') {
                self.pos += 1;
                break;
            }
            self.parse_flow_node()?;
            self.skip_ws_and_comments();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => {
                    self.exit_depth();
                    return Err(YamlError::new(
                        self.pos,
                        "expected `,` or `]` in flow sequence",
                    ));
                }
            }
        }

        self.nodes.push(TapeNode {
            kind: TapeKind::SeqEnd,
            offset: self.pos as u32,
        });
        let end_idx = self.nodes.len() as u32;
        if let TapeKind::SeqStart {
            end_idx: ref mut slot,
        } = self.nodes[start_node_idx].kind
        {
            *slot = end_idx;
        }
        self.exit_depth();
        Ok(())
    }

    fn parse_flow_map(&mut self) -> Result<(), YamlError> {
        self.enter_depth()?;
        let start_node_idx = self.nodes.len();
        self.nodes.push(TapeNode {
            kind: TapeKind::MapStart { end_idx: 0 },
            offset: self.pos as u32,
        });
        self.pos += 1;

        loop {
            self.skip_ws_and_comments();
            if self.bytes.get(self.pos) == Some(&b'}') {
                self.pos += 1;
                break;
            }
            self.parse_map_key()?;
            self.skip_ws_and_comments();
            if self.bytes.get(self.pos) != Some(&b':') {
                self.exit_depth();
                return Err(YamlError::new(self.pos, "expected `:` in flow mapping"));
            }
            self.pos += 1;
            self.skip_ws_and_comments();
            if matches!(self.bytes.get(self.pos), Some(b',' | b'}')) {
                self.nodes.push(TapeNode {
                    kind: TapeKind::Null,
                    offset: self.pos as u32,
                });
            } else {
                self.parse_flow_node()?;
            }
            self.skip_ws_and_comments();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => {
                    self.exit_depth();
                    return Err(YamlError::new(
                        self.pos,
                        "expected `,` or `}` in flow mapping",
                    ));
                }
            }
        }

        self.nodes.push(TapeNode {
            kind: TapeKind::MapEnd,
            offset: self.pos as u32,
        });
        let end_idx = self.nodes.len() as u32;
        if let TapeKind::MapStart {
            end_idx: ref mut slot,
        } = self.nodes[start_node_idx].kind
        {
            *slot = end_idx;
        }
        self.exit_depth();
        Ok(())
    }

    fn parse_flow_node(&mut self) -> Result<(), YamlError> {
        let anchor = self.parse_node_properties()?;
        self.skip_ws_and_comments();
        let idx = self.nodes.len() as u32;
        if let Some(name) = anchor {
            self.anchors.insert(name, idx);
        }
        match self.bytes.get(self.pos) {
            Some(b'*') => self.parse_alias(),
            Some(b'[') => self.parse_flow_seq(),
            Some(b'{') => self.parse_flow_map(),
            _ => self.parse_plain_or_quoted_scalar(true),
        }
    }

    fn parse_map_key(&mut self) -> Result<(), YamlError> {
        if matches!(self.bytes.get(self.pos), Some(b'"' | b'\'')) {
            return self.parse_quoted_scalar();
        }
        let start = self.pos;
        let mut last_non_ws = start;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b':'
                && self.bytes.get(self.pos + 1).map_or(true, |&n| {
                    matches!(n, b' ' | b'\t' | b'\n' | b'\r' | b',' | b'}')
                })
            {
                break;
            }
            if b == b'\n' || b == b'\r' {
                break;
            }
            if b != b' ' && b != b'\t' {
                last_non_ws = self.pos + 1;
            }
            self.pos += 1;
        }
        self.nodes.push(TapeNode {
            kind: TapeKind::PlainStr {
                start: start as u32,
                end: last_non_ws as u32,
            },
            offset: start as u32,
        });
        Ok(())
    }

    fn parse_plain_or_quoted_scalar(&mut self, in_flow: bool) -> Result<(), YamlError> {
        if matches!(self.bytes.get(self.pos), Some(b'"' | b'\'')) {
            return self.parse_quoted_scalar();
        }

        let start = self.pos;
        while self.pos < self.bytes.len() {
            let (masks, consumed) = simd::scan_chunk(self.bytes, self.pos);
            let interesting = masks.newlines | masks.structural;
            if interesting != 0 {
                let first = interesting.trailing_zeros() as usize;
                let cand = self.pos + first;
                let ch = self.bytes[cand];
                let stop = matches!(ch, b'\n' | b'\r')
                    || (in_flow && matches!(ch, b',' | b']' | b'}'))
                    || (ch == b'#' && cand > start && matches!(self.bytes[cand - 1], b' ' | b'\t'));
                if stop {
                    self.pos = cand;
                    break;
                }
                self.pos = cand + 1;
            } else {
                self.pos += consumed;
            }
        }

        let trimmed = self.src[start..self.pos].trim_end_matches([' ', '\t']);
        let end = start + trimmed.len();
        self.nodes.push(TapeNode {
            kind: TapeKind::PlainStr {
                start: start as u32,
                end: end as u32,
            },
            offset: start as u32,
        });
        Ok(())
    }

    fn parse_block_scalar(&mut self, parent_indent: isize) -> Result<(), YamlError> {
        let offset = self.pos;
        let folded = self.bytes[self.pos] == b'>';
        self.pos += 1;

        #[derive(Clone, Copy)]
        enum Chomp {
            Clip,
            Strip,
            Keep,
        }
        let mut chomp = Chomp::Clip;
        let mut explicit_indent: Option<usize> = None;

        for _ in 0..2 {
            if self.pos >= self.bytes.len() {
                break;
            }
            match self.bytes[self.pos] {
                b'-' => {
                    chomp = Chomp::Strip;
                    self.pos += 1;
                }
                b'+' => {
                    chomp = Chomp::Keep;
                    self.pos += 1;
                }
                b'1'..=b'9' => {
                    explicit_indent = Some((self.bytes[self.pos] - b'0') as usize);
                    self.pos += 1;
                }
                _ => break,
            }
        }

        self.skip_horiz_ws();
        if self.bytes.get(self.pos) == Some(&b'#') {
            self.skip_to_eol_simd();
        }
        if self.pos < self.bytes.len() && !self.consume_newline() {
            return Err(YamlError::new(
                self.pos,
                "expected newline after block scalar header",
            ));
        }

        let base = parent_indent.max(0) as usize;
        let mut block_indent = explicit_indent.map(|d| base + d);
        let mut lines: Vec<(&'a str, bool)> = Vec::new();

        while self.pos < self.bytes.len() {
            let line_start = self.pos;
            let spaces = simd::count_spaces(self.bytes, line_start);
            self.pos += spaces;
            let after_spaces = self.pos;
            self.skip_to_eol_simd();
            let line_end = self.pos;
            let is_blank = after_spaces == line_end;

            if !is_blank {
                let indent = match block_indent {
                    Some(bi) => {
                        if spaces < bi {
                            self.pos = line_start;
                            break;
                        }
                        bi
                    }
                    None => {
                        if (spaces as isize) <= parent_indent {
                            self.pos = line_start;
                            break;
                        }
                        block_indent = Some(spaces);
                        spaces
                    }
                };
                lines.push((&self.src[line_start + indent..line_end], spaces > indent));
            } else {
                lines.push(("", false));
            }

            if !self.consume_newline() {
                break;
            }
        }

        let mut trailing_blanks = 0usize;
        while lines.last().is_some_and(|(s, _)| s.is_empty()) {
            lines.pop();
            trailing_blanks += 1;
        }

        let mut out = String::new();
        if !folded {
            for (idx, (line, _)) in lines.iter().enumerate() {
                if idx > 0 {
                    out.push('\n');
                }
                out.push_str(line);
            }
        } else {
            let mut prev_folded = false;
            for (idx, (line, more_indented)) in lines.iter().enumerate() {
                if line.is_empty() {
                    out.push('\n');
                    prev_folded = false;
                } else {
                    if idx > 0 {
                        if prev_folded && !more_indented {
                            out.push(' ');
                        } else {
                            out.push('\n');
                        }
                    }
                    out.push_str(line);
                    prev_folded = !more_indented;
                }
            }
        }

        match chomp {
            Chomp::Strip => {}
            Chomp::Clip => {
                if !lines.is_empty() {
                    out.push('\n');
                }
            }
            Chomp::Keep => {
                if !lines.is_empty() {
                    out.push('\n');
                }
                for _ in 0..trailing_blanks {
                    out.push('\n');
                }
            }
        }

        let id = self.owned_strings.len() as u32;
        self.owned_strings.push(out);
        self.nodes.push(TapeNode {
            kind: TapeKind::OwnedStr(id),
            offset: offset as u32,
        });
        Ok(())
    }

    fn parse_quoted_scalar(&mut self) -> Result<(), YamlError> {
        let quote = self.bytes[self.pos];
        let offset = self.pos;
        self.pos += 1;
        let start = self.pos;
        let mut has_escape = false;

        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if quote == b'"' {
                if b == b'\\' {
                    has_escape = true;
                    if self.pos + 1 >= self.bytes.len() {
                        break;
                    }
                    self.pos += 2;
                    continue;
                }
                if b == b'"' {
                    let end = self.pos;
                    self.pos += 1;
                    if !has_escape {
                        self.nodes.push(TapeNode {
                            kind: TapeKind::QuotedStr {
                                start: start as u32,
                                end: end as u32,
                            },
                            offset: offset as u32,
                        });
                    } else {
                        let s = unescape_double_quoted(&self.src[start..end])
                            .ok_or_else(|| YamlError::new(offset, "invalid escape sequence"))?;
                        let id = self.owned_strings.len() as u32;
                        self.owned_strings.push(s);
                        self.nodes.push(TapeNode {
                            kind: TapeKind::OwnedStr(id),
                            offset: offset as u32,
                        });
                    }
                    return Ok(());
                }
            } else if b == b'\'' {
                if self.bytes.get(self.pos + 1) == Some(&b'\'') {
                    has_escape = true;
                    self.pos += 2;
                    continue;
                }
                let end = self.pos;
                self.pos += 1;
                if !has_escape {
                    self.nodes.push(TapeNode {
                        kind: TapeKind::QuotedStr {
                            start: start as u32,
                            end: end as u32,
                        },
                        offset: offset as u32,
                    });
                } else {
                    let s = self.src[start..end].replace("''", "'");
                    let id = self.owned_strings.len() as u32;
                    self.owned_strings.push(s);
                    self.nodes.push(TapeNode {
                        kind: TapeKind::OwnedStr(id),
                        offset: offset as u32,
                    });
                }
                return Ok(());
            }
            self.pos += 1;
        }
        Err(YamlError::new(offset, "unterminated quoted string"))
    }
}

fn unescape_double_quoted(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '0' => out.push('\0'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'a' => out.push('\x07'),
            'v' => out.push('\x0b'),
            'e' => out.push('\x1b'),
            ' ' => out.push(' '),
            'x' => {
                let hex: String = [chars.next()?, chars.next()?].into_iter().collect();
                out.push(u8::from_str_radix(&hex, 16).ok()? as char);
            }
            'u' => {
                let hex: String = (0..4).map(|_| chars.next()).collect::<Option<_>>()?;
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            'U' => {
                let hex: String = (0..8).map(|_| chars.next()).collect::<Option<_>>()?;
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}
