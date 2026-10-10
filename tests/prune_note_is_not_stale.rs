//! The operator-facing prune text must not contradict the code — [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459).
//!
//! ⚠⚠ This exists because the two were emitted together on prod, one line apart:
//!
//! ```text
//! WARN  PRUNE_ENABLED is set: parts below the proven floor WILL be deleted from the log
//!       note="pruning is NOT implemented in the pass yet ... nothing is deleted."
//! INFO  archive pass starting (archive-only; nothing is deleted)
//! ```
//!
//! The note and the startup message predated server#516, which wired the floor to
//! `apply_retention`. The code was right and the text was a release stale — in the one place
//! an operator looks while arming a destructive flag, and a reader would reasonably have
//! believed the note over the warning.
//!
//! It is exactly the representation drift this tier exists to prevent, produced here by the
//! same change that built the tier.

/// The note must not claim pruning is unimplemented while `prune_pass` exists.
#[test]
fn the_note_does_not_claim_pruning_is_unimplemented() {
    let note = noetl_server::services::event_archive::prune_readiness_note();
    for stale in ["NOT implemented", "not implemented", "nothing is deleted"] {
        assert!(
            !note.contains(stale),
            "prune_readiness_note() still contains {stale:?}:\n\n  {note}\n\n\
             `prune_pass` is wired and deletes parts below the floor, so this text tells an \
             operator the opposite of what the code does."
        );
    }
}

/// And it must say the thing that is actually true and operationally useful: arming the flag
/// reclaims nothing until the backlog drains.
#[test]
fn the_note_explains_why_arming_may_reclaim_nothing() {
    let note = noetl_server::services::event_archive::prune_readiness_note();
    assert!(
        note.contains("floor"),
        "the note must mention the floor — it is what decides whether anything is deleted"
    );
    assert!(
        note.to_lowercase().contains("refuse") || note.to_lowercase().contains("until"),
        "the note must say that arming the flag reclaims NOTHING until the archive backlog \
         is drained. Without it, an operator who arms prune and sees zero reclaimed bytes \
         reasonably concludes the feature is broken: {note}"
    );
    assert!(
        note.contains("noetl/ai-meta#459"),
        "the note must cite the issue so the reasoning is reachable"
    );
}

/// ⚠ The startup message must not be an unconditional "nothing is deleted".
#[test]
fn the_startup_message_distinguishes_armed_from_archive_only() {
    const SRC: &str = include_str!("../src/services/event_archive.rs");
    // Strip test blocks so a test string cannot satisfy this.
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
        "production slice implausible — the assertion below would be vacuous"
    );
    assert!(
        prod.contains("prune_armed = cfg.prune_enabled"),
        "⚠ the archive-pass startup log does not record whether prune is armed. It used to \
         say \"archive-only; nothing is deleted\" unconditionally, which is false on a pod \
         that is pruning."
    );
}
