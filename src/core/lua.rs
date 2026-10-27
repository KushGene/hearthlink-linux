//! A small reader for the Lua that World of Warcraft writes into SavedVariables files.
//!
//! WoW only ever writes plain assignments of tables, strings, numbers and booleans, but
//! addons and players can leave odd things behind, so the reader is strict about syntax
//! and reports where it stopped.

use serde_json::{Map, Number, Value};
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Lua {
    Nil,
    Bool(bool),
    Num(f64),
    Str(String),
    Table(Table),
}

/// A Lua table, keeping positional entries apart from keyed ones (that's how
/// WoW writes arrays: `"x", -- [1]`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub positional: Vec<Lua>,
    pub keyed: Vec<(Lua, Lua)>,
}

impl Table {
    pub fn get(&self, key: &str) -> Option<&Lua> {
        self.keyed.iter().rev().find_map(|(k, v)| match k {
            Lua::Str(s) if s == key => Some(v),
            _ => None,
        })
    }
}

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (line {})", self.message, self.line)
    }
}

impl std::error::Error for ParseError {}

const MAX_DEPTH: usize = 200;

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, msg: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError { message: msg.into(), line: self.line })
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.src.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn skip_ws(&mut self) -> Result<(), ParseError> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c) => {
                    self.bump();
                }
                Some(b'-') if self.peek_at(1) == Some(b'-') => {
                    self.pos += 2;
                    if self.peek() == Some(b'[') {
                        if let Some(level) = self.long_bracket_level() {
                            self.long_string(level)?;
                            continue;
                        }
                    }
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                // A UTF-8 byte order mark at the start of the file.
                Some(0xEF) if self.pos == 0 && self.src.starts_with(&[0xEF, 0xBB, 0xBF]) => {
                    self.pos += 3;
                }
                _ => return Ok(()),
            }
        }
    }

    /// At `[`, returns the level of a long bracket `[==[` without consuming it.
    fn long_bracket_level(&self) -> Option<usize> {
        let mut i = self.pos + 1;
        let mut level = 0;
        while self.src.get(i) == Some(&b'=') {
            level += 1;
            i += 1;
        }
        (self.src.get(i) == Some(&b'[')).then_some(level)
    }

    fn long_string(&mut self, level: usize) -> Result<String, ParseError> {
        self.pos += level + 2;
        // A newline right after the opening bracket is skipped.
        if self.peek() == Some(b'\r') {
            self.bump();
        }
        if self.peek() == Some(b'\n') {
            self.bump();
        }
        let start = self.pos;
        loop {
            match self.peek() {
                None => return self.err("the file ends inside a long string"),
                Some(b']') => {
                    let mut i = self.pos + 1;
                    let mut l = 0;
                    while self.src.get(i) == Some(&b'=') {
                        l += 1;
                        i += 1;
                    }
                    if l == level && self.src.get(i) == Some(&b']') {
                        let s = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();
                        self.pos = i + 1;
                        return Ok(s);
                    }
                    self.bump();
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    fn string(&mut self, quote: u8) -> Result<String, ParseError> {
        self.bump();
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.bump() else {
                return self.err("the file ends inside a string");
            };
            match c {
                c if c == quote => break,
                b'\n' => return self.err("a string runs past the end of its line"),
                b'\\' => {
                    let Some(e) = self.bump() else {
                        return self.err("the file ends inside a string");
                    };
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'a' => out.push(7),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'v' => out.push(11),
                        b'\\' => out.push(b'\\'),
                        b'"' => out.push(b'"'),
                        b'\'' => out.push(b'\''),
                        b'\n' => out.push(b'\n'),
                        b'\r' => {
                            if self.peek() == Some(b'\n') {
                                self.bump();
                            }
                            out.push(b'\n');
                        }
                        b'x' => {
                            let h = self.src.get(self.pos..self.pos + 2).unwrap_or_default();
                            let v = std::str::from_utf8(h).ok().and_then(|s| u8::from_str_radix(s, 16).ok());
                            match v {
                                Some(v) if h.len() == 2 => {
                                    out.push(v);
                                    self.pos += 2;
                                }
                                _ => return self.err("bad \\x escape in a string"),
                            }
                        }
                        b'z' => {
                            while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c)) {
                                self.bump();
                            }
                        }
                        b'u' => {
                            if self.bump() != Some(b'{') {
                                return self.err("bad \\u escape in a string");
                            }
                            let start = self.pos;
                            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                                self.pos += 1;
                            }
                            let hex = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
                            if self.bump() != Some(b'}') || hex.is_empty() {
                                return self.err("bad \\u escape in a string");
                            }
                            let cp = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
                            match cp {
                                Some(ch) => {
                                    let mut buf = [0u8; 4];
                                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                                }
                                None => return self.err("bad \\u escape"),
                            }
                        }
                        d if d.is_ascii_digit() => {
                            let mut v: u32 = (d - b'0') as u32;
                            for _ in 0..2 {
                                match self.peek() {
                                    Some(n) if n.is_ascii_digit() => {
                                        v = v * 10 + (n - b'0') as u32;
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            if v > 255 {
                                return self.err("bad decimal escape in a string");
                            }
                            out.push(v as u8);
                        }
                        other => {
                            return self.err(format!("bad escape \\{} in a string", other as char));
                        }
                    }
                }
                other => out.push(other),
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn number(&mut self) -> Result<f64, ParseError> {
        let start = self.pos;
        let neg = if self.peek() == Some(b'-') {
            self.pos += 1;
            true
        } else {
            false
        };
        if self.peek() == Some(b'0') && matches!(self.peek_at(1), Some(b'x' | b'X')) {
            self.pos += 2;
            let hs = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.pos += 1;
            }
            let hex = std::str::from_utf8(&self.src[hs..self.pos]).unwrap_or("");
            let Ok(v) = u64::from_str_radix(hex, 16) else {
                return self.err("bad hexadecimal number");
            };
            let v = v as f64;
            return Ok(if neg { -v } else { v });
        }
        while let Some(c) = self.peek() {
            let exp_sign = matches!(c, b'+' | b'-')
                && matches!(self.src.get(self.pos.wrapping_sub(1)), Some(b'e' | b'E'));
            if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || exp_sign {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
        // WoW writes infinities and NaN as words when a value overflowed.
        if text == "-" {
            for (word, v) in [("inf", f64::NEG_INFINITY), ("nan", f64::NAN)] {
                if self.src[self.pos..].starts_with(word.as_bytes()) {
                    self.pos += word.len();
                    return Ok(v);
                }
            }
        }
        text.parse::<f64>().or_else(|_| self.err("bad number"))
    }

    fn name(&mut self) -> Option<String> {
        let c = self.peek()?;
        if !(c.is_ascii_alphabetic() || c == b'_') {
            return None;
        }
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
            self.pos += 1;
        }
        Some(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned())
    }

    fn expect(&mut self, c: u8) -> Result<(), ParseError> {
        self.skip_ws()?;
        if self.peek() == Some(c) {
            self.bump();
            Ok(())
        } else {
            self.err(format!("expected '{}'", c as char))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Lua, ParseError> {
        if depth > MAX_DEPTH {
            return self.err("tables are nested too deeply");
        }
        self.skip_ws()?;
        match self.peek() {
            None => self.err("the file ends in the middle of a value"),
            Some(b'{') => self.table(depth + 1).map(Lua::Table),
            Some(q @ (b'"' | b'\'')) => self.string(q).map(Lua::Str),
            Some(b'[') => match self.long_bracket_level() {
                Some(level) => self.long_string(level).map(Lua::Str),
                None => self.err("bad long string"),
            },
            Some(c) if c.is_ascii_digit() || c == b'-' || c == b'.' => self.number().map(Lua::Num),
            Some(_) => match self.name().as_deref() {
                Some("true") => Ok(Lua::Bool(true)),
                Some("false") => Ok(Lua::Bool(false)),
                Some("nil") => Ok(Lua::Nil),
                Some("inf") => Ok(Lua::Num(f64::INFINITY)),
                Some("nan") => Ok(Lua::Num(f64::NAN)),
                _ => self.err(format!("unexpected character '{}'", self.peek().unwrap_or(b'?') as char)),
            },
        }
    }

    fn table(&mut self, depth: usize) -> Result<Table, ParseError> {
        self.bump();
        let mut t = Table::default();
        loop {
            self.skip_ws()?;
            match self.peek() {
                None => return self.err("the file ends inside a table"),
                Some(b'}') => {
                    self.bump();
                    return Ok(t);
                }
                Some(b'[') if self.long_bracket_level().is_none() => {
                    self.bump();
                    let key = self.value(depth)?;
                    self.expect(b']')?;
                    self.expect(b'=')?;
                    let val = self.value(depth)?;
                    t.keyed.push((key, val));
                }
                Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                    let save = (self.pos, self.line);
                    let name = self.name().unwrap_or_default();
                    self.skip_ws()?;
                    if self.peek() == Some(b'=') {
                        self.bump();
                        let val = self.value(depth)?;
                        t.keyed.push((Lua::Str(name), val));
                    } else {
                        (self.pos, self.line) = save;
                        let val = self.value(depth)?;
                        t.positional.push(val);
                    }
                }
                Some(_) => {
                    let val = self.value(depth)?;
                    t.positional.push(val);
                }
            }
            self.skip_ws()?;
            match self.peek() {
                Some(b',' | b';') => {
                    self.bump();
                }
                Some(b'}') => {}
                None => return self.err("the file ends inside a table"),
                _ => return self.err("expected ',' or '}'"),
            }
        }
    }
}

/// Reads every `name = value` assignment in a SavedVariables file. Parsing stops at the
/// first error; the globals read before it are kept and the error is returned alongside.
#[cfg_attr(not(test), allow(dead_code))]
pub fn read_globals(src: &[u8]) -> (Vec<(String, Lua)>, Option<ParseError>) {
    let (g, e) = read_globals_inner(src);
    (g, e.map(|(_, e)| e))
}

/// Like [`read_globals`], but an error says which global it happened in (if any).
fn read_globals_inner(src: &[u8]) -> (Vec<(String, Lua)>, Option<(Option<String>, ParseError)>) {
    let mut p = Parser { src, pos: 0, line: 1 };
    let mut out = Vec::new();
    loop {
        if let Err(e) = p.skip_ws() {
            return (out, Some((None, e)));
        }
        if p.peek().is_none() {
            return (out, None);
        }
        let Some(name) = p.name() else {
            return (out, Some((None, ParseError { message: "expected a variable name".into(), line: p.line })));
        };
        if let Err(e) = p.expect(b'=') {
            return (out, Some((Some(name), e)));
        }
        match p.value(0) {
            Ok(v) => out.push((name, v)),
            Err(e) => return (out, Some((Some(name), e))),
        }
        let _ = p.skip_ws();
        if p.peek() == Some(b';') {
            p.bump();
        }
    }
}

/// Reads one global out of a SavedVariables file. A damaged later global doesn't matter;
/// a damaged or cut-off value of the global itself is an error.
pub fn read_global(src: &[u8], name: &str) -> Result<Option<Lua>, ParseError> {
    let (globals, err) = read_globals_inner(src);
    if let Some((_, v)) = globals.into_iter().rev().find(|(n, _)| n == name) {
        return Ok(Some(v));
    }
    match err {
        Some((Some(n), e)) if n == name => Err(e),
        Some((None, e)) => Err(e),
        _ => Ok(None),
    }
}

fn number_json(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e15 {
        Value::Number(Number::from(v as i64))
    } else {
        Number::from_f64(v).map(Value::Number).unwrap_or(Value::Null)
    }
}

fn key_string(k: &Lua) -> Option<String> {
    match k {
        Lua::Str(s) => Some(s.clone()),
        Lua::Num(n) => match number_json(*n) {
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        },
        Lua::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Converts to JSON the way the ForeverDB uploader does: a table with only positional
/// entries is an array (nil holes become null), anything else is an object with string
/// keys, and an empty table is an empty object.
pub fn to_json(v: &Lua) -> Value {
    match v {
        Lua::Nil => Value::Null,
        Lua::Bool(b) => Value::Bool(*b),
        Lua::Num(n) => number_json(*n),
        Lua::Str(s) => Value::String(s.clone()),
        Lua::Table(t) => {
            if t.keyed.is_empty() && !t.positional.is_empty() {
                Value::Array(t.positional.iter().map(to_json).collect())
            } else {
                // Sorted keys, independent of serde_json's `preserve_order`.
                let mut m = std::collections::BTreeMap::new();
                for (i, v) in t.positional.iter().enumerate() {
                    m.insert((i + 1).to_string(), to_json(v));
                }
                for (k, v) in &t.keyed {
                    if matches!(v, Lua::Nil) {
                        continue;
                    }
                    if let Some(k) = key_string(k) {
                        m.insert(k, to_json(v));
                    }
                }
                Value::Object(m.into_iter().collect::<Map<_, _>>())
            }
        }
    }
}

/// Escapes a string for writing it into a Lua file the addon will load.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(src: &str) -> String {
        let (g, e) = read_globals(format!("X = {src}").as_bytes());
        assert!(e.is_none(), "{e:?}");
        serde_json::to_string(&to_json(&g[0].1)).unwrap()
    }

    // These mirror the ForeverDB uploader's own self-test cases.
    #[test]
    fn foreverdb_reader_cases() {
        assert_eq!(j(r#"{ ["a"] = 1, b = "x\"y", [3] = true, }"#), r#"{"3":true,"a":1,"b":"x\"y"}"#);
        assert_eq!(j("{\n\"x\", -- [1]\n\"y\", -- [2]\n}"), r#"["x","y"]"#);
        assert_eq!(j("{}"), "{}");
        assert_eq!(j(r#"{ 1, 2, ["k"] = 3 }"#), r#"{"1":1,"2":2,"k":3}"#);
        assert_eq!(j(r#""caf\195\169""#), "\"café\"");
        assert_eq!(j("{ -1.5e-3, -42, 0.25, 1790443596 }"), "[-0.0015,-42,0.25,1790443596]");
        assert_eq!(j(r#"{ [1.0] = "a", [2] = "b" }"#), r#"{"1":"a","2":"b"}"#);
        assert_eq!(j("{ nil, 2 }"), "[null,2]");
        assert_eq!(
            j(r#"{ ["q"] = { ["lines"] = { "A", "B" }, ["n"] = { } } }"#),
            r#"{"q":{"lines":["A","B"],"n":{}}}"#
        );
        assert_eq!(j("--[[ block\ncomment ]] { x = 1, -- line\n y = false }"), r#"{"x":1,"y":false}"#);
        assert_eq!(j(r#"{ 0x1F, "\x41\u{00e9}" }"#), r#"[31,"Aé"]"#);
        assert_eq!(j("'it\\'s \\z\n   fine'"), "\"it's fine\"");
    }

    #[test]
    fn picks_one_global() {
        let src = "ForeverDB_DB = {\n[\"loot\"] = {\n[4888] = { [\"drops\"] = 3 },\n},\n}\nOther_DB = { 1, 2 }\n";
        let v = read_global(src.as_bytes(), "ForeverDB_DB").unwrap().unwrap();
        assert_eq!(serde_json::to_string(&to_json(&v)).unwrap(), r#"{"loot":{"4888":{"drops":3}}}"#);
        let damaged = "ForeverDB_DB = { [\"a\"] = 1 }\nBroken = {\n";
        assert!(read_global(damaged.as_bytes(), "ForeverDB_DB").unwrap().is_some());
        assert!(read_global(b"ForeverDB_DB = { [\"a\"] = ", "ForeverDB_DB").is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("a\"b\\c\nd\u{1}"), r#""a\"b\\c\nd\001""#);
    }
}
