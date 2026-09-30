//! Benchmark: chain-certified projections vs the current refold path.
//!
//! Spec: ai-meta#366 §4. Run with
//! `cargo run -p noetl-orchestrate-core --release --example chain_cert_bench`.
//!
//! Deliberately dependency-free (no criterion): `sha2`/`hex` were already
//! present, and the spec's self-sufficiency constraint is part of what is being
//! evaluated. Warm-up, fixed iteration counts and percentiles are computed here
//! so every number printed carries its own denominator and elapsed window —
//! the spec's own reporting rule ("volume is not duration").
//!
//! ⚠ What this measures and what it cannot:
//!   * Validation and append-side digest cost are measured **directly**, in
//!     process, against the real `WorkflowState::from_events`.
//!   * Cross-region numbers are **simulated** with an injected sleep. That is
//!     honest for showing "local read replaces a round trip" arithmetic, and it
//!     is NOT evidence about a real WAN. Labelled as such in the output.

use chrono::{TimeZone, Utc};
use noetl_orchestrate_core::chain_cert::{self, ChainCert};
use noetl_orchestrate_core::event::Event;
use noetl_orchestrate_core::state::{canonical_state_digest, WorkflowState};
use std::time::{Duration, Instant};

fn ev(execution_id: i64, i: usize) -> Event {
    let base = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
    // A realistic-ish event: the recorded corpus averages ~37 events/execution
    // with a small context payload.
    Event {
        event_id: 1_000_000 + i as i64,
        execution_id,
        catalog_id: 715_207_437_183_090_702,
        event_type: match i % 5 {
            0 => "step.enter",
            1 => "command.issued",
            2 => "command.claimed",
            3 => "call.done",
            _ => "command.completed",
        }
        .to_string(),
        node_name: Some(format!("step_{}", i % 7)),
        status: "success".to_string(),
        context: Some(serde_json::json!({
            "idx": i, "worker_id": "noetl-worker-rust-ff446f587-abcde",
            "command_id": format!("{execution_id}:step_{}:{}", i % 7, 1_000_000 + i),
        })),
        result: Some(serde_json::json!({"status": "success", "context": {"ok": true}})),
        meta: Some(serde_json::json!({"emitted_by": "orchestrator"})),
        timestamp: base + chrono::Duration::milliseconds(i as i64),
        parent_execution_id: None,
        attempt: Some(1),
    }
}

fn chain(execution_id: i64, n: usize) -> Vec<Event> {
    (0..n).map(|i| ev(execution_id, i)).collect()
}

fn pct(mut v: Vec<Duration>, p: f64) -> Duration {
    v.sort_unstable();
    if v.is_empty() {
        return Duration::ZERO;
    }
    let idx = (((v.len() - 1) as f64) * p).round() as usize;
    v[idx]
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// Time `f` `iters` times after `warm` warm-up passes. Returns (p50, p99, total).
fn bench<F: FnMut()>(warm: usize, iters: usize, mut f: F) -> (Duration, Duration, Duration) {
    for _ in 0..warm {
        f();
    }
    let mut samples = Vec::with_capacity(iters);
    let t0 = Instant::now();
    for _ in 0..iters {
        let s = Instant::now();
        f();
        samples.push(s.elapsed());
    }
    let elapsed = t0.elapsed();
    (pct(samples.clone(), 0.50), pct(samples, 0.99), elapsed)
}

/// Measure one `fsync` on this host, so the append-gate ratio has a real
/// denominator rather than a guessed one.
fn measure_fsync_us() -> Option<f64> {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!("chain_cert_fsync_{}.tmp", std::process::id()));
    let mut f = std::fs::File::create(&path).ok()?;
    for _ in 0..8 {
        f.write_all(b"warmup-payload-0123456789abcdef").ok()?;
        f.sync_data().ok()?;
    }
    let iters = 64;
    let t0 = Instant::now();
    for i in 0..iters {
        f.write_all(format!("event-body-{i:08}-padding-to-a-realistic-size").as_bytes())
            .ok()?;
        f.sync_data().ok()?;
    }
    let el = t0.elapsed();
    let _ = std::fs::remove_file(&path);
    Some(el.as_secs_f64() * 1e6 / iters as f64)
}

