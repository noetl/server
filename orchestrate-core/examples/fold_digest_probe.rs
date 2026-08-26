//! Phase 0 probe — is the `WorkflowState` digest deterministic ACROSS PROCESSES?
//!
//! The whole event-sourced control-flow design rests on
//! `sha256(fold(events)) == the digest the record carries`, compared between a
//! process that folded and a process that re-folds. If the serialisation is not
//! canonical, that comparison is meaningless — and it would fail *silently*,
//! because a same-process check (which is what every existing test and every
//! kind measurement does) always agrees.
//!
//! Run it twice and compare stdout. Same bytes ⇒ deterministic.
//!
//!   cargo run -q --example fold_digest_probe

use std::collections::HashMap;

use noetl_orchestrate_core::state::WorkflowState;
use sha2::{Digest, Sha256};

fn main() {
    // Enough distinct keys that a hash-order difference is overwhelmingly
    // likely to show. With 16 keys, two random orders colliding is ~1/16!.
    let mut ctx: HashMap<String, serde_json::Value> = HashMap::new();
    for i in 0..16 {
        ctx.insert(format!("key_{i:02}"), serde_json::json!({ "n": i }));
    }
    let mut marks: HashMap<String, i64> = HashMap::new();
    for i in 0..16 {
        marks.insert(format!("mark_{i:02}"), i as i64);
    }

    let mut ws = WorkflowState::new(42, 7);
    ws.ctx = ctx;
    ws.ctx_set_marks = marks;

    // (A) what `orch_snapshot::save` digests today: raw serde output.
    let bytes = serde_json::to_vec(&ws).expect("serialise");
    let digest = hex::encode(Sha256::digest(&bytes));

    // The RAW key order — read off the serialised text, NOT by re-parsing.
    // Re-parsing was the first version of this line and it was useless:
    // serde_json's `Map` is a BTreeMap unless `preserve_order` is on, so the
    // round trip SORTS the keys and every run printed an identical order while
    // the digests differed. The bytes are the evidence; the parse is not.
    let text = String::from_utf8_lossy(&bytes);
    let raw_order: Vec<String> = text
        .split("\"key_")
        .skip(1)
        .take(6)
        .map(|s| format!("key_{}", &s[..2]))
        .collect();

    // (B) the canonical form: round-trip through `serde_json::Value`, whose
    // object map is a BTreeMap, so the key order becomes the sorted order
    // regardless of the HashMap's per-process hash seed.
    let value = serde_json::to_value(&ws).expect("to_value");
    let canon_bytes = serde_json::to_vec(&value).expect("canon serialise");
    let canonical = hex::encode(Sha256::digest(&canon_bytes));

    // NEGATIVE CONTROL. Without it, a "canonicaliser" that returned a constant
    // would satisfy the stability check above and prove nothing. Perturb ONE
    // byte of one value and the canonical digest must move.
    let mut perturbed = serde_json::to_value(&ws).expect("to_value");
    perturbed["ctx"]["key_07"]["n"] = serde_json::json!(999);
    let perturbed_digest = hex::encode(Sha256::digest(
        serde_json::to_vec(&perturbed).expect("perturbed serialise"),
    ));

    println!("raw_digest={digest}");
    println!("raw_ctx_order={raw_order:?}");
    println!("canonical_digest={canonical}");
    println!("perturbed_digest={perturbed_digest}");
    println!(
        "negative_control={}",
        if perturbed_digest == canonical { "FAILED-identical" } else { "ok-differs" }
    );
}
