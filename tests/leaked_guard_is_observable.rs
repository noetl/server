//! The leaked-guard signal must be WIRED, not merely computed — noetl/server#447.
//!
//! ⚠ `classify_in_flight` is a pure function whose tests pass whether or not
//! anything stamps the clock it reads. If the set sites stop recording
//! `orchestrate_in_flight_since`, every held guard reports an age of **0
//! seconds**, nothing ever crosses the stale threshold, and the gauge reads a
//! confident 0 while executions sit stranded forever.
//!
//! That failure is worse than no metric: a permanently-0 leak gauge is
//! affirmative evidence of health. This file asserts the clock is actually
//! stamped and cleared alongside the flag it shadows.

const EVENTS_RS: &str = include_str!("../src/handlers/events.rs");
const STATE_RS: &str = include_str!("../src/state.rs");

/// Source with comments stripped — this crate documents the hazard in prose, and
/// a matcher that reads prose as code reports the warning as the defect.
/// Production code only — every `#[cfg(test)]` module removed.
///
/// ⚠ Not cosmetic, and not as simple as truncating at the first `#[cfg(test)]`:
/// `events.rs` INTERLEAVES test modules with production code (the first is at
/// line ~2073, while the guard's set sites are at ~3155 and ~3742). Truncating
/// would hide the very code being checked. So each test module is removed by
/// brace matching.
///
/// The counts here are of set/clear sites, and test fixtures legitimately assign
/// `orchestrate_in_flight_since` to build scenarios — scanning them makes the
/// counts disagree and reports a healthy file as broken, which is exactly how
/// this check first failed.
fn production_code(src: &str) -> String {
    let code = code_only(src);
    let mut out = String::with_capacity(code.len());
    let mut rest = code.as_str();
    while let Some(i) = rest.find("#[cfg(test)]") {
        out.push_str(&rest[..i]);
        // Skip to the module's opening brace, then match to its close.
        let after = &rest[i..];
        let Some(open) = after.find('{') else { break };
        let mut depth = 0usize;
        let mut end = None;
        for (off, ch) in after[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + off + 1);
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

fn code_only(src: &str) -> String {
    src.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("///") && !t.starts_with('*')
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// ⭐ Every site that sets the guard must stamp the clock, and the site that
/// clears it must clear the clock.
#[test]
fn the_guard_clock_is_stamped_and_cleared_with_the_flag() {
    let code = production_code(EVENTS_RS);

    let sets = code.matches("orchestrate_in_flight = true").count();
    let stamps = code.matches("orchestrate_in_flight_since = Some(").count();
    assert!(sets > 0, "no set site found — has the guard been renamed?");
    assert_eq!(
        stamps, sets,
        "{sets} site(s) set the in-flight guard but only {stamps} stamp \
         `orchestrate_in_flight_since`. An unstamped guard reports an age of 0 \
         forever, so it never crosses the stale threshold and the leak gauge \
         reads a confident 0 while executions sit stranded (noetl/server#447)."
    );

    let clears = code.matches("orchestrate_in_flight = false").count();
    let unstamps = code.matches("orchestrate_in_flight_since = None").count();
    assert_eq!(
        unstamps, clears,
        "{clears} site(s) clear the guard but only {unstamps} clear the clock; a \
         stale timestamp left behind would make the NEXT drive look instantly old"
    );
}

/// The field must exist on the cached state, or nothing above compiles meaning.
#[test]
fn the_cached_state_carries_the_clock() {
    assert!(
        code_only(STATE_RS).contains("orchestrate_in_flight_since"),
        "ExecOrchState no longer carries the in-flight clock"
    );
}

/// ⭐ The sample must reach the gauges. A computed-but-unpublished number is the
/// noetl/ai-meta#343 failure mode: correct, tested, and connected to nothing.
#[test]
fn the_sample_is_published_to_the_gauges() {
    let code = code_only(EVENTS_RS);
    assert!(
        code.contains("record_orchestrate_in_flight("),
        "the reconcile poller never publishes the guard sample, so the gauges \
         stay absent from /metrics and the leak remains invisible"
    );
    assert!(
        code.contains("sample_in_flight_ages(&state)"),
        "nothing samples the guard ages"
    );
}

/// ⚠⚠ Observation must stay observation. This change deliberately does NOT
/// release a stale guard — that is noetl/server#447 options 1 and 2, which trade
/// a silent hang for possible duplicate work and are the owner's call.
#[test]
fn the_reporter_does_not_release_a_guard() {
    let code = code_only(EVENTS_RS);
    let from = code
        .find("async fn sample_in_flight_ages")
        .expect("the sampler");
    let body = &code[from..from + 1200.min(code.len() - from)];
    assert!(
        !body.contains("orchestrate_in_flight = false"),
        "the sampler clears a guard — that is a semantics change, not \
         observation, and it is explicitly held for the owner:\n{body}"
    );
}

/// Positive control: the matchers must be able to see what they look for, and
/// must not fire on prose describing it.
#[test]
fn the_matchers_can_actually_fail() {
    let planted = "fn f() {\n    cache.orchestrate_in_flight = true;\n}";
    assert_eq!(
        code_only(planted)
            .matches("orchestrate_in_flight = true")
            .count(),
        1
    );
    assert_eq!(
        code_only(planted)
            .matches("orchestrate_in_flight_since = Some(")
            .count(),
        0,
        "an unstamped set site must be visible to the matcher"
    );

    let discussed = "/// we set orchestrate_in_flight = true here\nfn f() {}";
    assert_eq!(
        code_only(discussed)
            .matches("orchestrate_in_flight = true")
            .count(),
        0,
        "the matcher counts comments as code"
    );
}

/// ⭐ noetl/server#447 — every skip site must consult the expiry, not the raw flag.
///
/// ⚠ A correct `in_flight_guard_holds` that no skip site calls changes nothing:
/// the execution still wedges, and the unit tests still pass. That is the
/// noetl/ai-meta#343 shape, and it is why this assertion is here rather than
/// only in the unit tests.
#[test]
fn every_skip_site_consults_the_expiry() {
    let code = production_code(EVENTS_RS);

    // The sites that decide "a drive is already in flight, skip this trigger".
    let skips = code
        .matches("record_orchestrate_drive(\"skipped_in_flight\")")
        .count();
    let expiries = code
        .matches("expire_in_flight_guard(&mut cache, execution_id)")
        .count();

    assert!(
        skips > 0,
        "no skip site found — has the guard been restructured?"
    );
    assert_eq!(
        expiries, skips,
        "{skips} site(s) skip on the in-flight guard but only {expiries} check \
         whether it has EXPIRED first. A skip site that reads the raw flag will \
         strand the execution forever exactly as before — the guard has one \
         clear path (on apply) and a drive that is never applied never clears it."
    );
}

/// The expiry must be loud. A guard that silently releases hides the fact that a
/// drive was lost, which is the signal someone needs to find the real cause.
#[test]
fn an_expiry_is_recorded_and_logged() {
    let code = code_only(EVENTS_RS);
    let from = code
        .find("fn expire_in_flight_guard")
        .expect("the expiry fn");
    let body = &code[from..];
    let body = &body[..body.find("\n}").map(|i| i + 2).unwrap_or(body.len())];

    assert!(
        body.contains("record_orchestrate_drive("),
        "expiry must increment a counter — a silent release trades a visible \
         hang for an invisible one:\n{body}"
    );
    assert!(
        body.contains("warn!"),
        "expiry must log at WARN naming the execution; it means a dispatched \
         drive was never applied, which is a real fault even though the \
         execution now recovers:\n{body}"
    );
    assert!(
        body.contains("orchestrate_in_flight = false")
            && body.contains("orchestrate_in_flight_since = None"),
        "expiry must clear BOTH the flag and the clock, or the next drive is \
         born stale:\n{body}"
    );
}
