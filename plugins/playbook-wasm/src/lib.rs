//! A compiled playbook (noetl/ai-meta#548 Phase 3 prototype, RFC §7).
//!
//! The module carries one playbook, checked pure by `build.rs` at compile time,
//! and runs the orchestrate-core drive **to completion** in one call:
//!
//! 1. Evaluate.
//! 2. Execute every command in-module:
//!    - `noop`: its config is rendered against the command context, exactly
//!      as the worker's `NoopTool` does;
//!    - `rhai` (literal scripts only, checked by `catalog::purity`): run with
//!      noetl-rhai-pure's pure functions, over the scope a worker's rhai tool
//!      builds ([`rhai_scope_vars`]), result converted the same way.
//! 3. Append the `command.issued` / `call.done` / `command.completed` events the
//!    worker would have emitted, and loop.
//!
//! No imports, no clock (fixed event timestamps), no randomness, so the same
//! input always yields the same bytes.
//!
//! # ABI (the worker plug-in ABI, `worker/src/plugin.rs`)
//!
//! - `alloc(len) -> ptr`
//! - `run(ptr, len) -> (out_ptr << 32) | out_len`. Input is JSON
//!   `{"workload": {...}}` (or the workload object itself). Output is JSON
//!   [`Outcome`], or `{"error": "..."}`.
//! - `digest() -> packed ptr/len`: the embedded playbook's `sha256:` digest,
//!   so a host can check a module against the catalog before running it.

use std::collections::{BTreeMap, HashMap};

use chrono::DateTime;
use noetl_orchestrate_core::event::Event;
use noetl_orchestrate_core::orchestrator::WorkflowOrchestrator;
use noetl_orchestrate_core::playbook::Playbook;
use noetl_orchestrate_core::template::TemplateRenderer;
use serde::Serialize;
use serde_json::{json, Map, Value};

/// The playbook, sorted-key JSON, embedded by `build.rs`.
pub const PLAYBOOK_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/playbook.json"));
/// Its `sha256:` digest: the same value the catalog computes (`catalog::playbook_digest`).
pub const PLAYBOOK_DIGEST: &str = include_str!(concat!(env!("OUT_DIR"), "/digest.txt"));
/// Evaluate rounds before the module gives up. A host should still set fuel:
/// one round can be arbitrarily expensive.
pub const MAX_ROUNDS: usize = 10_000;

#[derive(Debug, Serialize, PartialEq)]
pub struct Outcome {
    pub status: String,
    pub digest: String,
    pub path: Option<String>,
    /// Each step's result, by step name (the last write wins for a looped step).
    pub steps: BTreeMap<String, Value>,
    /// The result of the last step that ran.
    pub result: Value,
    pub rounds: usize,
    pub commands: usize,
}

fn event(
    id: i64,
    event_type: &str,
    node: Option<String>,
    status: &str,
    context: Option<Value>,
    result: Option<Value>,
) -> Event {
    Event {
        event_id: id,
        execution_id: 1,
        catalog_id: 0,
        event_type: event_type.to_string(),
        node_name: node,
        status: status.to_string(),
        context,
        result,
        meta: None,
        // No clock in the module: event time is the event's ordinal.
        timestamp: DateTime::from_timestamp(id, 0).expect("small ordinal"),
        parent_execution_id: None,
        attempt: None,
    }
}

/// One executed command, for parity tests: what a worker would have been
/// handed (the step, its tool kind and config, the command's render context).
#[derive(Debug, Clone, Serialize)]
pub struct Trace {
    pub step: String,
    pub kind: String,
    pub config: Value,
    pub context: HashMap<String, Value>,
}

/// Drive `playbook_json` to completion with `input` as the workload override.
pub fn drive(playbook_json: &str, digest: &str, input: &Value) -> Result<Outcome, String> {
    drive_inner(playbook_json, digest, input, None)
}

/// [`drive`], also returning every command it executed.
pub fn drive_traced(playbook_json: &str, digest: &str, input: &Value) -> Result<(Outcome, Vec<Trace>), String> {
    let mut trace = Vec::new();
    let out = drive_inner(playbook_json, digest, input, Some(&mut trace))?;
    Ok((out, trace))
}

