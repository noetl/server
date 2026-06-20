//! Static input-binding analysis for the atomic-working-item context contract
//! (RFC noetl/ai-meta#115 Phase 5 / tenet 6).
//!
//! Builds directly on the explicit input-binding work (noetl/ai-meta#77, shipped
//! BREAKING as v3.0.0): a step declares what it consumes via `input:` / `args:`
//! and the tool's own templates (`command:` / `query:` / `code:` / `url:` …).
//! This module statically extracts the set of **base dispatch-context top-level
//! keys** a step's tool definition references, so the drive can hand a worker a
//! *minimal* working-item context — only the upstream step outputs / refs the
//! step actually binds — instead of the whole accumulated context (§6.1).
//!
//! ## Conservative by construction
//!
//! Narrowing is a pure optimization that only ever drops keys a step provably
//! never names. If *any* reference can't be statically bounded — a whole-context
//! `{{ ctx }}` spread, an unparseable fragment — [`analyze`] reports
//! `bounded = false` and [`project_context`] returns `None`, so the caller
//! passes the **full** context unchanged (today's behavior). A narrowed context
//! is therefore always a superset of what the worker needs to re-render any
//! deferred template: the server still renders the tool against the full context
//! server-side, and the worker rebuilds its `ctx` / `workload` shims from
//! whatever flat context arrives, so every retained key resolves.
//!
//! ## Reference → base-key mapping
//!
//! The flat dispatch context carries each upstream step's output under its step
//! name as a top-level key, plus the structured `workload` block (the `ctx`
//! namespace is a worker-rebuilt alias of the same flat map — it is not itself a
//! base key). So a template reference maps to a base key as:
//!
//! - `{{ ctx.foo }}` → base key `foo` (ctx aliases the flat map; the real key is
//!   the segment after `ctx`). A bare `{{ ctx }}` spread is **unbounded**.
//! - `{{ workload.x }}` → base key `workload` (the structured block).
//! - `{{ generate_data.id }}` / `{{ generate_data }}` → base key `generate_data`.
//! - `{{ iter.item }}`, `{{ _prev }}`, `{{ output.x }}` … → no base key (these
//!   are injected at execution time, per loop iteration or per pipeline tool
//!   item, not carried in the base dispatch context).

use std::collections::{BTreeSet, HashMap};

use minijinja::Environment;
use serde_json::Value;

use crate::playbook::Step;

/// Roots injected into the render context at execution time (per loop iteration,
/// per pipeline tool item, or as a tool-result namespace) rather than carried in
/// the base dispatch context. Referencing one of these does **not** require
/// keeping a base-context key.
const INJECTED_ROOTS: &[&str] = &[
    "iter",
    "item",
    "loop",
    "_prev",
    "_results",
    "outcome",
    "output",
    "result",
    "__cursor_frame",
    "__cursor_row",
];

/// Result of statically analyzing a step's tool definition for the base-context
/// keys it references.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclaredInputs {
    /// Base dispatch-context top-level keys the step's tool templates reference.
    pub needed_keys: BTreeSet<String>,
    /// True when every reference resolved to a concrete base key (or an
    /// injected/local root). False forces the full-context fallback —
    /// [`project_context`] returns `None`.
    pub bounded: bool,
}

/// Statically analyze a step's tool definition (+ step-level `input:`/`args:`)
/// for the set of base-context keys it references. Pure; no rendering, no I/O.
pub fn analyze(step: &Step) -> DeclaredInputs {
    let mut templates: Vec<String> = Vec::new();

    // The tool definition is what the drive renders into the worker command and
    // ships; its templates (rendered server-side AND those deferred to the
    // worker) bound the base keys the worker can possibly read.
    if let Ok(tool_v) = serde_json::to_value(&step.tool) {
        collect_templates(&tool_v, &mut templates);
    }
    // Step-level `input:`/`args:` (Step.args) feeds the step's context too.
    if let Some(args) = &step.args {
        if let Ok(args_v) = serde_json::to_value(args) {
            collect_templates(&args_v, &mut templates);
        }
    }

    let env = Environment::new();
    let mut needed = BTreeSet::new();
    let mut bounded = true;

    for tmpl in &templates {
        match env.template_from_str(tmpl) {
            Ok(t) => {
                for var in t.undeclared_variables(true) {
                    if !classify(&var, &mut needed) {
                        bounded = false;
                    }
                }
            }
            // A fragment that doesn't parse as a standalone template (odd /
            // partial syntax) can't be bounded — fall back to full context.
            Err(_) => bounded = false,
        }
    }

    DeclaredInputs {
        needed_keys: needed,
        bounded,
    }
}

