//! Workload-form inference from a playbook / agent YAML payload.
//!
//! Port of the Python `noetl.server.api.mcp.ui_schema` helper —
//! kept structurally close so Rust-side output is byte-identical
//! to the Python wire shape for the same input.
//!
//! Public entry point: [`infer_ui_schema`].
//!
//! ## Approach
//!
//! 1. Parse the document with `serde_yaml` for the structural shape.
//! 2. Scan the raw text for inline `# ui:` directives next to each
//!    workload key. The directive parser is a small regex; it
//!    ignores anything it can't recognise so a malformed comment
//!    never breaks registration.
//!
//! Supported directives (always after the key/value on the same
//! line):
//!
//! - `# ui:secret` — mark the field as masked input.
//! - `# ui:enum=[a,b,c]` — force `kind=enum` and populate options.
//! - `# ui:credential=pg_*` — restrict to a credential picker
//!   filtered by glob.
//! - `# ui:description=Some help text` — per-field description.
//!
//! The inference is intentionally forgiving: malformed YAML or
//! unknown directives just return an empty / less-rich schema
//! rather than raising. Callers should treat the returned schema
//! as a best-effort hint, not a strict contract.

use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// One workload field inferred from a playbook's YAML.
///
/// Mirrors `noetl.server.api.mcp.schema.UiSchemaField` — see that
/// module's docstring for the inference rules.
///
/// Optional fields are serialized as explicit `null` rather than
/// omitted to match the Python pydantic wire shape (which has no
/// `exclude_none` config on this model).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiSchemaField {
    /// Workload key.
    pub name: String,

    /// Field kind: `string|integer|number|boolean|object|array|null|enum`.
    pub kind: String,

    /// Default value parsed from YAML.
    #[serde(default)]
    pub default: serde_json::Value,

    /// Human-readable hint, from a `# ui:description=...` comment.
    pub description: Option<String>,

    /// True when `# ui:secret` directive is present.
    #[serde(default)]
    pub secret: bool,

    /// Credential picker filter (e.g. `pg_*`).
    pub credential_glob: Option<String>,

    /// Enum options, when `kind=enum`.
    pub options: Option<Vec<serde_json::Value>>,

    /// Nested fields when `kind=object`.
    pub children: Option<Vec<UiSchemaField>>,
}

/// Inferred workload form for a catalog resource (Playbook | Agent | Mcp).
///
/// Mirrors `noetl.server.api.mcp.schema.UiSchemaResponse`.  Optional
/// fields are serialized as `null` (not omitted) to match the
/// Python pydantic wire shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiSchemaResponse {
    /// Catalog path.
    pub path: String,

    /// Catalog version inspected (Postgres `smallint`).
    pub version: i16,

    /// Resource kind (Playbook | Agent | Mcp), lower-cased.
    pub kind: String,

    /// `metadata.name` from the resource.
    pub title: Option<String>,

    /// `metadata.description` rendered as markdown source.
    pub description_markdown: Option<String>,

    /// True when `metadata.exposed_in_ui` is set on the resource.
    #[serde(default)]
    pub exposed_in_ui: bool,

    /// Top-level workload fields.
    #[serde(default)]
    pub fields: Vec<UiSchemaField>,

    /// When this schema was inferred.
    pub generated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Compiled regexes
// ---------------------------------------------------------------------------

/// Match each `# ui:foo[=bar]` token within a line.
///
/// The Python version uses a `(?=(?:\s+#|\s*$))` lookahead to stop
/// at the next directive. Rust's `regex` crate has no lookahead, but
/// the value class `[^\n#]*` already cannot contain another `#`, so
/// the simpler greedy form has the same behaviour (the trailing
/// whitespace before the next `#` is just absorbed into the value
/// and trimmed afterward).
fn directive_inline_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"#\s*ui:(?P<key>[A-Za-z_][A-Za-z0-9_]*)(?:\s*=\s*(?P<value>[^\n#]*))?",
        )
        .expect("static regex must compile")
    })
}