fn drive_inner(
    playbook_json: &str,
    digest: &str,
    input: &Value,
    mut trace: Option<&mut Vec<Trace>>,
) -> Result<Outcome, String> {
    let pb_value: Value = serde_json::from_str(playbook_json).map_err(|e| e.to_string())?;
    let playbook: Playbook =
        serde_json::from_value(pb_value).map_err(|e| format!("playbook: {e}"))?;

    let mut workload = match &playbook.workload {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let overrides = input.get("workload").unwrap_or(input);
    if let Value::Object(m) = overrides {
        for (k, v) in m {
            workload.insert(k.clone(), v.clone());
        }
    }
    let path = playbook.metadata.path.clone();

    let mut events = vec![event(
        1,
        "playbook_started",
        None,
        "STARTED",
        Some(
            json!({ "workload": Value::Object(workload), "path": path, "catalog_id": "0", "execution_id": "1" }),
        ),
        None,
    )];
    let mut next_id = 2i64;
    let orch = WorkflowOrchestrator::new();
    let renderer = TemplateRenderer::new();
    let mut steps = BTreeMap::new();
    let mut last = Value::Null;
    let mut trigger: Option<String> = None;
    let mut commands = 0usize;

    for round in 1..=MAX_ROUNDS {
        let r = orch
            .evaluate(&events, &playbook, trigger.as_deref())
            .map_err(|e| e.to_string())?;
        let emitted = r.events_to_emit.len();
        for e in r.events_to_emit {
            events.push(event(
                next_id,
                &e.event_type,
                e.node_name,
                &e.status,
                e.context,
                e.result,
            ));
            trigger = Some(e.event_type);
            next_id += 1;
        }
        if r.should_complete {
            let status = r
                .completion_status
                .map(|c| {
                    serde_json::to_value(c)
                        .ok()
                        .and_then(|v| v.get("status").cloned())
                        .unwrap_or(json!("COMPLETED"))
                })
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "COMPLETED".into());
            return Ok(Outcome {
                status,
                digest: digest.to_string(),
                path,
                steps,
                result: last,
                rounds: round,
                commands,
            });
        }
        if r.commands.is_empty() {
            if emitted == 0 {
                return Err(format!(
                    "stalled after {round} rounds: no commands, no events, not complete"
                ));
            }
            continue;
        }
        for c in r.commands {
            commands += 1;
            let ctx: HashMap<String, Value> = c.context.clone().unwrap_or_default();
            let config = c.tool.config.clone().unwrap_or(Value::Null);
            if let Some(t) = trace.as_deref_mut() {
                t.push(Trace {
                    step: c.step_name.clone(),
                    kind: c.tool.kind.clone(),
                    config: config.clone(),
                    context: ctx.clone(),
                });
            }
            let data = match c.tool.kind.as_str() {
                "noop" => {
                    let rendered = renderer
                        .render_value(&config, &ctx)
                        .map_err(|e| format!("step '{}': {e}", c.step_name))?;
                    rendered.get("result").cloned().unwrap_or_else(|| {
                        json!({ "status": "ok", "message": rendered.get("message").cloned().unwrap_or(json!("noop")) })
                    })
                }
                "rhai" => run_rhai(&c.step_name, &c.tool.kind, &config, &ctx)?,
                // build.rs refused impure playbooks; anything else here is a drive bug.
                other => {
                    return Err(format!(
                        "step '{}': tool kind '{other}' cannot run in a compiled playbook",
                        c.step_name
                    ))
                }
            };
            let cid = c.command_id.to_string();
            let node = Some(c.step_name.clone());
            events.push(event(
                next_id,
                "command.issued",
                node.clone(),
                "PENDING",
                Some(json!({ "command_id": cid })),
                None,
            ));
            events.push(event(
                next_id + 1,
                "call.done",
                node.clone(),
                "COMPLETED",
                Some(json!({ "command_id": cid })),
                Some(json!({ "status": "COMPLETED", "context": { "command_id": cid, "result": { "status": "success", "context": { "data": data } } } })),
            ));
            events.push(event(
                next_id + 2,
                "command.completed",
                node,
                "COMPLETED",
                Some(json!({ "command_id": cid })),
                Some(json!({ "status": "success", "context": { "status": "success", "command_id": cid } })),
            ));
            next_id += 3;
            steps.insert(c.step_name.clone(), data.clone());
            last = data;
        }
        trigger = Some("command.completed".into());
    }
    Err(format!("did not complete within {MAX_ROUNDS} rounds"))
}