/// Given the full flat dispatch context and a step, return a narrowed context
/// holding only the base keys the step references — or `None` when the step
/// can't be statically bounded (the caller then passes the full context
/// unchanged). Keys named but absent from `full` are simply omitted (the worker
/// resolves them to undefined exactly as it would under the full context).
pub fn project_context(
    step: &Step,
    full: &HashMap<String, Value>,
) -> Option<HashMap<String, Value>> {
    let di = analyze(step);
    if !di.bounded {
        return None;
    }
    let mut out = HashMap::with_capacity(di.needed_keys.len());
    for k in &di.needed_keys {
        if let Some(v) = full.get(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    Some(out)
}

/// Map one (possibly dotted) undeclared-variable path to the base context key it
/// needs, inserting into `needed`. Returns `false` when the reference can't be
/// bounded (a bare whole-context `ctx` spread).
fn classify(path: &str, needed: &mut BTreeSet<String>) -> bool {
    let mut segs = path.split('.');
    let root = match segs.next() {
        Some(r) if !r.is_empty() => r,
        _ => return true,
    };
    if INJECTED_ROOTS.contains(&root) {
        return true;
    }
    if root == "ctx" {
        // `ctx` aliases the whole flat context; the real base key is the next
        // segment. A bare `{{ ctx }}` (no segment) is a whole-context spread.
        match segs.next() {
            Some(k) if !k.is_empty() => {
                needed.insert(k.to_string());
                true
            }
            _ => false,
        }
    } else {
        needed.insert(root.to_string());
        true
    }
}

/// Recursively collect every string value that carries a Jinja template marker.
fn collect_templates(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            if s.contains("{{") || s.contains("{%") {
                out.push(s.clone());
            }
        }
        Value::Array(a) => {
            for x in a {
                collect_templates(x, out);
            }
        }
        Value::Object(m) => {
            for x in m.values() {
                collect_templates(x, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step_from_yaml(y: &str) -> Step {
        serde_yaml::from_str(y).expect("parse step")
    }

    fn ctx(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn declared_input_binds_only_named_upstream_keys() {
        // A step whose single tool binds exactly two upstream outputs.
        let step = step_from_yaml(
            r#"
step: consume
tool:
  kind: python
  input:
    rec_id: "{{ generate_data.id }}"
    stats: "{{ ctx.stats_ref }}"
  code: "print(rec_id)"
"#,
        );
        let di = analyze(&step);
        assert!(di.bounded, "static references must bound");
        let keys: Vec<&str> = di.needed_keys.iter().map(|s| s.as_str()).collect();
        assert_eq!(keys, vec!["generate_data", "stats_ref"]);
    }

    #[test]
    fn project_drops_unreferenced_upstream_outputs() {
        let step = step_from_yaml(
            r#"
step: consume
tool:
  kind: shell
  command: "echo {{ generate_data.id }}"
"#,
        );
        let full = ctx(&[
            ("generate_data", serde_json::json!({"id": 7})),
            ("unrelated_big", serde_json::json!({"blob": "x".repeat(10_000)})),
            ("another_step", serde_json::json!({"k": "v"})),
        ]);
        let narrowed = project_context(&step, &full).expect("bounded");
        assert_eq!(narrowed.len(), 1);
        assert!(narrowed.contains_key("generate_data"));
        assert!(!narrowed.contains_key("unrelated_big"));
        assert!(!narrowed.contains_key("another_step"));
    }

    #[test]
    fn workload_reference_keeps_workload_block() {
        let step = step_from_yaml(
            r#"
step: consume
tool:
  kind: http
  url: "{{ workload.api_url }}/v1"
"#,
        );
        let di = analyze(&step);
        assert!(di.bounded);
        assert!(di.needed_keys.contains("workload"));
    }

    #[test]
    fn bare_ctx_spread_is_unbounded_fallback() {
        let step = step_from_yaml(
            r#"
step: consume
tool:
  kind: python
  input:
    everything: "{{ ctx | tojson }}"
  code: "pass"
"#,
        );
        let di = analyze(&step);
        assert!(!di.bounded, "a whole-context spread must force full context");
        let full = ctx(&[("a", serde_json::json!(1)), ("b", serde_json::json!(2))]);
        assert!(
            project_context(&step, &full).is_none(),
            "unbounded → caller passes full context"
        );
    }

    #[test]
    fn injected_roots_need_no_base_key() {
        // iter / _prev / output are injected per-iteration or per-tool-item.
        let step = step_from_yaml(
            r#"
step: body
tool:
  kind: python
  input:
    item: "{{ iter.item }}"
    prev: "{{ _prev.value }}"
  code: "print({{ output.x }})"
"#,
        );
        let di = analyze(&step);
        assert!(di.bounded);
        assert!(
            di.needed_keys.is_empty(),
            "injected roots add no base keys, got {:?}",
            di.needed_keys
        );
    }

    #[test]
    fn pipeline_items_are_walked() {
        // Flat (name-as-field) pipeline form: each item's input is scanned.
        let step = step_from_yaml(
            r#"
step: pipe
tool:
  - name: fetch
    kind: http
    input:
      url: "{{ source_cfg.endpoint }}"
  - name: store
    kind: postgres
    input:
      payload: "{{ ctx.row_ref }}"
    query: "INSERT INTO t VALUES ('{{ payload }}')"
"#,
        );
        let di = analyze(&step);
        assert!(di.bounded);
        assert!(di.needed_keys.contains("source_cfg"));
        assert!(di.needed_keys.contains("row_ref"));
    }

    #[test]
    fn no_templates_yields_empty_bounded() {
        let step = step_from_yaml(
            r#"
step: literal
tool:
  kind: shell
  command: "echo hello"
"#,
        );
        let di = analyze(&step);
        assert!(di.bounded);
        assert!(di.needed_keys.is_empty());
    }
}
