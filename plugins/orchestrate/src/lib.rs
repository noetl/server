//! The `system/orchestrate` WASM plug-in (noetl/ai-meta#108, step 2 of #107).
//!
//! Wraps the pure drive core (`noetl-orchestrate-core`) behind the worker
//! plug-in ABI. Input bytes are a JSON [`OrchestrateInput`] (the bounded event
//! slice + the playbook + the trigger event type); output bytes are the JSON
//! [`OrchestrationResult`](noetl_orchestrate_core::orchestrator::OrchestrationResult)
//! (or a JSON error envelope `{"error": "..."}`).
//!
//! The whole drive core — the condition evaluator and the minijinja template
//! engine included — is wasm-resident (noetl/ai-meta#109), so this plug-in
//! needs **no** host `render` callback: it computes the next commands entirely
//! in-guest and hands them back over the byte data-plane. That retires the main
//! feasibility risk #108 flagged ("does the template engine compile to wasm32,
//! or must the plug-in call back to a host capability?").
//!
//! Built explicitly for `wasm32-unknown-unknown`:
//! ```text
//! cargo build --release --target wasm32-unknown-unknown
//! ```

use noetl_orchestrate_core::event::Event;
use noetl_orchestrate_core::orchestrator::{OrchestrationResult, WorkflowOrchestrator};
use noetl_orchestrate_core::playbook::Playbook;
use noetl_orchestrate_core::state::WorkflowState;
use serde::{Deserialize, Serialize};

/// The plug-in's input contract — the drive read-set the scheduler hands in.
///
/// The host serializes this to JSON, copies it into the guest's linear memory,
/// and the guest deserializes it here. `Serialize` is derived so the kernel
/// scheduler (host side) can build the same shape it sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestrateInput {
    /// The bounded event slice for one execution, oldest-first (the same slice
    /// `trigger_orchestrator` loads: projection snapshot + events-since).
    pub events: Vec<Event>,
    /// The playbook (blueprint) for the execution, loaded from the catalog.
    pub playbook: Playbook,
    /// The event type that triggered this evaluation (`None` on a cold start).
    #[serde(default)]
    pub trigger_event_type: Option<String>,
}

/// The plug-in's **state** input contract — an already-built `WorkflowState`
/// plus the playbook + trigger. This is what the in-server shadow uses: it hands
/// the plug-in the same incrementally-maintained `WorkflowState` the in-process
/// orchestrator drives, so the diff isolates the wasm runtime from any
/// event-slice / snapshot reconstruction (noetl/ai-meta#108 slice 4). The
/// kernel scheduler will later use the event-slice [`OrchestrateInput`] path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestrateStateInput {
    /// The drive state (the projection-snapshot shape the server already
    /// serializes). The plug-in mutates its own deserialized copy.
    pub state: WorkflowState,
    /// The most recent event's timestamp (stamps a cursor-drain completion).
    #[serde(default)]
    pub latest_ts: Option<chrono::DateTime<chrono::Utc>>,
    pub playbook: Playbook,
    #[serde(default)]
    pub trigger_event_type: Option<String>,
}

/// The error envelope returned when evaluation fails, so the scheduler can tell
/// a real `OrchestrationResult` from a drive error without a side channel.
#[derive(Debug, Serialize, Deserialize)]
pub struct OrchestrateError {
    pub error: String,
}

/// The pure round-trip: decode input bytes → run the drive → encode output
/// bytes. Native-testable (the in-process half of the shadow-diff); the wasm
/// `run` export is a thin linear-memory wrapper over this.
pub fn orchestrate(input: &[u8]) -> Vec<u8> {
    match orchestrate_inner(input) {
        Ok(bytes) => bytes,
        Err(msg) => serde_json::to_vec(&OrchestrateError { error: msg }).unwrap_or_else(|_| {
            br#"{"error":"failed to serialize orchestrate error"}"#.to_vec()
        }),
    }
}

fn orchestrate_inner(input: &[u8]) -> Result<Vec<u8>, String> {
    let parsed: OrchestrateInput =
        serde_json::from_slice(input).map_err(|e| format!("decode OrchestrateInput: {e}"))?;
    let orchestrator = WorkflowOrchestrator::new();
    let result: OrchestrationResult = orchestrator
        .evaluate(
            &parsed.events,
            &parsed.playbook,
            parsed.trigger_event_type.as_deref(),
        )
        .map_err(|e| format!("evaluate: {e}"))?;
    serde_json::to_vec(&result).map_err(|e| format!("encode OrchestrationResult: {e}"))
}

