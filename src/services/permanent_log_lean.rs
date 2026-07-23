//! Keep the **permanent** `noetl.event` log lean (noetl/ai-meta#195, EHDB
//! write-behind-cache boundary, RFC `docs/rfc/ehdb-layered-platform.md` §0/§0.2).
//!
//! ## The gap this closes
//!
//! A step's business result rides **inline** in the `call.done` event when it is
//! ≤ 100 KB (`worker/src/executor/command.rs` `INLINE_CONTEXT_MAX_BYTES`); only a
//! larger result is externalized to the byte-source and carried as a
//! `reference` + `extracted` predicate block. So small business payloads
//! accumulate **permanently** in the append-only `noetl.event` log — the exact
//! thing the boundary model forbids: the **permanent control-plane log (role a)
//! must stay lean/reference-based**; full business context belongs only in the
//! **transient processing cache (role b)** (the `noetl_events` WAL, 24 h /
//! `discard=old`), which is sunk to the customer store and evicted.
//!
//! ## Why stripping here is safe for the drive
//!
//! This transform runs in the `system/event_materializer` path
//! (`events_project`), which is **strictly downstream of the `noetl_events`
//! publish fork** (`event_write::emit_events`). By the time the materializer
//! runs, the worker's off-server `state_builder` has **already consumed the
//! full-payload envelope off the WAL** and driven the execution — the live
//! drive (`dispatch_offserver_stateless_drive`) performs **zero `noetl.event`
//! reads**. So slimming the persisted row does not touch the drive decision or
//! its latency.
//!
//! The only readers of the persisted `result` are recovery paths — the
//! server-built `rebuild_state` fall-through (cold execute-time descriptor after
//! a server restart, or a `system/*` execution) — and the status/replay APIs.
//! Those already resolve `noetl://` references via `hydrate_result_references`
//! before `WorkflowState::from_events`, so a **staged, resolvable reference +
//! `extracted`** keeps them correct: with `refs_in_state=true` the drive folds
//! the small `extracted` predicate block (identical shape to a large result);
//! with `refs_in_state=false` it resolves the staged payload from the byte
//! source.
//!
//! ## Shape produced
//!
//! For an over-floor inline step result, the persisted `result` is rewritten
//! from the inline shape
//!
//! ```text
//! result.context.result = { "status": <s>, "context": <business payload> }
//! ```
//!
//! to the **same reference shape a large result already carries**
//!
//! ```text
//! result.context.result = { "status": <s>, "reference": {
//!     "kind": "result_ref", "ref": "noetl://…", "store": …, "scope": …,
//!     "extracted": <bounded predicate block>, "meta": { bytes, sha256, … } } }
//! ```
//!
//! so `hydrate_result_references` (which reads `result.context.result.reference`)
//! resolves it with no code change on the read side. The business payload is put
//! to the byte source (`ResultStoreService::put` → `noetl.result_store`, read
//! back by `resolve`) so it is **retrievable via the reference** and is itself
//! bounded/evictable (role b), never accumulating in the permanent log.
//!
//! Gated behind [`AppConfig::permanent_log_lean`] (`NOETL_PERMANENT_LOG_LEAN`,
//! **default off**) so the landing is behavior-neutral until an operator opts in;
//! the floor [`AppConfig::permanent_log_inline_max_bytes`]
//! (`NOETL_PERMANENT_LOG_INLINE_MAX_BYTES`) keeps trivial control scalars inline.

use serde_json::Value;

use crate::services::internal::EventEnvelope;
use crate::services::result_store::{PutResultBody, ResultStoreService};
use crate::state::AppState;

/// Max serialized size of the bounded `extracted` predicate block (mirrors the
/// worker's `MAX_EXTRACTED_BYTES`, `command.rs`). The block preserves navigable
/// structure for `when:`/`set:`/cursor evaluation without the bulk payload.
const MAX_EXTRACTED_BYTES: usize = 4096;
/// Strings up to this length are kept verbatim in `extracted`; longer ones
/// collapse to `{ "_len": n }`.
const MAX_EXTRACTED_SCALAR_BYTES: usize = 512;
/// Recursion ceiling for `extracted` — deeper nodes collapse to a shape summary.
const MAX_EXTRACTED_DEPTH: usize = 8;