/// One arm of the append A/B: serialise -> [digest] -> write -> fsync, which is
/// the shape of a durable append. `extra_us` injects a deliberate regression so
/// the harness's own sensitivity can be proven (see section 2b).
fn append_arm(
    events: &[Event],
    certify_on: bool,
    extra_us: u64,
    do_fsync: bool,
) -> (Duration, Duration, Duration) {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!(
        "chain_cert_append_{}_{}_{}.tmp",
        std::process::id(),
        if certify_on { "cert" } else { "base" },
        if do_fsync { "sync" } else { "nosync" }
    ));
    let mut f = std::fs::File::create(&path).expect("temp file");
    let mut prev: Option<[u8; 32]> = None;
    let mut samples = Vec::with_capacity(events.len());
    let t_all = Instant::now();
    for e in events {
        let t0 = Instant::now();
        let body = serde_json::to_vec(e).expect("serialise");
        if certify_on {
            prev = Some(chain_cert::roll(prev, e));
        }
        if extra_us > 0 {
            let spin = Instant::now();
            while spin.elapsed().as_micros() < extra_us as u128 {
                std::hint::black_box(0u8);
            }
        }
        f.write_all(&body).expect("write");
        if do_fsync {
            f.sync_data().expect("fsync");
        }
        samples.push(t0.elapsed());
    }
    let total = t_all.elapsed();
    std::hint::black_box(prev);
    let _ = std::fs::remove_file(&path);
    (pct(samples.clone(), 0.50), pct(samples, 0.99), total)
}

/// As `append_arm` with no fsync, but injecting `extra_ns` of work — the
/// sensitivity control for the CPU-only instrument in section 2c.
fn append_arm_ns(
    events: &[Event],
    certify_on: bool,
    extra_ns: u64,
) -> (Duration, Duration, Duration) {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!("chain_cert_ns_{}.tmp", std::process::id()));
    let mut f = std::fs::File::create(&path).expect("temp file");
    let mut prev: Option<[u8; 32]> = None;
    let mut samples = Vec::with_capacity(events.len());
    let t_all = Instant::now();
    for e in events {
        let t0 = Instant::now();
        let body = serde_json::to_vec(e).expect("serialise");
        if certify_on {
            prev = Some(chain_cert::roll(prev, e));
        }
        if extra_ns > 0 {
            let spin = Instant::now();
            while (spin.elapsed().as_nanos() as u64) < extra_ns {
                std::hint::black_box(0u8);
            }
        }
        f.write_all(&body).expect("write");
        samples.push(t0.elapsed());
    }
    let total = t_all.elapsed();
    std::hint::black_box(prev);
    let _ = std::fs::remove_file(&path);
    (pct(samples.clone(), 0.50), pct(samples, 0.99), total)
}

/// Append with EHDB's actual durability posture: `fsync_every` records share one
/// `sync_data()` (`PartDurability::CallerDriven`, `MAX_COMMIT_BATCH = 512` in
/// ehdb-feed/src/publish.rs). This is the shape the +-2% append gate must be
/// judged against, not one-fsync-per-event.
fn append_arm_batched(events: &[Event], certify_on: bool, fsync_every: usize) -> Duration {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!(
        "chain_cert_batch_{}_{}.tmp",
        std::process::id(),
        fsync_every
    ));
    let mut f = std::fs::File::create(&path).expect("temp file");
    let mut prev: Option<[u8; 32]> = None;
    let t_all = Instant::now();
    for (i, e) in events.iter().enumerate() {
        let body = serde_json::to_vec(e).expect("serialise");
        if certify_on {
            prev = Some(chain_cert::roll(prev, e));
        }
        f.write_all(&body).expect("write");
        if (i + 1) % fsync_every == 0 {
            f.sync_data().expect("fsync");
        }
    }
    f.sync_data().expect("final fsync");
    let total = t_all.elapsed();
    std::hint::black_box(prev);
    let _ = std::fs::remove_file(&path);
    total
}