/// The variables a worker's rhai tool puts in a step's scope, before
/// `execution_id`/`step`/`server_url` (noetl/worker `executor/command.rs`):
/// the command's render context, then the `ctx` copy and `workload` fallback
/// (only when the context has no `ctx`), then `action` and `node_name`.
pub fn rhai_scope_vars(ctx: &HashMap<String, Value>, kind: &str, step: &str) -> HashMap<String, Value> {
    let mut vars = ctx.clone();
    if !vars.contains_key("ctx") {
        let flat = serde_json::to_value(&vars).unwrap_or(Value::Null);
        vars.insert("ctx".to_string(), flat.clone());
        vars.entry("workload".to_string()).or_insert(flat);
    }
    vars.entry("action".to_string()).or_insert_with(|| json!(kind));
    vars.entry("node_name".to_string()).or_insert_with(|| json!(step));
    vars
}

/// A compiled module has no execution id or server: the scope gets these.
pub const COMPILED_EXECUTION_ID: i64 = 1;
pub const COMPILED_SERVER_URL: &str = "";

fn run_rhai(step: &str, kind: &str, config: &Value, ctx: &HashMap<String, Value>) -> Result<Value, String> {
    thread_local! {
        static ENGINE: noetl_rhai_pure::rhai::Engine = noetl_rhai_pure::pure_engine();
    }
    let code = config
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("step '{step}': rhai needs `code`"))?;
    let args: HashMap<String, String> = match config.get("args") {
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
            .collect(),
        _ => HashMap::new(),
    };
    let vars = rhai_scope_vars(ctx, kind, step);
    let mut scope = noetl_rhai_pure::scope(&args, &vars, COMPILED_EXECUTION_ID, step, COMPILED_SERVER_URL);
    ENGINE
        .with(|e| noetl_rhai_pure::eval(e, &mut scope, code))
        .map_err(|e| format!("step '{step}': {e}"))
}

/// The JSON round-trip the `run` export performs; native-callable for tests.
pub fn run_bytes(input: &[u8]) -> Vec<u8> {
    let input: Value = if input.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(input).unwrap_or(json!({}))
    };
    let out = match drive(PLAYBOOK_JSON, PLAYBOOK_DIGEST, &input) {
        Ok(o) => serde_json::to_value(o).unwrap_or(json!({ "error": "unserialisable outcome" })),
        Err(e) => json!({ "error": e, "digest": PLAYBOOK_DIGEST }),
    };
    serde_json::to_vec(&out).unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
