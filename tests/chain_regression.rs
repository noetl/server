//! **The permanent regression guard for the chain-store failure modes.**
//!
//! Five failure modes, each measured in kind on 2026-09-28 before it was fixed,
//! each locked here. See [noetl/ai-meta#357](https://github.com/noetl/ai-meta/issues/357).
//!
//! # ⚠⚠ Why this file needs no database
//!
//! `tests/chain_log_sourced.rs` proves the same properties end-to-end against a
//! real PostgreSQL — and **skips when `NOETL_TEST_PG_URL` is unset**, which is
//! every CI run. A skipped test reads identically to a passing one, so that suite
//! is a proof for a human running it deliberately, not a guard.
//!
//! `server`'s `test.yml` runs `cargo test --workspace --all-targets`, so a
//! DB-free suite is genuinely executed on every push. The chain store itself
//! needs no database — only the *log source* does — so four of the five modes are
//! reproducible from a temp directory, and the fifth (forward identity) is
//! expressed against the log slice, which is exactly what the Postgres oracle
//! supplies in the e2e version.
//!
//! The two suites are therefore complementary, not redundant: this one runs
//! always and covers the store; that one runs on demand and covers the wiring.
//!
//! # The five modes
//!
//! | # | mode | measured before the fix |
//! | :-: | :-- | :-- |
//! | 1 | mid-flight arming truncates a running execution | Postgres 3 events, store **1**, reported `authoritative first=1` |
//! | 2 | post-restart null-prev rows create pseudo-roots | store froze at 6 of 7; 534 of 595 kind executions carried >1 null-prev root |
//! | 3 | guarded read serves content the marker has outrun | watermark `1:7` over a 6-event partition, read returned `Some(6)` |
//! | 4 | forward identity | — (the property that must never regress) |
//! | 5 | flag-off is inert | — (0 files written) |

use std::sync::Arc;

use ehdb_l0::chain_populator::{populator_enabled, Authority, ChainPopulator, FromLog, LogEvent};
use ehdb_l0::chain_store_durable::DurableChainStore;
use ehdb_l0::substrate::{DurableSubstrate, LocalFsSubstrate};

struct Rig {
    _dir: tempfile::TempDir,
    store: DurableChainStore,
    substrate: Arc<dyn DurableSubstrate>,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("tempdir");
    let fs = LocalFsSubstrate::new(dir.path()).expect("substrate");
    let substrate: Arc<dyn DurableSubstrate> = Arc::new(fs);
    Rig {
        _dir: dir,
        store: DurableChainStore::new(Arc::clone(&substrate)),
        substrate,
    }
}

impl Rig {
    fn pop(&self) -> ChainPopulator<'_> {
        ChainPopulator::new(&self.store, self.substrate.as_ref())
    }
}

/// A log slice. ⚠ Carries NO `prev_event_id` — the edge is recomputed from
/// position, because the column is NULL on 643,420 of 645,677 measured prod-shaped
/// rows and 534 of 595 executions carry more than one null-prev root.
fn log(ids: &[&str]) -> Vec<LogEvent> {
    ids.iter()
        .map(|id| LogEvent {
            event_id: (*id).to_string(),
            parent_execution_id: None,
            payload: "{}".to_string(),
        })
        .collect()
}

// ===========================================================================
// MODE 1 — mid-flight arming must not truncate a running execution.
// ===========================================================================

/// ⭐⭐ The execution has been running for a while when the populator is armed.
/// The partition must hold the WHOLE log, rooted at the log's first event.
///
/// Before the fix: store held the tail from arming time and reported
/// `authoritative first=1` — `first=1` true of the store, false of the execution,
/// and the answer was indistinguishable from a complete chain (contiguous seqs,
/// gap check passes, root reports no predecessor).
#[test]
fn mode1_mid_flight_arming_does_not_truncate() {
    let r = rig();
    let p = r.pop();
    let full = log(&["e1", "e2", "e3", "e4", "e5"]);
    p.populate_from_log("running", &full).expect("populate");

    let chain = p
        .chain_if_authoritative("running")
        .expect("guarded read")
        .expect("a log-sourced partition must be servable");

    assert_eq!(
        chain.len(),
        full.len(),
        "the partition must hold the WHOLE log, not the tail from arming time"
    );
    assert_eq!(
        chain[0].event_id, "e1",
        "rooted at the LOG's first event, not at whichever event arrived first"
    );
    assert!(
        chain[0].prev_event_id.is_none(),
        "and that root is the only event without a predecessor"
    );
}