/// Build a bounded, navigable `extracted` predicate block from a business
/// payload — a faithful server-side port of the worker's `build_extracted`
/// (`worker/src/executor/command.rs`), so a slimmed small result is
/// indistinguishable from a large result on the read path. Structure is kept so
/// navigation expressions resolve (`{{ output.data.rows[0].x }}`,
/// `{{ step.count }}`); the bulk is summarised (arrays keep their first element,
/// large strings collapse to `{_len}`). Bounded to [`MAX_EXTRACTED_BYTES`] — a
/// truncated node sets `_truncated: true`.
pub fn build_extracted(payload: &Value) -> Value {
    let mut budget = MAX_EXTRACTED_BYTES;
    summarise_value(payload, 0, &mut budget)
}

fn summarise_value(v: &Value, depth: usize, budget: &mut usize) -> Value {
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => {
            *budget = budget.saturating_sub(v.to_string().len());
            v.clone()
        }
        Value::String(s) if s.len() <= MAX_EXTRACTED_SCALAR_BYTES => {
            *budget = budget.saturating_sub(s.len());
            v.clone()
        }
        Value::String(s) => serde_json::json!({ "_len": s.len() }),
        Value::Array(a) => {
            if a.is_empty() || depth >= MAX_EXTRACTED_DEPTH {
                serde_json::json!({ "_count": a.len() })
            } else {
                // Keep only the first element so `arr[0].<field>` resolves; the
                // 1-element array preserves index-0 access without the bulk.
                Value::Array(vec![summarise_value(&a[0], depth + 1, budget)])
            }
        }
        Value::Object(o) => {
            if depth >= MAX_EXTRACTED_DEPTH {
                return serde_json::json!({
                    "_count": o.len(),
                    "_keys": o.keys().take(64).cloned().collect::<Vec<_>>(),
                });
            }
            let mut out = serde_json::Map::new();
            for (k, val) in o {
                if *budget == 0 {
                    out.insert("_truncated".to_string(), Value::Bool(true));
                    break;
                }
                *budget = budget.saturating_sub(k.len() + 4);
                out.insert(k.clone(), summarise_value(val, depth + 1, budget));
            }
            Value::Object(out)
        }
    }
}

/// Locate the inline business payload inside a persisted `call.done`-shape
/// `result` and return a mutable handle to the `{ status, context }` node that
/// carries it, together with the payload, **iff** it is an over-floor inline
/// business result eligible for slimming.
///
/// The anchor is the exact shape `hydrate_result_references` reads on the way
/// back out: the reference lives at `result.context.result.reference`, so the
/// inline payload it replaces is its sibling `result.context.result.context`.
/// Returns `None` (leave the row untouched) for any other shape — an event
/// already carrying a `reference`, a non-object payload, a payload at/under the
/// floor, or an unexpected envelope — so the transform can never corrupt a row
/// it does not fully recognise.
fn eligible_inline_payload(
    result: &mut Value,
    floor_bytes: usize,
) -> Option<(&mut serde_json::Map<String, Value>, Value)> {
    // Navigate to `result.context.result`, the worker's `build_call_done_result`
    // output node (`{status, context}` inline / `{status, reference}` external).
    let inner = result
        .get_mut("context")?
        .get_mut("result")?
        .as_object_mut()?;
    // Already externalized (large result) → nothing to do.
    if inner.contains_key("reference") {
        return None;
    }
    // A completed step result: a string `status` plus an object `context`
    // payload. Anything else (a primitive context, a bare status, a
    // control-shape) is skipped rather than guessed at.
    if !inner.get("status").map(Value::is_string).unwrap_or(false) {
        return None;
    }
    let payload = inner.get("context")?;
    if !payload.is_object() {
        return None;
    }
    // Keep trivial payloads inline — the floor bounds churn on the byte source
    // and keeps small control scalars in the permanent row.
    let approx = serde_json::to_string(payload).map(|s| s.len()).unwrap_or(0);
    if approx <= floor_bytes {
        return None;
    }
    let payload = payload.clone();
    Some((inner, payload))
}

