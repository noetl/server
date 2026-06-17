//! Jinja2-style template rendering using minijinja.
//!
//! This module provides template rendering for NoETL playbooks,
//! supporting variables, filters, and control structures.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use minijinja::{value::ValueKind, Environment, Error, ErrorKind, UndefinedBehavior, Value};
use std::collections::HashMap;

use crate::error::{CoreError, CoreResult};

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
    ) -> CoreResult<String> {
        // Quick check for non-template strings
        if !contains_template_syntax(template) {
            return Ok(template.to_string());
        }

        // Convert JSON context to minijinja Value
        let ctx = json_to_value(context);

        let tmpl = self
            .env
            .template_from_str(template)
            .map_err(|e| CoreError::Template(format!("Template parse error: {}", e)))?;

        tmpl.render(ctx)
            .map_err(|e| CoreError::Template(format!("Template render error: {}", e)))
    }

    /// Render a template and return the result as a JSON value.
    /// Attempts to parse the rendered string as JSON if it looks like JSON.
    pub fn render_to_value(
        &self,
        template: &str,
        context: &HashMap<String, serde_json::Value>,
    ) -> CoreResult<serde_json::Value> {
        let rendered = self.render(template, context)?;

        // Try to parse as JSON if it looks like JSON
        let trimmed = rendered.trim();
        if (trimmed.starts_with('{') && trimmed.ends_with('}'))
            || (trimmed.starts_with('[') && trimmed.ends_with(']'))
        {
            if let Ok(value) = serde_json::from_str(&rendered) {
                return Ok(value);
            }
            // The output looks like a container but isn't valid JSON.
            // minijinja renders maps/lists with Python-style repr — a
            // `null` field surfaces as the token `undefined` (a JSON
            // `null` in the context maps to `Value::UNDEFINED`), and
            // other scalars can carry non-JSON spelling too.  When the
            // template is a single `{{ expr }}` whole-object reference,
            // re-render with `| tojson` so the value round-trips as
            // valid JSON — `minijinja_to_json` maps undefined/none back
            // to JSON `null`, so a `null` field stays `null` instead of
            // corrupting the payload into an unparseable string the
            // downstream step then receives as a raw `str`.  See
            // noetl/ai-meta#89 (cursor pagination's terminal
            // `next_cursor: null`).  Mirrors the `| tojson` retry in the
            // noetl-tools `TemplateEngine::render_value`.
            if let Some(value) = self.retry_single_expr_as_json(template, context) {
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

    /// When `template` is a single `{{ expr }}` reference whose plain
    /// render produced container-shaped-but-invalid JSON, re-render the
    /// expression with `| tojson` appended and parse the result.
    ///
    /// Returns `None` when the template isn't a lone `{{ expr }}`, when
    /// the expression already pipes through `tojson` (the plain render
    /// would already be valid JSON in that case), or when the retried
    /// render still doesn't parse — callers fall back to their existing
    /// behaviour.  See noetl/ai-meta#89.
    fn retry_single_expr_as_json(
        &self,
        template: &str,
        context: &HashMap<String, serde_json::Value>,
    ) -> Option<serde_json::Value> {
        let t = template.trim();
        if !(t.starts_with("{{") && t.ends_with("}}") && t.matches("{{").count() == 1) {
            return None;
        }
        let inner = t[2..t.len() - 2].trim();
        if inner.is_empty() || inner.contains("tojson") {
            return None;
        }
        let json_tmpl = format!("{{{{ {} | tojson }}}}", inner);
        let rendered = self.render(&json_tmpl, context).ok()?;
        serde_json::from_str::<serde_json::Value>(&rendered).ok()
    }

    /// Render a nested structure (dict or list) recursively.
    pub fn render_value(
        &self,
        value: &serde_json::Value,
        context: &HashMap<String, serde_json::Value>,
    ) -> CoreResult<serde_json::Value> {
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

    /// Evaluate a condition expression.
    pub fn evaluate_condition(
        &self,
        condition: &str,
        context: &HashMap<String, serde_json::Value>,
    ) -> CoreResult<bool> {
        // Wrap condition in {{ }} if not already
        let template = if contains_template_syntax(condition) {
            condition.to_string()
        } else {
            format!("{{{{ {} }}}}", condition)
        };

        let rendered = self.render(&template, context)?;
        let trimmed = rendered.trim().to_lowercase();

        // Python-compatible truthiness.  A condition like
        // `{{ (start.error is defined) and request_id }}` renders to the
        // *value* of the truthy operand (e.g. "req-123"), NOT the literal
        // "true" — minijinja's `and`/`or` return the operand, like Python's.
        // The reference Python orchestrator applies `bool(...)` to the
        // rendered value, so any non-empty, non-false-like string is truthy.
        // Treating only "true"/"1"/"yes" as true silently skipped every arc
        // whose `when` resolved to such a value (e.g. the auth0_login routing
        // — noetl/ai-meta#49), stalling the playbook.  The falsy set mirrors
        // Python `bool()` on the stringified value plus the false-like
        // keywords Jinja emits and empty containers.
        Ok(!matches!(
            trimmed.as_str(),
            "" | "false" | "0" | "no" | "none" | "null" | "off" | "{}" | "[]"
        ))
    }
}

/// Check if a string contains Jinja2 template syntax.
fn contains_template_syntax(s: &str) -> bool {
    (s.contains("{{") && s.contains("}}")) || (s.contains("{%") && s.contains("%}"))
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

    // -----------------------------------------------------------------
    // noetl/ai-meta#89 — a JSON `null` field in a step result, when the
    // whole envelope is re-injected into a downstream input via the
    // single-expression `{{ step }}` reference, must round-trip as JSON
    // `null` (not the JS token `undefined`) so the consuming step
    // receives a parsed object, not an unparseable `str`.
    // -----------------------------------------------------------------

    #[test]
    fn test_issue_89_null_field_in_whole_object_reference() {
        let renderer = TemplateRenderer::new();
        let mut ctx = HashMap::new();
        ctx.insert(
            "fetch_page".to_string(),
            serde_json::json!({
                "body": {
                    "next_cursor": null,
                    "limit": 10,
                    "events": [{"id": 31}, {"id": 32}]
                },
                "status_code": 200
            }),
        );
        let out = renderer.render_to_value("{{ fetch_page }}", &ctx).unwrap();
        assert!(out.is_object(), "expected parsed object, got: {:?}", out);
        assert!(
            out["body"]["next_cursor"].is_null(),
            "next_cursor must be JSON null, got: {:?}",
            out["body"]["next_cursor"]
        );
        assert_eq!(out["body"]["limit"], serde_json::json!(10));
        assert_eq!(out["body"]["events"][1]["id"], serde_json::json!(32));
        assert_eq!(out["status_code"], serde_json::json!(200));
    }

    #[test]
    fn test_issue_89_top_level_null_field_round_trips() {
        // The terminal cursor page shape: `next_cursor` lives directly
        // on the rendered object, mirroring the real API envelope's
        // `{"events": [...], "next_cursor": null, "limit": 10}`.
        let renderer = TemplateRenderer::new();
        let mut ctx = HashMap::new();
        ctx.insert(
            "page".to_string(),
            serde_json::json!({
                "events": [{"id": 35}],
                "next_cursor": null,
                "limit": 10
            }),
        );
        let out = renderer.render_to_value("{{ page }}", &ctx).unwrap();
        assert!(out.is_object(), "expected parsed object, got: {:?}", out);
        assert!(out["next_cursor"].is_null());
        // Round-trip equality against the original (null preserved).
        assert_eq!(
            out,
            serde_json::json!({
                "events": [{"id": 35}],
                "next_cursor": null,
                "limit": 10
            })
        );
    }

    #[test]
    fn test_issue_89_array_with_null_element_round_trips() {
        let renderer = TemplateRenderer::new();
        let mut ctx = HashMap::new();
        ctx.insert(
            "rows".to_string(),
            serde_json::json!([{"v": 1}, {"v": null}]),
        );
        let out = renderer.render_to_value("{{ rows }}", &ctx).unwrap();
        assert!(out.is_array(), "expected parsed array, got: {:?}", out);
        assert!(out[1]["v"].is_null());
    }

    #[test]
    fn test_issue_89_explicit_tojson_still_parses() {
        // A user who already wrote `| tojson` must not be double-piped;
        // the plain render is already valid JSON and parses directly.
        let renderer = TemplateRenderer::new();
        let mut ctx = HashMap::new();
        ctx.insert(
            "page".to_string(),
            serde_json::json!({"next_cursor": null, "limit": 10}),
        );
        let out = renderer
            .render_to_value("{{ page | tojson }}", &ctx)
            .unwrap();
        assert!(out.is_object());
        assert!(out["next_cursor"].is_null());
    }

    #[test]
    fn test_issue_89_scalar_renders_unaffected() {
        // The retry only fires for container-shaped output; scalars keep
        // their existing typed parsing (number stays a number, etc.).
        let renderer = TemplateRenderer::new();
        let ctx = make_context();
        assert_eq!(
            renderer.render_to_value("{{ age }}", &ctx).unwrap(),
            serde_json::json!(30)
        );
        assert_eq!(
            renderer.render_to_value("{{ name }}", &ctx).unwrap(),
            serde_json::json!("Alice")
        );
        assert_eq!(
            renderer.render_to_value("{{ active }}", &ctx).unwrap(),
            serde_json::json!(true)
        );
    }

}
