//! Cross-server parity harness — Phase D R5 R7
//! (noetl/ai-meta#49 / noetl/server#148).
//!
//! Loads `tests/parity_harness/events.json` (a synthetic event log
//! exercising all six replay projections + payload refs), folds it
//! through both the Rust [`fold_replay_state`] and a pre-recorded
//! Python snapshot (`tests/parity_harness/expected.json` generated
//! by `tests/parity_harness/regenerate_expected.py`), and asserts
//! structural parity field-by-field.
//!
//! ## What "parity" means here
//!
//! - **Structural** — same projection keys, same per-key field
//!   values (status, counters, summaries, etc.).  This is the
//!   load-bearing contract: the Rust port produces the same
//!   logical view as the Python source-of-truth.
//! - **NOT byte-for-byte hex** — Python and Rust hash different
//!   digest inputs (Python feeds the `normalize_replayed_*_projection`
//!   flat-row layer; Rust hashes the typed state directly).  The
//!   `checksum` + `projection_checksums` hex values are explicitly
//!   OUT of scope for the parity harness; they're a Rust-side
//!   determinism contract verified by the R4 unit tests.
//!
//! ## How to regenerate `expected.json`
//!
//! ```bash
//! cd tests/parity_harness
//! python3 regenerate_expected.py
//! ```
//!
//! The Python script is a self-contained standalone extract of
//! `noetl/server/api/replay/service.py::fold_replay_state` —
//! re-sync that extract whenever the Python implementation
//! changes (it's the load-bearing parity contract).

use std::fs;

use chrono::Utc;
use noetl_server::services::replay::{
    fold_replay_state, ReplayEventRow, ReplayProjection, ReplayState,
};
use serde_json::Value;

const EVENTS_PATH: &str = "tests/parity_harness/events.json";
const EXPECTED_PATH: &str = "tests/parity_harness/expected.json";

/// Load the events.json fixture and convert each entry to a
/// [`ReplayEventRow`].  Mirrors what the sqlx FromRow decode
/// would produce in production, just sourced from JSON instead
/// of a Postgres rowset.
fn load_events_fixture() -> Vec<ReplayEventRow> {
    let raw = fs::read_to_string(EVENTS_PATH)
        .unwrap_or_else(|e| panic!("read {EVENTS_PATH}: {e}"));
    let parsed: Vec<Value> = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("parse {EVENTS_PATH}: {e}"));
    parsed
        .into_iter()
        .map(|v| ReplayEventRow {
            event_id: v["event_id"].as_i64().expect("event_id i64"),
            event_type: v["event_type"]
                .as_str()
                .expect("event_type str")
                .to_string(),
            node_name: v.get("node_name").and_then(|x| x.as_str()).map(String::from),
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            created_at: Utc::now(),
            stage_id: v.get("stage_id").and_then(|x| x.as_str()).map(String::from),
            frame_id: v.get("frame_id").and_then(|x| x.as_str()).map(String::from),
            command_id: v.get("command_id").and_then(|x| x.as_i64()),
            worker_id: v.get("worker_id").and_then(|x| x.as_str()).map(String::from),
            aggregate_type: v
                .get("aggregate_type")
                .and_then(|x| x.as_str())
                .map(String::from),
            aggregate_id: v
                .get("aggregate_id")
                .and_then(|x| x.as_str())
                .map(String::from),
            meta: v
                .get("meta")
                .filter(|m| !m.is_null())
                .cloned(),
            result: v
                .get("result")
                .filter(|m| !m.is_null())
                .cloned(),
        })
        .collect()
}

fn load_expected() -> Value {
    let raw = fs::read_to_string(EXPECTED_PATH)
        .unwrap_or_else(|e| panic!("read {EXPECTED_PATH}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {EXPECTED_PATH}: {e}"))
}

fn rust_fold() -> ReplayState {
    let events = load_events_fixture();
    fold_replay_state(&events, "default", "default", 999, ReplayProjection::All)
}

#[test]
fn parity_top_level_counts_match() {
    let state = rust_fold();
    let expected = load_expected();
    assert_eq!(state.event_count, expected["event_count"].as_u64().unwrap());
    assert_eq!(
        state.last_event_id,
        expected["last_event_id"].as_i64(),
    );
    assert_eq!(
        state.last_event_type.as_deref(),
        expected["last_event_type"].as_str(),
    );
}

