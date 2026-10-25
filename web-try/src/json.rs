//! A small JSON writer, so results cross the ABI as text without serde.
//! Floats are written as Rust prints them (shortest round-trip digits) with
//! `.0` on whole numbers, as serde_json writes them; non-finite floats are
//! `null`, as in serde_json. Strings escape exactly what serde_json escapes.
use std::fmt::Write as _;

#[derive(Default)]
pub struct Json {
    out: String,
    /// Whether the innermost open container already holds a value.
    filled: Vec<bool>,
    /// A key was just written; its value follows without a comma.
    after_key: bool,
}

impl Json {
    fn separate(&mut self) {
        if self.after_key {
            self.after_key = false;
            return;
        }
        if let Some(filled) = self.filled.last_mut() {
            if *filled {
                self.out.push(',');
            }
            *filled = true;
        }
    }

    pub fn begin_object(&mut self) {
        self.separate();
        self.out.push('{');
        self.filled.push(false);
    }

    pub fn end_object(&mut self) {
        self.filled.pop();
        self.out.push('}');
    }

    pub fn begin_array(&mut self) {
        self.separate();
        self.out.push('[');
        self.filled.push(false);
    }

    pub fn end_array(&mut self) {
        self.filled.pop();
        self.out.push(']');
    }

    pub fn key(&mut self, key: &str) {
        self.separate();
        push_string(&mut self.out, key);
        self.out.push(':');
        self.after_key = true;
    }

    pub fn null(&mut self) {
        self.separate();
        self.out.push_str("null");
    }

    pub fn bool(&mut self, value: bool) {
        self.separate();
        self.out.push_str(if value { "true" } else { "false" });
    }

    pub fn u64(&mut self, value: u64) {
        self.separate();
        let _ = write!(self.out, "{value}");
    }

    pub fn i64(&mut self, value: i64) {
        self.separate();
        let _ = write!(self.out, "{value}");
    }

    pub fn f64(&mut self, value: f64) {
        self.separate();
        push_f64(&mut self.out, value);
    }

    pub fn string(&mut self, value: &str) {
        self.separate();
        push_string(&mut self.out, value);
    }

    /// Already-serialized JSON, written as one value.
    pub fn raw(&mut self, json: &str) {
        self.separate();
        self.out.push_str(json);
    }

    pub fn field_f64(&mut self, key: &str, value: f64) {
        self.key(key);
        self.f64(value);
    }

    pub fn field_u64(&mut self, key: &str, value: u64) {
        self.key(key);
        self.u64(value);
    }

    pub fn field_str(&mut self, key: &str, value: &str) {
        self.key(key);
        self.string(value);
    }

    pub fn finish(self) -> String {
        self.out
    }
}

pub fn push_f64(out: &mut String, value: f64) {
    if !value.is_finite() {
        out.push_str("null");
        return;
    }
    let start = out.len();
    let _ = write!(out, "{value}");
    if !out[start..].contains(['.', 'e', 'E']) {
        out.push_str(".0");
    }
}

pub fn push_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_serde_shaped_json() {
        let mut json = Json::default();
        json.begin_object();
        json.field_f64("a", 1.0);
        json.field_f64("b", 0.5686);
        json.key("c");
        json.begin_array();
        json.u64(3);
        json.null();
        json.f64(f64::NAN);
        json.begin_object();
        json.end_object();
        json.end_array();
        json.field_str("d", "q\"\\\n\u{1}é");
        json.key("e");
        json.bool(false);
        json.end_object();
        assert_eq!(
            json.finish(),
            r#"{"a":1.0,"b":0.5686,"c":[3,null,null,{}],"d":"q\"\\\n\u0001é","e":false}"#
        );
    }
}