/// Slim one envelope's `result` in place: stage the inline business payload to
/// the byte source and rewrite it to a resolvable `reference` + `extracted`.
/// Returns the payload byte count when it stripped, or `None` when the envelope
/// was left untouched (ineligible shape, or a staging failure — which is logged
/// and the row is left inline rather than dropping the payload).
async fn slim_one(env: &mut EventEnvelope, state: &AppState, floor_bytes: usize) -> Option<usize> {
    let execution_id = env.execution_id?;
    let step = env.node_name.clone().unwrap_or_default();
    let result = env.result.as_mut()?;

    // Peek eligibility (immutable enough) — clone the payload out so the
    // subsequent async stage does not hold a borrow across the await.
    let payload = {
        let (_node, payload) = eligible_inline_payload(result, floor_bytes)?;
        payload
    };
    let approx_bytes = serde_json::to_string(&payload)
        .map(|s| s.len())
        .unwrap_or(0);

    // Stage the payload to the byte source so the reference resolves on the
    // recovery/status read paths. `put` writes the `noetl.result_store` row that
    // `resolve` reads back, independent of the dual-write flag. Use the SAME
    // per-execution pool `events_project`'s resolve path reads from
    // (`state.pools.pool_for(execution_id)`) so the reference is resolvable.
    let result_store = ResultStoreService::new(
        state.pools.pool_for(execution_id).clone(),
        state.snowflake.clone(),
    );
    let body = PutResultBody {
        name: if step.is_empty() {
            "result".to_string()
        } else {
            step.clone()
        },
        data: payload.clone(),
        scope: "execution".to_string(),
        source_step: (!step.is_empty()).then(|| step.clone()),
        store: None,
        ttl: None,
        correlation: None,
        compress: false,
    };
    let put = match result_store.put(execution_id, &body).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                execution_id,
                step = %step,
                error = %e,
                "permanent_log_lean: byte-source stage failed; leaving result inline",
            );
            return None;
        }
    };

    // Rewrite `result.context.result` from `{status, context}` to
    // `{status, reference{…}}` — the exact shape a large result carries.
    let extracted = build_extracted(&payload);
    let (node, _payload) = eligible_inline_payload(result, floor_bytes)?;
    rewrite_node_to_reference(
        node,
        &put.r#ref,
        &put.store,
        &put.scope,
        put.bytes,
        put.sha256.as_deref(),
        extracted,
    );

    tracing::debug!(
        execution_id,
        step = %step,
        payload_bytes = approx_bytes,
        result_ref = %put.r#ref,
        "permanent_log_lean: stripped inline business result to reference",
    );
    Some(approx_bytes)
}

/// Replace the inline `context` payload on a `{status, context}` result node
/// with the `reference` block a large result carries — the shape
/// `hydrate_result_references` reads back (`…/reference/ref`,
/// `…/reference/extracted`). Pure so the produced shape is unit-testable
/// without a live byte source.
fn rewrite_node_to_reference(
    node: &mut serde_json::Map<String, Value>,
    ref_uri: &str,
    store: &str,
    scope: &str,
    bytes: u64,
    sha256: Option<&str>,
    extracted: Value,
) {
    node.remove("context");
    let mut reference = serde_json::json!({
        "kind": "result_ref",
        "ref": ref_uri,
        "store": store,
        "scope": scope,
        "extracted": extracted,
        "meta": {
            "bytes": bytes,
            "media_type": "application/json",
            "content_type": "application/json",
        },
    });
    if let Some(sha) = sha256 {
        reference["meta"]["sha256"] = Value::String(sha.to_string());
    }
    node.insert("reference".to_string(), reference);
}