/// ⚠⚠ A partition built by the per-event path must NEVER be served, because that
/// path cannot know whether it saw the execution's first event. This is the
/// mechanism that makes mode 1 impossible rather than merely unlikely.
#[test]
fn mode1_a_per_event_partition_is_never_served() {
    let r = rig();
    let p = r.pop();
    p.populate("emit", "x", None, None, "{}").expect("append");
    p.populate("emit", "y", Some("x"), None, "{}")
        .expect("append");

    assert!(
        p.authority("emit").expect("authority").is_trustworthy(),
        "the watermark exists — that is not the same as being servable"
    );
    assert_eq!(
        p.coverage("emit").expect("coverage"),
        None,
        "the per-event path must record NO coverage"
    );
    assert_eq!(
        p.chain_if_authoritative("emit").expect("guarded read"),
        None,
        "and without coverage the guarded read must refuse, however complete the \
         partition happens to be"
    );
}

// ===========================================================================
// MODE 2 — post-restart null-prev rows must not create pseudo-roots.
// ===========================================================================

/// ⭐⭐ The prod shape: a log whose middle carries extra `prev = NULL` rows,
/// because the server's in-memory head map was cold after a restart.
///
/// Before the fix the store rejected the first such row as not-the-head and the
/// partition then never grew again, while the watermark advanced past it.
#[test]
fn mode2_post_restart_null_prev_rows_yield_one_root_and_real_links() {
    let r = rig();
    let p = r.pop();
    // 7 events; in Postgres, #5 and #6 would carry prev = NULL.
    let l = log(&["a1", "a2", "a3", "a4", "a5", "a6", "a7"]);
    assert_eq!(
        p.populate_from_log("restarted", &l).expect("populate"),
        FromLog::InSync {
            total: 7,
            appended: 7
        }
    );

    let chain = p
        .chain_if_authoritative("restarted")
        .expect("guarded")
        .expect("servable");
    assert_eq!(chain.len(), 7, "every event, not the pre-restart prefix");

    let roots = chain.iter().filter(|e| e.prev_event_id.is_none()).count();
    assert_eq!(
        roots, 1,
        "exactly ONE chain root. This is the 534-of-595 shape; a log with several \
         null-prev rows must still produce a single root."
    );
    for w in chain.windows(2) {
        assert_eq!(
            w[1].prev_event_id.as_deref(),
            Some(w[0].event_id.as_str()),
            "every event must carry a REAL prev link to its predecessor"
        );
    }
}

/// The store must keep tracking an execution that keeps emitting after a restart —
/// i.e. a longer log extends the partition rather than being rejected.
#[test]
fn mode2_the_partition_keeps_tracking_after_a_restart() {
    let r = rig();
    let p = r.pop();
    p.populate_from_log("grow", &log(&["g1", "g2", "g3"]))
        .expect("first");
    // The server restarts; the execution emits two more.
    assert_eq!(
        p.populate_from_log("grow", &log(&["g1", "g2", "g3", "g4", "g5"]))
            .expect("second"),
        FromLog::InSync {
            total: 5,
            appended: 2
        },
        "a longer log must EXTEND by exactly the remainder, not freeze and not \
         re-append"
    );
    let chain = p.chain_if_authoritative("grow").unwrap().expect("servable");
    assert_eq!(chain.len(), 5);
    assert_eq!(chain[4].prev_event_id.as_deref(), Some("g4"));
}

// ===========================================================================
// MODE 3 — the guarded read must refuse when the marker outruns the content.
// ===========================================================================