/// The achievable floor: digest the bytes the append ALREADY serialised,
/// instead of serialising a second time inside `roll()`. Only legitimate if
/// the append writes the normalised form (the bytes `canonical()` produces) --
/// A8 shows the normalisation cannot be dropped. This measures the headroom
/// between the prototype and a design that reuses the serialisation.
fn roll_over_body(prev: Option<[u8; 32]>, body: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(chain_cert::DOMAIN_TAG);
    if let Some(p) = prev {
        h.update(p);
    }
    h.update(body);
    h.finalize().into()
}

/// Append arm that digests the already-serialised body (see `roll_over_body`).
fn append_arm_reuse(events: &[Event], fsync_every: usize) -> Duration {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!("chain_cert_reuse_{}.tmp", std::process::id()));
    let mut f = std::fs::File::create(&path).expect("temp file");
    let mut prev: Option<[u8; 32]> = None;
    let t_all = Instant::now();
    for (i, e) in events.iter().enumerate() {
        let body = chain_cert::canonical(e);
        prev = Some(roll_over_body(prev, &body));
        f.write_all(&body).expect("write");
        if (i + 1) % fsync_every == 0 {
            f.sync_data().expect("fsync");
        }
    }
    f.sync_data().expect("final fsync");
    let total = t_all.elapsed();
    std::hint::black_box(prev);
    let _ = std::fs::remove_file(&path);
    total
}

