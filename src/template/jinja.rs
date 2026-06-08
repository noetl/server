//! Jinja2-style template rendering using minijinja.
//!
//! This module provides template rendering for NoETL playbooks,
//! supporting variables, filters, and control structures.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use minijinja::{value::ValueKind, Environment, Error, ErrorKind, UndefinedBehavior, Value};
use std::collections::HashMap;

use crate::error::{AppError, AppResult};

/// Template renderer with custom filters and context.
pub struct TemplateRenderer {
    env: Environment<'static>,
}

impl Default for TemplateRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl TemplateRenderer {
    /// Create a new template renderer with custom filters.
    pub fn new() -> Self {
        let mut env = Environment::new();

        // Allow `{{ undefined.attribute }}` style chains to resolve
        // to undefined rather than throwing.  The default `Lenient`
        // behaviour fails attribute access on an undefined parent —
        // so a template like
        //   `{{ ctx.from_start | default(start.data.executed) }}`
        // throws "undefined value" when `ctx` isn't populated (e.g.
        // arc `set:` block isn't processed), even though the
        // `default()` filter would otherwise catch the missing
        // value.  `Chainable` returns undefined for the missing
        // attribute, lets `default()` see it, and matches the
        // Python reference impl's permissive Jinja2 behavior.
        // See noetl/ai-meta#54 Phase F R5 e2e finding.
        env.set_undefined_behavior(UndefinedBehavior::Chainable);

        // Add custom filters
        env.add_filter("b64encode", filter_b64encode);
        env.add_filter("b64decode", filter_b64decode);
        env.add_filter("tojson", filter_tojson);
        env.add_filter("fromjson", filter_fromjson);
        env.add_filter("default", filter_default);
        env.add_filter("int", filter_int);
        env.add_filter("float", filter_float);
        env.add_filter("string", filter_string);
        env.add_filter("lower", filter_lower);
        env.add_filter("upper", filter_upper);
        env.add_filter("trim", filter_trim);
        env.add_filter("split", filter_split);
        env.add_filter("join", filter_join);
        env.add_filter("first", filter_first);
        env.add_filter("last", filter_last);
        env.add_filter("length", filter_length);
        env.add_filter("keys", filter_keys);
        env.add_filter("values", filter_values);
        env.add_filter("items", filter_items);
        env.add_filter("get", filter_get);
        env.add_filter("safe", filter_safe);

        // Add custom tests
        env.add_test("defined", test_defined);
        env.add_test("undefined", test_undefined);
        env.add_test("none", test_none);
        env.add_test("string", test_string);
        env.add_test("number", test_number);
        env.add_test("sequence", test_sequence);
        env.add_test("mapping", test_mapping);

        Self { env }
    }

