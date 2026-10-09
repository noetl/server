//! The full async durability window — [noetl/ai-meta#460](https://github.com/noetl/ai-meta/issues/460).
//!
//! ⚠⚠ `ehdb-l0` has tracked this all along in `UnreplicatedTracker`, and
//! `L0Engine::unreplicated_snapshot()` had **zero callers in the server**. The number
//! existed, was correct, and nothing published it — the existence-vs-reachability failure
//! this repo keeps paying for.
//!
//! It matters because the gauge that *was* published measures less than the exposure:
//!
//! - `ehdb_oldest_unsealed_age_seconds` stops at **sealing**;
//! - replication is **asynchronous**, so a sealed part has a real window in which the only
//!   copy is local.
//!
//! So "sealed" is not "durable", and a gauge that stops at sealing reports the system safer
//! than it is — understating the exposure in the reassuring direction.

/// ⚠ These tests share a PROCESS-WIDE prometheus registry, and `init_age_seal_series()`
/// resets the very gauges they set. `cargo test` does not serialise tests, so without this
/// lock one test's `init` races another's `set` — and the first run of this file passed by
/// luck before the race was forced. Poisoning is tolerated: a panic in one test must not
/// cascade into unrelated failures.
static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialised() -> std::sync::MutexGuard<'static, ()> {
    match REGISTRY_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// The three series must be pinned, for the usual reason: absence is not zero, and a scrape
/// that cannot see them cannot tell a 7-hour window from a healthy one.
#[test]
fn the_durability_window_series_are_pinned() {
    let _g = serialised();
    noetl_server::metrics::init_age_seal_series();
    let text = noetl_server::metrics::gather_text().expect("render /metrics");
    for g in [
        "noetl_ehdb_unreplicated_oldest_age_seconds",
        "noetl_ehdb_unreplicated_records",
        "noetl_ehdb_unreplicated_shards",
    ] {
        assert!(
            text.lines().any(|l| l.starts_with(g)),
            "{g} is ABSENT — and an absent durability-window gauge reads exactly like a \
             healthy one"
        );
    }
}

/// ⚠ The unsealed gauge and the durability-window gauge are DIFFERENT measurements and must
/// not be collapsed into one. This test exists so a later cleanup does not "deduplicate"
/// them on the grounds that they look similar.
#[test]
fn the_unsealed_gauge_and_the_window_gauge_are_distinct_series() {
    let _g = serialised();
    noetl_server::metrics::init_age_seal_series();
    noetl_server::metrics::ehdb_oldest_unsealed_age_seconds().set(60);
    noetl_server::metrics::ehdb_unreplicated_oldest_age_seconds().set(3600);
    assert_eq!(noetl_server::metrics::ehdb_oldest_unsealed_age_seconds().get(), 60);
    assert_eq!(
        noetl_server::metrics::ehdb_unreplicated_oldest_age_seconds().get(),
        3600,
        "the window gauge must be independently settable: it is legitimately LARGER than the \
         unsealed one, because it also counts sealed parts whose upload has not completed"
    );
    let text = noetl_server::metrics::gather_text().unwrap();
    assert!(text.contains("noetl_ehdb_oldest_unsealed_age_seconds 60"));
    assert!(text.contains("noetl_ehdb_unreplicated_oldest_age_seconds 3600"));
}

/// The count and shard-count companions carry what a max-over-shards age cannot.
///
/// ⚠ One shard at 7h and eleven at 0 reads identically to all twelve at 7h if only the max
/// is published. The shard count is what separates "one stuck shard" from "the whole store".
#[test]
fn the_window_carries_its_breadth_not_only_its_depth() {
    let _g = serialised();
    noetl_server::metrics::init_age_seal_series();
    noetl_server::metrics::ehdb_unreplicated_records().set(4242);
    noetl_server::metrics::ehdb_unreplicated_shards().set(1);
    let text = noetl_server::metrics::gather_text().unwrap();
    assert!(text.contains("noetl_ehdb_unreplicated_records 4242"));
    assert!(
        text.contains("noetl_ehdb_unreplicated_shards 1"),
        "without the shard count, a single stuck shard is indistinguishable from every shard \
         being stuck"
    );
}

/// ⚠⚠⚠ The reachability guard.
///
/// `unreplicated_snapshot()` existing is not the point — it existed before this change and
/// was never called. This asserts the server actually reads it AND publishes it, in the
/// production source, with `#[cfg(test)]` blocks stripped so a test mentioning the call
/// cannot satisfy the check.
#[test]
fn the_tracker_is_actually_read_and_published() {
    const SRC: &str = include_str!("../src/handlers/ehdb_embedded.rs");

    // Strip test blocks by brace matching, then assert the slice is plausible BEFORE
    // asserting anything about it — a matcher that over-consumed would make this vacuous.
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
        prod.len() > SRC.len() / 2 && prod.contains("pub fn spawn_age_seal_task"),
        "production slice implausible ({} of {} bytes) — every assertion below would be \
         vacuous",
        prod.len(),
        SRC.len()
    );

    assert!(
        prod.contains("unreplicated_snapshot()"),
        "⚠⚠ the server does not call `unreplicated_snapshot()`. The tracker computes the \
         durability window correctly and nothing reads it — which is exactly the state this \
         change fixed, and it reports as a clean build from every other angle."
    );
    assert!(
        prod.contains("ehdb_unreplicated_oldest_age_seconds()"),
        "the snapshot is read but never published — a value computed and dropped is not a \
         measurement"
    );
    assert!(
        prod.contains("ehdb_unreplicated_shards()"),
        "the breadth of the window is not published"
    );
}
