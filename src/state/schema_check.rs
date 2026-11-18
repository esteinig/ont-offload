// SPDX-License-Identifier: AGPL-3.0-or-later
//! A small JSON Schema checker for the tests: the subset the schemas in
//! `schema/` use (`type`, `const`, `enum`, `required`, `properties`,
//! `additionalProperties: false`, `items`, `oneOf`, `minimum` and local
//! `$ref`s). Enough to catch a field added, renamed or retyped on one side
//! only.

use serde_json::Value;

/// Load `schema/<name>` from the repository.
pub(crate) fn load(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("schema")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Problems found validating `v` against `root`, as `path: problem`.
pub(crate) fn validate(root: &Value, v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    check(root, root, v, "$", &mut out);
    out
}

fn type_ok(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "integer" => v.is_u64() || v.is_i64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => false,
    }
}

fn check(root: &Value, s: &Value, v: &Value, at: &str, out: &mut Vec<String>) {
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let target = r
            .strip_prefix("#/")
            .map_or(&Value::Null, |p| p.split('/').fold(root, |n, k| &n[k]));
        if target.is_null() {
            out.push(format!("{at}: unresolved $ref {r}"));
        } else {
            check(root, target, v, at, out);
        }
        return;
    }
    if let Some(c) = s.get("const") {
        if c != v {
            out.push(format!("{at}: {v} is not {c}"));
        }
    }
    if let Some(alts) = s.get("oneOf").and_then(Value::as_array) {
        let ok = alts
            .iter()
            .filter(|a| {
                let mut o = Vec::new();
                check(root, a, v, at, &mut o);
                o.is_empty()
            })
            .count();
        if ok != 1 {
            out.push(format!("{at}: matches {ok} of oneOf"));
        }
    }
    match s.get("type") {
        Some(Value::String(t)) if !type_ok(t, v) => out.push(format!("{at}: {v} is not {t}")),
        Some(Value::Array(ts)) if !ts.iter().filter_map(Value::as_str).any(|t| type_ok(t, v)) => {
            out.push(format!("{at}: {v} is none of {ts:?}"));
        }
        _ => {}
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array) {
        if !e.contains(v) {
            out.push(format!("{at}: {v} not in enum"));
        }
    }
    if let (Some(m), Some(n)) = (s.get("minimum").and_then(Value::as_i64), v.as_i64()) {
        if n < m {
            out.push(format!("{at}: {n} < {m}"));
        }
    }
    if let Some(obj) = v.as_object() {
        let props = s.get("properties").and_then(Value::as_object);
        for r in s
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(k) = r.as_str() {
                if !obj.contains_key(k) {
                    out.push(format!("{at}: missing {k}"));
                }
            }
        }
        for (k, val) in obj {
            match props.and_then(|p| p.get(k)) {
                Some(ps) => check(root, ps, val, &format!("{at}.{k}"), out),
                None if s.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    out.push(format!("{at}: unexpected {k}"));
                }
                None => {}
            }
        }
    }
    if let (Some(items), Some(arr)) = (s.get("items"), v.as_array()) {
        for (i, x) in arr.iter().enumerate() {
            check(root, items, x, &format!("{at}[{i}]"), out);
        }
    }
}