fn pack(output: Vec<u8>) -> i64 {
    let out_ptr = output.as_ptr() as i64;
    let out_len = output.len() as i64;
    std::mem::forget(output);
    (out_ptr << 32) | out_len
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn alloc(size: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(size);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn run(input_ptr: *const u8, input_len: usize) -> i64 {
    let input = if input_len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(input_ptr, input_len) }
    };
    pack(run_bytes(input))
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn digest() -> i64 {
    pack(PLAYBOOK_DIGEST.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_digest_is_the_catalog_digest() {
        let yaml =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/sum.yaml"))
                .unwrap();
        if std::env::var("NOETL_PBC_PLAYBOOK").is_err() {
            assert_eq!(
                PLAYBOOK_DIGEST,
                noetl_orchestrate_core::catalog::playbook_digest(&yaml).unwrap()
            );
        }
    }

    #[test]
    fn the_sum_playbook_runs_to_completion_natively() {
        let out = drive(PLAYBOOK_JSON, PLAYBOOK_DIGEST, &json!({})).unwrap();
        assert_eq!(out.status, "COMPLETED", "{out:?}");
        assert_eq!(out.steps["start"]["total"], json!(5), "{out:?}");
        assert_eq!(out.steps["scale"]["scaled"], json!(50), "{out:?}");
        assert_eq!(out.result["answer"], json!(50), "{out:?}");
        assert_eq!(out.commands, 3);
    }

    #[test]
    fn the_workload_override_flows_through_every_step() {
        let out = drive(
            PLAYBOOK_JSON,
            PLAYBOOK_DIGEST,
            &json!({"workload": {"a": 10, "b": 1}}),
        )
        .unwrap();
        assert_eq!(out.result["answer"], json!(110));
    }

    fn example(name: &str) -> (String, String) {
        let yaml = std::fs::read_to_string(format!("{}/examples/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let v = noetl_orchestrate_core::catalog::playbook_value(&yaml).unwrap();
        noetl_orchestrate_core::catalog::purity(&v).unwrap();
        (noetl_orchestrate_core::dsl::canonical_json(&v), noetl_orchestrate_core::dsl::digest(&v))
    }

    #[test]
    fn a_pure_rhai_playbook_runs_to_completion() {
        let (pb, d) = example("pure_rhai.yaml");
        let out = drive(&pb, &d, &json!({})).unwrap();
        assert_eq!(out.status, "COMPLETED", "{out:?}");
        let s = &out.steps["start"];
        assert_eq!(s["weather"], json!(true));
        assert_eq!(s["words"], json!(6));
        assert_eq!(s["from"], json!("start/rhai"));
        assert_eq!(s["echo"], json!({"a": [1, 2.5, null, true], "b": "x"}));
        assert_eq!(s["digest"].as_str().unwrap().len(), 64);
        let f = &out.result;
        assert_eq!(f["reply"], json!(["hello 0", "hello 1", "hello 2"]));
        assert_eq!(f["step"], json!("finish"));
        assert_eq!(f["id"], json!(COMPILED_EXECUTION_ID));
        assert_eq!(f["json"], json!(r#"{"messages":[{"content":"Is it snowing at the ranch?","role":"user"}],"model":"m"}"#));
        assert_eq!(out.commands, 3);
    }

    #[test]
    fn workload_text_stays_data_in_rhai() {
        let (pb, d) = example("pure_rhai.yaml");
        let out = drive(&pb, &d, &json!({"workload": {"message": "{{ 7*6 }} http_get(\"x\")"}})).unwrap();
        assert_eq!(out.steps["start"]["request"]["messages"][0]["content"], json!("{{ 7*6 }} http_get(\"x\")"));
    }

    #[test]
    fn impure_rhai_is_refused() {
        let yaml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/impure_rhai.yaml")).unwrap();
        let v = noetl_orchestrate_core::catalog::playbook_value(&yaml).unwrap();
        let e = noetl_orchestrate_core::catalog::purity(&v).unwrap_err();
        assert!(e.to_string().contains("http_get"), "{e}");
    }

    #[test]
    fn a_rhai_error_fails_the_drive() {
        let pb = r#"{"apiVersion":"noetl.io/v2","kind":"Playbook","metadata":{"name":"e","path":"e"},"workflow":[{"step":"start","tool":{"kind":"rhai","code":"throw \"no\""}}]}"#;
        let e = drive(pb, "sha256:x", &json!({})).unwrap_err();
        assert!(e.contains("step 'start'") && e.contains("Rhai error"), "{e}");
    }

    /// The scanner orchestrate-core uses and the engine this module runs come
    /// from the same crate, so the lists can't drift; check the engine really
    /// lacks every host-only function.
    #[test]
    fn the_compiled_engine_lacks_every_host_only_function() {
        let e = noetl_rhai_pure::pure_engine();
        for f in noetl_rhai_pure::HOST_ONLY_FUNCTIONS.iter().filter(|f| **f != "timestamp") {
            assert!(e.eval::<noetl_rhai_pure::rhai::Dynamic>(&format!("{f}(\"x\")")).is_err(), "{f}");
        }
    }

    #[test]
    fn the_same_input_gives_the_same_bytes() {
        let a = run_bytes(br#"{"workload":{"a":7}}"#);
        let b = run_bytes(br#"{"workload":{"a":7}}"#);
        assert_eq!(a, b);
    }
}