    /// Render a template string with the given context.
    pub fn render(
        &self,
        template: &str,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<String> {
        // Quick check for non-template strings
        if !contains_template_syntax(template) {
            return Ok(template.to_string());
        }

        // Convert JSON context to minijinja Value
        let ctx = json_to_value(context);

        let tmpl = self
            .env
            .template_from_str(template)
            .map_err(|e| AppError::Template(format!("Template parse error: {}", e)))?;

        tmpl.render(ctx)
            .map_err(|e| AppError::Template(format!("Template render error: {}", e)))
    }

    /// Render a template and return the result as a JSON value.
    /// Attempts to parse the rendered string as JSON if it looks like JSON.
    pub fn render_to_value(
        &self,
        template: &str,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<serde_json::Value> {
        let rendered = self.render(template, context)?;

        // Try to parse as JSON if it looks like JSON
        let trimmed = rendered.trim();
        if (trimmed.starts_with('{') && trimmed.ends_with('}'))
            || (trimmed.starts_with('[') && trimmed.ends_with(']'))
        {
            if let Ok(value) = serde_json::from_str(&rendered) {
                return Ok(value);
            }
        }

        // Try to parse as primitive values
        if let Ok(b) = trimmed.parse::<bool>() {
            return Ok(serde_json::Value::Bool(b));
        }
        if let Ok(i) = trimmed.parse::<i64>() {
            return Ok(serde_json::Value::Number(i.into()));
        }
        if let Ok(f) = trimmed.parse::<f64>() {
            if let Some(n) = serde_json::Number::from_f64(f) {
                return Ok(serde_json::Value::Number(n));
            }
        }
        if trimmed == "null" || trimmed == "None" || trimmed.is_empty() {
            return Ok(serde_json::Value::Null);
        }

        Ok(serde_json::Value::String(rendered))
    }

    /// Render a nested structure (dict or list) recursively.
    pub fn render_value(
        &self,
        value: &serde_json::Value,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<serde_json::Value> {
        match value {
            serde_json::Value::String(s) => self.render_to_value(s, context),
            serde_json::Value::Object(map) => {
                let mut result = serde_json::Map::new();
                for (k, v) in map {
                    let rendered_key = self.render(k, context)?;
                    let rendered_value = self.render_value(v, context)?;
                    result.insert(rendered_key, rendered_value);
                }
                Ok(serde_json::Value::Object(result))
            }
            serde_json::Value::Array(arr) => {
                let result: Result<Vec<_>, _> =
                    arr.iter().map(|v| self.render_value(v, context)).collect();
                Ok(serde_json::Value::Array(result?))
            }
            _ => Ok(value.clone()),
        }
    }

    /// Render a nested structure, but **preserve** any `{{ ... }}`
    /// expression that references one of `deferred_roots` as a
    /// whole-word identifier.
    ///
    /// Used for `task_sequence` pipeline configs: `_prev` / `_results`
    /// are RUNTIME values the task_sequence tool injects per sub-task,
    /// so their references must survive the server's command-build
    /// render and reach the worker verbatim.  Everything else (the
    /// `{{ pg_auth }}` credential alias, `{{ item }}`, `{{ execution_id }}`,
    /// step results, …) renders now, so the worker's keychain-alias
    /// resolution still sees a resolved alias string.  See
    /// noetl/server#72 / noetl/ai-meta#54.
    pub fn render_value_deferring(
        &self,
        value: &serde_json::Value,
        context: &HashMap<String, serde_json::Value>,
        deferred_roots: &[&str],
    ) -> AppResult<serde_json::Value> {
        match value {
            serde_json::Value::String(s) => {
                self.render_to_value_deferring(s, context, deferred_roots)
            }
            serde_json::Value::Object(map) => {
                let mut result = serde_json::Map::new();
                for (k, v) in map {
                    // Object KEYS are never deferred-bearing in practice;
                    // render normally.
                    let rendered_key = self.render(k, context)?;
                    let rendered_value =
                        self.render_value_deferring(v, context, deferred_roots)?;
                    result.insert(rendered_key, rendered_value);
                }
                Ok(serde_json::Value::Object(result))
            }
            serde_json::Value::Array(arr) => {
                let result: Result<Vec<_>, _> = arr
                    .iter()
                    .map(|v| self.render_value_deferring(v, context, deferred_roots))
                    .collect();
                Ok(serde_json::Value::Array(result?))
            }
            _ => Ok(value.clone()),
        }
    }

    /// Render a single template string while preserving `{{ ... }}`
    /// blocks that reference a deferred root.  When no deferred block
    /// is present this is exactly `render_to_value`.  When one IS
    /// present the result stays a JSON string carrying the
    /// partially-rendered template (task_sequence renders the rest).
    fn render_to_value_deferring(
        &self,
        template: &str,
        context: &HashMap<String, serde_json::Value>,
        deferred_roots: &[&str],
    ) -> AppResult<serde_json::Value> {
        let (protected, stash) = protect_deferred(template, deferred_roots);
        if stash.is_empty() {
            // Nothing deferred — preserve the normal JSON-coercion path.
            return self.render_to_value(template, context);
        }
        let rendered = self.render(&protected, context)?;
        let restored = restore_deferred(&rendered, &stash);
        // The restored string still holds unresolved `{{ _prev/_results }}`
        // templates, so it can't be a finished JSON value — keep it as a
        // string for task_sequence to render at dispatch time.
        Ok(serde_json::Value::String(restored))
    }

    /// Evaluate a condition expression.
    pub fn evaluate_condition(
        &self,
        condition: &str,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<bool> {
        // Wrap condition in {{ }} if not already
        let template = if contains_template_syntax(condition) {
            condition.to_string()
        } else {
            format!("{{{{ {} }}}}", condition)
        };

        let rendered = self.render(&template, context)?;
        let trimmed = rendered.trim().to_lowercase();

        // Evaluate as boolean
        Ok(matches!(trimmed.as_str(), "true" | "1" | "yes"))
    }
}

/// Check if a string contains Jinja2 template syntax.
fn contains_template_syntax(s: &str) -> bool {
    (s.contains("{{") && s.contains("}}")) || (s.contains("{%") && s.contains("%}"))
}

/// True when `word` appears in `haystack` delimited by non-identifier
/// chars on both sides (ASCII-alphanumeric or `_`).  Used to detect
/// whether a `{{ ... }}` expression references a deferred root like
/// `_prev` without false-matching e.g. `my_prev`.
fn contains_word(haystack: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(word) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_ident(bytes[abs - 1]);
        let after = abs + word.len();
        let after_ok = after >= bytes.len() || !is_ident(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Replace each `{{ ... }}` block that references one of `roots`
/// (whole-word) with a unique null-delimited placeholder, returning
/// the masked template + the (placeholder, original-block) pairs to
/// restore afterwards.  `{%` statement blocks are left untouched —
/// the deferred vars only appear in `{{ }}` value expressions in
/// practice.
fn protect_deferred(template: &str, roots: &[&str]) -> (String, Vec<(String, String)>) {
    if roots.is_empty() || !template.contains("{{") {
        return (template.to_string(), Vec::new());
    }
    let mut out = String::with_capacity(template.len());
    let mut stash: Vec<(String, String)> = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        // Emit everything before the block verbatim.
        out.push_str(&rest[..open]);
        let after_open = &rest[open..];
        match after_open.find("}}") {
            Some(close_rel) => {
                let close = close_rel + 2; // include the closing `}}`
                let block = &after_open[..close];
                let inner = &block[2..block.len() - 2];
                if roots.iter().any(|r| contains_word(inner, r)) {
                    let placeholder = format!("\u{0}NOETL_DEFER_{}\u{0}", stash.len());
                    out.push_str(&placeholder);
                    stash.push((placeholder, block.to_string()));
                } else {
                    out.push_str(block);
                }
                rest = &after_open[close..];
            }
            None => {
                // Unterminated `{{` — emit the remainder and stop.
                out.push_str(after_open);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    (out, stash)
}

/// Restore the original `{{ ... }}` blocks masked by `protect_deferred`.
fn restore_deferred(rendered: &str, stash: &[(String, String)]) -> String {
    let mut out = rendered.to_string();
    for (placeholder, original) in stash {
        out = out.replace(placeholder.as_str(), original);
    }
    out
}

/// Convert a JSON HashMap to a minijinja Value.
fn json_to_value(json: &HashMap<String, serde_json::Value>) -> Value {
    let converted: HashMap<String, Value> = json
        .iter()
        .map(|(k, v)| (k.clone(), json_value_to_minijinja(v)))
        .collect();
    Value::from_object(converted)
}

/// Convert a serde_json::Value to a minijinja Value.
fn json_value_to_minijinja(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::UNDEFINED,
        serde_json::Value::Bool(b) => Value::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(f) = n.as_f64() {
                Value::from(f)
            } else {
                Value::UNDEFINED
            }
        }
        serde_json::Value::String(s) => Value::from(s.as_str()),
        serde_json::Value::Array(arr) => {
            let items: Vec<Value> = arr.iter().map(json_value_to_minijinja).collect();
            Value::from(items)
        }
        serde_json::Value::Object(map) => {
            let items: HashMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), json_value_to_minijinja(v)))
                .collect();
            Value::from_object(items)
        }
    }
}

// ============================================================================
// Custom Filters
// ============================================================================

/// Base64 encode filter.
fn filter_b64encode(value: &Value) -> Result<String, Error> {
    let s = value.to_string();
    Ok(BASE64.encode(s.as_bytes()))
}

/// Base64 decode filter.
fn filter_b64decode(value: &Value) -> Result<String, Error> {
    let s = value.to_string();
    let decoded = BASE64.decode(s.as_bytes()).map_err(|e| {
        Error::new(
            ErrorKind::InvalidOperation,
            format!("b64decode error: {}", e),
        )
    })?;
    String::from_utf8(decoded)
        .map_err(|e| Error::new(ErrorKind::InvalidOperation, format!("utf8 error: {}", e)))
}

/// JSON encode filter.
fn filter_tojson(value: &Value) -> Result<String, Error> {
    // Convert Value back to JSON
    let json_val = minijinja_to_json(value);
    serde_json::to_string(&json_val)
        .map_err(|e| Error::new(ErrorKind::InvalidOperation, format!("tojson error: {}", e)))
}

/// JSON decode filter.
fn filter_fromjson(value: &Value) -> Result<Value, Error> {
    let s = value.to_string();
    let json_val: serde_json::Value = serde_json::from_str(&s).map_err(|e| {
        Error::new(
            ErrorKind::InvalidOperation,
            format!("fromjson error: {}", e),
        )
    })?;
    Ok(json_value_to_minijinja(&json_val))
}

/// Default value filter.
fn filter_default(value: &Value, default: Option<&Value>) -> Value {
    if value.is_undefined() || value.is_none() {
        default.cloned().unwrap_or(Value::from(""))
    } else {
        value.clone()
    }
}

/// Convert to integer filter.
fn filter_int(value: &Value) -> Result<i64, Error> {
    if let Some(i) = value.as_i64() {
        return Ok(i);
    }
    let s = value.to_string();
    // Try parsing as float first then convert to int
    if let Ok(f) = s.parse::<f64>() {
        return Ok(f as i64);
    }
    s.parse::<i64>()
        .map_err(|e| Error::new(ErrorKind::InvalidOperation, format!("int error: {}", e)))
}

/// Convert to float filter.
fn filter_float(value: &Value) -> Result<f64, Error> {
    if let Some(i) = value.as_i64() {
        return Ok(i as f64);
    }
    let s = value.to_string();
    s.parse::<f64>()
        .map_err(|e| Error::new(ErrorKind::InvalidOperation, format!("float error: {}", e)))
}

/// Convert to string filter.
fn filter_string(value: &Value) -> String {
    value.to_string()
}

/// Lowercase filter.
fn filter_lower(value: &Value) -> String {
    value.to_string().to_lowercase()
}

/// Uppercase filter.
fn filter_upper(value: &Value) -> String {
    value.to_string().to_uppercase()
}

/// Trim whitespace filter.
fn filter_trim(value: &Value) -> String {
    value.to_string().trim().to_string()
}

/// Split string filter.
fn filter_split(value: &Value, sep: Option<&Value>) -> Vec<String> {
    let s = value.to_string();
    let separator = sep
        .map(|v| v.to_string())
        .unwrap_or_else(|| " ".to_string());
    s.split(&separator).map(|s| s.to_string()).collect()
}

/// Join list filter.
fn filter_join(value: &Value, sep: Option<&Value>) -> Result<String, Error> {
    let separator = sep.map(|v| v.to_string()).unwrap_or_default();
    let iter = value
        .try_iter()
        .map_err(|_| Error::new(ErrorKind::InvalidOperation, "join requires a sequence"))?;
    let items: Vec<String> = iter.map(|v| v.to_string()).collect();
    Ok(items.join(&separator))
}

/// Get first element filter.
fn filter_first(value: &Value) -> Result<Value, Error> {
    let mut iter = value
        .try_iter()
        .map_err(|_| Error::new(ErrorKind::InvalidOperation, "first requires a sequence"))?;
    iter.next()
        .ok_or_else(|| Error::new(ErrorKind::InvalidOperation, "sequence is empty"))
}

/// Get last element filter.
fn filter_last(value: &Value) -> Result<Value, Error> {
    let iter = value
        .try_iter()
        .map_err(|_| Error::new(ErrorKind::InvalidOperation, "last requires a sequence"))?;
    iter.last()
        .ok_or_else(|| Error::new(ErrorKind::InvalidOperation, "sequence is empty"))
}

/// Length filter.
fn filter_length(value: &Value) -> Result<usize, Error> {
    if let Some(s) = value.as_str() {
        return Ok(s.len());
    }
    if let Some(len) = value.len() {
        return Ok(len);
    }
    Err(Error::new(
        ErrorKind::InvalidOperation,
        "length requires string, sequence, or mapping",
    ))
}

/// Get dict keys filter.
fn filter_keys(value: &Value) -> Result<Vec<String>, Error> {
    if value.kind() != ValueKind::Map {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "keys requires a mapping",
        ));
    }
    let iter = value
        .try_iter()
        .map_err(|_| Error::new(ErrorKind::InvalidOperation, "cannot iterate keys"))?;
    Ok(iter.map(|v| v.to_string()).collect())
}

/// Get dict values filter.
fn filter_values(value: &Value) -> Result<Vec<Value>, Error> {
    if value.kind() != ValueKind::Map {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "values requires a mapping",
        ));
    }
    let iter = value
        .try_iter()
        .map_err(|_| Error::new(ErrorKind::InvalidOperation, "cannot iterate values"))?;
    let mut result = Vec::new();
    for key in iter {
        if let Ok(val) = value.get_item(&key) {
            result.push(val);
        }
    }
    Ok(result)
}

