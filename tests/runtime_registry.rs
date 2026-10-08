//! **Proofs that the D8 runtime registry is reached, not merely linked**
//! (noetl/ai-meta#455 P1–P4).
//!
//! Measured on prod v3.124.0: `register_kind`, `discover`, `list_live_at` and
//! `watch_since` had **zero** callers in this repo. The registry was linked, released and
//! dormant. These tests drive the real `RuntimeStore` — a real engine on a real substrate,
//! no mocks — through the exact functions `runtime_registry` calls in production.
//!
//! The load-bearing proof is **expiry by lease**: an instance disappears because nothing
//! renewed it, not because something marked it dead. A status column somebody has to
//! remember to update is the representation-drift shape this platform keeps finding; a
//! lease cannot drift, because the absence of a heartbeat *is* the signal.

use std::sync::Arc;

use ehdb_l0::runtime::{RuntimeEvent, RuntimeKind, RuntimeStore};
use ehdb_l0::substrate::{DurableSubstrate, LocalFsSubstrate};
use noetl_server::runtime_registry as reg;

fn dir(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("noetl-reg-{tag}-{}-{n}", std::process::id()))
}

fn store(tag: &str) -> (RuntimeStore, std::path::PathBuf) {
    let root = dir(tag);
    let sub = root.join("substrate");
    let local = root.join("local");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    let s: Arc<dyn DurableSubstrate> = Arc::new(LocalFsSubstrate::new(&sub).unwrap());
    (
        RuntimeStore::open(RuntimeStore::config(&local), s).unwrap(),
        root,
    )
}

const SEC: u64 = 1_000_000;