/// ⭐⭐ Watermark ahead of the stored chain ⇒ **cannot answer**.
///
/// Measured before the fix: Postgres 7, store 6, watermark `1:7`, guarded read
/// `Some(6)` — a contiguous, gap-check-passing, silently STALE answer.
#[test]
fn mode3_a_marker_ahead_of_the_content_refuses() {
    let r = rig();
    let p = r.pop();
    p.populate_from_log("wm", &log(&["w1", "w2"]))
        .expect("populate");
    assert!(
        p.chain_if_authoritative("wm").unwrap().is_some(),
        "precondition: it is servable while marker and content agree"
    );

    r.substrate
        .put_overwrite("chain/wm/wm", b"1:9")
        .expect("advance the marker past the content");

    assert_eq!(
        p.chain_if_authoritative("wm").unwrap(),
        None,
        "a marker ahead of the stored head must read as CANNOT ANSWER, never as a \
         short chain the caller has no way to tell is short"
    );
    // ⭐ And the content is not lost — refusing to serve is not deleting.
    assert_eq!(
        p.authority("wm").unwrap().label(),
        "authoritative",
        "the marker survives, which is what a repair pass needs to find this"
    );
}

/// Coverage claiming more than is stored must also refuse — the crash-mid-extend
/// shape, since coverage is claimed before the appends.
#[test]
fn mode3_coverage_ahead_of_the_content_refuses() {
    let r = rig();
    let p = r.pop();
    p.populate_from_log("cov", &log(&["c1", "c2"]))
        .expect("populate");
    r.substrate
        .put_overwrite("chain/cov/cov", b"c1:9")
        .expect("advance coverage");
    assert_eq!(
        p.chain_if_authoritative("cov").unwrap(),
        None,
        "coverage ahead of content must read as CANNOT ANSWER"
    );
}

// ===========================================================================
// MODE 4 — forward identity: the store holds exactly what the log holds.
// ===========================================================================

/// ⭐ The store's content must equal the log's, element for element, in order.
///
/// In the e2e suite the log comes from `noetl.event` and this is the independent
/// oracle check. Here the log slice IS that content, so the property is the same
/// one stated without a database.
#[test]
fn mode4_forward_identity_across_several_shapes() {
    let shapes: &[(&str, &[&str])] = &[
        ("single", &["s1"]),
        ("pair", &["p1", "p2"]),
        ("seven", &["x1", "x2", "x3", "x4", "x5", "x6", "x7"]),
        (
            "long",
            &["l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9"],
        ),
    ];
    let mut checked = 0usize;
    for (name, ids) in shapes {
        let r = rig();
        let p = r.pop();
        let l = log(ids);
        p.populate_from_log(name, &l).expect("populate");
        let chain = p.chain_if_authoritative(name).unwrap().expect("servable");

        assert_eq!(
            chain
                .iter()
                .map(|e| e.event_id.as_str())
                .collect::<Vec<_>>(),
            *ids,
            "{name}: the store's ids must equal the log's, in order"
        );
        assert_eq!(
            chain.iter().map(|e| e.exec_seq).collect::<Vec<_>>(),
            (1..=ids.len() as u64).collect::<Vec<_>>(),
            "{name}: sequences must be 1..n with no holes"
        );
        checked += ids.len();
    }
    // ⚠ Print the denominator: a loop that silently iterated zero shapes would
    // pass every assertion above.
    assert_eq!(
        checked, 19,
        "expected to check 19 events across 4 shapes; checked {checked}. A guard \
         measuring nothing passes."
    );
}

/// An execution the log genuinely has no events for answers `Some(empty)` —
/// a TRUE negative, and distinct from `None` (cannot answer).
#[test]
fn mode4_an_empty_log_is_trusted_and_is_not_cannot_answer() {
    let r = rig();
    let p = r.pop();
    p.populate_from_log("empty", &[]).expect("populate empty");
    assert_eq!(
        p.chain_if_authoritative("empty").unwrap().map(|c| c.len()),
        Some(0),
        "a populated-but-eventless execution must answer Some(empty)"
    );
    assert_eq!(
        p.chain_if_authoritative("never-seen").unwrap(),
        None,
        "and an execution nothing populated must answer None — these two must \
         never collapse into each other"
    );
}

// ===========================================================================
// MODE 5 — flag-off is inert.
// ===========================================================================

/// ⚠ The populator flag defaults OFF, so a deploy carrying this code changes
/// nothing until somebody arms it.
#[test]
fn mode5_the_populator_flag_defaults_off() {
    // Read through the one shared authority, so this also pins that the server
    // and the engine crate agree on the flag's name and parse.
    if std::env::var(ehdb_l0::chain_populator::POPULATOR_ENV).is_ok() {
        eprintln!("SKIP: the flag is set in this environment");
        return;
    }
    assert!(
        !populator_enabled(),
        "with the flag absent the populator must be off — this is what makes the \
         deploy inert on arrival"
    );
}

