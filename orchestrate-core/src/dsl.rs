//! The DSL knowledge base, generated from the code (ai-meta RFC
//! `docs/rfc/dsl-knowledge-base.md`, Phase 1).
//!
//! An AI or a person asking "what can I write against this NoETL?" must get an
//! answer derived from the types that will parse the playbook, not from prose
//! that drifts. This module is that answer, in two parts:
//!
//! - [`manifest`]: every tool kind in [`ToolKind::ALL`] with its gate state,
//!   plus the grammar's `apiVersion` / document `kind`. Pure, no extra deps.
//! - [`playbook_schema`] (feature `json-schema`): the playbook JSON Schema
//!   derived by `schemars` from the authoring types in [`crate::playbook`], with
//!   `tool.kind` narrowed to the kinds the given [`ToolKindGates`] accept. So
//!   the schema a server serves matches, flag for flag, what its
//!   `ToolSpec.kind` deserialiser accepts.
//!
//! ## What the schema does NOT claim
//!
//! - **It is permissive where the parser is permissive.** No
//!   `additionalProperties: false`: `ToolSpec` flattens an `extra` map and real
//!   playbooks carry kind-specific keys. A stricter schema would reject
//!   playbooks the server runs, and would teach a generator a grammar the
//!   server does not have (the lesson of noetl/ai-meta#201).
//! - **It is closed where the parser is closed.** `tool.kind` is an enum,
//!   because [`ToolKind`] is one. This is the correction to the CLI-generated
//!   schema, whose `kind` is an open string.
//! - **Semantic rules are not in it.** A `start` step, unique step names,
//!   `next` targets that exist: those live in the server's `validate_playbook`
//!   and are enforced by running it (RFC Phase 2 `POST /api/dsl/validate`), not
//!   by describing it here.
//! - **"Accepted" is not "executable".** This is the parser's vocabulary. Which
//!   kinds a live worker can actually run is reported by workers (RFC Phase 3);
//!   the manifest says so rather than guessing.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::playbook::{ToolKind, ToolKindGates};

/// The grammar version every playbook must declare as `apiVersion`.
pub const API_VERSION: &str = "noetl.io/v2";
/// The document `kind` of an executable playbook.
pub const PLAYBOOK_KIND: &str = "Playbook";
/// This crate's version: the version of the grammar's source of truth.
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Canonical location of the published schema: noetl.ai is the machine-readable
/// home (RFC §2.8); noetl.dev renders the human view and stays docs-only.
pub const SCHEMA_ID: &str = "https://noetl.ai/schema/v2/playbook.schema.json";

/// One tool kind as the knowledge base reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolKindEntry {
    /// Wire name (`tool.kind` value).
    pub name: String,
    /// Is this kind behind a per-kind flag?
    pub gated: bool,
    /// The flag that enables it, when gated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_env: Option<&'static str>,
    /// Does the parser accept it under the given gates?
    pub enabled: bool,
}

