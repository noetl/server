//! Parity: every rhai command a compiled playbook runs gives byte-for-byte the
//! result the worker's rhai tool (noetl-tools RhaiTool, the code a worker pod
//! runs) gives for the same command, scope and metadata.
use noetl_playbook_wasm::{
    drive_traced, rhai_scope_vars, COMPILED_EXECUTION_ID, COMPILED_SERVER_URL,
};
use serde_json::{json, Value};
use std::collections::HashMap;

fn compiled(path: &str) -> (String, String) {
    let yaml = std::fs::read_to_string(path).unwrap();
    let v = noetl_orchestrate_core::catalog::playbook_value(&yaml).unwrap();
    noetl_orchestrate_core::catalog::purity(&v).unwrap();
    (
        noetl_orchestrate_core::dsl::canonical_json(&v),
        noetl_orchestrate_core::dsl::digest(&v),
    )
}

/// Run every rhai command of `playbook` (with `workload`) through RhaiTool and
/// compare it with the compiled step result.
pub fn assert_parity(playbook: &str, workload: Value) -> usize {
    let (pb, d) = compiled(playbook);
    let (out, trace) = drive_traced(&pb, &d, &json!({ "workload": workload })).unwrap();
    let tool = noetl_tools::tools::RhaiTool::new();
    let mut n = 0;
    for t in trace.iter().filter(|t| t.kind == "rhai") {
        let code = t.config["code"].as_str().unwrap();
        let args: HashMap<String, String> = t
            .config
            .get("args")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let mut ctx =
            noetl_tools::ExecutionContext::new(COMPILED_EXECUTION_ID, &t.step, COMPILED_SERVER_URL);
        ctx.variables = rhai_scope_vars(&t.context, &t.kind, &t.step);
        let worker = tool
            .execute_script(code, &args, &ctx)
            .unwrap()
            .data
            .unwrap();
        let mine = &out.steps[&t.step];
        assert_eq!(
            serde_json::to_vec(&worker).unwrap(),
            serde_json::to_vec(mine).unwrap(),
            "step {}",
            t.step
        );
        n += 1;
    }
    n
}

#[test]
fn pure_rhai_example_matches_the_worker() {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/pure_rhai.yaml");
    assert_eq!(assert_parity(p, json!({})), 2);
    assert_eq!(
        assert_parity(
            p,
            json!({"message": "Dinner nearby? {{ x }} `y` ${z}", "limit": 1})
        ),
        2
    );
}

/// Extra playbooks to check (e.g. a site's compiled playbooks):
/// NOETL_PBC_PARITY="a.yaml=workload.json;b.yaml=w2.json".
#[test]
fn listed_playbooks_match_the_worker() {
    let Ok(list) = std::env::var("NOETL_PBC_PARITY") else {
        return;
    };
    for item in list.split(';').filter(|s| !s.is_empty()) {
        let (pb, wl) = item.split_once('=').unwrap_or((item, ""));
        let workload: Value = if wl.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&std::fs::read_to_string(wl).unwrap()).unwrap()
        };
        let n = assert_parity(pb, workload);
        eprintln!("parity ok: {pb} ({n} rhai steps)");
    }
}
