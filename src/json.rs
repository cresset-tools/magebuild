//! A tiny, dependency-free JSON value + pretty printer.
//!
//! magebuild's declared dependency set does not include `serde_json`, and
//! `--json` output is small and structural, so a minimal emitter is enough. It
//! escapes strings per RFC 8259 and pretty-prints with two-space indentation.

/// A JSON value.
#[derive(Debug, Clone)]
pub enum Json {
    #[cfg_attr(not(test), allow(dead_code))]
    Null,
    Bool(bool),
    Num(u128),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn s(v: impl Into<String>) -> Json {
        Json::Str(v.into())
    }

    /// Pretty-print with two-space indentation and a trailing newline.
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out.push('\n');
        out
    }

    fn write(&self, out: &mut String, indent: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => out.push_str(&n.to_string()),
            Json::Str(s) => write_str(out, s),
            Json::Arr(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    pad(out, indent + 1);
                    item.write(out, indent + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                pad(out, indent);
                out.push(']');
            }
            Json::Obj(fields) => {
                if fields.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                for (i, (k, v)) in fields.iter().enumerate() {
                    pad(out, indent + 1);
                    write_str(out, k);
                    out.push_str(": ");
                    v.write(out, indent + 1);
                    if i + 1 < fields.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                pad(out, indent);
                out.push('}');
            }
        }
    }
}

fn pad(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn write_str(out: &mut String, s: &str) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_nests() {
        let v = Json::Obj(vec![
            ("name".into(), Json::s("a\"b\\c\nd")),
            ("n".into(), Json::Num(3)),
            ("ok".into(), Json::Bool(true)),
            ("list".into(), Json::Arr(vec![Json::s("x"), Json::Null])),
            ("empty".into(), Json::Arr(vec![])),
        ]);
        let out = v.to_pretty();
        assert!(out.contains("\"name\": \"a\\\"b\\\\c\\nd\""));
        assert!(out.contains("\"n\": 3"));
        assert!(out.contains("\"ok\": true"));
        assert!(out.contains("\"empty\": []"));
        assert!(out.ends_with("\n"));
    }
}
