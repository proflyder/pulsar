//! Minimal JSON reader and writer for the wake-event files: the sidecars, `stats.json` and the
//! model manifest. The workspace has no serde; the format is fixed by the wake-event contract v1,
//! so a small parser (objects, arrays, strings, numbers, booleans, null) is enough. Values are
//! kept in the order they appear. Numbers parse as `f64`; the callers convert to integers where
//! the contract fixes one.

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// The value of an object member; None for other values and missing keys.
    pub(crate) fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(v) => Some(*v),
            _ => None,
        }
    }

    pub(crate) fn as_u64(&self) -> Option<u64> {
        let v = self.as_f64()?;
        (v >= 0.0 && v.fract() == 0.0 && v <= u64::MAX as f64).then_some(v as u64)
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

/// Parses one JSON value (surrounding white space allowed); None when malformed.
pub(crate) fn parse(text: &str) -> Option<Json> {
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
    };
    let v = p.value()?;
    p.skip();
    (p.i == p.b.len()).then_some(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn skip(&mut self) {
        while self.b.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        self.skip();
        (self.b.get(self.i) == Some(&c)).then(|| self.i += 1)
    }

    fn value(&mut self) -> Option<Json> {
        self.skip();
        match *self.b.get(self.i)? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Json::Str),
            b't' => self.literal("true", Json::Bool(true)),
            b'f' => self.literal("false", Json::Bool(false)),
            b'n' => self.literal("null", Json::Null),
            _ => self.number(),
        }
    }

    fn literal(&mut self, word: &str, v: Json) -> Option<Json> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Some(v)
        } else {
            None
        }
    }

    fn object(&mut self) -> Option<Json> {
        self.eat(b'{')?;
        let mut fields = Vec::new();
        self.skip();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Some(Json::Obj(fields));
        }
        loop {
            self.skip();
            let key = self.string()?;
            self.eat(b':')?;
            let v = self.value()?;
            fields.push((key, v));
            self.skip();
            match self.b.get(self.i)? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Some(Json::Obj(fields));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<Json> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.skip();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Some(Json::Arr(items));
        }
        loop {
            items.push(self.value()?);
            self.skip();
            match self.b.get(self.i)? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(Json::Arr(items));
                }
                _ => return None,
            }
        }
    }

    /// A double-quoted string with the standard escapes; a lone surrogate is kept as U+FFFD
    /// (the files Pulsar writes never contain one).
    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            let c = *self.b.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let e = *self.b.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let code = self.hex4()?;
                            let ch = char::from_u32(code).unwrap_or('\u{fffd}');
                            out.push(ch);
                        }
                        _ => return None,
                    }
                }
                _ => {
                    // A multi-byte UTF-8 character: take its bytes as they are.
                    let start = self.i - 1;
                    let len = utf8_len(c);
                    if len == 0 || start + len > self.b.len() {
                        return None;
                    }
                    out.push_str(std::str::from_utf8(&self.b[start..start + len]).ok()?);
                    self.i = start + len;
                }
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let s = std::str::from_utf8(self.b.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(s, 16).ok()
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        while self
            .b
            .get(self.i)
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
        {
            self.i += 1;
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        s.parse::<f64>().ok().map(Json::Num)
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

/// The JSON string literal of `s`.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_manifest_and_sidecar_shapes() {
        let m = parse(r#"{"micro":{"probability_cutoff":0.87,"sliding_window_size":12}}"#).unwrap();
        assert_eq!(
            m.get("micro").and_then(|v| v.get("probability_cutoff")),
            Some(&Json::Num(0.87))
        );
        let s = parse(
            r#"{"schema":1,"id":"x","kind":"hit","utc":"2026-11-01T10:15:00.123Z",
               "score_peak":0.934,"cutoff":0.9,"window":10,"dry_run":false,
               "outcome":null,"stt_text":"привет","label":"false","note":"a \"b\"\\c"}"#,
        )
        .unwrap();
        assert_eq!(s.get("outcome"), Some(&Json::Null));
        assert_eq!(s.get("stt_text").and_then(Json::as_str), Some("привет"));
        assert_eq!(s.get("note").and_then(Json::as_str), Some("a \"b\"\\c"));
        assert_eq!(s.get("schema").and_then(Json::as_u64), Some(1));
        assert_eq!(s.get("dry_run").and_then(Json::as_bool), Some(false));
        assert_eq!(s.get("score_peak").and_then(Json::as_f64), Some(0.934));
        assert_eq!(s.get("window").and_then(Json::as_u64), Some(10));
        assert_eq!(s.get("missing"), None);
    }

    #[test]
    fn rejects_malformed_text() {
        for text in ["", "{", "{\"a\":}", "[1,]", "{\"a\" 1}", "tru"] {
            assert!(parse(text).is_none(), "{text}");
        }
        assert_eq!(
            parse("  [1, 2.5, \"x\"]  ").unwrap(),
            Json::Arr(vec![Json::Num(1.0), Json::Num(2.5), Json::Str("x".into()),])
        );
    }

    #[test]
    fn escapes_round_trip() {
        for s in ["", "привет", "a\"b", "back\\slash", "line\nbreak", "\u{1}"] {
            let text = format!("{{\"v\":{}}}", escape(s));
            assert_eq!(
                parse(&text).unwrap().get("v").and_then(Json::as_str),
                Some(s)
            );
        }
    }
}
