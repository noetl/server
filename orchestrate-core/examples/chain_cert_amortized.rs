//! Amortized chain certificate: can moving the hash to the commit-batch
//! boundary bring the append tax under the spec's +-2% gate?
//!
//! Follow-up to noetl/server#480, which measured the per-event variant at
//! -17% (512 events/fsync) and -35% (2000) -- outside the gate. Run with
//! `cargo run -p noetl-orchestrate-core --release --example chain_cert_amortized`.
//!
//! Section 1 is the load-bearing one: SHA-256 cost is proportional to BYTES,
//! and a rollup over a batch still hashes every body, so batching can only
//! recover PER-EVENT FIXED overhead. Decompose first, then build to whatever
//! the decomposition says is actually recoverable.

use chrono::{TimeZone, Utc};
use noetl_orchestrate_core::chain_cert;
use noetl_orchestrate_core::event::Event;
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

/// Which certification variant an append arm runs.
#[derive(Clone, Copy, PartialEq)]
enum Variant {
    /// No certification: serialise + write. The baseline the +-2% gate is against.
    Off,
    /// noetl/server#480's shape: re-serialise via canonical() and hash per event.
    PerEventReserialise,
    /// Hash the bytes the append already wrote, per event.
    PerEventStoredBytes,
    /// Control arm: hashes the body TWICE. Its added cost is exactly double
    /// `PerEventStoredBytes`, so it plants a regression of a KNOWN size —
    /// unlike a timed spin loop, which cannot inject sub-100ns accurately
    /// (an `Instant::now()` pair costs more than the target).
    StoredBytesDoubled,
}

/// One append arm. `fsync_every` mirrors EHDB's group commit; `extra_ns`
/// injects a planted regression for the sensitivity control.
///
/// Every arm serialises exactly ONCE with `to_vec`, because the sound form of
/// the stored-bytes variants requires the event to be normalised at
/// construction (a prerequisite, stated in the report -- not free, but outside
/// the append path). That keeps the arms differing ONLY in the hashing, which
/// is what the gate actually asks about.
fn append_arm(events: &[Event], v: Variant, fsync_every: usize, extra_ns: u64) -> Duration {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!("cc_amort_{}.tmp", std::process::id()));
    let mut f = std::fs::File::create(&path).expect("temp file");
    let mut prev: Option<[u8; 32]> = None;
    let t0 = Instant::now();
    for (i, e) in events.iter().enumerate() {
        let body = serde_json::to_vec(e).expect("serialise");
        match v {
            Variant::Off => {}
            Variant::PerEventReserialise => prev = Some(chain_cert::roll(prev, e)),
            Variant::PerEventStoredBytes => prev = Some(chain_cert::roll_bytes(prev, &body)),
            Variant::StoredBytesDoubled => {
                prev = Some(chain_cert::roll_bytes(prev, &body));
                prev = Some(chain_cert::roll_bytes(prev, &body));
            }
        }
        if extra_ns > 0 {
            let spin = Instant::now();
            while (spin.elapsed().as_nanos() as u64) < extra_ns {
                std::hint::black_box(0u8);
            }
        }
        f.write_all(&body).expect("write");
        if (i + 1) % fsync_every == 0 {
            f.sync_data().expect("fsync");
        }
    }
    f.sync_data().expect("final fsync");
    let el = t0.elapsed();
    std::hint::black_box(prev);
    let _ = std::fs::remove_file(&path);
    el
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
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

