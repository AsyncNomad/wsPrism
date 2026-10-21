//! `${VAR}` in configuration values (M6): the same file in every
//! environment, with the parts that differ (addresses, URLs, prefixes) taken
//! from the environment.
//!
//! - `${NAME}`: the variable's value; startup fails if it is not set.
//! - `${NAME:-default}`: `default` if it is not set or empty.
//! - `$${`: a literal `${`.
//!
//! Only string values are rewritten, after YAML parsing: a value can never
//! change the file's structure (a `: ` or `#` in it stays text), and keys
//! are never rewritten. A value that is a single placeholder and whose
//! result reads as a YAML number or boolean becomes one (`max_conns:
//! ${MAX}`); everything else stays a string.
//!
//! Inside `{ }` or `[ ]` (YAML flow style) quote the placeholder
//! (`{ max: "${MAX}" }`): unquoted, its braces end the flow. Quoting does
//! not change the typing above.
//!
//! Secrets should still use the `*_env` settings (`secret_env`, `url_env`):
//! those are read where they are used and never pass through the parsed
//! configuration.

use serde_norway::Value;
use wsprism_protocol::error::{Result, WsPrismError};

/// Whether `text` uses placeholders at all (files without are parsed
/// directly, keeping line numbers in errors).
pub fn uses_placeholders(text: &str) -> bool {
    text.contains("${")
}

/// Rewrite every string value of `value` with `lookup`.
pub fn substitute(value: &mut Value, lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    walk(value, "", lookup)
}

fn walk(value: &mut Value, path: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    match value {
        Value::String(s) => {
            if s.contains('$') {
                let (text, whole) = expand(s, lookup)
                    .map_err(|e| WsPrismError::BadRequest(format!("{}: {e}", display(path))))?;
                *value = if whole {
                    typed(text)
                } else {
                    Value::String(text)
                };
            }
        }
        Value::Sequence(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                walk(item, &format!("{path}[{i}]"), lookup)?;
            }
        }
        Value::Mapping(map) => {
            for (k, v) in map.iter_mut() {
                let key = match k {
                    Value::String(k) => k.clone(),
                    other => format!("{other:?}"),
                };
                let child = if path.is_empty() {
                    key
                } else {
                    format!("{path}.{key}")
                };
                walk(v, &child, lookup)?;
            }
        }
        Value::Tagged(t) => walk(&mut t.value, path, lookup)?,
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn display(path: &str) -> &str {
    if path.is_empty() {
        "(root)"
    } else {
        path
    }
}

/// A whole-placeholder result: a number or boolean if it reads as one.
fn typed(text: String) -> Value {
    match serde_norway::from_str::<Value>(&text) {
        Ok(v @ (Value::Number(_) | Value::Bool(_))) => v,
        _ => Value::String(text),
    }
}

/// Expand placeholders in `s`; also says whether `s` was exactly one
/// placeholder.
fn expand(
    s: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<(String, bool), String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut placeholders = 0;
    let mut literal = false;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if let Some(after) = tail.strip_prefix("$${") {
            out.push_str("${");
            literal = true;
            rest = after;
        } else if let Some(after) = tail.strip_prefix("${") {
            let end = after
                .find('}')
                .ok_or_else(|| format!("unclosed ${{ in {s:?}"))?;
            let inner = &after[..end];
            let (name, default) = match inner.split_once(":-") {
                Some((n, d)) => (n, Some(d)),
                None => (inner, None),
            };
            if !valid_name(name) {
                return Err(format!(
                    "${{{inner}}}: not a variable name ([A-Za-z_][A-Za-z0-9_]*)"
                ));
            }
            let value = match (lookup(name).filter(|v| !v.is_empty()), default) {
                (Some(v), _) => v,
                (None, Some(d)) => d.to_string(),
                (None, None) => return Err(format!("environment variable {name} is not set")),
            };
            out.push_str(&value);
            placeholders += 1;
            rest = &after[end + 1..];
        } else {
            out.push('$');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    let whole = placeholders == 1
        && !literal
        && s.starts_with("${")
        && s.ends_with('}')
        && s.matches("${").count() == 1;
    Ok((out, whole))
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "HOST" => Some("10.0.0.5".into()),
            "PORT" => Some("8080".into()),
            "ON" => Some("true".into()),
            "TRICKY" => Some("a: b # c\nd: [e]".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    fn run(yaml: &str) -> Result<Value> {
        let mut v: Value = serde_norway::from_str(yaml).unwrap();
        substitute(&mut v, &env)?;
        Ok(v)
    }

    #[test]
    fn strings_numbers_and_defaults() {
        let v = run(
            "a: \"${HOST}:${PORT}\"\nb: ${PORT}\nc: ${ON}\nd: ${NOPE:-x}\ne: ${EMPTY:-y}\nf: pre-${PORT}\ng: \"$${HOST}\"\nh: cost $5\n",
        )
        .unwrap();
        assert_eq!(v["a"], Value::String("10.0.0.5:8080".into()));
        assert_eq!(v["b"], serde_norway::from_str::<Value>("8080").unwrap());
        assert_eq!(v["c"], Value::Bool(true));
        assert_eq!(v["d"], Value::String("x".into()));
        assert_eq!(v["e"], Value::String("y".into()));
        assert_eq!(v["f"], Value::String("pre-8080".into()));
        assert_eq!(v["g"], Value::String("${HOST}".into()));
        assert_eq!(v["h"], Value::String("cost $5".into()));
    }

    #[test]
    fn values_never_change_the_structure() {
        let v = run("a: ${TRICKY}\nb: [\"${TRICKY}\"]\n").unwrap();
        assert_eq!(v["a"], Value::String("a: b # c\nd: [e]".into()));
        assert_eq!(v["b"][0], Value::String("a: b # c\nd: [e]".into()));
        assert_eq!(v.as_mapping().unwrap().len(), 2);
    }

    #[test]
    fn keys_are_not_rewritten() {
        let v = run("\"${HOST}\": 1\n").unwrap();
        assert!(v.get("${HOST}").is_some());
    }

    #[test]
    fn errors_name_the_variable_and_the_setting() {
        let e = run("gateway:\n  listen: \"${MISSING}\"\n").unwrap_err();
        let m = e.to_string();
        assert!(m.contains("MISSING") && m.contains("gateway.listen"), "{m}");
        let e = run("t:\n  - x: ${1BAD}\n").unwrap_err().to_string();
        assert!(
            e.contains("t[0].x") && e.contains("not a variable name"),
            "{e}"
        );
        let e = run("x: \"${OPEN\"\n").unwrap_err().to_string();
        assert!(e.contains("unclosed"), "{e}");
    }
}
