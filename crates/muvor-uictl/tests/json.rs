//! A JSON subset reader, test-only.
//!
//! `vectors.json` is the oracle for the wire layer, so it has to be parsed —
//! but adding `serde_json` to reach it would put a dependency in the crate
//! that encodes frames for a broker holding `/dev/uinput`. 120 lines of
//! parser is the cheaper of the two.
#![allow(dead_code)]

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    pub fn get(&self, k: &str) -> Option<&Json> {
        match self { Self::Obj(m) => m.get(k), _ => None }
    }
    pub fn arr(&self) -> &[Json] {
        match self { Self::Arr(v) => v, _ => &[] }
    }
    pub fn str(&self) -> &str {
        match self { Self::Str(s) => s, _ => "" }
    }
    pub fn num(&self) -> f64 {
        match self { Self::Num(n) => *n, _ => f64::NAN }
    }
    pub fn usize(&self) -> usize { self.num() as usize }
}

pub fn parse(s: &str) -> Result<Json, String> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let v = value(b, &mut i)?;
    ws(b, &mut i);
    if i != b.len() { return Err(format!("trailing input at byte {i}")); }
    Ok(v)
}

fn ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') { *i += 1; }
}

fn value(b: &[u8], i: &mut usize) -> Result<Json, String> {
    ws(b, i);
    match b.get(*i) {
        None => Err("unexpected end".into()),
        Some(b'{') => object(b, i),
        Some(b'[') => array(b, i),
        Some(b'"') => Ok(Json::Str(string(b, i)?)),
        Some(b't') => lit(b, i, "true", Json::Bool(true)),
        Some(b'f') => lit(b, i, "false", Json::Bool(false)),
        Some(b'n') => lit(b, i, "null", Json::Null),
        _ => number(b, i),
    }
}

fn lit(b: &[u8], i: &mut usize, w: &str, v: Json) -> Result<Json, String> {
    if b[*i..].starts_with(w.as_bytes()) { *i += w.len(); Ok(v) }
    else { Err(format!("bad literal at {i}")) }
}

fn number(b: &[u8], i: &mut usize) -> Result<Json, String> {
    let st = *i;
    while *i < b.len()
        && matches!(b[*i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') { *i += 1; }
    std::str::from_utf8(&b[st..*i]).ok()
        .and_then(|s| s.parse().ok())
        .map(Json::Num)
        .ok_or_else(|| format!("bad number at {st}"))
}

fn string(b: &[u8], i: &mut usize) -> Result<String, String> {
    *i += 1; // opening quote
    let mut out = String::new();
    while *i < b.len() {
        match b[*i] {
            b'"' => { *i += 1; return Ok(out); }
            b'\\' => {
                *i += 1;
                let e = *b.get(*i).ok_or("escape at end")?;
                *i += 1;
                match e {
                    b'"' => out.push('"'), b'\\' => out.push('\\'),
                    b'/' => out.push('/'), b'n' => out.push('\n'),
                    b't' => out.push('\t'), b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'), b'f' => out.push('\u{c}'),
                    b'u' => {
                        let h = std::str::from_utf8(&b[*i..*i + 4]).map_err(|e| e.to_string())?;
                        let cp = u32::from_str_radix(h, 16).map_err(|e| e.to_string())?;
                        *i += 4;
                        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                    }
                    _ => return Err(format!("bad escape \\{}", e as char)),
                }
            }
            _ => {
                let st = *i;
                while *i < b.len() && b[*i] != b'"' && b[*i] != b'\\' { *i += 1; }
                out.push_str(std::str::from_utf8(&b[st..*i]).map_err(|e| e.to_string())?);
            }
        }
    }
    Err("unterminated string".into())
}

fn array(b: &[u8], i: &mut usize) -> Result<Json, String> {
    *i += 1;
    let mut v = Vec::new();
    ws(b, i);
    if b.get(*i) == Some(&b']') { *i += 1; return Ok(Json::Arr(v)); }
    loop {
        v.push(value(b, i)?);
        ws(b, i);
        match b.get(*i) {
            Some(b',') => { *i += 1; }
            Some(b']') => { *i += 1; return Ok(Json::Arr(v)); }
            _ => return Err(format!("bad array at {i}")),
        }
    }
}

fn object(b: &[u8], i: &mut usize) -> Result<Json, String> {
    *i += 1;
    let mut m = BTreeMap::new();
    ws(b, i);
    if b.get(*i) == Some(&b'}') { *i += 1; return Ok(Json::Obj(m)); }
    loop {
        ws(b, i);
        let k = string(b, i)?;
        ws(b, i);
        if b.get(*i) != Some(&b':') { return Err(format!("expected ':' at {i}")); }
        *i += 1;
        m.insert(k, value(b, i)?);
        ws(b, i);
        match b.get(*i) {
            Some(b',') => { *i += 1; }
            Some(b'}') => { *i += 1; return Ok(Json::Obj(m)); }
            _ => return Err(format!("bad object at {i}")),
        }
    }
}

pub fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("bad hex"))
        .collect()
}