/// ⚠⚠ The server's chain source must resolve to OFF unless the populator is armed
/// too. Two advance gates on with the populator off would point a reader at a
/// store nothing fills, which makes `decide()` report `Empty` on a RUNNING
/// execution — the cliff this whole program exists to close.
///
/// ⚠ Asserted at the SOURCE. `chain_source_from_env` needs a live `DbPool`, so a
/// runtime assertion here would need a database — which is exactly what this suite
/// must not need. An assertion on env vars alone would pass whether or not the arm
/// consults the populator at all, which is the difference between testing the gate
/// and testing the environment.
#[test]
fn mode5_the_chain_store_arm_consults_the_populator_gate() {
    let src = include_str!("../src/db/queries/event_chain.rs");
    let non_test = src.split("#[cfg(test)]").next().expect("non-test region");
    let at = non_test
        .find(r#""chain" | "chain_store" =>"#)
        .expect("the chain-store arm is gone — re-anchor this guard, do not delete it");
    let end = non_test[at..]
        .find("_ => None,")
        .map(|i| at + i)
        .expect("end of the match not found — the extraction broke");
    let arm = &non_test[at..end];
    assert!(
        arm.len() > 200,
        "extracted {} bytes of the chain-store arm — implausibly small; a guard \
         measuring nothing passes",
        arm.len()
    );
    assert!(
        arm.contains("populator_enabled()"),
        "the chain-store arm does not consult populator_enabled(), so \
         NOETL_CHAIN_SOURCE=chain with the populator OFF would hand back a source \
         that reads an UNPOPULATED store.\n{arm}"
    );
}

// ===========================================================================
// The guard on the guard.
// ===========================================================================

/// ⚠⚠ This suite must never become DB-gated.
///
/// `chain_log_sourced.rs` returns early when `NOETL_TEST_PG_URL` is unset, which
/// is every CI run — so those tests are a deliberate-human proof, not a guard. If
/// someone "unifies" the two suites by adding the same early return here, the
/// permanent regression coverage silently becomes zero while still reporting `ok`.
#[test]
fn this_suite_does_not_skip_without_a_database() {
    let src = include_str!("chain_regression.rs");
    assert!(
        src.len() > 4000,
        "extracted {} bytes — implausibly small; a guard measuring nothing passes",
        src.len()
    );

    // ⚠⚠ Scan everything ABOVE this guard only. Its own assertions name the idioms
    // it forbids, so scanning the whole file makes the guard flag itself — which is
    // exactly what happened, and is the same self-reference the
    // `..._has_exactly_one_call_site` guard in `event_write.rs` records.
    let cut = src
        .find("fn this_suite_does_not_skip_without_a_database")
        .expect("this guard's own anchor is gone — re-anchor rather than delete");
    let above = &src[..cut];
    assert!(
        above.len() > 3000,
        "only {} bytes above the guard — the split is wrong and the scan would \
         measure almost nothing",
        above.len()
    );

    // Check for the skip IDIOM, not for a word count. Counting mentions of the
    // variable was the first version and it was brittle: adding a sentence of
    // prose broke it, which trains people to bump the number rather than read it.
    let code: String = above
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("///") && !t.starts_with("//!")
        })
        .collect::<Vec<_>>()
        .join("\n");

    for idiom in ["NOETL_TEST_PG_URL", "PgPoolOptions", "sqlx::query"] {
        assert!(
            !code.contains(idiom),
            "the permanent regression suite now contains {idiom:?} in CODE. It must \
             run WITHOUT a database: `chain_log_sourced.rs` already returns early \
             when no database is configured, which is every CI run, and a suite \
             that skips reports `ok` while covering nothing. Keep the e2e proof \
             there and the guard here."
        );
    }

    // And no early-return skip of any flavour in a #[test] body.
    let skips = code.matches("eprintln!(\"SKIP").count();
    assert_eq!(
        skips, 1,
        "expected exactly one deliberate skip (the flag-already-set case in \
         mode5); found {skips}. Every other test here must always run."
    );
}
