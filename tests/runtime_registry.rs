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

// ---------------------------------------------------------------------------
// The registration API's core: how a worker / gateway / EHDB instance joins.
//
// `agents/rules/data-access-boundary.md` lists `noetl.runtime` as server-owned, so a
// member registers through the server's API rather than opening its own D8 store. That is
// also the only thing that WORKS: a member writing its own local store would be invisible
// to the server's topology, because the stores are separate.
// ---------------------------------------------------------------------------

#[test]
fn upsert_registers_once_then_renews() {
    let (mut st, root) = store("upsert");
    let t0 = 1_000 * SEC;
    let ttl = 180 * SEC;

    let fresh = reg::upsert_in(&mut st, RuntimeKind::Worker, "wk-1", "noetl-worker/1", t0).unwrap();
    assert!(fresh, "the first call must be a fresh registration");

    let again =
        reg::upsert_in(&mut st, RuntimeKind::Worker, "wk-1", "noetl-worker/1", t0 + 60 * SEC)
            .unwrap();
    assert!(
        !again,
        "the second call must RENEW, not re-register — otherwise every heartbeat reads as \
         a fresh arrival in watch_since and a consumer cannot tell a restart from a beat"
    );

    let live = reg::discover_in(&st, RuntimeKind::Worker, t0 + 60 * SEC, ttl).unwrap();
    assert_eq!(live.len(), 1, "one worker, not two");
    assert_eq!(live[0].id(), "wk-1");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn upsert_preserves_the_kind_across_a_renewal() {
    // The same ehdb P2 hazard, now through the API path: a renewal must not reclassify.
    let (mut st, root) = store("upsertkind");
    let t0 = 1_000 * SEC;
    let ttl = 180 * SEC;
    reg::upsert_in(&mut st, RuntimeKind::Gateway, "gw-9", "noetl-gateway/1", t0).unwrap();
    reg::upsert_in(&mut st, RuntimeKind::Gateway, "gw-9", "noetl-gateway/1", t0 + 60 * SEC)
        .unwrap();

    assert_eq!(
        reg::discover_in(&st, RuntimeKind::Gateway, t0 + 60 * SEC, ttl)
            .unwrap()
            .len(),
        1,
        "still a Gateway after renewal"
    );
    assert!(
        reg::discover_in(&st, RuntimeKind::Worker, t0 + 60 * SEC, ttl)
            .unwrap()
            .is_empty(),
        "a renewal must not reclassify a Gateway as a Worker"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// ⚠ An unknown kind must be REFUSED, not silently coerced.
///
/// `RuntimeKind::default()` is `Worker` — correctly, since every pre-P2 record was one. So
/// a caller's typo defaulting to `Worker` would file a gateway under the wrong role and
/// make `discover(Gateway)` wrong in a way nothing reports.
#[test]
fn an_unknown_kind_is_refused_rather_than_defaulted_to_worker() {
    assert_eq!(reg::parse_kind("worker"), Some(RuntimeKind::Worker));
    assert_eq!(reg::parse_kind("Gateway"), Some(RuntimeKind::Gateway));
    assert_eq!(reg::parse_kind("EHDB"), Some(RuntimeKind::Ehdb));
    assert_eq!(reg::parse_kind("execution"), Some(RuntimeKind::Execution));
    for bad in ["wrker", "", "worker ", "Workers", "node", "servers"] {
        assert_eq!(
            reg::parse_kind(bad),
            None,
            "{bad:?} must be refused; defaulting it to Worker would misfile a member"
        );
    }
}

#[test]
fn the_whole_fleet_registers_through_one_store_and_discovery_separates_it() {
    let (mut st, root) = store("fleetapi");
    let t0 = 1_000 * SEC;
    let ttl = 180 * SEC;
    for (kind, id) in [
        (RuntimeKind::Server, "noetl-server-rust-embedded-0:2"),
        (RuntimeKind::Worker, "noetl-worker-rust-abc"),
        (RuntimeKind::Worker, "noetl-worker-system-pool-def"),
        (RuntimeKind::Gateway, "noetl-gateway-xyz"),
        (RuntimeKind::Ehdb, "noetl-cmdbus-writer-0"),
    ] {
        assert!(
            reg::upsert_in(&mut st, kind, id, "c", t0).unwrap(),
            "{id} must register fresh"
        );
    }
    let t = reg::topology_in(&st, t0, ttl).unwrap();
    let by: std::collections::BTreeMap<&str, usize> =
        t.kinds.iter().map(|g| (g.kind.as_str(), g.count)).collect();
    assert_eq!(by.get("Server"), Some(&1));
    assert_eq!(by.get("Worker"), Some(&2), "both worker pools");
    assert_eq!(by.get("Gateway"), Some(&1));
    assert_eq!(by.get("Ehdb"), Some(&1));
    assert_eq!(t.live_total, 5);
    assert_eq!(t.unknown_timestamp, 0);

    // ⭐ And the whole fleet drops out on TTL together when nothing renews — the lease is
    // the guarantee, so correctness does not depend on any shutdown hook running.
    let later = reg::topology_in(&st, t0 + ttl + SEC, ttl).unwrap();
    assert_eq!(
        later.live_total, 0,
        "every member must expire by lease once renewals stop; got {later:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn deregister_is_a_shortcut_and_the_lease_is_still_the_guarantee() {
    let (mut st, root) = store("dereg");
    let t0 = 1_000 * SEC;
    let ttl = 180 * SEC;
    reg::upsert_in(&mut st, RuntimeKind::Worker, "wk-bye", "c", t0).unwrap();
    assert_eq!(
        reg::discover_in(&st, RuntimeKind::Worker, t0, ttl).unwrap().len(),
        1
    );

    assert!(st.deregister("wk-bye").unwrap(), "a clean departure removes it");
    assert!(
        reg::discover_in(&st, RuntimeKind::Worker, t0 + SEC, ttl)
            .unwrap()
            .is_empty(),
        "deregistered immediately, without waiting out the lease"
    );

    // The control: a member that does NOT deregister still goes away. Correctness must not
    // depend on a shutdown hook, which is exactly what a crashed pod never runs.
    reg::upsert_in(&mut st, RuntimeKind::Worker, "wk-crash", "c", t0).unwrap();
    assert!(
        reg::discover_in(&st, RuntimeKind::Worker, t0 + ttl + SEC, ttl)
            .unwrap()
            .is_empty(),
        "a crashed member must expire by lease even though it never deregistered"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// The pool mirror: how the fleet becomes discoverable with NO worker change.
//
// The worker has posted /api/worker/pool/register and /heartbeat since long before the
// registry existed. Mirroring those two calls server-side populates discover(Worker)
// without touching the worker, its ehdb pin, or its deployment.
// ---------------------------------------------------------------------------

#[test]
fn pool_kind_maps_what_the_worker_actually_sends() {
    // The literal string the Rust worker posts, and the server's own default.
    assert_eq!(reg::pool_kind("worker_pool"), Some(RuntimeKind::Worker));
    assert_eq!(reg::pool_kind("worker"), Some(RuntimeKind::Worker));
    assert_eq!(reg::pool_kind("gateway"), Some(RuntimeKind::Gateway));
    assert_eq!(reg::pool_kind("ehdb"), Some(RuntimeKind::Ehdb));
    assert_eq!(reg::pool_kind("tier"), Some(RuntimeKind::Ehdb));
    assert_eq!(reg::pool_kind("server"), Some(RuntimeKind::Server));
    // ⚠ Unrecognised must be skipped, not defaulted. Misfiling a gateway as a Worker
    // makes discover(Gateway) wrong in a way nothing reports.
    for bad in ["", "pool", "node", "worker-pool", "runtime"] {
        assert_eq!(
            reg::pool_kind(bad),
            None,
            "{bad:?} must not be mirrored under a guessed kind"
        );
    }
}

#[test]
fn sanitise_id_derives_a_legal_substrate_key_and_refuses_the_rest() {
    // Real pool names pass through untouched.
    assert_eq!(
        reg::sanitise_id("noetl-worker-rust-f7bb67444-gmqf9").as_deref(),
        Some("noetl-worker-rust-f7bb67444-gmqf9")
    );
    assert_eq!(reg::sanitise_id("shard.3").as_deref(), Some("shard.3"));
    assert_eq!(reg::sanitise_id("pod:2").as_deref(), Some("pod:2"));
    // A `/` would be a directory traversal in a substrate key.
    assert_eq!(reg::sanitise_id("a/b").as_deref(), Some("a-b"));
    assert_eq!(reg::sanitise_id("a b").as_deref(), Some("a-b"));
    // Refused rather than normalised into something meaningless.
    assert_eq!(reg::sanitise_id(""), None, "an empty id registers nothing findable");
    assert_eq!(
        reg::sanitise_id(".."),
        None,
        "a dot-run resolves as a path segment; validate_op refuses it and so must this"
    );
    assert_eq!(reg::sanitise_id("..."), None);
}

#[test]
fn a_mirrored_worker_pool_registration_is_discoverable_as_a_worker() {
    let (mut st, root) = store("mirror");
    let t0 = 1_000 * SEC;
    let ttl = 180 * SEC;

    // Exactly what the mirror does for the body the worker already posts.
    let kind = reg::pool_kind("worker_pool").expect("worker_pool must map");
    let id = reg::sanitise_id("noetl-worker-rust-f7bb67444-gmqf9").expect("legal id");
    reg::upsert_in(&mut st, kind, &id, "worker_pool:ready", t0).unwrap();

    let found = reg::discover_in(&st, RuntimeKind::Worker, t0, ttl).unwrap();
    assert_eq!(found.len(), 1, "the pool must be discoverable as a Worker");
    assert_eq!(found[0].id(), "noetl-worker-rust-f7bb67444-gmqf9");
    assert_eq!(found[0].contract, "worker_pool:ready");

    // Its heartbeat renews rather than duplicating.
    assert!(
        !reg::upsert_in(&mut st, kind, &id, "worker_pool:heartbeat", t0 + 60 * SEC).unwrap(),
        "a mirrored heartbeat must renew, not re-register"
    );
    assert_eq!(
        reg::discover_in(&st, RuntimeKind::Worker, t0 + 60 * SEC, ttl)
            .unwrap()
            .len(),
        1,
        "still exactly one worker after a heartbeat"
    );

    // ⭐ And a killed worker drops out on TTL, because it stops beating. No reaper.
    assert!(
        reg::discover_in(&st, RuntimeKind::Worker, t0 + 60 * SEC + ttl + SEC, ttl)
            .unwrap()
            .is_empty(),
        "a worker that stopped heartbeating must expire by lease"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The mirror runs inside a live request handler, so on a server with no embedded engine it
/// must be a harmless no-op rather than an error or a panic.
///
/// ⚠ This is the control that matters most for shipping it: the authoritative registration
/// has already succeeded by the time the mirror is attempted, so a mirror that could fail
/// would turn a working endpoint into a broken one on exactly the deployments that do not
/// run the embedded engine.
#[test]
fn the_mirror_is_a_harmless_no_op_when_the_registry_is_off() {
    // No NOETL_EHDB_EMBEDDED in the test environment, so `store()` is None.
    assert!(
        !reg::registry_enabled(),
        "precondition: the registry must be off here, or this proves nothing"
    );
    // Must not panic, must not error.
    reg::mirror_pool_register("worker_pool", "noetl-worker-x", "ready");
    reg::mirror_pool_heartbeat("worker_pool", "noetl-worker-x");
    reg::mirror_pool_deregister("noetl-worker-x");
    // And the direct paths report unavailability rather than pretending to succeed.
    assert!(reg::upsert(RuntimeKind::Worker, "x", "c").is_err());
    assert!(reg::topology().is_none());
    assert!(reg::watch(0).is_none());
}

// ---------------------------------------------------------------------------
// The write-rate throttle, and P4's ephemeral executions.
// ---------------------------------------------------------------------------

/// ⚠⚠ The throttle exists because of a MEASUREMENT, not a guess. With the real 7-member
/// prod fleet the mirror appended **22.9 ops/min** for **6.7 MB/day** — 22x the
/// ~0.3 MB/day I had estimated from one member at 60 s. The per-record size was right
/// (203 B measured); the rate was wrong on both member count and interval. D8 cannot be
/// compacted, because `watch_since` reads history, so the only lever is writing fewer ops.
#[test]
fn the_throttle_skips_a_redundant_beat_without_affecting_liveness() {
    let (mut st, root) = store("throttle");
    let t0 = 1_000 * SEC;
    let ttl = 180 * SEC;
    let floor = ttl / 3; // 60s

    assert_eq!(
        reg::upsert_throttled_in(&mut st, RuntimeKind::Worker, "wk-t", "c", t0, floor).unwrap(),
        reg::UpsertOutcome::Registered,
        "first call is a fresh registration"
    );

    // A beat 18s later — the real observed interval — is redundant and must be skipped.
    assert_eq!(
        reg::upsert_throttled_in(
            &mut st,
            RuntimeKind::Worker,
            "wk-t",
            "c",
            t0 + 18 * SEC,
            floor
        )
        .unwrap(),
        reg::UpsertOutcome::Skipped,
        "a beat inside the floor must not append an op"
    );

    // ⭐ And the skip costs nothing: the member is still live, because the floor is well
    // inside the lease. A throttle that could expire a healthy member would be a bug.
    assert_eq!(
        reg::discover_in(&st, RuntimeKind::Worker, t0 + 18 * SEC, ttl)
            .unwrap()
            .len(),
        1,
        "skipping a beat must not affect liveness"
    );

    // Past the floor, it renews.
    assert_eq!(
        reg::upsert_throttled_in(
            &mut st,
            RuntimeKind::Worker,
            "wk-t",
            "c",
            t0 + floor + SEC,
            floor
        )
        .unwrap(),
        reg::UpsertOutcome::Renewed,
        "a beat past the floor must renew"
    );
    // ...and the renewal actually moved the lease, so renewing keeps it alive indefinitely.
    assert_eq!(
        reg::discover_in(&st, RuntimeKind::Worker, t0 + floor + ttl, ttl)
            .unwrap()
            .len(),
        1,
        "the renewal must have moved last_seen, not merely returned Renewed"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A record with no timestamp predates the field, so it is renewed rather than skipped —
/// the one case where writing is the conservative choice, because its liveness is unknown.
#[test]
fn a_timestampless_record_is_renewed_rather_than_skipped() {
    let (mut st, root) = store("throttle0");
    let t0 = 1_000 * SEC;
    // `register` (not `register_at`) writes the pre-P1 shape with no timestamp.
    st.register("legacy-wk", "c").unwrap();
    let got = st.get("legacy-wk").unwrap().expect("registered");
    assert_eq!(
        got.last_seen_micros, 0,
        "precondition: this fixture must actually have no timestamp, or the test is vacuous"
    );
    assert_eq!(
        reg::upsert_throttled_in(&mut st, RuntimeKind::Worker, "legacy-wk", "c", t0, 60 * SEC)
            .unwrap(),
        reg::UpsertOutcome::Renewed,
        "an unknown last_seen must be renewed, never skipped"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn is_terminal_event_matches_both_spellings_prod_actually_stores() {
    // ⚠ Both spellings. Prod stores `playbook.completed` and historically
    // `playbook_completed`; knowing only one would leave half of all executions to expire
    // by TTL instead of being removed promptly — a silent half-failure.
    for t in [
        "playbook.completed",
        "playbook_completed",
        "playbook.failed",
        "playbook_failed",
        "execution.completed",
        "execution.failed",
    ] {
        assert!(reg::is_terminal_event(t), "{t} must be terminal");
    }
    for t in [
        "playbook.initialized",
        "playbook_started",
        "command.issued",
        "call.done",
        "step.completed",
        "",
    ] {
        assert!(
            !reg::is_terminal_event(t),
            "{t} must NOT be terminal — removing an execution mid-run would make a running \
             one undiscoverable"
        );
    }
}

/// ⭐ P4 end to end: an execution is discoverable while it runs and gone once it finishes.
#[test]
fn an_execution_is_discoverable_while_running_and_removed_when_it_finishes() {
    let (mut st, root) = store("p4");
    let t0 = 1_000 * SEC;
    let exec_ttl = 300 * SEC;

    reg::register_in(&mut st, RuntimeKind::Execution, "exec-12345", "execution", t0).unwrap();
    let live = reg::discover_in(&st, RuntimeKind::Execution, t0 + 5 * SEC, exec_ttl).unwrap();
    assert_eq!(live.len(), 1, "a running execution must be discoverable");
    assert_eq!(live[0].id(), "exec-12345");

    // The terminal event's effect.
    assert!(st.deregister("exec-12345").unwrap());
    assert!(
        reg::discover_in(&st, RuntimeKind::Execution, t0 + 6 * SEC, exec_ttl)
            .unwrap()
            .is_empty(),
        "a finished execution must leave discovery at once, not wait out its lease"
    );

    // ⭐ The backstop: one that crashes without a terminal event still goes away.
    reg::register_in(&mut st, RuntimeKind::Execution, "exec-crashed", "execution", t0).unwrap();
    assert_eq!(
        reg::discover_in(&st, RuntimeKind::Execution, t0 + 10 * SEC, exec_ttl)
            .unwrap()
            .len(),
        1,
        "still live well inside its lease"
    );
    assert!(
        reg::discover_in(&st, RuntimeKind::Execution, t0 + exec_ttl + SEC, exec_ttl)
            .unwrap()
            .is_empty(),
        "an execution that never emitted a terminal event must expire by lease"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_execution_gets_a_longer_lease_than_a_heartbeating_member() {
    // An execution does not heartbeat, so sharing the fleet lease would drop a
    // long-running one out of discovery while it is still running.
    let fleet = reg::ttl_for(RuntimeKind::Worker);
    let exec = reg::ttl_for(RuntimeKind::Execution);
    assert_eq!(fleet, reg::ttl_micros());
    assert_eq!(exec, reg::execution_ttl_micros());
    assert!(
        exec > fleet,
        "the execution lease ({exec}) must exceed the fleet lease ({fleet}), or a \
         long-running execution vanishes from discovery while running"
    );
}

/// The heartbeat floor must sit strictly inside the lease, or the throttle could skip the
/// beat that would have kept a member alive.
#[test]
fn the_heartbeat_floor_is_strictly_inside_the_lease() {
    let floor = reg::heartbeat_floor_micros();
    let ttl = reg::ttl_micros();
    assert!(floor > 0, "a zero floor would skip nothing");
    assert!(
        floor * 2 < ttl,
        "the floor ({floor}) must leave room for a missed beat inside the lease ({ttl})"
    );
}

// ---------------------------------------------------------------------------
// Call-site proofs. A hook nobody calls is the noetl/ai-meta#326 defect: the
// embedded shadow lived on the materializer, which prod's scheduled traffic never
// reaches, and sat armed and unexercised reporting the all-zero series a healthy
// shadow reports. These assert the wiring, not just the function.
// ---------------------------------------------------------------------------

#[test]
fn the_execution_registration_is_called_from_the_execute_path() {
    let src = include_str!("../src/handlers/execute.rs");
    assert!(
        src.contains("mirror_execution_started("),
        "execute.rs must call mirror_execution_started, or a live execution is never \
         registered and P4 is dormant"
    );
    // It must sit on the "started" outcome, not the duplicate branch.
    let at = src
        .find("mirror_execution_started(")
        .expect("call site present");
    let tail = &src[at..];
    assert!(
        tail.contains("status: \"started\""),
        "the registration must be on the started outcome; a duplicate started nothing"
    );
}

#[test]
fn the_execution_deregistration_is_called_from_the_emit_chokepoint() {
    let src = include_str!("../src/handlers/event_write.rs");
    assert!(
        src.contains("mirror_execution_finished("),
        "event_write.rs must call mirror_execution_finished, or a finished execution is \
         only ever removed by its lease lapsing"
    );
    assert!(
        src.contains("is_terminal_event("),
        "the removal must be gated on a terminal event, not fire for every row"
    );
}

// ---------------------------------------------------------------------------
// The `Ehdb` kind: registered from a SUCCESSFUL publish, never from configuration.
// ---------------------------------------------------------------------------

#[test]
fn an_ehdb_tier_is_discoverable_once_registered() {
    let (mut st, root) = store("ehdbkind");
    let t0 = 1_000 * SEC;
    let ttl = 180 * SEC;
    // The exact prod address shape: dots and a colon, both legal in D8's charset.
    let addr = "noetl-cmdbus-writer-0.noetl.svc.cluster.local:9103";
    assert_eq!(
        reg::sanitise_id(addr).as_deref(),
        Some(addr),
        "a writer address must survive sanitisation unchanged, or the id would drift from \
         the thing it names"
    );
    reg::upsert_in(&mut st, RuntimeKind::Ehdb, addr, "ehdb-tier", t0).unwrap();

    let found = reg::discover_in(&st, RuntimeKind::Ehdb, t0, ttl).unwrap();
    assert_eq!(found.len(), 1, "the tier must be discoverable as Ehdb");
    assert_eq!(found[0].id(), addr);
    // It must NOT leak into another role.
    assert!(
        reg::discover_in(&st, RuntimeKind::Worker, t0, ttl)
            .unwrap()
            .is_empty(),
        "an Ehdb tier must not appear as a Worker"
    );
    // And it expires like anything else once publishes stop.
    assert!(
        reg::discover_in(&st, RuntimeKind::Ehdb, t0 + ttl + SEC, ttl)
            .unwrap()
            .is_empty(),
        "a tier that stops answering must expire by lease"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// ⚠⚠ The registration must be driven by a successful round-trip, not by configuration.
///
/// Registering a writer because an env var names it would claim liveness for an address
/// that might be down — the representation-drift failure this registry exists to avoid. So
/// this asserts the call site sits behind a success check.
#[test]
fn the_ehdb_registration_fires_only_on_a_successful_publish() {
    let src = include_str!("../src/event_bus.rs");
    assert!(
        src.contains("mirror_ehdb_tier_seen("),
        "event_bus.rs must call mirror_ehdb_tier_seen, or discover(Ehdb) stays empty"
    );
    let at = src.find("mirror_ehdb_tier_seen(").expect("call site");
    let window = &src[at.saturating_sub(400)..at];
    assert!(
        window.contains("out.is_ok()"),
        "the call must be gated on a successful publish; registering from configuration \
         would claim liveness for a writer that may be down"
    );
    assert!(
        window.contains("addr_for_execution("),
        "it must register the address actually routed to, not every configured one — a \
         single success says nothing about writers that were never contacted"
    );
}

/// The hot path must not take the registry mutex on every publish.
///
/// ⚠ Asserted as a PROPERTY, not as a particular mechanism: the throttle moved from a
/// single atomic to a per-address table when a second call site appeared, and a test
/// naming `EHDB_LAST_SEEN.load` would have failed for the mechanism change while saying
/// nothing about whether the property still held.
#[test]
fn the_ehdb_hook_throttles_before_it_takes_the_registry_lock() {
    let src = include_str!("../src/runtime_registry.rs");
    let at = src
        .find("pub fn mirror_ehdb_tier_seen(")
        .expect("the hook exists");
    let body = &src[at..at + 1800];

    let throttle_at = body
        .find("tier_seen_due(")
        .expect("the hook must consult the throttle");
    let store_at = body
        .find("store()")
        .expect("the hook must reach the registry store");
    assert!(
        throttle_at < store_at,
        "the throttle must be consulted BEFORE the registry is touched, or every publish          queues behind the registry — an observability surface slowing the thing it observes"
    );
    // Both locks must be non-blocking: the throttle's and the registry's.
    // ⚠ Counted as CALLS (`.try_lock()`), not as the bare token: the doc comments in this
    // function mention `try_lock` twice, so counting the token read 4 where the answer is
    // 2 — a comment standing in for a caller, the same false reading this repo keeps
    // finding.
    assert_eq!(
        body.matches(".try_lock()").count(),
        2,
        "both the throttle and the registry must be taken with try_lock: a publish must          never wait on either"
    );
}

// ---------------------------------------------------------------------------
// noetl/ai-meta#455 P2 — the `Ehdb` kind, reached on a path prod's traffic takes.
//
// ⚠⚠ v3.130.0 placed this hook on the EVENTS publish and shipped it. Prod measured
// `ehdb_registered` = 0 for 18 minutes, because `should_publish` returns false for
// system executions (`event_write.rs`, reason `system_execution` = 22 on prod) and
// prod's only regular traffic — the hourly cleanup and the 3-minutely watchdog — IS
// system executions. The events publish is therefore never called there.
//
// The two structural tests above PASSED throughout. They assert the call site is
// correctly gated; neither asserts the gated path is one prod traffic traverses.
// That is `existence != reachability` (noetl/ai-meta#326) in the one place I had
// quoted #326 while placing a different hook correctly.
// ---------------------------------------------------------------------------

/// ⚠⚠ RED before the fix. One global last-seen slot served one call site adequately.
/// With two buses reporting, the first address to claim the slot throttles the OTHER out
/// for a whole floor interval — so a live tier stays unregistered while every counter
/// reads healthy. One cap serving two addresses is the same defect as one cap serving two
/// directions (noetl/worker#314).
#[test]
fn two_tier_addresses_do_not_throttle_each_other() {
    let mut seen = Vec::new();
    let floor = 60_000_000; // 60s
    let t0 = 1_000_000_000u64;

    let cmdbus = "noetl-cmdbus-writer-0.noetl.svc.cluster.local:9100";
    let events = "noetl-cmdbus-writer-0.noetl.svc.cluster.local:9103";

    assert!(
        reg::tier_seen_due(&mut seen, cmdbus, t0, floor),
        "the first sighting of an address is always due"
    );
    assert!(
        reg::tier_seen_due(&mut seen, events, t0 + 1, floor),
        "a DIFFERENT address one microsecond later must still be due — the two buses are \
         two tiers, and throttling them together makes one of them invisible"
    );

    // Negative control: the throttle must still actually throttle, or this test would
    // pass against a function that returns `true` unconditionally.
    assert!(
        !reg::tier_seen_due(&mut seen, cmdbus, t0 + floor - 1, floor),
        "the SAME address inside the floor must NOT be due"
    );
    assert!(
        reg::tier_seen_due(&mut seen, cmdbus, t0 + floor, floor),
        "the same address at the floor boundary must be due again"
    );
}

/// The throttle tracks a config-derived address set, so it is small — but an unbounded
/// vec on the publish path is a leak waiting for a rotating address.
#[test]
fn the_tier_throttle_is_bounded_and_still_admits_a_new_address() {
    let mut seen = Vec::new();
    let floor = 60_000_000;
    for i in 0..300 {
        reg::tier_seen_due(&mut seen, &format!("writer-{i}:9100"), 1_000_000 + i, floor);
    }
    assert!(
        seen.len() <= 64,
        "the throttle grew to {} entries on the publish path",
        seen.len()
    );
    // Eviction must not make a genuinely new tier permanently invisible.
    assert!(
        reg::tier_seen_due(&mut seen, "a-brand-new-writer:9100", 9_999_999_999, floor),
        "a new address must still register once the table is full — refusing would hide a \
         live tier forever"
    );
}

/// ⭐ The reachability assertion the two older structural tests do not make: the hook must
/// sit on the COMMAND publish, which prod's system executions do traverse
/// (`noetl_command_publish_total{pool="system"}` = 4 on prod while
/// `noetl_event_ingest_total` was absent entirely).
#[test]
fn the_command_publish_also_registers_the_tier() {
    let src = include_str!("../src/handlers/execute.rs");
    assert!(
        src.contains("mirror_ehdb_tier_seen("),
        "the command publish must register the tier too: the events publish is skipped for \
         system executions, which is ALL of prod's regular traffic, so an events-only hook \
         leaves discover(Ehdb) empty forever"
    );
}

/// ⚠⚠ `Deferred` is NOT evidence of liveness. It means the on-path budget elapsed and a
/// background task is still retrying — the writer may be down. Registering on it would
/// claim liveness from a timeout, the same misclassification as `tier-load` reporting
/// `ok:true` for an explicit refusal.
#[test]
fn the_tier_registration_is_not_claimed_from_a_deferred_publish() {
    let src = include_str!("../src/handlers/execute.rs");
    let at = src
        .find("mirror_ehdb_tier_seen(")
        .expect("the command-bus hook exists");
    // Everything before the call site: the nearest preceding match arm is the one whose
    // body the call sits in. Measured at any distance, so the length of the comment above
    // the call cannot change the verdict.
    let before = &src[..at];
    let landed = before.rfind("PublishOutcome::Landed");
    let deferred = before.rfind("PublishOutcome::Deferred");
    let landed = landed.expect(
        "the hook must sit inside the Landed arm — the nearest preceding arm is what the \
         registration is claiming",
    );
    assert!(
        deferred.is_none_or(|d| d < landed),
        "the hook sits under Deferred: a publish whose on-path budget merely elapsed says \
         nothing about whether the writer is up, so registering there would claim liveness \
         from a timeout — the same misclassification as reporting ok on an explicit refusal"
    );

    // Positive control: the search is capable of finding a Deferred arm at all, so the
    // assertion above is measuring arm order and not a typo that never matches.
    assert!(
        src.contains("PublishOutcome::Deferred"),
        "execute.rs must still have a Deferred arm, or this test proves nothing"
    );
}
