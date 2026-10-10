//! **Recovery drill: cold-load the event log from the off-box replica ALONE.**
//!
//! This is the acceptance proof for A2/A3 (noetl/ai-meta#460) and the one that
//! cannot be satisfied by metadata. Every other signal — `replica_set_size`,
//! `survives_node_loss`, `parts_under_replicated`, even an object count in the
//! bucket — is a statement *about* the replica. This one reads the records back
//! out of it with the primary absent from the replica set entirely.
//!
//! ⚠ `#[ignore]` and env-driven on purpose: it needs a real replica payload,
//! and a test that silently passes with no data is worse than no test. It is
//! also a drill worth re-running, so it takes the directory rather than
//! hardcoding one.
//!
//! ```text
//! gcloud storage cp -r 'gs://<replica-bucket>/*' /tmp/drill/
//! NOETL_RECOVERY_DRILL_DIR=/tmp/drill \
//!   cargo test --test recovery_drill -- --ignored --nocapture
//! ```
//!
//! ⚠⚠ Point it at a **copy**, not at the live replica bucket. A cold load is a
//! read, but the engine writes its durable manifest to every replica on any
//! subsequent upload, and a recovery drill must not be able to mutate the thing
//! it is validating.
//!
//! # What makes it a proof rather than a check
//!
//! The expected record count is taken from the **manifest inside the replica**,
//! and then every part is actually read. So the two ways this could pass
//! vacuously are both closed:
//!
//! - an empty replica would give `expected == 0` and `got == 0`, so the test
//!   refuses a zero expectation outright;
//! - a replica holding a complete manifest and incomplete bytes — the exact
//!   state prod was in before the backfill — makes `replay_all` **error**,
//!   because a part whose only replica is absent is unreachable. It does not
//!   return a short log.

use std::sync::Arc;

use ehdb_l0::substrate::DurableSubstrate;
use ehdb_l0::{L0Config, L0EventLogEngine, LocalFsSubstrate, ReplicaTarget};

fn drill_dir() -> Option<std::path::PathBuf> {
    std::env::var("NOETL_RECOVERY_DRILL_DIR")
        .ok()
        .map(std::path::PathBuf::from)
}