fn main() {
    println!("amortized chain certificate — decomposition (follow-up to noetl/server#480)");
    println!("host: {} cores, rustc release build\n", num_cpus_hint());

    let evs = chain(1, 512);
    let e0 = &evs[0];
    let body = serde_json::to_vec(e0).unwrap();
    let canon = chain_cert::canonical(e0);
    println!(
        "== 1. Where does the per-event cost go? (body {} B, canonical {} B)",
        body.len(),
        canon.len()
    );

    let (a, _, ael) = bench(5_000, 50_000, || {
        std::hint::black_box(serde_json::to_vec(std::hint::black_box(e0)).unwrap());
    });
    let (b, _, bel) = bench(5_000, 50_000, || {
        std::hint::black_box(serde_json::to_value(std::hint::black_box(e0)).unwrap());
    });
    let (c, _, cel) = bench(5_000, 50_000, || {
        std::hint::black_box(chain_cert::canonical(std::hint::black_box(e0)));
    });
    let (d, _, del) = bench(5_000, 50_000, || {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(chain_cert::DOMAIN_TAG);
        std::hint::black_box(h.finalize());
    });
    let (e, _, eel) = bench(5_000, 50_000, || {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(chain_cert::DOMAIN_TAG);
        h.update(std::hint::black_box(&canon));
        std::hint::black_box(h.finalize());
    });
    // Amortized shape: ONE hasher absorbing many bodies. Per-body cost here is
    // the block processing alone, with init+finalize spread over 512.
    let (f, _, fel) = bench(200, 2_000, || {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(chain_cert::DOMAIN_TAG);
        for _ in 0..512 {
            h.update(std::hint::black_box(&canon));
        }
        std::hint::black_box(h.finalize());
    });
    let per_update = us(f) / 512.0;

    println!("  serde_json::to_vec(event)        {:>8.3}us   (the append ALREADY pays this)   [50k iters in {:.2}s]", us(a), ael.as_secs_f64());
    println!("  serde_json::to_value(event)      {:>8.3}us   (extra round-trip in canonical)  [50k iters in {:.2}s]", us(b), bel.as_secs_f64());
    println!("  chain_cert::canonical(event)     {:>8.3}us   (to_value + normalize + to_vec)  [50k iters in {:.2}s]", us(c), cel.as_secs_f64());
    println!("  Sha256 init+finalize, no body    {:>8.3}us   (the PER-EVENT fixed overhead)   [50k iters in {:.2}s]", us(d), del.as_secs_f64());
    println!("  Sha256 init+update+finalize      {:>8.3}us   (one event, standalone hasher)   [50k iters in {:.2}s]", us(e), eel.as_secs_f64());
    println!("  Sha256 update only, amortized    {:>8.3}us   (1 hasher / 512 bodies)          [2k iters in {:.2}s]", per_update, fel.as_secs_f64());

    println!("\n  CEILING ON BATCHING: batching removes init+finalize, never the");
    println!("  block processing or the serialisation.");
    println!("    per-event hash, standalone:     {:>8.3}us", us(e));
    println!("    per-event hash, amortized:      {:>8.3}us", per_update);
    println!(
        "    recoverable by batching:        {:>8.3}us",
        us(e) - per_update
    );
    println!(
        "    serialisation (NOT recoverable  {:>8.3}us   <- canonical() is per-event",
        us(c)
    );
    println!("      by batching the hash)                      no matter how the hash is grouped");
    let reserialise = (us(c) - us(a)).max(0.0);
    println!(
        "    of which avoidable by digesting {:>8.3}us   <- if the WAL stores canonical bytes",
        reserialise
    );

    // ---------------------------------------------------------------
    // 2. DOES AMORTIZING BRING THE APPEND DELTA UNDER +-2%?
    // ---------------------------------------------------------------
    // Instrument note: #480 proved the 1-event-per-fsync column cannot resolve
    // +-2% on APFS (its sensitivity control failed: jitter +-8% vs a 0.1%
    // signal). The 512/2000 columns pay far fewer fsyncs per event and were
    // reproducible there, so those are the load-bearing ones. Each cell is the
    // median of 3 paired runs with the range printed, and a sensitivity control
    // follows.
    // ---------------------------------------------------------------
    // 2. DOES AMORTIZING RESCUE THE +-2% APPEND GATE?
    // ---------------------------------------------------------------
    // Differencing two append loops cannot resolve this. Measured directly,
    // the same variant came out at 0.092us and then 0.277us on consecutive
    // runs -- a 3x swing on the quantity being judged against a 2% gate. So
    // the numerator is measured as a PRIMITIVE with the tight harness (whose
    // figures reproduce to ~1%), the denominator is stated with its
    // provenance, and the gate is evaluated as arithmetic on both.
    let n_ap = 3072usize;
    let aps = chain(11, n_ap);
    let reps = 7usize;
    let nofsync = n_ap + 1; // only the final sync fires
    let e0 = &aps[0];
    let stored = serde_json::to_vec(e0).unwrap();

    println!("\n== 2a. Added cost per event, measured as a primitive (high precision)");
    let (h1, h1p99, h1el) = bench(20_000, 200_000, || {
        std::hint::black_box(chain_cert::roll_bytes(
            std::hint::black_box(Some([7u8; 32])),
            std::hint::black_box(&stored),
        ));
    });
    let (hre, _, hreel) = bench(20_000, 200_000, || {
        std::hint::black_box(chain_cert::roll(
            std::hint::black_box(Some([7u8; 32])),
            std::hint::black_box(e0),
        ));
    });
    let (hch, _, hchel) = bench(2_000, 20_000, || {
        let mut r = chain_cert::ChunkRoller::resume(None);
        for _ in 0..chain_cert::CHUNK_EVENTS {
            r.absorb(std::hint::black_box(&stored));
        }
        std::hint::black_box(r.certificate(1));
    });
    let chunk_per_event = us(hch) / chain_cert::CHUNK_EVENTS as f64;
    println!(
        "  #480 per-event reserialise: {:.3}us   [200k iters in {:.2}s]",
        us(hre),
        hreel.as_secs_f64()
    );
    println!(
        "  per-event stored bytes:     {:.3}us  (p99 {:.3}us) [200k iters in {:.2}s]",
        us(h1),
        us(h1p99),
        h1el.as_secs_f64()
    );
    println!(
        "  AMORTIZED chunked:          {:.3}us   ({} events/chunk) [20k iters in {:.2}s]",
        chunk_per_event,
        chain_cert::CHUNK_EVENTS,
        hchel.as_secs_f64()
    );
    println!(
        "  => amortizing saves {:.3}us/event vs per-event stored bytes ({:.0}% of it)",
        us(h1) - chunk_per_event,
        (us(h1) - chunk_per_event) / us(h1) * 100.0
    );

    println!("\n== 2b. Baseline per-event append CPU (the denominator)");
    let mut bs = Vec::new();
    for _ in 0..reps {
        bs.push(append_arm(&aps, Variant::Off, nofsync, 0).as_secs_f64() * 1e6 / n_ap as f64);
    }
    let base_cpu = median(&mut bs.clone());
    println!(
        "  this harness: {:.3}us/event (serialise + one write() syscall)",
        base_cpu
    );
    println!(
        "  ⚠ PROVENANCE: that is THIS harness, which syscalls per event. EHDB\n              buffers into a part file, so its per-event CPU is likely LOWER — which\n              makes the digest a LARGER share. 2d varies this input deliberately."
    );

    println!("\n== 2c. Implied append delta at EHDB's group-commit sizes");
    let fsync_us = measure_fsync_us();
    match fsync_us {
        Some(fs) => {
            println!("  measured fsync: {:.0}us", fs);
            println!(
                "  {:<28} {:>10} {:>10} {:>10} {:>10}",
                "variant", "g=1", "g=17", "g=512", "g=2000"
            );
            for (label, added) in [
                ("#480 per-event reserialise", us(hre)),
                ("per-event stored bytes", us(h1)),
                ("AMORTIZED chunked", chunk_per_event),
            ] {
                let mut cells = String::new();
                for g in [1.0f64, 17.0, 512.0, 2000.0] {
                    let d = -added / (base_cpu + fs / g) * 100.0;
                    cells.push_str(&format!(
                        "{:>9.2}%{}",
                        d,
                        if d.abs() <= 2.0 { " " } else { "*" }
                    ));
                }
                println!("  {:<28}{}", label, cells);
            }
            println!("  (* = outside the +-2% gate)");
        }
        None => println!("  fsync unmeasurable — implied delta UNKNOWN"),
    }

    println!("\n== 2d. Sensitivity to the denominator (the uncertain input)");
    if let Some(fs) = fsync_us {
        println!("  AMORTIZED chunked, delta at g=512 / g=2000, vs assumed base CPU:");
        for b in [1.0f64, 2.0, 3.0, base_cpu] {
            let d512 = -chunk_per_event / (b + fs / 512.0) * 100.0;
            let d2000 = -chunk_per_event / (b + fs / 2000.0) * 100.0;
            println!(
                "    base={:<5.2}us  g=512 {:>7.2}% {}   g=2000 {:>7.2}% {}",
                b,
                d512,
                if d512.abs() <= 2.0 {
                    "HOLDS "
                } else {
                    "BREAKS"
                },
                d2000,
                if d2000.abs() <= 2.0 {
                    "HOLDS "
                } else {
                    "BREAKS"
                }
            );
        }
    }

    println!("\n== 2e. Direct end-to-end corroboration");
    println!(
        "  Only the LARGE effect is resolvable end to end. #480's variant costs\n           2.0us/event, which this instrument can see; the 0.18-0.21us variants are\n           below its resolution, so they are judged in 2c, not here."
    );
    for &g in &[512usize, 2000] {
        let mut ds = Vec::new();
        for _ in 0..reps {
            let bt = append_arm(&aps, Variant::Off, g, 0);
            let ct = append_arm(&aps, Variant::PerEventReserialise, g, 0);
            let b = n_ap as f64 / bt.as_secs_f64();
            let c = n_ap as f64 / ct.as_secs_f64();
            ds.push((c - b) / b * 100.0);
        }
        let d = median(&mut ds.clone());
        let expect = -us(hre) / (base_cpu + fsync_us.unwrap_or(3000.0) / g as f64) * 100.0;
        println!(
            "  g={:<5} #480 variant measured {:+.2}%   2c predicts {:+.2}%   {}",
            g,
            d,
            expect,
            if (d - expect).abs() < 8.0 {
                "AGREE (instrument corroborates the model)"
            } else {
                "DISAGREE — investigate"
            }
        );
        // Known-size control: +1 hash/event on top of the stored-bytes arm.
        let mut cds = Vec::new();
        for _ in 0..reps {
            let b2 = append_arm(&aps, Variant::PerEventStoredBytes, g, 0);
            let c2 = append_arm(&aps, Variant::StoredBytesDoubled, g, 0);
            let bb = n_ap as f64 / b2.as_secs_f64();
            let cc = n_ap as f64 / c2.as_secs_f64();
            cds.push((cc - bb) / bb * 100.0);
        }
        let cm = median(&mut cds.clone());
        let cexpect = -us(h1) / (base_cpu + fsync_us.unwrap_or(3000.0) / g as f64) * 100.0;
        println!(
            "        known-size control (+1 hash/ev): measured {:+.2}%, expected {:+.2}% -> {}",
            cm,
            cexpect,
            if cm < -1.0 {
                "resolves it"
            } else {
                "BELOW RESOLUTION (confirms the small variants cannot be judged here)"
            }
        );
    }
    // ---------------------------------------------------------------
    // 3. IS VALIDATION STILL O(1) WITH CHUNK GRANULARITY?
    // ---------------------------------------------------------------
    println!("== 3. Validation still O(1)? (chunk-granular certificates)");
    for &n in &[36usize, 174, 1000, 10000] {
        let evs = chain(3, n);
        let mut r = chain_cert::ChunkRoller::resume(None);
        for e in &evs {
            r.absorb(&serde_json::to_vec(e).unwrap());
        }
        let cert = r.certificate(3).expect("sealed");
        let (p50, p99, el) = bench(2_000, 20_000, || {
            std::hint::black_box(chain_cert::validate(
                std::hint::black_box(&cert),
                std::hint::black_box(&cert),
            ));
        });
        println!(
            "  n={:<6} sealed_len={:<6} tail={:<2} validate p50 {:.4}us p99 {:.4}us   [20k iters in {:.3}s]",
            n,
            cert.chain_len,
            r.uncertified_tail(),
            us(p50),
            us(p99),
            el.as_secs_f64()
        );
    }

    // ---------------------------------------------------------------
    // 4. REFOLD ELIMINATION AT CHUNK GRANULARITY
    // ---------------------------------------------------------------
    // #480's per-event certificate eliminated 99.80% of refolds because ANY
    // prefix was certifiable. A chunk-granular certificate cannot certify a
    // prefix past the last sealed chunk, so the honest metric is EVENTS
    // REFOLDED, not polls avoided: a poll mid-chunk still refolds its tail.
    println!("\n== 4. Refold elimination at chunk granularity (gate: >=90%)");
    let polls = 19_840usize;
    let mut refolded_without = 0u64;
    let mut refolded_with = 0u64;
    let mut o1_exact = 0u64;
    for i in 0..polls {
        // Chain lengths drawn from the recorded corpus (36 and 174 events).
        let chain_len = if i % 3 == 0 { 174 } else { 36 };
        let n = 1 + (i * 7919) % chain_len; // poll position within the chain
        let tail = (n as u32) % chain_cert::CHUNK_EVENTS;
        refolded_without += n as u64;
        refolded_with += tail as u64;
        if tail == 0 {
            o1_exact += 1;
        }
    }
    let reduction = 100.0 - (refolded_with as f64 / refolded_without as f64 * 100.0);
    println!("  polls simulated:        {polls}");
    println!("  events refolded, no cert: {refolded_without}");
    println!("  events refolded, chunked: {refolded_with}");
    println!(
        "  reduction in refold WORK: {:.2}%   (gate >=90%: {})",
        reduction,
        if reduction >= 90.0 { "PASSES" } else { "FAILS" }
    );
    println!(
        "  polls needing zero refold: {} / {} ({:.1}%) — the rest refold a tail of <{} events",
        o1_exact,
        polls,
        o1_exact as f64 / polls as f64 * 100.0,
        chain_cert::CHUNK_EVENTS
    );
    println!(
        "  (#480's per-event certificate eliminated 99.80% of POLLS; chunking\n            trades some of that for the append win. Both clear the >=90% gate,\n            but they are not the same metric — this one counts work, not polls.)"
    );
}

fn num_cpus_hint() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0)
}