/// Get dict items filter (as list of [key, value] pairs).
fn filter_items(value: &Value) -> Result<Vec<Vec<Value>>, Error> {
    if value.kind() != ValueKind::Map {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "items requires a mapping",
        ));
    }
    let iter = value
        .try_iter()
        .map_err(|_| Error::new(ErrorKind::InvalidOperation, "cannot iterate items"))?;
    let mut result = Vec::new();
    for key in iter {
        if let Ok(val) = value.get_item(&key) {
            result.push(vec![key.clone(), val]);
        }
    }
    Ok(result)
}

/// Get value from dict by key.
fn filter_get(value: &Value, key: &Value) -> Value {
    value.get_item(key).unwrap_or(Value::UNDEFINED)
}

/// Mark as safe (no-op in our implementation).
fn filter_safe(value: &Value) -> Value {
    value.clone()
}

// ============================================================================
// Custom Tests
// ============================================================================

/// Test if value is defined.
fn test_defined(value: &Value) -> bool {
    !value.is_undefined()
}

/// Test if value is undefined.
fn test_undefined(value: &Value) -> bool {
    value.is_undefined()
}

/// Test if value is none/null.
fn test_none(value: &Value) -> bool {
    value.is_none()
}

/// Test if value is a string.
fn test_string(value: &Value) -> bool {
    value.kind() == ValueKind::String
}