#[test]
fn a_registered_server_is_discoverable_and_a_silent_one_expires_by_lease() {
    let (mut st, root) = store("lease");
    let t0 = 1_000 * SEC;
    let ttl = 180 * SEC;

    reg::register_in(&mut st, RuntimeKind::Server, "srv-a", "noetl-server/test", t0).unwrap();

    // Live immediately, and found BY KIND — not by inferring kind from the id's shape.
    let found = reg::discover_in(&st, RuntimeKind::Server, t0, ttl).unwrap();
    assert_eq!(found.len(), 1, "the server must be discoverable at once");
    assert_eq!(found[0].id(), "srv-a");
    assert_eq!(found[0].contract, "noetl-server/test");

    // Still live just inside the lease.
    let inside = reg::discover_in(&st, RuntimeKind::Server, t0 + ttl - SEC, ttl).unwrap();
    assert_eq!(inside.len(), 1, "must still be live one second inside the lease");

    // ⭐ THE PROOF: past the lease it is gone, and NOTHING marked it dead. No
    // deregister, no flag, no reaper — only the absence of a renewal.
    let expired = reg::discover_in(&st, RuntimeKind::Server, t0 + ttl + SEC, ttl).unwrap();
    assert!(
        expired.is_empty(),
        "a server that stopped heartbeating must expire by lease; got {expired:?}"
    );

    // And the record is still in the log — expiry is a READ-TIME verdict, not a delete.
    // That distinction is what lets `watch_since` replay history at all.
    assert!(
        st.get("srv-a").unwrap().is_some(),
        "expiry must not delete the registration; it is a read-time liveness verdict"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_heartbeat_renews_the_lease_so_a_live_server_does_not_expire() {
    let (mut st, root) = store("renew");
    let t0 = 1_000 * SEC;
    let ttl = 180 * SEC;
    reg::register_in(&mut st, RuntimeKind::Server, "srv-b", "c", t0).unwrap();

    // Renew at 60 s intervals across 10 minutes — well past the 180 s lease.
    let mut now = t0;
    for _ in 0..10 {
        now += 60 * SEC;
        assert!(
            reg::heartbeat_in(&mut st, "srv-b", now).unwrap(),
            "heartbeat must find a registration to renew"
        );
    }
    assert!(now > t0 + ttl, "the test must actually cross the lease window");

    let live = reg::discover_in(&st, RuntimeKind::Server, now, ttl).unwrap();
    assert_eq!(
        live.len(),
        1,
        "a continuously renewed server must stay live {}s after registering",
        (now - t0) / SEC
    );

    // The negative control for the renewal itself: stop renewing and it expires. Without
    // this, "stays live" is consistent with a TTL that never expires anything.
    let gone = reg::discover_in(&st, RuntimeKind::Server, now + ttl + SEC, ttl).unwrap();
    assert!(
        gone.is_empty(),
        "once renewals stop the lease must lapse — otherwise the test above proves nothing"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// ⚠⚠ Regression guard for a real bug found in ehdb P2: `heartbeat` reset the kind to
/// `Worker`, because the append path defaults it. A renewed `Server` silently stopped
/// being discoverable as a `Server` **one heartbeat after registering** — a registration
/// that vanishes while being actively renewed.
#[test]
fn a_heartbeat_does_not_change_the_kind() {
    let (mut st, root) = store("kind");
    let t0 = 1_000 * SEC;
    let ttl = 180 * SEC;
    reg::register_in(&mut st, RuntimeKind::Server, "srv-c", "c", t0).unwrap();
    reg::heartbeat_in(&mut st, "srv-c", t0 + 60 * SEC).unwrap();

    let as_server = reg::discover_in(&st, RuntimeKind::Server, t0 + 60 * SEC, ttl).unwrap();
    assert_eq!(
        as_server.len(),
        1,
        "still discoverable as a Server after a heartbeat"
    );
    let as_worker = reg::discover_in(&st, RuntimeKind::Worker, t0 + 60 * SEC, ttl).unwrap();
    assert!(
        as_worker.is_empty(),
        "a heartbeat must not reclassify a Server as a Worker"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn discovery_separates_kinds_so_the_fleet_is_queryable_by_role() {
    let (mut st, root) = store("fleet");
    let t0 = 1_000 * SEC;
    let ttl = 180 * SEC;
    for (kind, id) in [
        (RuntimeKind::Server, "srv-1"),
        (RuntimeKind::Gateway, "gw-1"),
        (RuntimeKind::Worker, "wk-1"),
        (RuntimeKind::Worker, "wk-2"),
        (RuntimeKind::Ehdb, "ehdb-1"),
        (RuntimeKind::Execution, "exec-1"),
    ] {
        reg::register_in(&mut st, kind, id, "c", t0).unwrap();
    }

    let t = reg::topology_in(&st, t0, ttl).unwrap();
    assert_eq!(t.live_total, 6, "all six must be live: {t:?}");
    assert_eq!(
        t.unknown_timestamp, 0,
        "every registration here carries a timestamp, so coverage must report 0 unknown"
    );
    let by_kind: std::collections::BTreeMap<&str, usize> =
        t.kinds.iter().map(|g| (g.kind.as_str(), g.count)).collect();
    assert_eq!(by_kind.get("Server"), Some(&1));
    assert_eq!(by_kind.get("Gateway"), Some(&1));
    assert_eq!(by_kind.get("Worker"), Some(&2));
    assert_eq!(by_kind.get("Ehdb"), Some(&1));
    assert_eq!(by_kind.get("Execution"), Some(&1));

    // Every kind is present as a GROUP even at count 0, so an empty role reads as 0
    // rather than as a missing key — the absent-vs-zero rule applied to a JSON body.
    assert_eq!(t.kinds.len(), 6, "all six kinds must appear as groups");
    assert_eq!(by_kind.get("Playbook"), Some(&0));
    let _ = std::fs::remove_dir_all(&root);
}

/// ⭐ An ephemeral execution is discoverable while live and gone after, with **no
/// tombstone and no reaper** — which is what makes TTL the right primitive for ephemera
/// rather than a delete.
#[test]
fn an_ephemeral_execution_is_discoverable_while_live_and_gone_after() {
    let (mut st, root) = store("exec");
    let t0 = 1_000 * SEC;
    let short = 30 * SEC;
    reg::register_in(&mut st, RuntimeKind::Execution, "exec-42", "playbook/x", t0).unwrap();

    let live = reg::discover_in(&st, RuntimeKind::Execution, t0 + 10 * SEC, short).unwrap();
    assert_eq!(live.len(), 1, "a live execution must be discoverable");
    assert_eq!(live[0].id(), "exec-42");

    let gone = reg::discover_in(&st, RuntimeKind::Execution, t0 + short + SEC, short).unwrap();
    assert!(
        gone.is_empty(),
        "an execution must vanish by not being renewed — no tombstone was written"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `watch_since` must deliver arrivals AND departures, and — the property that makes it a
/// watch rather than a scan — return **nothing** when nothing changed.
#[test]
fn watch_delivers_arrivals_and_departures_and_is_quiet_when_nothing_changed() {
    let (mut st, root) = store("watch");
    let t0 = 1_000 * SEC;

    reg::register_in(&mut st, RuntimeKind::Server, "srv-w", "c", t0).unwrap();
    let (ops, cursor) = st.watch_since(0).unwrap();
    assert!(!ops.is_empty(), "the registration must be delivered");
    assert!(
        ops.iter().any(|o| o.worker_id == "srv-w"
            && matches!(o.event, RuntimeEvent::Register)
            && o.kind == RuntimeKind::Server),
        "the arrival must carry the id, the event and the KIND: {ops:?}"
    );

    // ⭐ Resuming from the cursor with nothing new must yield nothing. A "watch" that
    // re-delivers the whole log every poll looks identical in any test that only polls
    // from 0, which is why this assertion exists.
    let (none, same) = st.watch_since(cursor).unwrap();
    assert!(
        none.is_empty(),
        "resuming from the cursor must be quiet when nothing changed; got {none:?}"
    );
    assert_eq!(same, cursor, "an empty poll must not move the cursor");

    // A departure is an op like any other, so a consumer learns about leaving too.
    st.deregister("srv-w").unwrap();
    let (after, _) = st.watch_since(cursor).unwrap();
    assert!(
        after
            .iter()
            .any(|o| o.worker_id == "srv-w" && matches!(o.event, RuntimeEvent::Deregister)),
        "the departure must be delivered: {after:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The id that goes into the registry must be a safe substrate key.
#[test]
fn the_self_id_is_constrained_to_the_substrate_key_charset() {
    // `self_id` reads the environment, so assert the property on its output rather than
    // trying to control HOSTNAME from a test that shares a process with others.
    let id = reg::self_id();
    assert!(!id.is_empty(), "an empty id would register nothing findable");
    for c in id.chars() {
        assert!(
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'),
            "id {id:?} carries {c:?}, outside D8's charset — a `/` in a substrate key is a \
             directory traversal waiting to happen"
        );
    }
}

/// The heartbeat interval must stay under the lease, or a healthy process expires between
/// its own renewals and the registry flaps.
#[test]
fn the_heartbeat_interval_is_clamped_below_the_ttl() {
    let hb = reg::heartbeat_interval_secs();
    let ttl = reg::ttl_micros() / 1_000_000;
    assert!(hb > 0, "a zero interval would busy-loop");
    assert!(
        hb < ttl,
        "heartbeat {hb}s must be under the {ttl}s lease, or a live instance expires \
         between its own renewals"
    );
}