/// Match the start of a top-level workload key line so we can
/// correlate the parsed dict back to the raw text. Accepts any indent
/// of two or more spaces so this works for the common 2-space indent
/// and the also-valid 4-space style.
fn top_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?P<indent> {2,})(?P<name>[A-Za-z_][A-Za-z0-9_-]*)\s*:")
            .expect("static regex must compile")
    })
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Return ordered top-level workload fields inferred from the YAML.
///
/// Empty list when the document has no `workload:` block, when the
/// YAML can't be parsed, or when `workload:` isn't a mapping.
pub fn infer_ui_schema(yaml_text: &str) -> Vec<UiSchemaField> {
    if yaml_text.trim().is_empty() {
        return Vec::new();
    }

    let parsed: serde_yaml::Value = match serde_yaml::from_str(yaml_text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let workload = match parsed.get("workload") {
        Some(serde_yaml::Value::Mapping(m)) => m,
        _ => return Vec::new(),
    };

    let directives = scan_inline_directives(yaml_text);
    let mut fields = Vec::with_capacity(workload.len());
    for (key, value) in workload {
        let name = match key {
            serde_yaml::Value::String(s) => s.clone(),
            other => serde_yaml::to_string(other).unwrap_or_default().trim().to_string(),
        };
        let directive = directives.get(&name).cloned().unwrap_or_default();
        fields.push(field_from_value(&name, value, &directive));
    }
    fields
}

// ---------------------------------------------------------------------------
// Inline-comment scanning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
struct Directive {
    secret: bool,
    enum_options: Option<Vec<String>>,
    credential: Option<String>,
    description: Option<String>,
}

/// Walk the raw text once and pull `# ui:` directives per top-level
/// workload key.
///
/// Returns a mapping of `key_name -> Directive` aggregated from inline
/// comments on the same line as the key/value. Only the immediate
/// children of `workload:` are considered — without that filter the
/// parser would happily attach a `# ui:secret` on a nested mapping
/// to an unrelated top-level field that happens to share the same
/// key name. Phase 1 mirrors the Python helper exactly here.
///
/// Detection of "immediate child" is data-driven: the first non-empty,
/// deeper-than-`workload:` line we see fixes `child_indent` for the
/// rest of the block. Lines at deeper indents are skipped.
fn scan_inline_directives(yaml_text: &str) -> HashMap<String, Directive> {
    let mut out: HashMap<String, Directive> = HashMap::new();
    let mut in_workload = false;
    let mut workload_indent: i32 = -1;
    let mut child_indent: i32 = -1;

    for line in yaml_text.split('\n') {
        let stripped = line.trim_start();
        if stripped.is_empty() {
            continue;
        }

        if stripped.starts_with("workload:") {
            in_workload = true;
            workload_indent = (line.len() - stripped.len()) as i32;
            child_indent = -1;
            continue;
        }

        if !in_workload {
            continue;
        }

        let indent = (line.len() - stripped.len()) as i32;
        if indent <= workload_indent {
            // Left the workload block.
            in_workload = false;
            child_indent = -1;
            continue;
        }

        // Establish the immediate-child indent on the first qualifying line.
        if child_indent == -1 {
            child_indent = indent;
        }

        // Filter strictly to the immediate children of `workload:`.
        if indent != child_indent {
            continue;
        }

        let Some(caps) = top_key_re().captures(line) else {
            continue;
        };
        let key_name = caps.name("name").unwrap().as_str().to_string();
        let directive = extract_directives_from_line(line);
        if !is_empty_directive(&directive) {
            out.insert(key_name, directive);
        }
    }
    out
}

fn is_empty_directive(d: &Directive) -> bool {
    !d.secret && d.enum_options.is_none() && d.credential.is_none() && d.description.is_none()
}

/// Pull every `# ui:foo` / `# ui:foo=bar` token from a single raw line.
fn extract_directives_from_line(line: &str) -> Directive {
    let mut d = Directive::default();
    for caps in directive_inline_re().captures_iter(line) {
        let key = caps.name("key").map(|m| m.as_str()).unwrap_or("");
        let raw_value = caps
            .name("value")
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        match key {
            "secret" => {
                d.secret = true;
            }
            "enum" => {
                let parsed = parse_directive_value_list(&raw_value);
                d.enum_options = Some(parsed);
            }
            "credential" => {
                if !raw_value.is_empty() {
                    d.credential = Some(strip_quotes(&raw_value));
                }
            }
            "description" => {
                if !raw_value.is_empty() {
                    d.description = Some(strip_quotes(&raw_value));
                }
            }
            _ => {
                // Unknown directive — silently ignored to mirror Python.
            }
        }
    }
    d
}

/// Parse the value half of a `# ui:foo=bar` directive into a list of
/// strings. Used for `enum=[a,b,c]`. Mirrors Python's
/// `_parse_directive_value` shape for the list case.
fn parse_directive_value_list(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = trimmed[1..trimmed.len() - 1].trim();
        if inner.is_empty() {
            return Vec::new();
        }
        return inner
            .split(',')
            .map(|piece| strip_quotes(piece.trim()))
            .filter(|s| !s.is_empty())
            .collect();
    }
    // Single value, no brackets — wrap into a single-element list to
    // match Python's behaviour: `enum_options=[value]`.
    vec![strip_quotes(trimmed)]
}