/// State-input round-trip: decode an [`OrchestrateStateInput`] → run
/// `evaluate_state` on its (owned) state → encode the `OrchestrationResult`.
/// The in-server shadow's wasm entry point.
pub fn orchestrate_state(input: &[u8]) -> Vec<u8> {
    match orchestrate_state_inner(input) {
        Ok(bytes) => bytes,
        Err(msg) => serde_json::to_vec(&OrchestrateError { error: msg }).unwrap_or_else(|_| {
            br#"{"error":"failed to serialize orchestrate error"}"#.to_vec()
        }),
    }
}

fn orchestrate_state_inner(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut parsed: OrchestrateStateInput =
        serde_json::from_slice(input).map_err(|e| format!("decode OrchestrateStateInput: {e}"))?;
    let orchestrator = WorkflowOrchestrator::new();
    let result: OrchestrationResult = orchestrator
        .evaluate_state(
            &mut parsed.state,
            parsed.latest_ts,
            &parsed.playbook,
            parsed.trigger_event_type.as_deref(),
        )
        .map_err(|e| format!("evaluate_state: {e}"))?;
    serde_json::to_vec(&result).map_err(|e| format!("encode OrchestrationResult: {e}"))
}

// ---- wasm32 data-plane ABI -------------------------------------------------
// The host's contract (worker `WasmPluginHost::invoke_bytes`): the guest exports
// `memory` + `alloc(size) -> ptr` + `run(in_ptr, in_len) -> packed_i64` where
// `packed = (out_ptr << 32) | out_len`. `std` on wasm32-unknown-unknown brings a
// global allocator, so `Vec` backs both `alloc` and the output buffer; the host
// uses a fresh `Store` per invocation, so leaking within a call is discarded
// with the instance.

/// Data-plane ABI: hand back an isolated block in linear memory for the host to
/// write the input buffer into.
#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn alloc(size: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(size);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Entry point: read the input buffer the host copied into linear memory, run
/// the drive, write the output, and return `packed = (out_ptr << 32) | out_len`.
#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn run(input_ptr: *const u8, input_len: usize) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let output = orchestrate(input);
    let out_ptr = output.as_ptr() as i64;
    let out_len = output.len() as i64;
    std::mem::forget(output);
    (out_ptr << 32) | out_len
}

