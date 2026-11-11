//! A compiled playbook (noetl/ai-meta#548 Phase 3 prototype, RFC §7).
//!
//! The module carries one playbook, checked pure by `build.rs` at compile time,
//! and runs the orchestrate-core drive **to completion** in one call:
//!
//! 1. Evaluate.
//! 2. Execute every command in-module. Only `noop` is accepted: its config is
//!    rendered against the command context, exactly as the worker's `NoopTool`
//!    does.
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

/// Drive `playbook_json` to completion with `input` as the workload override.
pub fn drive(playbook_json: &str, digest: &str, input: &Value) -> Result<Outcome, String> {
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
            if c.tool.kind != "noop" {
                // build.rs refused impure playbooks; a non-noop here is a drive bug.
                return Err(format!(
                    "step '{}': tool kind '{}' cannot run in a compiled playbook",
                    c.step_name, c.tool.kind
                ));
            }
            commands += 1;
            let ctx: HashMap<String, Value> = c.context.clone().unwrap_or_default();
            let config = c.tool.config.clone().unwrap_or(Value::Null);
            let rendered = renderer
                .render_value(&config, &ctx)
                .map_err(|e| format!("step '{}': {e}", c.step_name))?;
            let data = rendered.get("result").cloned().unwrap_or_else(|| {
                json!({ "status": "ok", "message": rendered.get("message").cloned().unwrap_or(json!("noop")) })
            });
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

    #[test]
    fn the_same_input_gives_the_same_bytes() {
        let a = run_bytes(br#"{"workload":{"a":7}}"#);
        let b = run_bytes(br#"{"workload":{"a":7}}"#);
        assert_eq!(a, b);
    }
}