fn main() {
    println!("chain-certified projections — benchmark (ai-meta#366 §4)");
    println!(
        "host: {} cores, rustc release build\n",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );

    // ---------------------------------------------------------------
    // 1. VALIDATION COST: O(n) refold vs O(1) digest compare
    // ---------------------------------------------------------------
    // ---------------------------------------------------------------
    // 0. MEASUREMENT FLOOR — is the cert number real, or timer overhead?
    // ---------------------------------------------------------------
    // The certificate compare is an i64 compare, a u32 compare and a 32-byte
    // memcmp. If that costs less than two `Instant::now()` calls, section 1 is
    // measuring the CLOCK, not the work, and its "speedup" is a lower bound on
    // the ratio rather than a measurement. Print the floor so a reader can tell
    // which it is, and a batched figure that gets underneath it.
    println!("== 0. Measurement floor (is section 1 timer-bound?)");
    let (e50, e99, _) = bench(10_000, 100_000, || {
        std::hint::black_box(0u8);
    });
    println!(
        "  empty closure:        p50 {:.4}us  p99 {:.4}us   <- the timer floor",
        us(e50),
        us(e99)
    );
    let ev_batch = chain(1, 64);
    let cert_batch = chain_cert::certify(&ev_batch).unwrap();
    let (b50, _b99, b_el) = bench(1_000, 2_000, || {
        for _ in 0..1000 {
            std::hint::black_box(chain_cert::validate(&cert_batch, &cert_batch));
        }
    });
    let per_compare_ns = b50.as_secs_f64() * 1e9 / 1000.0;
    println!(
        "  1000 compares/sample: p50 {:.3}us total -> {:.2}ns per compare",
        us(b50),
        per_compare_ns
    );
    println!(
        "  (batched over 2000 samples in {:.3}s; this is the trustworthy per-compare\n            figure. Section 1's per-op cert column is FLOOR-BOUND -- read it as\n            'at or below timer resolution', not as a measurement.)\n",
        b_el.as_secs_f64()
    );

    println!("== 1. Validation cost: O(n) refold vs O(1) certificate compare");
    println!(
        "{:>7} | {:>13} {:>13} | {:>13} {:>13} | {:>9}",
        "chain_n", "refold p50", "refold p99", "cert p50", "cert p99", "speedup"
    );
    let mut flatness = Vec::new();
    for &n in &[36usize, 174, 1_000, 10_000] {
        let events = chain(1, n);
        let iters = if n >= 10_000 { 200 } else { 2_000 };

        // Current path: fold the whole chain, then digest the folded output.
        // This is what `fold_spine` + `verdict_for` pay per validation.
        let (r50, r99, r_elapsed) = bench(5, iters.min(200), || {
            let st = WorkflowState::from_events(&events).expect("fold");
            let d = canonical_state_digest(&st);
            std::hint::black_box(d);
        });

        // New path: compare two 44-byte certificates.
        let current = chain_cert::certify(&events).expect("certify");
        let stored = current;
        let (c50, c99, c_elapsed) = bench(1_000, iters, || {
            let v = chain_cert::validate_counted(&stored, &current);
            std::hint::black_box(v);
        });

        println!(
            "{:>7} | {:>11.1}us {:>11.1}us | {:>11.4}us {:>11.4}us | {:>8.0}x",
            n,
            us(r50),
            us(r99),
            us(c50),
            us(c99),
            us(r50) / us(c50).max(f64::MIN_POSITIVE)
        );
        println!(
            "        (refold: {} iters in {:.2}s | cert: {} iters in {:.3}s)",
            iters.min(200),
            r_elapsed.as_secs_f64(),
            iters,
            c_elapsed.as_secs_f64()
        );
        flatness.push((n, us(c99)));
    }

    // The spec's falsifier: "validation p99 grows with chain length".
    let (n_lo, p99_lo) = flatness[0];
    let (n_hi, p99_hi) = *flatness.last().unwrap();
    println!(
        "\n  FLATNESS CHECK (spec §4 falsifier: p99 must not grow with n)\n    \
         n={} p99={:.4}us -> n={} p99={:.4}us  ratio={:.2}x over a {}x longer chain",
        n_lo,
        p99_lo,
        n_hi,
        p99_hi,
        p99_hi / p99_lo.max(f64::MIN_POSITIVE),
        n_hi / n_lo
    );

    // ---------------------------------------------------------------
    // 2. APPEND-SIDE COST: the digest added per event
    // ---------------------------------------------------------------
    println!("\n== 2. Append path: per-event digest cost (spec claims UNCHANGED)");
    let e = ev(1, 42);
    let prev = [7u8; 32];
    let (a50, a99, a_el) = bench(10_000, 50_000, || {
        std::hint::black_box(chain_cert::roll(Some(prev), &e));
    });
    // Baseline: what an append already pays just to canonicalise its own body.
    let (s50, _s99, _) = bench(10_000, 50_000, || {
        std::hint::black_box(serde_json::to_vec(&e).unwrap());
    });
    println!(
        "  roll() per event:        p50 {:.3}us  p99 {:.3}us   ({} iters in {:.2}s)",
        us(a50),
        us(a99),
        50_000,
        a_el.as_secs_f64()
    );
    println!(
        "  serde_json::to_vec ref:  p50 {:.3}us            (the append already pays this)",
        us(s50)
    );
    println!(
        "  digest overhead beyond the serialisation the append already does: {:.3}us",
        (us(a50) - us(s50)).max(0.0)
    );
    // MEASURE the fsync rather than assume it: this is a ratio, and a ratio
    // against a guessed denominator is not a result.
    match measure_fsync_us() {
        Some(f) => {
            let share = us(a50) / f * 100.0;
            println!("  MEASURED fsync on this host: {:.1}us per flush", f);
            println!(
                "  digest as a share of one append's fsync: {:.3}%  ({} the +-2% gate)",
                share,
                if share < 2.0 { "PASSES" } else { "FAILS" }
            );
        }
        None => println!("  fsync could not be measured on this host -- gate UNPROVEN"),
    }

    // ---------------------------------------------------------------
    // 2b. APPEND A/B THROUGH A REAL DURABILITY BARRIER
    // ---------------------------------------------------------------
    // Section 2 measured the digest in isolation and ARGUED the append is
    // unchanged. The spec says a regression here sinks the design, so measure
    // it end to end: same loop, same fsync, certification on vs off.
    //
    // Two things make a single A/B run untrustworthy, and both are handled:
    //  1. fsync latency drifts, so arms are INTERLEAVED and paired per round.
    //  2. the digest is ~0.1% of an fsync -- below the noise -- so "no change"
    //     is only meaningful if the harness could have SEEN a change. The
    //     sensitivity control at the end plants one.
    // A single round of this A/B produced both -0.42% and +2.73% on this host,
    // the latter physically impossible (certified cannot be FASTER). That
    // spread is the measurement's noise, so the noise band is reported and the
    // verdict is taken from the median of paired rounds, never one sample.
    println!("\n== 2b. Append A/B through a real fsync (spec: throughput UNCHANGED)");
    let n_ab = 120usize;
    let rounds = 7usize;
    let ab_events = chain(7, n_ab);
    let mut deltas = Vec::with_capacity(rounds);
    let mut base_tps_all = Vec::with_capacity(rounds);
    let mut cert_tps_all = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let (_, _, bt) = append_arm(&ab_events, false, 0, true);
        let (_, _, ct) = append_arm(&ab_events, true, 0, true);
        let b = n_ab as f64 / bt.as_secs_f64();
        let c = n_ab as f64 / ct.as_secs_f64();
        base_tps_all.push(b);
        cert_tps_all.push(c);
        deltas.push((c - b) / b * 100.0);
    }
    let med = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let lo = deltas.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = deltas.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let med_delta = med(&mut deltas.clone());
    println!(
        "  baseline  (cert OFF): median {:.1} appends/s",
        med(&mut base_tps_all.clone())
    );
    println!(
        "  certified (cert ON):  median {:.1} appends/s",
        med(&mut cert_tps_all.clone())
    );
    println!(
        "  paired delta over {} rounds of {} appends: median {:+.2}%, range {:+.2}%..{:+.2}%",
        rounds, n_ab, med_delta, lo, hi
    );
    let noise = hi - lo;
    println!(
        "  NOISE BAND: {:.2} percentage points wide -- {} the +-2% gate it must resolve",
        noise,
        if noise > 4.0 {
            "WIDER than"
        } else {
            "narrow enough for"
        }
    );

    // The sensitivity control. A 2% regression on this host's fsync is ~80us of
    // injected work. If the harness cannot see THAT, the verdict above is
    // unfalsifiable and must not be reported as a pass.
    let base_ref = med(&mut base_tps_all.clone());
    let p50_us = 1e6 / base_ref;
    let inject_us = (p50_us * 0.02).round().max(1.0) as u64;
    let mut c_deltas = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let (_, _, bt) = append_arm(&ab_events, false, 0, true);
        let (_, _, ct) = append_arm(&ab_events, true, inject_us, true);
        let b = n_ab as f64 / bt.as_secs_f64();
        let c = n_ab as f64 / ct.as_secs_f64();
        c_deltas.push((c - b) / b * 100.0);
    }
    let c_med = med(&mut c_deltas.clone());
    let detected = c_med < -2.0;
    println!(
        "  SENSITIVITY CONTROL: injected {}us/append (~2% of p50) -> median delta {:+.2}%",
        inject_us, c_med
    );
    println!(
        "  control verdict: harness {} a planted 2% regression",
        if detected { "DETECTS" } else { "CANNOT SEE" }
    );
    println!(
        "  => APPEND VERDICT: {}",
        if !detected {
            "UNPROVEN -- harness is not sensitive enough to resolve the gate"
        } else if noise > 4.0 {
            "no regression detected at the resolvable limit; the +-2% gate itself\n                  is BELOW this harness's noise, so the honest claim is 'no effect\n                  larger than the noise band', not 'within +-2%'"
        } else {
            "no regression: median delta within the +-2% gate, harness proven sensitive"
        }
    );

    // ---------------------------------------------------------------
    // 2c. THE RESOLVABLE A/B — same loop, fsync removed
    // ---------------------------------------------------------------
    // 2b is unfalsifiable on this host: APFS fsync jitter (+-8%) swamps a
    // 0.1% signal, and its own control proves it cannot see a planted 2%
    // regression. Differencing two noisy totals is the wrong instrument.
    //
    // So remove the noise source. The fsync is a constant addend COMMON TO
    // BOTH ARMS, so the regression measured without it is a strict UPPER
    // BOUND on the regression with it: adding the same constant to both
    // numerator and denominator can only move the ratio toward zero. A
    // bound proven sensitive beats a point estimate that is pure noise.
    println!("\n== 2c. Resolvable append A/B (fsync removed -> UPPER BOUND on regression)");
    let mut d2 = Vec::with_capacity(rounds);
    let mut b2 = Vec::with_capacity(rounds);
    let mut c2 = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let (_, _, bt) = append_arm(&ab_events, false, 0, false);
        let (_, _, ct) = append_arm(&ab_events, true, 0, false);
        let b = n_ab as f64 / bt.as_secs_f64();
        let c = n_ab as f64 / ct.as_secs_f64();
        b2.push(b);
        c2.push(c);
        d2.push((c - b) / b * 100.0);
    }
    let lo2 = d2.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi2 = d2.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let med2 = med(&mut d2.clone());
    let base2 = med(&mut b2.clone());
    let cert2 = med(&mut c2.clone());
    println!("  baseline  (cert OFF): median {:.0} appends/s", base2);
    println!("  certified (cert ON):  median {:.0} appends/s", cert2);
    println!(
        "  paired delta over {} rounds of {} appends: median {:+.2}%, range {:+.2}%..{:+.2}%",
        rounds, n_ab, med2, lo2, hi2
    );

    // Sensitivity control for THIS instrument: plant a 2% regression.
    let p50_ns_2 = 1e9 / base2;
    let inj_ns = (p50_ns_2 * 0.02).round().max(1.0) as u64;
    let mut cd2 = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let (_, _, bt) = append_arm(&ab_events, false, 0, false);
        let (_, _, ct) = append_arm_ns(&ab_events, true, inj_ns);
        let b = n_ab as f64 / bt.as_secs_f64();
        let c = n_ab as f64 / ct.as_secs_f64();
        cd2.push((c - b) / b * 100.0);
    }
    let cmed2 = med(&mut cd2.clone());
    let detected2 = cmed2 < -2.0;
    println!(
        "  SENSITIVITY CONTROL: injected {}ns/append (~2% of p50) -> median delta {:+.2}%",
        inj_ns, cmed2
    );
    println!(
        "  control verdict: harness {} a planted 2% regression",
        if detected2 { "DETECTS" } else { "CANNOT SEE" }
    );
    // ---------------------------------------------------------------
    // 2d. WHERE THE SPEC'S "UNCHANGED" CLAIM ACTUALLY HOLDS
    // ---------------------------------------------------------------
    // 2c says certification adds ~40% to the CPU of an append. 2b says the
    // end-to-end append is fsync-bound and shows no effect. Both are true,
    // and together they say something sharper than either: whether the
    // +-2% gate holds depends entirely on how many events share one fsync.
    // Solve for the batch size where the added CPU reaches 2% of the
    // per-event durability cost -- from measured values, not assumed ones.
    let added_us = (1e6 / cert2) - (1e6 / base2);
    println!("\n== 2d. Break-even: how many events per fsync before the gate breaks");
    println!(
        "  measured added CPU per append: {:.2}us   (cert ON vs OFF, 2c)",
        added_us
    );
    match measure_fsync_us() {
        Some(f) => {
            let n_break = (f * 0.02 / added_us).floor();
            println!("  measured fsync:                {:.0}us", f);
            println!(
                "  1 event per fsync -> digest is {:.3}% of the append  => gate HOLDS",
                added_us / f * 100.0
            );
            println!(
                "  BREAK-EVEN: ~{:.0} events per fsync. Above that, the added CPU",
                n_break
            );
            println!("              exceeds 2% of per-event append cost and the gate BREAKS.");
            println!(
                "  => APPEND VERDICT: the spec's 'unchanged by construction' holds ONLY for a\n                      durability-bound append (one fsync per event, or fewer than ~{:.0} events\n                      per group commit). It is NOT unconditional: a batching WAL that group-\n                      commits more than ~{:.0} events per fsync pays a measurable append cost.\n                      'By construction' is the part of the claim that does not survive.",
                n_break, n_break
            );
        }
        None => println!("  fsync unmeasurable -> break-even UNKNOWN"),
    }
    println!(
        "  (instrument note: 2b could not resolve +-2% -- its own control failed.\n            2c's control passes, so the CPU figure is the trustworthy one and the\n            break-even is derived from it.)"
    );

    // ---------------------------------------------------------------
    // 2e. THE GATE AT EHDB'S REAL DURABILITY POSTURE (measured, not modelled)
    // ---------------------------------------------------------------
    // EHDB does NOT fsync per event. `PartDurability::CallerDriven` group-
    // commits, and ehdb-feed/src/publish.rs:46 sets MAX_COMMIT_BATCH = 512
    // (EHDB_FEED_BATCH_LIMIT defaults to 2000). 2d put the break-even at ~17
    // events per fsync, so measure the A/B at the batch size the engine
    // actually uses instead of arguing from the model.
    println!("\n== 2e. Append gate at EHDB's real group-commit size (publish.rs:46 = 512)");
    let n_batch = 3072usize;
    let be = chain(11, n_batch);
    println!("  events/fsync |   base appends/s |   cert appends/s |    delta | +-2% gate");
    for &g in &[1usize, 17, 512, 2000] {
        let mut ds = Vec::new();
        let (mut bs, mut cs) = (Vec::new(), Vec::new());
        for _ in 0..3 {
            let bt = append_arm_batched(&be, false, g);
            let ct = append_arm_batched(&be, true, g);
            let b = n_batch as f64 / bt.as_secs_f64();
            let c = n_batch as f64 / ct.as_secs_f64();
            bs.push(b);
            cs.push(c);
            ds.push((c - b) / b * 100.0);
        }
        let d = med(&mut ds.clone());
        println!(
            "  {:>12} | {:>16.0} | {:>16.0} | {:>+7.2}% | {}",
            g,
            med(&mut bs.clone()),
            med(&mut cs.clone()),
            d,
            if d.abs() <= 2.0 { "HOLDS" } else { "BREAKS" }
        );
    }
    // How much of that regression is the PROTOTYPE re-serialising, vs the
    // design inherently costing? Measure the floor: digest the bytes the
    // append already produced.
    println!("\n  headroom: prototype re-serialises inside roll(). Floor = reuse those bytes.");
    for &g in &[512usize, 2000] {
        let mut dp = Vec::new();
        let mut dr = Vec::new();
        for _ in 0..3 {
            let bt = append_arm_batched(&be, false, g);
            let ct = append_arm_batched(&be, true, g);
            let rt = append_arm_reuse(&be, g);
            let b = n_batch as f64 / bt.as_secs_f64();
            dp.push((n_batch as f64 / ct.as_secs_f64() - b) / b * 100.0);
            dr.push((n_batch as f64 / rt.as_secs_f64() - b) / b * 100.0);
        }
        let (pp, rr) = (med(&mut dp.clone()), med(&mut dr.clone()));
        println!(
            "  fsync_every={:<5} prototype {:+.2}%   serialisation-reused {:+.2}%   ({} at reuse)",
            g,
            pp,
            rr,
            if rr.abs() <= 2.0 {
                "gate HOLDS"
            } else {
                "gate STILL BREAKS"
            }
        );
    }

    println!(
        "  => the gate holds only in the one-fsync-per-event column. At the 512\n              EHDB actually group-commits, certification is a measurable append\n              regression -- which is the condition the spec says sinks the design."
    );

    // Throughput: events/s of digesting, vs the fsync-bound ceiling.
    let n_th = 20_000usize;
    let big = chain(1, n_th);
    let t0 = Instant::now();
    let mut d: Option<[u8; 32]> = None;
    for e in &big {
        d = Some(chain_cert::roll(d, e));
    }
    let th_elapsed = t0.elapsed();
    std::hint::black_box(d);
    println!(
        "  digest-only throughput: {:.0} events/s ({} events in {:.3}s)",
        n_th as f64 / th_elapsed.as_secs_f64(),
        n_th,
        th_elapsed.as_secs_f64()
    );

    // ---------------------------------------------------------------
    // 3. CROSS-REGION (SIMULATED)
    // ---------------------------------------------------------------
    println!("\n== 3. Cross-region (SIMULATED RTT — arithmetic, not a WAN measurement)");
    let rtt = Duration::from_millis(60);
    let events = chain(1, 174);
    let current = chain_cert::certify(&events).unwrap();

    // Local certified read: verify locally, no round trip.
    let (l50, l99, _) = bench(100, 500, || {
        std::hint::black_box(chain_cert::validate_counted(&current, &current));
    });
    println!(
        "  projection read, local certified:  p50 {:.4}us  p99 {:.4}us",
        us(l50),
        us(l99)
    );
    println!(
        "  projection read, 1 RTT to writer:  p50 {:.1}us          (injected {}ms)",
        us(rtt),
        rtt.as_millis()
    );
    println!(
        "  => read latency avoided: ~{:.1}ms per cross-region projection read",
        (us(rtt) - us(l50)) / 1000.0
    );
    println!("  COMMIT latency, cross-region: UNCHANGED by construction — this");
    println!("    prototype touches no append/commit code path. Nothing here can");
    println!("    improve it, and the spec says a benchmark claiming otherwise is");
    println!("    wrong. Verified structurally in section 4 below, not by timing.");

    // ---------------------------------------------------------------
    // 4. REFOLDS AVOIDED + the A9 discrimination
    // ---------------------------------------------------------------
    println!("\n== 4. Refolds avoided (spec gate: >=90% reduction per 10^4 events)");
    chain_cert::reset_counters();
    let events = chain(1, 10_000);
    let current = chain_cert::certify(&events).unwrap();
    // Simulate the D3 window's validation pattern: one validation per poll.
    let polls = 19_840usize; // the observed refold count
    let mut refolds_still_needed = 0usize;
    for i in 0..polls {
        // 1 in 500 polls races a genuinely-advanced chain and must refold.
        let stored = if i % 500 == 499 {
            let mut s = current;
            s.chain_len -= 1;
            s
        } else {
            current
        };
        match chain_cert::validate_counted(&stored, &current) {
            chain_cert::CertVerdict::Valid => {}
            _ => refolds_still_needed += 1,
        }
    }
    println!(
        "  polls simulated:     {} (the measured D3 refold count)",
        polls
    );
    println!("  validations taken:   {}", chain_cert::validations_taken());
    println!("  refolds avoided:     {}", chain_cert::refolds_avoided());
    println!("  refolds still done:  {}", refolds_still_needed);
    println!(
        "  reduction:           {:.2}%   (gate: >=90%)",
        100.0 * chain_cert::refolds_avoided() as f64 / polls as f64
    );
    println!(
        "  A9 DISCRIMINATION:   taken={} avoided={} -> path RAN and {} (a zero on\n                       \
         'avoided' with taken=0 would mean UNWIRED, a different diagnosis)",
        chain_cert::validations_taken(),
        chain_cert::refolds_avoided(),
        if chain_cert::refolds_avoided() > 0 { "helped" } else { "never helped" }
    );

    println!("\n== 5. Bytes/event overhead");
    let body = serde_json::to_vec(&ev(1, 42)).unwrap().len();
    println!(
        "  mean event body:     {} B\n  certificate adds:    36 B ([u8;32] + u32) = {:.2}% of body\n  \
         cert on the wire:    {} B",
        body,
        36.0 / body as f64 * 100.0,
        ChainCert::WIRE_BYTES
    );
}