/// Every kind in [`ToolKind::ALL`], sorted by name, with its state under `gates`.
///
/// Gated-off kinds are **listed** (with `enabled: false` and the flag that
/// enables them) so a generator learns they exist but must not be used, which
/// is the same distinction the registration error draws.
pub fn tool_kinds(gates: ToolKindGates) -> Vec<ToolKindEntry> {
    let mut v: Vec<ToolKindEntry> = ToolKind::ALL
        .iter()
        .map(|k| ToolKindEntry {
            name: k.to_string(),
            gated: k.gate_env().is_some(),
            gate_env: k.gate_env(),
            enabled: gates.allows(k),
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// The tool-kind manifest under `gates`.
///
/// `executable` is reported as `"not_reported"`: this process cannot know what
/// a worker has registered until workers report their capabilities (RFC
/// Phase 3). Saying so is the point: the field exists so a consumer never
/// mistakes "the parser accepts it" for "something can run it".
pub fn manifest(gates: ToolKindGates) -> Value {
    let kinds = tool_kinds(gates);
    let enabled: Vec<&str> = kinds
        .iter()
        .filter(|k| k.enabled)
        .map(|k| k.name.as_str())
        .collect();
    json!({
        "api_version": API_VERSION,
        "document_kind": PLAYBOOK_KIND,
        "orchestrate_core_version": CORE_VERSION,
        "tool_kinds": kinds,
        "enabled_tool_kinds": enabled,
        "executable": "not_reported",
        "executable_note": "Kinds the parser accepts. Which kinds a live worker can execute is \
                            reported by workers (RFC dsl-knowledge-base Phase 3) and is not \
                            known to this process.",
    })
}

/// The playbook JSON Schema (draft 2020-12) with `tool.kind` narrowed to `gates`.
#[cfg(feature = "json-schema")]
pub fn playbook_schema(gates: ToolKindGates) -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(crate::playbook::Playbook))
        .expect("schemars emits valid JSON");
    if let Some(obj) = schema.as_object_mut() {
        obj.insert(
            "$schema".into(),
            Value::String("https://json-schema.org/draft/2020-12/schema".into()),
        );
        obj.insert("$id".into(), Value::String(SCHEMA_ID.into()));
        obj.insert("title".into(), Value::String("NoETL Playbook".into()));
        obj.insert(
            "description".into(),
            Value::String(format!(
                "NoETL playbook grammar ({API_VERSION}), generated from the Rust authoring types \
                 in noetl-orchestrate-core {CORE_VERSION}. Permissive like the parser (unknown \
                 keys are not rejected); `tool.kind` is closed to the kinds this server \
                 accepts. Semantic rules (a `start` step, unique step names, existing `next` \
                 targets) are enforced by the server's validator, not by this schema."
            )),
        );
        if let Some(defs) = obj.get_mut("$defs").and_then(Value::as_object_mut) {
            if let Some(tk) = defs.get_mut("ToolKind") {
                narrow_tool_kind(tk, gates);
            }
        }
    }
    schema
}

/// Narrow a schemars-emitted `ToolKind` definition to the kinds `gates` allows.
///
/// schemars renders a unit-variant enum either as `{"enum": [...]}` or, when
/// variants carry doc comments, as `{"oneOf": [{"const": .., "description": ..},
/// {"enum": [...]}, ...]}`. Both shapes are filtered in place so the variant
/// descriptions survive.
#[cfg(feature = "json-schema")]
fn narrow_tool_kind(def: &mut Value, gates: ToolKindGates) {
    let allowed = |v: &Value| -> bool {
        v.as_str()
            .and_then(|s| ToolKind::parse_with(s, gates))
            .is_some()
    };
    let Some(obj) = def.as_object_mut() else {
        return;
    };
    if let Some(Value::Array(vals)) = obj.get_mut("enum") {
        vals.retain(allowed);
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(alts)) = obj.get_mut(key) {
            for alt in alts.iter_mut() {
                if let Some(Value::Array(vals)) = alt.get_mut("enum") {
                    vals.retain(allowed);
                }
            }
            alts.retain(|alt| match (alt.get("const"), alt.get("enum")) {
                (Some(c), _) => allowed(c),
                (None, Some(Value::Array(vals))) => !vals.is_empty(),
                _ => true,
            });
        }
    }
}

/// Serialise `v` with object keys sorted at every level.
///
/// The digest must not depend on whether some other crate in the build turned
/// on serde_json's `preserve_order` feature (Cargo unifies features), so key
/// order is fixed here rather than inherited from `Map`'s backing type.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).expect("string serialises"));
                out.push(':');
                write_canonical(&map[*k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&serde_json::to_string(scalar).expect("scalar serialises")),
    }
}

/// `sha256:<hex>` of [`canonical_json`]: the identity of a schema or manifest.
pub fn digest(v: &Value) -> String {
    let mut h = Sha256::new();
    h.update(canonical_json(v).as_bytes());
    format!("sha256:{}", hex::encode(h.finalize()))
}

/// Pretty JSON with sorted keys and a trailing newline: the committed-artifact
/// form, stable across serde_json feature sets.
pub fn pretty_canonical(v: &Value) -> String {
    // `Map` is a BTreeMap unless some crate in the build enables serde_json's
    // `preserve_order`, in which case it keeps insertion order. Rebuilding it
    // with keys inserted in sorted order is sorted under both.
    let mut s = serde_json::to_string_pretty(&sort_value(v.clone())).expect("serialises");
    s.push('\n');
    s
}