/// Test if value is a number.
fn test_number(value: &Value) -> bool {
    value.kind() == ValueKind::Number
}

/// Test if value is a sequence (list).
fn test_sequence(value: &Value) -> bool {
    value.kind() == ValueKind::Seq
}

/// Test if value is a mapping (dict).
fn test_mapping(value: &Value) -> bool {
    value.kind() == ValueKind::Map
}

/// Convert minijinja Value back to serde_json::Value.
fn minijinja_to_json(value: &Value) -> serde_json::Value {
    if value.is_undefined() || value.is_none() {
        return serde_json::Value::Null;
    }
    // Check for bool by testing is_true and kind
    if value.kind() == ValueKind::Bool {
        return serde_json::Value::Bool(value.is_true());
    }
    if let Some(i) = value.as_i64() {
        return serde_json::Value::Number(i.into());
    }
    if let Some(s) = value.as_str() {
        return serde_json::Value::String(s.to_string());
    }
    if value.kind() == ValueKind::Seq {
        if let Ok(iter) = value.try_iter() {
            let arr: Vec<serde_json::Value> = iter.map(|v| minijinja_to_json(&v)).collect();
            return serde_json::Value::Array(arr);
        }
    }
    if value.kind() == ValueKind::Map {
        let mut map = serde_json::Map::new();
        if let Ok(iter) = value.try_iter() {
            for key in iter {
                if let Ok(val) = value.get_item(&key) {
                    map.insert(key.to_string(), minijinja_to_json(&val));
                }
            }
        }
        return serde_json::Value::Object(map);
    }
    // Fallback to string
    serde_json::Value::String(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_word_respects_boundaries() {
        assert!(contains_word("_prev.item_name", "_prev"));
        assert!(contains_word("foo(_prev.x) | default(1)", "_prev"));
        assert!(contains_word("_results['save']", "_results"));
        assert!(!contains_word("my_prev.x", "_prev"));
        assert!(!contains_word("execution_id", "_prev"));
    }

    #[test]
    fn test_render_value_deferring_preserves_prev_renders_rest() {
        // The exact shape that broke iterator_save_test: an INSERT query
        // mixing a now-resolvable `{{ execution_id }}` with runtime-only
        // `{{ _prev.* }}` refs.  After the deferring render, execution_id
        // is filled but _prev refs survive verbatim for task_sequence.
        let renderer = TemplateRenderer::new();
        let mut ctx = HashMap::new();
        ctx.insert("execution_id".to_string(), serde_json::json!("321"));
        ctx.insert("pg_auth".to_string(), serde_json::json!("pg_k8s"));

        let cfg = serde_json::json!({
            "save_item": {
                "kind": "postgres",
                "credential": "{{ pg_auth }}",
                "query": "INSERT INTO t (id, item_name, item_value) VALUES ('{{ execution_id }}:{{ _prev.item_name }}', '{{ _prev.item_name }}', {{ _prev.item_value }})"
            }
        });

        let out = renderer
            .render_value_deferring(&cfg, &ctx, &["_prev", "_results"])
            .unwrap();
        let save = &out["save_item"];
        // Credential alias resolved now (worker auth_alias needs it).
        assert_eq!(save["credential"], serde_json::json!("pg_k8s"));
        let query = save["query"].as_str().unwrap();
        // execution_id rendered:
        assert!(query.contains("'321:{{ _prev.item_name }}'"), "got: {query}");
        // _prev refs preserved verbatim for task_sequence:
        assert!(query.contains("'{{ _prev.item_name }}'"), "got: {query}");
        assert!(query.contains("{{ _prev.item_value }})"), "got: {query}");
        // No placeholder leaked:
        assert!(!query.contains("NOETL_DEFER"), "placeholder leaked: {query}");
    }

    #[test]
    fn test_render_value_deferring_no_deferred_is_normal_render() {
        // When no _prev/_results refs are present, behaves exactly like
        // render_value (including JSON-coercion to a real value).
        let renderer = TemplateRenderer::new();
        let mut ctx = HashMap::new();
        ctx.insert("item".to_string(), serde_json::json!({"name": "item1", "value": 100}));
        let cfg = serde_json::json!({"transform": {"args": {"item": "{{ item }}"}}});
        let out = renderer
            .render_value_deferring(&cfg, &ctx, &["_prev", "_results"])
            .unwrap();
        assert_eq!(out["transform"]["args"]["item"]["name"], serde_json::json!("item1"));
        assert_eq!(out["transform"]["args"]["item"]["value"], serde_json::json!(100));
    }

    fn make_context() -> HashMap<String, serde_json::Value> {
        let mut ctx = HashMap::new();
        ctx.insert("name".to_string(), serde_json::json!("Alice"));
        ctx.insert("age".to_string(), serde_json::json!(30));
        ctx.insert("active".to_string(), serde_json::json!(true));
        ctx.insert(
            "items".to_string(),
            serde_json::json!(["apple", "banana", "cherry"]),
        );
        ctx.insert(
            "user".to_string(),
            serde_json::json!({"email": "alice@example.com", "id": 123}),
        );
        ctx
    }

    #[test]
    fn test_jinja_conditional_short_circuits_on_undefined_else_branch() {
        // noetl/ai-meta#67 — pin the renderer's handling of
        // `{{ A if A else B.x }}` when only A is in the context
        // and B is undefined.  Expected per Chainable undefined +
        // Jinja2 semantics: short-circuit picks `A.x`, never
        // accesses `B.x` → renders to A's value (or attribute).
        // This is the surface symptom of #67 but NOT the root
        // cause — the actual bug was upstream in the orchestrator's
        // R4 fan-in barrier, not the template engine; the renderer
        // itself is well-behaved.  Test kept to defend against
        // regression in Chainable semantics.
        let mut ctx = HashMap::new();
        ctx.insert(
            "process_high".to_string(),
            serde_json::json!({
                "category": "high",
                "processed": 30
            }),
        );
        // NO process_low in ctx.

        let renderer = TemplateRenderer::new();
        let s = renderer
            .render(
                "{{ process_high.category if process_high else process_low.category }}",
                &ctx,
            )
            .expect("render should succeed under Chainable undefined");
        println!("PROBE RENDER OUTPUT: {:?}", s);
        assert_eq!(s, "high", "expected 'high' from short-circuit; got {:?}", s);
    }

    #[test]
    fn test_simple_variable() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer.render("Hello, {{ name }}!", &ctx).unwrap();
        assert_eq!(result, "Hello, Alice!");
    }

    #[test]
    fn test_no_template() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer.render("Plain text", &ctx).unwrap();
        assert_eq!(result, "Plain text");
    }

    #[test]
    fn test_nested_variable() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer.render("Email: {{ user.email }}", &ctx).unwrap();
        assert_eq!(result, "Email: alice@example.com");
    }

    #[test]
    fn test_b64encode_filter() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer.render("{{ name | b64encode }}", &ctx).unwrap();
        assert_eq!(result, "QWxpY2U=");
    }

    #[test]
    fn test_default_filter() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer
            .render("{{ missing | default('fallback') }}", &ctx)
            .unwrap();
        assert_eq!(result, "fallback");
    }

    #[test]
    fn test_lower_upper_filters() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer.render("{{ name | lower }}", &ctx).unwrap();
        assert_eq!(result, "alice");

        let result = renderer.render("{{ name | upper }}", &ctx).unwrap();
        assert_eq!(result, "ALICE");
    }

    #[test]
    fn test_length_filter() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer.render("{{ items | length }}", &ctx).unwrap();
        assert_eq!(result, "3");
    }

    #[test]
    fn test_first_last_filters() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer.render("{{ items | first }}", &ctx).unwrap();
        assert_eq!(result, "apple");

        let result = renderer.render("{{ items | last }}", &ctx).unwrap();
        assert_eq!(result, "cherry");
    }

    #[test]
    fn test_join_filter() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer.render("{{ items | join(', ') }}", &ctx).unwrap();
        assert_eq!(result, "apple, banana, cherry");
    }

    #[test]
    fn test_conditional() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer
            .render("{% if active %}Active{% else %}Inactive{% endif %}", &ctx)
            .unwrap();
        assert_eq!(result, "Active");
    }

    #[test]
    fn test_for_loop() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer
            .render("{% for item in items %}{{ item }} {% endfor %}", &ctx)
            .unwrap();
        assert_eq!(result, "apple banana cherry ");
    }

    #[test]
    fn test_evaluate_condition() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        assert!(renderer.evaluate_condition("age > 25", &ctx).unwrap());
        assert!(!renderer.evaluate_condition("age < 25", &ctx).unwrap());
        assert!(renderer.evaluate_condition("active", &ctx).unwrap());
    }

    #[test]
    fn test_render_to_value_json() {
        let renderer = TemplateRenderer::new();
        let mut ctx = HashMap::new();
        ctx.insert("data".to_string(), serde_json::json!({"key": "value"}));

        let result = renderer
            .render_to_value("{{ data | tojson }}", &ctx)
            .unwrap();
        assert_eq!(result, serde_json::json!({"key": "value"}));
    }

    #[test]
    fn test_render_to_value_number() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let result = renderer.render_to_value("{{ age }}", &ctx).unwrap();
        assert_eq!(result, serde_json::json!(30));
    }

    #[test]
    fn test_render_value_nested() {
        let renderer = TemplateRenderer::new();
        let ctx = make_context();

        let value = serde_json::json!({
            "greeting": "Hello, {{ name }}!",
            "info": {
                "age_str": "Age: {{ age }}"
            }
        });

        let result = renderer.render_value(&value, &ctx).unwrap();
        assert_eq!(result["greeting"], "Hello, Alice!");
        assert_eq!(result["info"]["age_str"], "Age: 30");
    }
}