#[test]
fn parity_execution_status_and_last_node_name() {
    let state = rust_fold();
    let expected = load_expected();
    assert_eq!(
        state.execution.status,
        expected["execution"]["status"].as_str().unwrap(),
    );
    assert_eq!(
        state.execution.last_node_name.as_deref(),
        expected["execution"]["last_node_name"].as_str(),
    );
}

#[test]
fn parity_execution_payload_refs() {
    let state = rust_fold();
    let expected = load_expected();
    let expected_refs = expected["execution"]["payload_refs"]
        .as_array()
        .expect("payload_refs is array");
    assert_eq!(
        state.execution.payload_refs.len(),
        expected_refs.len(),
        "payload_refs count differs",
    );
    for (rust, py) in state.execution.payload_refs.iter().zip(expected_refs) {
        assert_eq!(rust.event_id, py["event_id"].as_i64().unwrap());
        // Python's execution.payload_refs entries don't carry a
        // pre-computed summary (they only have {event_id, reference}).
        // The Rust port pre-computes one for consistency — the
        // parity test compares only the fields Python emits.
        assert_eq!(&rust.reference, &py["reference"]);
    }
}

#[test]
fn parity_stage_projection() {
    let state = rust_fold();
    let expected = load_expected();
    let expected_stages = expected["stages"].as_object().expect("stages map");
    assert_eq!(
        state.stages.len(),
        expected_stages.len(),
        "stages count differs",
    );
    for (key, expected_stage) in expected_stages {
        let rust_stage = state
            .stages
            .get(key)
            .unwrap_or_else(|| panic!("stage `{key}` missing in Rust fold"));
        assert_eq!(rust_stage.stage_id, key.as_str());
        assert_eq!(
            rust_stage.status,
            expected_stage["status"].as_str().unwrap(),
            "stage `{key}` status mismatch",
        );
        assert_eq!(
            rust_stage.opened_event_id,
            expected_stage.get("opened_event_id").and_then(|v| v.as_i64()),
            "stage `{key}` opened_event_id mismatch",
        );
        assert_eq!(
            rust_stage.closed_event_id,
            expected_stage.get("closed_event_id").and_then(|v| v.as_i64()),
            "stage `{key}` closed_event_id mismatch",
        );
    }
}

#[test]
fn parity_frame_projection() {
    let state = rust_fold();
    let expected = load_expected();
    let expected_frames = expected["frames"].as_object().expect("frames map");
    assert_eq!(state.frames.len(), expected_frames.len());
    for (key, expected_frame) in expected_frames {
        let rust_frame = state
            .frames
            .get(key)
            .unwrap_or_else(|| panic!("frame `{key}` missing in Rust fold"));
        assert_eq!(rust_frame.frame_id, key.as_str());
        assert_eq!(
            rust_frame.status,
            expected_frame["status"].as_str().unwrap(),
            "frame `{key}` status mismatch",
        );
        assert_eq!(
            rust_frame.row_count,
            expected_frame["row_count"].as_i64().unwrap_or(0),
            "frame `{key}` row_count mismatch",
        );
        assert_eq!(
            rust_frame.events_emitted,
            expected_frame["events_emitted"].as_i64().unwrap_or(0),
            "frame `{key}` events_emitted mismatch",
        );
        // R6 payload-resolver fields.
        match expected_frame.get("output_ref") {
            Some(Value::Null) | None => {
                assert!(
                    rust_frame.output_ref.is_none(),
                    "frame `{key}` Rust set output_ref but Python did not",
                );
            }
            Some(py_ref) => {
                assert_eq!(
                    rust_frame.output_ref.as_ref().unwrap(),
                    py_ref,
                    "frame `{key}` output_ref mismatch",
                );
            }
        }
    }
}