fn sort_value(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut m = Map::new();
            for (k, v) in entries {
                m.insert(k, sort_value(v));
            }
            Value::Object(m)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_value).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_lists_every_kind_once_sorted() {
        let kinds = tool_kinds(ToolKindGates::OFF);
        assert_eq!(kinds.len(), ToolKind::ALL.len());
        let names: Vec<&str> = kinds.iter().map(|k| k.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names, sorted, "sorted and unique");
    }

    #[test]
    fn gated_kinds_are_listed_but_disabled_when_off() {
        for k in tool_kinds(ToolKindGates::OFF) {
            if let Some(env) = k.gate_env {
                assert!(k.gated && !k.enabled, "{} must be off by default", k.name);
                assert!(env.starts_with("NOETL_TOOL_KIND_"));
            } else {
                assert!(!k.gated && k.enabled, "{} is ungated", k.name);
            }
        }
        assert!(tool_kinds(ToolKindGates::ALL_ON).iter().all(|k| k.enabled));
    }

    #[test]
    fn enabled_set_agrees_with_the_registration_oracle() {
        // The manifest must say exactly what `ToolKind::parse_with` (the
        // registration validator's oracle) accepts. Two answers to one question
        // are two chances to disagree.
        for gates in [
            ToolKindGates::OFF,
            ToolKindGates::ALL_ON,
            ToolKindGates {
                mcp: true,
                agent: false,
            },
        ] {
            for k in tool_kinds(gates) {
                assert_eq!(
                    k.enabled,
                    ToolKind::parse_with(&k.name, gates).is_some(),
                    "{} under {gates:?}",
                    k.name
                );
            }
        }
    }

    #[test]
    fn manifest_never_claims_executability() {
        assert_eq!(
            manifest(ToolKindGates::ALL_ON)["executable"],
            "not_reported"
        );
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_level() {
        let v = json!({"b": 1, "a": {"d": [ {"z": 1, "y": 2} ], "c": null}});
        assert_eq!(
            canonical_json(&v),
            r#"{"a":{"c":null,"d":[{"y":2,"z":1}]},"b":1}"#
        );
    }

    #[test]
    fn digest_is_stable_and_order_independent() {
        let a = json!({"x": 1, "y": [1, 2]});
        let b = json!({"y": [1, 2], "x": 1});
        assert_eq!(digest(&a), digest(&b));
        assert_ne!(digest(&a), digest(&json!({"x": 1, "y": [2, 1]})));
        assert!(digest(&a).starts_with("sha256:"));
    }
}

#[cfg(all(test, feature = "json-schema"))]
mod schema_tests {
    use super::*;

    fn tool_kind_values(schema: &Value) -> Vec<String> {
        let def = &schema["$defs"]["ToolKind"];
        let mut out = Vec::new();
        let mut take = |v: &Value| {
            if let Some(s) = v.as_str() {
                out.push(s.to_string());
            }
        };
        if let Some(vals) = def["enum"].as_array() {
            vals.iter().for_each(&mut take);
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(alts) = def[key].as_array() {
                for alt in alts {
                    if let Some(c) = alt.get("const") {
                        take(c);
                    }
                    if let Some(vals) = alt["enum"].as_array() {
                        vals.iter().for_each(&mut take);
                    }
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn schema_tool_kind_enum_is_exactly_the_open_vocabulary() {
        for gates in [
            ToolKindGates::OFF,
            ToolKindGates::ALL_ON,
            ToolKindGates {
                mcp: true,
                agent: false,
            },
        ] {
            let got = tool_kind_values(&playbook_schema(gates));
            let mut want: Vec<String> = ToolKind::ALL
                .iter()
                .filter(|k| gates.allows(k))
                .map(|k| k.to_string())
                .collect();
            want.sort();
            assert_eq!(got, want, "under {gates:?}");
        }
    }

    #[test]
    fn digest_changes_iff_gates_change() {
        let off = digest(&playbook_schema(ToolKindGates::OFF));
        assert_eq!(off, digest(&playbook_schema(ToolKindGates::OFF)));
        assert_ne!(off, digest(&playbook_schema(ToolKindGates::ALL_ON)));
    }

    #[test]
    fn schema_requires_what_the_types_require() {
        let s = playbook_schema(ToolKindGates::OFF);
        let mut req: Vec<&str> = s["required"]
            .as_array()
            .expect("root has required")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        req.sort();
        // apiVersion, kind, metadata, workflow are non-Option on `Playbook`;
        // workload / vars / keychain / workbook default. If schemars ever
        // marked a defaulted Option required (the #201 trap), real playbooks
        // without `workload:` would fail.
        assert_eq!(req, vec!["apiVersion", "kind", "metadata", "workflow"]);
        let step_req: Vec<&str> = s["$defs"]["Step"]["required"]
            .as_array()
            .expect("Step has required")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let mut step_req = step_req;
        step_req.sort();
        assert_eq!(step_req, vec!["step", "tool"]);
        assert_eq!(s["$defs"]["ToolSpec"]["required"], json!(["kind"]));
    }

    #[test]
    fn schema_is_not_stricter_than_the_parser() {
        let s = canonical_json(&playbook_schema(ToolKindGates::ALL_ON));
        assert!(
            !s.contains(r#""additionalProperties":false"#),
            "the parser accepts unknown keys; the schema must not reject them"
        );
    }
}
