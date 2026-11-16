//! Modelin ürettiği bozuk araç argümanı JSON'unu onarır.
//!
//! Ele alınanlar: kod çiti (```json), sondaki virgül, kapanmamış string/obje/dizi (kesilmiş akış),
//! boş girdi. Sonuç her zaman bir JSON nesnesidir; onarılamazsa `None`.

use serde_json::{Map, Value};

pub fn parse_args(raw: &str) -> Option<Value> {
    let text = strip_fence(raw.trim());
    if text.is_empty() {
        return Some(Value::Object(Map::new()));
    }
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return into_object(v);
    }
    serde_json::from_str::<Value>(&repair(text))
        .ok()
        .and_then(into_object)
}

/// Argümanı string olarak gömmüş (`"{\"a\":1}"`) modeller için bir kat daha açar.
fn into_object(v: Value) -> Option<Value> {
    match v {
        Value::Object(_) => Some(v),
        Value::String(s) => serde_json::from_str::<Value>(&s)
            .ok()
            .filter(Value::is_object),
        Value::Null => Some(Value::Object(Map::new())),
        _ => None,
    }
}

fn strip_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    let rest = rest.split_once('\n').map_or(rest, |(_, body)| body);
    rest.trim_end().strip_suffix("```").unwrap_or(rest).trim()
}

fn repair(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let (mut in_str, mut esc) = (false, false);
    for c in text.chars() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' => {
                stack.push('}');
                out.push(c);
            }
            '[' => {
                stack.push(']');
                out.push(c);
            }
            '}' | ']' => {
                drop_trailing_comma(&mut out);
                if stack.last() == Some(&c) {
                    stack.pop();
                    out.push(c);
                } // dengesiz kapanışı sessizce at
            }
            _ => out.push(c),
        }
    }
    if in_str {
        if esc {
            out.pop(); // yarım kalan kaçış
        }
        out.push('"');
    }
    drop_trailing_comma(&mut out);
    if out.trim_end().ends_with(':') {
        out.push_str("null");
    }
    while let Some(close) = stack.pop() {
        drop_trailing_comma(&mut out);
        out.push(close);
    }
    out
}

fn drop_trailing_comma(out: &mut String) {
    let trimmed = out.trim_end().len();
    if out[..trimmed].ends_with(',') {
        out.truncate(trimmed - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_and_empty() {
        assert_eq!(parse_args(r#"{"a":1}"#), Some(json!({"a":1})));
        assert_eq!(parse_args("  "), Some(json!({})));
        assert_eq!(parse_args("null"), Some(json!({})));
        assert_eq!(parse_args("[1,2]"), None);
    }

    #[test]
    fn fenced_and_trailing_commas() {
        assert_eq!(
            parse_args("```json\n{\"a\": [1,2,],}\n```"),
            Some(json!({"a":[1,2]}))
        );
    }

    #[test]
    fn truncated_streams_are_closed() {
        assert_eq!(
            parse_args(r#"{"path":"C:\\x","cmd":"ls -l"#),
            Some(json!({"path":"C:\\x","cmd":"ls -l"}))
        );
        assert_eq!(
            parse_args(r#"{"a":{"b":[1,2"#),
            Some(json!({"a":{"b":[1,2]}}))
        );
        assert_eq!(parse_args(r#"{"a":1,"b":"#), Some(json!({"a":1,"b":null})));
        assert_eq!(parse_args(r#"{"a":"x\"#), Some(json!({"a":"x"})));
    }

    #[test]
    fn double_encoded_object_is_unwrapped() {
        assert_eq!(parse_args(r#""{\"a\":1}""#), Some(json!({"a":1})));
    }

    #[test]
    fn braces_inside_strings_are_not_structure() {
        assert_eq!(
            parse_args(r#"{"code":"fn x() {"#),
            Some(json!({"code":"fn x() {"}))
        );
    }
}