/// Slim a batch of envelopes before they are persisted to `noetl.event`. Stages
/// each over-floor inline business result to the byte source and rewrites it to
/// a resolvable reference + `extracted`. Records the count + bytes stripped.
/// Best-effort per envelope: an ineligible shape or a staging failure leaves
/// that row inline — never drops a payload.
pub async fn slim_events_for_permanent_log(
    events: &mut [EventEnvelope],
    state: &AppState,
    floor_bytes: usize,
) {
    let mut stripped = 0usize;
    let mut bytes = 0usize;
    for env in events.iter_mut() {
        if let Some(n) = slim_one(env, state, floor_bytes).await {
            stripped += 1;
            bytes += n;
        }
    }
    if stripped > 0 {
        crate::metrics::record_permanent_log_slimmed(stripped as u64, bytes as u64);
        tracing::info!(
            stripped,
            bytes,
            "permanent_log_lean: stripped inline business results from the permanent log batch",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_done_result(payload: Value) -> Value {
        // The persisted `call.done` `result` shape: the worker's
        // `{status, context}` output nested under `result.context.result`.
        serde_json::json!({
            "status": "success",
            "context": {
                "command_id": "1:step:9",
                "call_index": 0,
                "result": { "status": "success", "context": payload }
            }
        })
    }

    #[test]
    fn eligible_detects_over_floor_inline_business_result() {
        let payload = serde_json::json!({ "rows": vec!["x"; 200], "count": 200 });
        let mut result = call_done_result(payload.clone());
        let got = eligible_inline_payload(&mut result, 512);
        assert!(
            got.is_some(),
            "over-floor inline object payload is eligible"
        );
        let (_node, p) = got.unwrap();
        assert_eq!(p, payload);
    }

    #[test]
    fn eligible_skips_small_payload_under_floor() {
        let mut result = call_done_result(serde_json::json!({ "ok": true }));
        assert!(
            eligible_inline_payload(&mut result, 512).is_none(),
            "a tiny payload under the floor stays inline",
        );
    }

    #[test]
    fn eligible_skips_already_referenced_large_result() {
        // A large result already carries `reference` — must not be touched.
        let mut result = serde_json::json!({
            "status": "success",
            "context": { "result": {
                "status": "success",
                "reference": { "kind": "result_ref", "ref": "noetl://execution/1/result/s/9" }
            }}
        });
        assert!(eligible_inline_payload(&mut result, 0).is_none());
    }

    #[test]
    fn eligible_skips_primitive_context_payload() {
        // Non-object payloads (a bare string/number) are left inline — the
        // `chk_event_result_shape` constraint requires object `context`, and a
        // scalar is not a business rowset worth externalizing.
        let mut result = call_done_result(serde_json::json!("just a string over the floor aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(eligible_inline_payload(&mut result, 8).is_none());
    }

    #[test]
    fn rewrite_produces_hydrate_compatible_reference_shape() {
        // The read path (`hydrate_result_references`) reads the reference at
        // `result.context.result.reference.ref` and the predicate block at
        // `.../reference/extracted`. Prove the rewrite lands the reference at
        // exactly that pointer, carrying the URN + a bounded `extracted`, and
        // that the top-level `result` keys stay `{status, context}` so the
        // `chk_event_result_shape` DB constraint still holds.
        let payload = serde_json::json!({ "rows": vec!["r"; 300], "count": 300 });
        let mut result = call_done_result(payload.clone());
        let (node, p) = eligible_inline_payload(&mut result, 512).expect("eligible");
        assert_eq!(p, payload);
        let extracted = build_extracted(&p);
        rewrite_node_to_reference(
            node,
            "noetl://execution/1/my_step/9",
            "db",
            "execution",
            1234,
            Some("deadbeef"),
            extracted,
        );
        // Reference at the exact pointer the drive read path consumes.
        assert_eq!(
            result
                .pointer("/context/result/reference/ref")
                .and_then(|v| v.as_str()),
            Some("noetl://execution/1/my_step/9"),
        );
        // `extracted` present + navigable for `when:`/`set:` off the reference.
        assert_eq!(
            result.pointer("/context/result/reference/extracted/count"),
            Some(&serde_json::json!(300)),
        );
        // The bulk payload is gone from the persisted row.
        assert!(result.pointer("/context/result/context").is_none());
        // Top-level `result` keys stay within the chk_event_result_shape allow-set.
        let top: Vec<&str> = result
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        for k in &top {
            assert!(
                matches!(*k, "status" | "reference" | "context"),
                "unexpected top key {k}"
            );
        }
        // Idempotent: a second pass sees the reference and does nothing.
        assert!(eligible_inline_payload(&mut result, 512).is_none());
    }

    #[test]
    fn extracted_is_bounded_and_navigable() {
        let payload = serde_json::json!({
            "count": 500,
            "status": "ok",
            "rows": (0..1000).map(|i| serde_json::json!({ "id": i, "name": "x".repeat(50) })).collect::<Vec<_>>(),
            "big": "y".repeat(10_000),
        });
        let ex = build_extracted(&payload);
        assert!(
            ex.to_string().len() <= MAX_EXTRACTED_BYTES,
            "extracted stays bounded"
        );
        // Scalars preserved for predicate evaluation.
        assert_eq!(ex["count"], serde_json::json!(500));
        assert_eq!(ex["status"], serde_json::json!("ok"));
        // The oversized string collapsed to a length marker.
        assert_eq!(ex["big"], serde_json::json!({ "_len": 10_000 }));
        // The array keeps its first element so `rows[0].id` still resolves.
        assert_eq!(ex["rows"][0]["id"], serde_json::json!(0));
    }
}
