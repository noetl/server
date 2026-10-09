//! noetl/ai-meta#460 B1 — a structural guard for where the put timing lives.
//!
//! The defect this exists to prevent already happened once. The latency histogram was
//! added to `ObjectBackend::put`, the wrapper. Then the retention/archival tier shipped
//! and its `ArchiveStore` impl called `GcsBackend::put` **directly** — so ~300 real
//! archival objects went to prod GCS while
//! `noetl_object_store_put_seconds_count{backend="gcs"}` sat at its seeded value of zero.
//!
//! An instrument on a caller measures that caller. An instrument on the primitive
//! measures everyone, because there is nowhere else to put the bytes. This test asserts
//! the timing is on the primitive and that no second copy has appeared on the wrapper's
//! GCS arm (which would double-count).

const BACKEND: &str = include_str!("../src/services/object_backend.rs");

/// The source with every `#[cfg(test)]` block removed, by brace matching.
///
/// Without this a doc-comment or a test asserting *about* the call would satisfy the
/// guard — the same "a comment counts as a caller" false negative that cleared two
/// other checks in this repo.
fn production_src(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(at) = rest.find("#[cfg(test)]") {
        out.push_str(&rest[..at]);
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
    out.push_str(rest);
    out
}

/// Assert the extraction before asserting about it: a slice that came back implausibly
/// small would make every check below pass against nearly nothing.
#[test]
fn extraction_is_plausible() {
    let prod = production_src(BACKEND);
    assert!(
        prod.len() > BACKEND.len() / 2,
        "production slice implausibly small: {} of {} bytes — the brace matcher likely \
         over-consumed, and every other assertion in this file would be vacuous",
        prod.len(),
        BACKEND.len()
    );
    assert!(
        prod.contains("impl GcsBackend"),
        "production slice lost the impl block it is supposed to be checking"
    );
}

#[test]
fn gcs_put_is_timed_in_the_primitive() {
    let prod = production_src(BACKEND);
    let marker = "crate::metrics::observe_object_store_put(";
    let gcs_at = prod
        .find("impl GcsBackend")
        .expect("GcsBackend impl block not found");
    let in_impl = &prod[gcs_at..];
    assert!(
        in_impl.contains(marker),
        "`GcsBackend` has no `{marker}` call.\n\nThe GCS put must be timed in the \
         primitive, not in a caller: the archive tier calls `GcsBackend::put` directly \
         through its `ArchiveStore` impl, so timing placed only on `ObjectBackend::put` \
         leaves the dominant writer unmeasured while the metric reads a healthy zero."
    );
}

#[test]
fn wrapper_does_not_double_count_gcs() {
    let prod = production_src(BACKEND);
    // The wrapper's GCS arm must delegate without timing — the primitive already timed it.
    let arm = "ObjectBackend::Gcs(g) => g.put(key, media_type, bytes).await";
    assert!(
        prod.contains(arm),
        "the wrapper's GCS arm is no longer a bare delegation to the primitive.\n\nIf \
         timing was added back around it, every GCS put is now counted twice and the \
         histogram's count no longer equals the number of puts."
    );
}

#[test]
fn postgres_arm_is_still_timed() {
    let prod = production_src(BACKEND);
    // Postgres has no primitive of its own here (it is a free function), so its timing
    // legitimately lives in the wrapper. Guard it so the move did not drop a backend.
    assert!(
        prod.contains("\"postgres\","),
        "the Postgres arm lost its `observe_object_store_put` label — moving the GCS \
         timing must not drop the other backend's measurement"
    );
}