fn strip_quotes(s: &str) -> String {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

// ---------------------------------------------------------------------------
// Field construction
// ---------------------------------------------------------------------------

fn field_from_value(
    name: &str,
    value: &serde_yaml::Value,
    directives: &Directive,
) -> UiSchemaField {
    let description = directives.description.clone();
    let secret = directives.secret;
    let credential_glob = directives.credential.clone();

    if let Some(enum_options) = &directives.enum_options {
        let options_json: Vec<serde_json::Value> = enum_options
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect();
        return UiSchemaField {
            name: name.to_string(),
            kind: "enum".to_string(),
            default: yaml_to_json(value),
            description,
            secret,
            credential_glob,
            options: Some(options_json),
            children: None,
        };
    }

    // Mirror Python's type-branching: bool → boolean, integer → integer,
    // float → number, null → null, mapping → object (recurse), sequence
    // → array, anything else → string.
    if value.is_bool() {
        return UiSchemaField {
            name: name.to_string(),
            kind: "boolean".to_string(),
            default: yaml_to_json(value),
            description,
            secret,
            credential_glob,
            options: None,
            children: None,
        };
    }

    if let Some(n) = value.as_i64() {
        return UiSchemaField {
            name: name.to_string(),
            kind: "integer".to_string(),
            default: serde_json::Value::Number(serde_json::Number::from(n)),
            description,
            secret,
            credential_glob,
            options: None,
            children: None,
        };
    }
    if let Some(n) = value.as_u64() {
        return UiSchemaField {
            name: name.to_string(),
            kind: "integer".to_string(),
            default: serde_json::Value::Number(serde_json::Number::from(n)),
            description,
            secret,
            credential_glob,
            options: None,
            children: None,
        };
    }
    if let Some(n) = value.as_f64() {
        // f64 → JSON number, falling back to null on NaN/inf which
        // mirrors Python's `json.dumps` rejecting non-finite floats.
        let default = serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
        return UiSchemaField {
            name: name.to_string(),
            kind: "number".to_string(),
            default,
            description,
            secret,
            credential_glob,
            options: None,
            children: None,
        };
    }

    if value.is_null() {
        return UiSchemaField {
            name: name.to_string(),
            kind: "null".to_string(),
            default: serde_json::Value::Null,
            description,
            secret,
            credential_glob,
            options: None,
            children: None,
        };
    }

    if let serde_yaml::Value::Mapping(map) = value {
        let children: Vec<UiSchemaField> = map
            .iter()
            .map(|(k, v)| {
                let child_name = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    other => serde_yaml::to_string(other)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                };
                field_from_value(&child_name, v, &Directive::default())
            })
            .collect();
        return UiSchemaField {
            name: name.to_string(),
            kind: "object".to_string(),
            default: yaml_to_json(value),
            description,
            secret,
            credential_glob,
            options: None,
            children: Some(children),
        };
    }

    if value.is_sequence() {
        return UiSchemaField {
            name: name.to_string(),
            kind: "array".to_string(),
            default: yaml_to_json(value),
            description,
            secret,
            credential_glob,
            options: None,
            children: None,
        };
    }

    // Fallback: string (or anything else like tag).
    UiSchemaField {
        name: name.to_string(),
        kind: "string".to_string(),
        default: yaml_to_json(value),
        description,
        secret,
        credential_glob,
        options: None,
        children: None,
    }
}