#[test]
#[ignore = "needs NOETL_RECOVERY_DRILL_DIR pointing at a copy of a replica's contents"]
fn the_log_cold_loads_from_the_off_box_replica_alone() {
    let dir = drill_dir().expect("NOETL_RECOVERY_DRILL_DIR must be set");
    assert!(dir.is_dir(), "{} is not a directory", dir.display());

    // --- what the replica CLAIMS, read from the manifest it carries ---
    let manifest_path = dir.join("manifest/d1_event_log/LATEST");
    let raw = std::fs::read(&manifest_path)
        .unwrap_or_else(|e| panic!("no durable manifest at {}: {e}", manifest_path.display()));
    let manifest: serde_json::Value = serde_json::from_slice(&raw).expect("manifest parses");
    let parts = manifest["parts"].as_array().expect("manifest has parts");
    let expected_records: u64 = parts
        .iter()
        .map(|p| p["record_count"].as_u64().unwrap_or(0))
        .sum();
    let expected_parts = parts.len();

    // ⚠ Refuse a vacuous pass. An empty or near-empty replica would otherwise
    // "recover everything it claims" and prove nothing.
    assert!(
        expected_parts > 0 && expected_records > 0,
        "the replica's manifest claims {expected_parts} parts / {expected_records} records \
         — there is nothing here to recover, so this run proves nothing"
    );

    // Every part must actually be present as an object, counted from the
    // substrate rather than from the manifest that names them.
    let substrate: Arc<dyn DurableSubstrate> = Arc::new(LocalFsSubstrate::new(&dir).unwrap());
    let objects = substrate.list_prefix("parts/d1_event_log/").unwrap();
    eprintln!(
        "DRILL  manifest: {expected_parts} parts / {expected_records} records   \
         objects present: {}",
        objects.len()
    );
    assert_eq!(
        objects.len(),
        expected_parts,
        "the replica names {expected_parts} parts and physically holds {} — this is the \
         pre-backfill state and recovery from it is not possible",
        objects.len()
    );

    // --- ⭐ the drill: cold-load with ONLY the replica in the set ---
    let fresh_root = std::env::temp_dir().join(format!(
        "noetl-recovery-drill-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    // ⚠⚠ The replica's **id** is load-bearing for recovery, and this is not
    // obvious. A part is resolved by matching `ReplicaLocation::replica`
    // against the ids in the replica set, so a recovery that mounts the right
    // bytes under a different name gets `no reachable replica (N listed)` —
    // every object present and nothing readable. Found by this drill failing
    // exactly that way on the first run, with all 41 prod parts in place.
    //
    // So the id comes from the manifest, not from a literal.
    let replica_ids: std::collections::BTreeSet<String> = parts
        .iter()
        .flat_map(|p| p["replicas"].as_array().cloned().unwrap_or_default())
        .filter_map(|r| r["replica"].as_str().map(|s| s.to_string()))
        .collect();
    eprintln!("DRILL  replica ids named by the manifest: {replica_ids:?}");
    // ⭐ Mount it under the REMOTE id only — the genuine "primary is gone"
    // shape. This works precisely because the backfill gave every part a
    // `replica-1-gcs` location; before it, 38 of 41 named only `replica-0` and
    // this exact call is what could not have succeeded.
    //
    // Deliberately NOT aliasing the dead primary's id onto this substrate. That
    // would also pass, and it would weaken the claim from "the remote can serve
    // the log" to "the bytes exist somewhere under some name".
    let remote_id = std::env::var("NOETL_RECOVERY_DRILL_REPLICA")
        .unwrap_or_else(|_| "replica-1-gcs".to_string());
    assert!(
        replica_ids.contains(&remote_id),
        "the manifest does not name {remote_id}; ids present: {replica_ids:?}"
    );
    let targets: Vec<ReplicaTarget> = vec![ReplicaTarget::new(
        remote_id.clone(),
        Arc::clone(&substrate),
    )];
    eprintln!("DRILL  mounting ONLY {remote_id} — the primary is absent from the set");

    // And every part must name it, or recovery from it alone is impossible by
    // construction. This is the assertion that would have failed before the
    // backfill: 38 of 41 parts named only the local device.
    let naming_remote = parts
        .iter()
        .filter(|p| {
            p["replicas"]
                .as_array()
                .map(|rs| rs.iter().any(|r| r["replica"].as_str() == Some(&remote_id)))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(
        naming_remote, expected_parts,
        "only {naming_remote} of {expected_parts} parts name {remote_id} — the rest are          unreachable with the primary gone"
    );
    assert!(
        !targets.is_empty(),
        "the manifest names no replica ids at all"
    );

    let engine = L0EventLogEngine::cold_load_replicated(
        L0Config::d1(&fresh_root).with_shard_count(1),
        targets,
    )
    .expect("the replica alone can serve a cold load");

    let records = engine
        .replay_all()
        .expect("every part is reachable from the replica alone");
    eprintln!("DRILL  replayed {} records from the replica alone", records.len());

    assert_eq!(
        records.len() as u64,
        expected_records,
        "recovered {} of the {expected_records} records the replica's own manifest claims",
        records.len()
    );

    // Sequences must be strictly increasing — a replay that returned the right
    // COUNT of garbage would otherwise pass.
    let mut prev = 0u64;
    for r in &records {
        assert!(
            r.global_sequence > prev,
            "sequences not strictly increasing at {} (prev {prev})",
            r.global_sequence
        );
        prev = r.global_sequence;
    }
    eprintln!(
        "DRILL  sequences strictly increasing, {} .. {}",
        records.first().map(|r| r.global_sequence).unwrap_or(0),
        prev
    );

    let _ = std::fs::remove_dir_all(&fresh_root);
}
