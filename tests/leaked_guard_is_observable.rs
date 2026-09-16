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
    let code = code_only(EVENTS_RS);

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