/// State-input entry point (same packed-`i64` data-plane ABI as `run`) — the
/// in-server shadow invokes this with an [`OrchestrateStateInput`].
#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn run_state(input_ptr: *const u8, input_len: usize) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let output = orchestrate_state(input);
    let out_ptr = output.as_ptr() as i64;
    let out_len = output.len() as i64;
    std::mem::forget(output);
    (out_ptr << 32) | out_len
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The miniature shadow-diff: the plug-in's JSON round-trip
    /// (`orchestrate(bytes)`) must reproduce the native `evaluate` byte-for-byte.
    /// Uses the auth0-arc-routing fixture from the core's own orchestrator tests
    /// (a real multi-arc `when:` flow that exercises the template engine).
    #[test]
    fn orchestrate_matches_native_evaluate() {
        let yaml = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: t
  path: t
workload:
  request_id: "req-123"
workflow:
  - step: start
    tool:
      kind: python
      code: |
        result = {"error": "Invalid JWT format"}
    next:
      spec:
        mode: exclusive
      arcs:
        - step: err_cb
          when: "{{ (start.error is defined) and request_id }}"
        - step: end_step
          when: "{{ start.error is defined and not request_id }}"
        - step: create
          when: "{{ start.sub is defined }}"
  - step: err_cb
    tool: { kind: python, code: "result = {}" }
  - step: end_step
    tool: { kind: python, code: "result = {}" }
  - step: create
    tool: { kind: python, code: "result = {}" }
"#;
        let playbook = serde_yaml::from_str::<Playbook>(yaml).unwrap();

        let mk = |event_id: i64, event_type: &str, node_name: Option<&str>| Event {
            event_id,
            execution_id: 12345,
            catalog_id: 67890,
            event_type: event_type.to_string(),
            node_name: node_name.map(|s| s.to_string()),
            status: String::new(),
            context: None,
            result: None,
            meta: None,
            timestamp: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            parent_execution_id: None,
            attempt: None,
        };

        let mut e_started = mk(1, "playbook_started", None);
        e_started.context = Some(serde_json::json!({
            "workload": { "request_id": "req-123" },
            "path": "t"
        }));
        let e_issued = mk(2, "command.issued", Some("start"));
        let mut e_done = mk(3, "call.done", Some("start"));
        e_done.result = Some(serde_json::json!({
            "status": "COMPLETED",
            "context": { "result": { "status": "success", "context": {
                "data": { "error": "Invalid JWT format" }, "status": "success" } } }
        }));
        let mut e_completed = mk(4, "command.completed", Some("start"));
        e_completed.result = Some(serde_json::json!({
            "status": "success", "context": { "status": "success", "command_id": "x" }
        }));

        let events = vec![e_started, e_issued, e_done, e_completed];
        let trigger = Some("command.completed".to_string());

        // Native drive — the reference half of the diff.
        let native = WorkflowOrchestrator::new()
            .evaluate(&events, &playbook, trigger.as_deref())
            .expect("native evaluate ok");

        // Plug-in path — JSON in, JSON out, through the same boundary the host uses.
        let input = OrchestrateInput {
            events,
            playbook,
            trigger_event_type: trigger,
        };
        let input_bytes = serde_json::to_vec(&input).expect("encode input");
        let output_bytes = orchestrate(&input_bytes);
        let plugin: OrchestrationResult =
            serde_json::from_slice(&output_bytes).expect("decode OrchestrationResult (not an error envelope)");

        // Byte-identical command issuance — the cutover gate in miniature.
        assert_eq!(
            serde_json::to_value(&native.commands).unwrap(),
            serde_json::to_value(&plugin.commands).unwrap(),
            "plug-in commands diverge from native drive"
        );
        assert_eq!(native.should_complete, plugin.should_complete);
        assert_eq!(
            serde_json::to_value(&native.events_to_emit).unwrap(),
            serde_json::to_value(&plugin.events_to_emit).unwrap(),
            "plug-in events_to_emit diverge from native drive"
        );
        // The fixture routes start→err_cb (request_id set); the drive issues the
        // err_cb command, so the round-trip carries a real command set.
        let steps: Vec<&str> = plugin.commands.iter().map(|c| c.step_name.as_str()).collect();
        assert!(steps.contains(&"err_cb"), "expected err_cb command, got {steps:?}");
    }

    /// The **state** path (used by the in-server shadow): `orchestrate_state`
    /// reproduces native `evaluate_state` on the same `WorkflowState`.
    #[test]
    fn orchestrate_state_matches_native_evaluate_state() {
        let yaml = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata: { name: test, path: test }
workflow:
  - step: start
    tool: { kind: python, code: "result = {}" }
    next:
      arcs:
        - step: end
  - step: end
    tool: { kind: python, code: "result = {}" }
"#;
        let playbook: Playbook = serde_yaml::from_str(yaml).unwrap();
        let mut started = Event {
            event_id: 1,
            execution_id: 12345,
            catalog_id: 67890,
            event_type: "playbook_started".to_string(),
            node_name: None,
            status: String::new(),
            context: Some(serde_json::json!({ "workload": {}, "path": "test" })),
            result: None,
            meta: None,
            timestamp: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            parent_execution_id: None,
            attempt: None,
        };
        started.event_id = 1;
        let events = vec![started];
        let state = WorkflowState::from_events(&events).expect("build state");
        let latest_ts = events.last().map(|e| e.timestamp);

        // Native evaluate_state on a clone (evaluate_state mutates its state).
        let native = WorkflowOrchestrator::new()
            .evaluate_state(&mut state.clone(), latest_ts, &playbook, None)
            .expect("native evaluate_state");

        // Plug-in state path — JSON in, JSON out.
        let input = OrchestrateStateInput {
            state,
            latest_ts,
            playbook,
            trigger_event_type: None,
        };
        let out = orchestrate_state(&serde_json::to_vec(&input).unwrap());
        let plugin: OrchestrationResult =
            serde_json::from_slice(&out).expect("decode OrchestrationResult (not an error)");

        assert_eq!(
            serde_json::to_value(&native.commands).unwrap(),
            serde_json::to_value(&plugin.commands).unwrap(),
            "state-path commands diverge from native evaluate_state"
        );
        assert!(!plugin.commands.is_empty(), "expected a first command");
    }

    /// A malformed input yields the error envelope, not a panic — the scheduler
    /// can always distinguish a drive failure from a result.
    #[test]
    fn malformed_input_returns_error_envelope() {
        let out = orchestrate(b"not json");
        let err: OrchestrateError = serde_json::from_slice(&out).expect("error envelope");
        assert!(err.error.contains("decode OrchestrateInput"), "{}", err.error);

        let out2 = orchestrate_state(b"not json");
        let err2: OrchestrateError = serde_json::from_slice(&out2).expect("error envelope");
        assert!(err2.error.contains("decode OrchestrateStateInput"), "{}", err2.error);
    }
}