/// Convert a `serde_yaml::Value` into a `serde_json::Value` for the
/// `default` slot of the response. We round-trip through JSON
/// serialisation rather than re-implementing the mapping by hand —
/// serde handles the boolean/number/string/null/array/object cases
/// faithfully.
fn yaml_to_json(value: &serde_yaml::Value) -> serde_json::Value {
    // serde_yaml + serde_json don't share a direct converter, so we
    // round-trip through a JSON string. Safe because all YAML scalars
    // we care about (bool / int / float / string / null) survive the
    // round trip without surprises.
    match serde_json::to_value(value) {
        Ok(v) => v,
        Err(_) => serde_json::Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn first_field(yaml: &str) -> UiSchemaField {
        let fields = infer_ui_schema(yaml);
        assert_eq!(fields.len(), 1, "expected 1 field for yaml: {yaml}");
        fields.into_iter().next().unwrap()
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(infer_ui_schema("").is_empty());
        assert!(infer_ui_schema("   ").is_empty());
        assert!(infer_ui_schema("\n\n").is_empty());
    }

    #[test]
    fn no_workload_block_returns_empty() {
        let yaml = "apiVersion: v1\nkind: Playbook\nmetadata:\n  name: foo\n";
        assert!(infer_ui_schema(yaml).is_empty());
    }

    #[test]
    fn malformed_yaml_returns_empty() {
        // Unbalanced quotes.
        let yaml = "workload:\n  key: 'unterminated\n";
        assert!(infer_ui_schema(yaml).is_empty());
    }

    #[test]
    fn workload_not_mapping_returns_empty() {
        let yaml = "workload: 42\n";
        assert!(infer_ui_schema(yaml).is_empty());

        let yaml = "workload:\n  - a\n  - b\n";
        assert!(infer_ui_schema(yaml).is_empty());
    }

    #[test]
    fn scalar_string_field() {
        let yaml = "workload:\n  name: hello\n";
        let f = first_field(yaml);
        assert_eq!(f.name, "name");
        assert_eq!(f.kind, "string");
        assert_eq!(f.default, serde_json::json!("hello"));
        assert!(!f.secret);
    }

    #[test]
    fn integer_field() {
        let yaml = "workload:\n  count: 42\n";
        let f = first_field(yaml);
        assert_eq!(f.kind, "integer");
        assert_eq!(f.default, serde_json::json!(42));
    }

    #[test]
    fn number_field() {
        let yaml = "workload:\n  ratio: 2.72\n";
        let f = first_field(yaml);
        assert_eq!(f.kind, "number");
        assert_eq!(f.default, serde_json::json!(2.72));
    }

    #[test]
    fn boolean_field() {
        let yaml = "workload:\n  enabled: true\n";
        let f = first_field(yaml);
        assert_eq!(f.kind, "boolean");
        assert_eq!(f.default, serde_json::json!(true));
    }

    #[test]
    fn null_field() {
        let yaml = "workload:\n  empty: null\n";
        let f = first_field(yaml);
        assert_eq!(f.kind, "null");
        assert_eq!(f.default, serde_json::Value::Null);
    }

    #[test]
    fn array_field() {
        let yaml = "workload:\n  items: [1, 2, 3]\n";
        let f = first_field(yaml);
        assert_eq!(f.kind, "array");
        assert_eq!(f.default, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn object_field_with_children() {
        let yaml = "workload:\n  db:\n    host: localhost\n    port: 5432\n";
        let f = first_field(yaml);
        assert_eq!(f.kind, "object");
        let children = f.children.expect("object field should have children");
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].name, "host");
        assert_eq!(children[0].kind, "string");
        assert_eq!(children[1].name, "port");
        assert_eq!(children[1].kind, "integer");
    }

    #[test]
    fn directive_secret() {
        let yaml = "workload:\n  password: hunter2  # ui:secret\n";
        let f = first_field(yaml);
        assert!(f.secret);
        assert_eq!(f.kind, "string");
    }

    #[test]
    fn directive_description() {
        let yaml = "workload:\n  host: localhost  # ui:description=The DB host\n";
        let f = first_field(yaml);
        assert_eq!(f.description.as_deref(), Some("The DB host"));
    }

    #[test]
    fn directive_credential() {
        let yaml = "workload:\n  alias: pg_main  # ui:credential=pg_*\n";
        let f = first_field(yaml);
        assert_eq!(f.credential_glob.as_deref(), Some("pg_*"));
    }

    #[test]
    fn directive_enum() {
        let yaml = "workload:\n  level: info  # ui:enum=[debug,info,warn,error]\n";
        let f = first_field(yaml);
        assert_eq!(f.kind, "enum");
        let options = f.options.expect("enum should have options");
        assert_eq!(options.len(), 4);
        assert_eq!(options[0], serde_json::json!("debug"));
        assert_eq!(options[3], serde_json::json!("error"));
    }

    #[test]
    fn multiple_directives_one_line() {
        let yaml = "workload:\n  pw: hunter2  # ui:secret # ui:description=API key\n";
        let f = first_field(yaml);
        assert!(f.secret);
        assert_eq!(f.description.as_deref(), Some("API key"));
    }

    #[test]
    fn nested_directive_does_not_leak_to_top_level() {
        // Phase 1 deliberately ignores nested directives — a
        // `# ui:secret` on `db.password` should NOT attach to a
        // top-level field named `password`.
        let yaml = "\
workload:
  db:
    password: hunter2  # ui:secret
  password: visible
";
        let fields = infer_ui_schema(yaml);
        assert_eq!(fields.len(), 2);
        let db_field = &fields[0];
        let pw_field = &fields[1];
        assert_eq!(db_field.kind, "object");
        assert_eq!(pw_field.name, "password");
        // The top-level password has no directive — Phase 1 ignores
        // the nested secret directive on db.password.
        assert!(!pw_field.secret);
    }

    #[test]
    fn enum_directive_overrides_value_kind() {
        // Even if the underlying value is a string, an enum directive
        // forces kind=enum and populates options.
        let yaml = "workload:\n  region: us-east-1  # ui:enum=[us-east-1,us-west-2,eu-west-1]\n";
        let f = first_field(yaml);
        assert_eq!(f.kind, "enum");
        assert_eq!(f.default, serde_json::json!("us-east-1"));
    }

    #[test]
    fn four_space_indent_supported() {
        // The _TOP_KEY_RE accepts any indent of two or more spaces.
        let yaml = "workload:\n    name: hello  # ui:description=greet\n";
        let f = first_field(yaml);
        assert_eq!(f.description.as_deref(), Some("greet"));
    }

    #[test]
    fn quoted_directive_value() {
        let yaml = "workload:\n  host: localhost  # ui:description=\"with spaces\"\n";
        let f = first_field(yaml);
        assert_eq!(f.description.as_deref(), Some("with spaces"));
    }

    #[test]
    fn workload_followed_by_other_top_level_keys() {
        // Make sure we stop scanning when workload: ends.
        let yaml = "\
workload:
  name: hello  # ui:description=greet
start:
  secret: should-not-attach  # ui:secret
";
        let fields = infer_ui_schema(yaml);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].description.as_deref(), Some("greet"));
        assert!(!fields[0].secret);
    }

    #[test]
    fn enum_empty_brackets_returns_empty_list() {
        let yaml = "workload:\n  level: info  # ui:enum=[]\n";
        let f = first_field(yaml);
        assert_eq!(f.kind, "enum");
        assert_eq!(f.options.as_ref().map(Vec::len), Some(0));
    }

    #[test]
    fn field_order_preserved() {
        let yaml = "workload:\n  alpha: 1\n  beta: 2\n  gamma: 3\n";
        let fields = infer_ui_schema(yaml);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }
}
