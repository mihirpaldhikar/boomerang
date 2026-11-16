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
use std::iter::Peekable;
use std::vec::IntoIter;

#[derive(Default, Clone, Copy)]
pub struct CheckOpts {
    pub allow_regexp: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Quoted(String),
    Str(String),
    Num(String),
    Op(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Cast,
}

const OP_CHARS: &str = "+-*/<>=~!@#%^&|?";
const TYPE_CONT: &[&str] = &["varying", "precision", "with", "without", "time", "zone"];

const FUNCS: &[(&str, &str)] = &[
    ("abs", "abs"),
    ("coalesce", "coalesce"),
    ("nullif", "nullif"),
    ("lower", "lower"),
    ("upper", "upper"),
    ("length", "length"),
    ("char_length", "length"),
    ("character_length", "length"),
    ("btrim", "trim"),
    ("trim", "trim"),
    ("ltrim", "ltrim"),
    ("rtrim", "rtrim"),
    ("replace", "replace"),
    ("round", "round"),
    ("substr", "substr"),
    ("substring", "substr"),
    ("greatest", "max"),
    ("least", "min"),
];

pub fn translate_check(def: &str, value_ident: Option<&str>, opts: &CheckOpts) -> Option<String> {
    let mut d = def.trim();
    loop {
        let before = d;
        for suf in [" NOT VALID", " NO INHERIT"] {
            if let Some(s) = d.strip_suffix(suf) {
                d = s.trim_end();
            }
        }
        if d == before {
            break;
        }
    }
    let body = d.strip_prefix("CHECK").unwrap_or(d).trim();
    if body.is_empty() {
        return None;
    }

    let mut toks = strip_casts(tokenize(body)?)?;
    if let Some(v) = value_ident {
        for t in &mut toks {
            if matches!(t, Tok::Word(w) if w.eq_ignore_ascii_case("value")) {
                *t = Tok::Quoted(v.to_string());
            }
        }
    }
    let rendered = render(&toks, opts)?;
    Some(if fully_wrapped(&toks) {
        rendered
    } else {
        format!("({rendered})")
    })
}

fn fully_wrapped(t: &[Tok]) -> bool {
    if t.first() != Some(&Tok::LParen) {
        return false;
    }
    let mut depth = 0;
    for (i, x) in t.iter().enumerate() {
        match x {
            Tok::LParen => depth += 1,
            Tok::RParen => {
                depth -= 1;
                if depth == 0 {
                    return i == t.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

fn tokenize(s: &str) -> Option<Vec<Tok>> {
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        match ch {
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            ']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            '.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            '\'' | '"' => {
                let q = ch;
                let start = i;
                i += 1;
                loop {
                    let x = *c.get(i)?;
                    i += 1;
                    if x == q {
                        if c.get(i) == Some(&q) {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
                let text: String = c[start..i].iter().collect();
                out.push(if q == '\'' {
                    Tok::Str(text)
                } else {
                    Tok::Quoted(text)
                });
            }
            ':' => {
                if c.get(i + 1) == Some(&':') {
                    out.push(Tok::Cast);
                    i += 2;
                } else {
                    return None;
                }
            }
            _ if ch.is_ascii_digit() => {
                let start = i;
                while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.') {
                    i += 1;
                }
                if matches!(c.get(i), Some(&'e') | Some(&'E')) {
                    let mut j = i + 1;
                    if matches!(c.get(j), Some(&'+') | Some(&'-')) {
                        j += 1;
                    }
                    if c.get(j).is_some_and(|d| d.is_ascii_digit()) {
                        i = j;
                        while i < c.len() && c[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                out.push(Tok::Num(c[start..i].iter().collect()));
            }
            _ if ch.is_alphabetic() || ch == '_' => {
                let start = i;
                while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$') {
                    i += 1;
                }
                let w: String = c[start..i].iter().collect();
                if w.len() == 1 && "eEbBxXnN".contains(&w) && c.get(i) == Some(&'\'') {
                    return None;
                }
                out.push(Tok::Word(w));
            }
            _ if OP_CHARS.contains(ch) => {
                let start = i;
                while i < c.len() && OP_CHARS.contains(c[i]) {
                    i += 1;
                }
                out.push(Tok::Op(c[start..i].iter().collect()));
            }
            _ => return None,
        }
    }
    Some(out)
}

type TokIter = Peekable<IntoIter<Tok>>;

fn skip_group(it: &mut TokIter) -> Option<()> {
    let mut depth = 0;
    loop {
        match it.next()? {
            Tok::LParen => depth += 1,
            Tok::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Some(());
                }
            }
            _ => {}
        }
    }
}

fn strip_casts(toks: Vec<Tok>) -> Option<Vec<Tok>> {
    let mut out = Vec::with_capacity(toks.len());
    let mut it: TokIter = toks.into_iter().peekable();
    while let Some(t) = it.next() {
        if t != Tok::Cast {
            out.push(t);
            continue;
        }
        if !matches!(it.next()?, Tok::Word(_) | Tok::Quoted(_)) {
            return None;
        }
        loop {
            let step = match it.peek() {
                Some(Tok::Dot) => 1,
                Some(Tok::Word(w)) if TYPE_CONT.contains(&w.to_ascii_lowercase().as_str()) => 2,
                Some(Tok::LParen) => 3,
                _ => 0,
            };
            match step {
                1 => {
                    it.next();
                    if !matches!(it.next()?, Tok::Word(_) | Tok::Quoted(_)) {
                        return None;
                    }
                }
                2 => {
                    it.next();
                }
                3 => skip_group(&mut it)?,
                _ => break,
            }
        }
        while it.peek() == Some(&Tok::LBracket) {
            it.next();
            if it.next()? != Tok::RBracket {
                return None;
            }
        }
    }
    Some(out)
}

#[derive(Default)]
struct Out {
    s: String,
    glue: bool,
}
impl Out {
    fn tok(&mut self, t: &str) {
        let tight = matches!(t, ")" | ",");
        if !self.s.is_empty() && !self.glue && !tight {
            self.s.push(' ');
        }
        self.s.push_str(t);
        self.glue = t == "(";
    }
    fn call_open(&mut self) {
        self.s.push('(');
        self.glue = true;
    }
}

fn is_word(t: Option<&Tok>, kw: &str) -> bool {
    matches!(t, Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw))
}

fn parse_array_arg(toks: &[Tok], start: usize) -> Option<(Vec<&[Tok]>, usize)> {
    let mut j = start;
    let mut depth = 0;
    while matches!(toks.get(j), Some(Tok::LParen)) {
        depth += 1;
        j += 1;
    }
    if depth == 0 || !is_word(toks.get(j), "ARRAY") {
        return None;
    }
    j += 1;
    if toks.get(j) != Some(&Tok::LBracket) {
        return None;
    }
    j += 1;

    let mut elems = Vec::new();
    let mut cur = j;
    let mut d = 0i32;
    loop {
        match toks.get(j)? {
            Tok::LParen => d += 1,
            Tok::RParen => d -= 1,
            Tok::LBracket => return None,
            Tok::RBracket if d == 0 => {
                if cur < j {
                    elems.push(&toks[cur..j]);
                }
                j += 1;
                break;
            }
            Tok::Comma if d == 0 => {
                elems.push(&toks[cur..j]);
                cur = j + 1;
            }
            _ => {}
        }
        j += 1;
    }
    for _ in 0..depth {
        if toks.get(j) != Some(&Tok::RParen) {
            return None;
        }
        j += 1;
    }
    Some((elems, j))
}

fn quantified(op: &str, quant: &str, items: &[String]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let all = quant.eq_ignore_ascii_case("ALL");
    let list = items.join(", ");
    if items.len() == 1 && matches!(op, "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=") {
        return Some(format!("{op} {list}"));
    }
    match (op, all) {
        ("=", false) => Some(format!("IN ({list})")),
        ("<>" | "!=", true) => Some(format!("NOT IN ({list})")),
        ("<" | "<=", false) | (">" | ">=", true) => Some(format!("{op} max({list})")),
        (">" | ">=", false) | ("<" | "<=", true) => Some(format!("{op} min({list})")),
        _ => None,
    }
}

fn render(toks: &[Tok], opts: &CheckOpts) -> Option<String> {
    let mut out = Out::default();
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::LParen => out.tok("("),
            Tok::RParen => out.tok(")"),
            Tok::Comma => out.tok(","),
            Tok::Num(s) | Tok::Str(s) | Tok::Quoted(s) => out.tok(s),
            Tok::LBracket | Tok::RBracket | Tok::Dot | Tok::Cast => return None,

            Tok::Op(op) => {
                if let Some(Tok::Word(q)) = toks.get(i + 1)
                    && matches!(q.to_ascii_uppercase().as_str(), "ANY" | "ALL" | "SOME")
                {
                    let (elems, next) = parse_array_arg(toks, i + 2)?;
                    let parts = elems
                        .iter()
                        .map(|e| render(e, opts))
                        .collect::<Option<Vec<_>>>()?;
                    out.tok(&quantified(op, q, &parts)?);
                    i = next;
                    continue;
                }
                match op.as_str() {
                    "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=" | "+" | "-" | "*" | "/" | "%"
                    | "||" | "&" | "|" | "<<" | ">>" => out.tok(op),
                    "~~" | "~~*" => out.tok("LIKE"),
                    "!~~" | "!~~*" => out.tok("NOT LIKE"),
                    "~" | "~*" | "!~" | "!~*" => {
                        if !opts.allow_regexp {
                            return None;
                        }
                        out.tok(if op.starts_with('!') {
                            "NOT REGEXP"
                        } else {
                            "REGEXP"
                        });
                        if op.ends_with('*') {
                            let Some(Tok::Str(p)) = toks.get(i + 1) else {
                                return None;
                            };
                            out.tok(&format!("'(?i){}", &p[1..]));
                            i += 1;
                        }
                    }
                    _ => return None,
                }
            }

            Tok::Word(w) => {
                let up = w.to_ascii_uppercase();
                let is_call = matches!(toks.get(i + 1), Some(Tok::LParen));
                match up.as_str() {
                    "IS" => {
                        let k = if is_word(toks.get(i + 1), "NOT") {
                            i + 2
                        } else {
                            i + 1
                        };
                        if is_word(toks.get(k), "DISTINCT") && is_word(toks.get(k + 1), "FROM") {
                            out.tok(if k == i + 2 { "IS" } else { "IS NOT" });
                            i = k + 2;
                            continue;
                        }
                        out.tok("IS");
                    }
                    "ILIKE" => out.tok("LIKE"),
                    "AND" | "OR" | "NOT" | "IN" | "NULL" | "TRUE" | "FALSE" | "BETWEEN"
                    | "LIKE" | "ESCAPE" | "CASE" | "WHEN" | "THEN" | "ELSE" | "END" => out.tok(&up),
                    _ if is_call => {
                        let &(_, name) = FUNCS.iter().find(|(pg, _)| pg.eq_ignore_ascii_case(w))?;
                        out.tok(name);
                        out.call_open();
                        i += 2;
                        continue;
                    }
                    "FROM" | "FOR" | "SIMILAR" | "ARRAY" | "DISTINCT" | "ANY" | "ALL" | "SOME" => {
                        return None;
                    }
                    _ => out.tok(w),
                }
            }
        }
        i += 1;
    }
    Some(out.s)
}