#[test]
fn parity_command_projection() {
    let state = rust_fold();
    let expected = load_expected();
    let expected_cmds = expected["commands"].as_object().expect("commands map");
    assert_eq!(state.commands.len(), expected_cmds.len());
    for (key, expected_cmd) in expected_cmds {
        let rust_cmd = state
            .commands
            .get(key)
            .unwrap_or_else(|| panic!("command `{key}` missing in Rust fold"));
        assert_eq!(rust_cmd.command_id, key.as_str());
        assert_eq!(
            rust_cmd.status,
            expected_cmd["status"].as_str().unwrap(),
            "command `{key}` status mismatch",
        );
        assert_eq!(
            rust_cmd.issued_event_id,
            expected_cmd.get("issued_event_id").and_then(|v| v.as_i64()),
        );
        assert_eq!(
            rust_cmd.terminal_event_id,
            expected_cmd.get("terminal_event_id").and_then(|v| v.as_i64()),
        );
    }
}

#[test]
fn parity_business_object_projection() {
    let state = rust_fold();
    let expected = load_expected();
    let expected_bos = expected["business_objects"]
        .as_object()
        .expect("business_objects map");
    assert_eq!(state.business_objects.len(), expected_bos.len());
    for (key, expected_bo) in expected_bos {
        let rust_bo = state
            .business_objects
            .get(key)
            .unwrap_or_else(|| panic!("BO `{key}` missing in Rust fold"));
        assert_eq!(rust_bo.object_key, key.as_str());
        assert_eq!(rust_bo.object_type, expected_bo["object_type"].as_str().unwrap());
        assert_eq!(rust_bo.object_id, expected_bo["object_id"].as_str().unwrap());
        assert_eq!(
            rust_bo.status,
            expected_bo["status"].as_str().unwrap(),
            "BO `{key}` status mismatch",
        );
        assert_eq!(
            rust_bo.version,
            expected_bo["version"].as_i64().unwrap(),
            "BO `{key}` version mismatch",
        );
        assert_eq!(
            rust_bo.event_count,
            expected_bo["event_count"].as_i64().unwrap(),
            "BO `{key}` event_count mismatch",
        );
        // Attributes match.
        let rust_attrs: Value = serde_json::to_value(&rust_bo.attributes).unwrap();
        assert_eq!(
            &rust_attrs,
            &expected_bo["attributes"],
            "BO `{key}` attributes mismatch",
        );
        // payload_refs count + per-entry reference equality.
        let expected_refs = expected_bo["payload_refs"].as_array().unwrap();
        assert_eq!(
            rust_bo.payload_refs.len(),
            expected_refs.len(),
            "BO `{key}` payload_refs count mismatch",
        );
        for (rust_pr, py_pr) in rust_bo.payload_refs.iter().zip(expected_refs) {
            assert_eq!(rust_pr.event_id, py_pr["event_id"].as_i64().unwrap());
            assert_eq!(&rust_pr.reference, &py_pr["reference"]);
            // Per-payload summary parity — sha256 + ref are the
            // load-bearing fields the parity test compares; other
            // fields fall through to None on both sides when the
            // reference doesn't carry them.
            assert_eq!(
                rust_pr.summary.sha256.as_deref(),
                py_pr["summary"]["sha256"].as_str(),
            );
            assert_eq!(
                rust_pr.summary.reference_uri.as_deref(),
                py_pr["summary"]["ref"].as_str(),
            );
        }
    }
}

#[test]
fn parity_loop_projection() {
    let state = rust_fold();
    let expected = load_expected();
    let expected_loops = expected["loops"].as_object().expect("loops map");
    assert_eq!(state.loops.len(), expected_loops.len());
    for (key, expected_loop) in expected_loops {
        let rust_loop = state
            .loops
            .get(key)
            .unwrap_or_else(|| panic!("loop `{key}` missing in Rust fold"));
        assert_eq!(rust_loop.loop_id, key.as_str());
        assert_eq!(
            rust_loop.done,
            expected_loop["done"].as_i64().unwrap(),
            "loop `{key}` done mismatch",
        );
        assert_eq!(
            rust_loop.failed,
            expected_loop["failed"].as_i64().unwrap(),
            "loop `{key}` failed mismatch",
        );
        assert_eq!(
            rust_loop.completed,
            expected_loop["completed"].as_bool().unwrap(),
            "loop `{key}` completed mismatch",
        );
        assert_eq!(
            rust_loop.total,
            expected_loop.get("total").and_then(|v| v.as_i64()),
            "loop `{key}` total mismatch",
        );
    }
}
