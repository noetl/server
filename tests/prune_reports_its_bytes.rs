//! The prune path must report the bytes it reclaimed — [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459).
//!
//! ⚠⚠ Written after the first production prune. It reclaimed **2.30 GiB** (df_used
//! 3,283,072 → 885,496 KiB, 306 parts, manifest 329 → 26) and
//! `noetl_ehdb_prune_bytes_reclaimed_total` still read **0**, because the counter was pinned
//! and never incremented. `pruned` counted the parts; nothing counted the bytes — the number
//! anyone actually wants, and the one that had to be measured externally with `df` and `du`.
//!
//! A metric that exists, is pinned, and is never fed is indistinguishable from a system that
//! reclaimed nothing. That is the failure class this tier was built to remove, reproduced by
//! the tier.

use std::collections::HashSet;

use chrono::{Duration, TimeZone, Utc};
use noetl_server::services::event_archive as ea;

/// ⚠ `noetl_ehdb_prune_bytes_reclaimed_total` is a PROCESS-GLOBAL counter, and two tests
/// here read it before/after. `cargo test` does not serialise tests, so without this lock
/// one test's increment lands inside the other's before/after window and the failure is
/// intermittent.
///
/// I first "fixed" this by running with `--test-threads=1`, which is not a fix: the suite
/// runner does not pass that flag, so the race would have shipped as a flaky test.
static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialised() -> std::sync::MutexGuard<'static, ()> {
    match COUNTER_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn t(h: i64) -> chrono::DateTime<chrono::Utc> {
    Utc.timestamp_opt(1_790_000_000, 0).unwrap() + Duration::hours(h)
}

fn cand(id: i64, status: &str, done_h: Option<i64>) -> ea::ArchiveCandidate {
    ea::ArchiveCandidate {
        execution_id: id,
        status: status.into(),
        completed_at: done_h.map(t),
        started_at: t(0),
    }
}

fn cfg(prune: bool) -> ea::RetentionConfig {
    ea::RetentionConfig {
        retention_hours: 48,
        archive_enabled: true,
        prune_enabled: prune,
        bucket: Some("b".into()),
        interval_secs: 300,
        max_per_pass: 100,
    }
}

/// ⭐ The counter must move by exactly the bytes the prune callback reported.
#[test]
fn the_reclaimed_bytes_counter_is_fed() {
    let _g = serialised();
    let before = noetl_server::metrics::ehdb_prune_bytes_reclaimed_total().get();

    // Everything archived, nothing constraining, so the floor computes and prune runs.
    let ids = [1i64, 2, 3];
    let cands: Vec<_> = ids.iter().map(|i| cand(*i, "COMPLETED", Some(-100))).collect();
    let set = ea::CandidateSet { candidates: cands, complete: true };
    let archived: HashSet<i64> = ids.into_iter().collect();

    let rep = ea::prune_pass(
        &cfg(true),
        &set,
        t(0),
        &archived,
        |_| {
            Some(vec![
                ea::HotExecution { execution_id: 1, min_sequence: 10, max_sequence: 20 },
                ea::HotExecution { execution_id: 2, min_sequence: 21, max_sequence: 30 },
                ea::HotExecution { execution_id: 3, min_sequence: 31, max_sequence: 40 },
            ])
        },
        // 4 parts, 7_777 bytes freed.
        |_floor| Some(Ok((4, 7_777))),
    );

    assert_eq!(rep.parts_dropped, 4, "{}", rep.describe());
    assert_eq!(rep.bytes_reclaimed, 7_777);

    let after = noetl_server::metrics::ehdb_prune_bytes_reclaimed_total().get();
    assert_eq!(
        after - before,
        7_777,
        "⚠⚠ noetl_ehdb_prune_bytes_reclaimed_total moved by {} instead of 7777. On prod this \
         counter read 0 after reclaiming 2.30 GiB — a pinned metric that is never fed looks \
         exactly like a system that reclaimed nothing.",
        after - before
    );
}

/// ⚠ A refused prune must NOT move the counter — otherwise the number stops meaning
/// "bytes actually freed".
#[test]
fn a_refused_prune_does_not_move_the_counter() {
    let _g = serialised();
    let before = noetl_server::metrics::ehdb_prune_bytes_reclaimed_total().get();

    // A backlog: 1 and 2 archived, 3 is not, so the floor refuses.
    let cands: Vec<_> = (1..=3).map(|i| cand(i, "COMPLETED", Some(-100))).collect();
    let set = ea::CandidateSet { candidates: cands, complete: true };
    let archived: HashSet<i64> = [1, 2].into_iter().collect();

    let rep = ea::prune_pass(
        &cfg(true),
        &set,
        t(0),
        &archived,
        |_| Some(vec![ea::HotExecution { execution_id: 3, min_sequence: 5, max_sequence: 9 }]),
        // If this is ever called the test below fails, which is the point.
        |_floor| Some(Ok((99, 999_999))),
    );

    assert_eq!(rep.parts_dropped, 0, "a backlog must block the prune: {}", rep.describe());
    assert_eq!(
        noetl_server::metrics::ehdb_prune_bytes_reclaimed_total().get(),
        before,
        "a REFUSED prune moved the reclaimed-bytes counter — the metric would then overstate \
         what was freed, which is worse than reading 0"
    );
}

/// ⚠⚠ The refusal reason must be observable in prod, not only at `debug`.
///
/// While prune sat refused for hours on prod, *why* was invisible: the `prune_refused_*`
/// counters said it refused, and `retention_floor_sequence` read 0 — but 0 is also what
/// "never ran" looks like, so neither answered the question. The pass's own report was being
/// written at a level prod does not collect.
#[test]
fn the_prune_verdict_is_logged_on_transition_not_only_at_debug() {
    const SRC: &str = include_str!("../src/services/event_archive.rs");
    let mut prod = String::with_capacity(SRC.len());
    let mut rest = SRC;
    while let Some(at) = rest.find("#[cfg(test)]") {
        prod.push_str(&rest[..at]);
        let after = &rest[at..];
        let Some(open) = after.find('{') else { break };
        let mut depth = 0usize;
        let mut end = None;
        for (i, c) in after[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(e) => rest = &after[e..],
            None => break,
        }
    }
    prod.push_str(rest);
    assert!(
        prod.len() > SRC.len() / 2 && prod.contains("pub fn spawn_archive_pass"),
        "production slice implausible — the assertions below would be vacuous"
    );
    assert!(
        prod.contains("prune pass verdict changed"),
        "⚠ the prune verdict is still debug-only. Prod does not emit debug, so a refusal's \
         REASON is invisible there — and `retention_floor_sequence == 0` cannot distinguish \
         \"refused\" from \"never ran\"."
    );
    assert!(
        prod.contains("last_prune_verdict"),
        "the verdict must be logged on TRANSITION. At INFO every interval this is ~288 \
         identical lines a day, which gets filtered out — the same invisibility by a \
         different route."
    );
    assert!(
        prod.contains("ehdb_prune_bytes_reclaimed_total().inc_by(bytes)"),
        "the reclaimed-bytes counter is still not fed from the prune path"
    );
}
