//! noetl/ai-meta#208 follow-up — the EHDB publish retry must span a **writer pod
//! restart**, not just a broken socket.
//!
//! The first cut of the retry (3 attempts × 250 ms ≈ 0.5 s of retrying) redialed
//! a cleanly-closed socket fine but ran out while the writer was still coming
//! back: the measured prod gap from the writer's SIGTERM to a re-dialable
//! replacement is ~2.7 s, and two `POST /api/execute` calls returned 500 during
//! it. Fail-closed and no silent loss, but not transparent — and after T5 there
//! is no NATS behind the bus.
//!
//! These tests drive the real `EhdbCommandPublisher` against a real ehdb ingest
//! listener that is taken away and brought back on the same address, which is
//! exactly the shape of a pod swap from the server's side.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ehdb_l0::substrate::DurableSubstrate;
use ehdb_l0::{D1EventLog, L0Config, L0Engine, LocalFsSubstrate};
use noetl_server::command_bus::EhdbCommandPublisher;
use tokio::net::TcpListener;

fn unique_dir(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("noetl-pub-retry-{tag}-{}-{n}", std::process::id()))
}

/// A writer serving ingest on `addr`, on its own runtime in its own thread.
///
/// The runtime is the unit of teardown on purpose: aborting a `serve_ingest`
/// task only stops the *accept loop*, leaving already-accepted connections alive
/// — so the server's router would keep publishing down a socket a real pod swap
/// would have severed, and the test would prove nothing. Dropping the whole
/// runtime drops every task and every socket with it, which is the pod-gone
/// shape.
struct Writer {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Writer {
    fn spawn(dir: std::path::PathBuf, obj: std::path::PathBuf, addr: std::net::SocketAddr) -> Self {
        let (stop, stop_rx) = std::sync::mpsc::channel::<()>();
        let (ready, ready_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let listener = TcpListener::bind(addr).await.expect("writer bind");
                let store: Arc<dyn DurableSubstrate> =
                    Arc::new(LocalFsSubstrate::new(&obj).unwrap());
                let engine = L0Engine::<D1EventLog>::open(L0Config::d1(&dir), store).unwrap();
                let writer = Arc::new(ehdb_feed::FeedWriter::new(engine));
                tokio::spawn(ehdb_feed::serve_ingest(listener, writer));
                ready.send(()).ok();
                let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
            });
            // `rt` drops here: listener + every accepted connection go with it.
        });
        ready_rx.recv().expect("writer came up");
        Self {
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn free_addr() -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    addr
}

fn publisher(addr: std::net::SocketAddr) -> EhdbCommandPublisher {
    let mut addrs = BTreeMap::new();
    addrs.insert(0u32, addr.to_string());
    EhdbCommandPublisher::new(1, addrs)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_publish_survives_a_writer_gap_longer_than_the_old_window() {
    let (obj, local) = (unique_dir("obj"), unique_dir("local"));
    let addr = free_addr().await;

    let pubr = publisher(addr);
    let w = Writer::spawn(local.clone(), obj.clone(), addr);
    assert!(
        pubr.publish(1, 1, b"{\"execution_pool\":\"shared\"}")
            .await
            .is_ok(),
        "baseline publish reaches a live writer"
    );

    // The writer goes away, and comes back 2.5 s later — past the old 0.5 s
    // retry window, inside the new one.
    drop(w);
    const GAP: Duration = Duration::from_millis(2_500);
    let (o2, l2) = (obj.clone(), local.clone());
    let restart = tokio::task::spawn_blocking(move || {
        std::thread::sleep(GAP);
        Writer::spawn(l2, o2, addr)
    });

    let started = Instant::now();
    let result = pubr.publish(2, 2, b"{\"execution_pool\":\"shared\"}").await;
    let waited = started.elapsed();
    let replacement = restart.await.unwrap();

    assert!(
        result.is_ok(),
        "publish must ride out the restart, got: {result:?} after {waited:?}"
    );
    assert!(
        waited >= Duration::from_millis(2_000),
        "it really did wait out the gap rather than hitting a still-live writer ({waited:?})"
    );
    assert!(
        waited < Duration::from_secs(10),
        "and returned as soon as the writer was back, not at the deadline ({waited:?})"
    );

    drop(replacement);
    for d in [&obj, &local] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writer_that_never_returns_fails_closed_within_the_deadline() {
    // The deadline is a ceiling on how long a caller's request is held: a bus
    // that is genuinely gone must still surface an error rather than hang.
    let addr = free_addr().await;
    let pubr = publisher(addr);

    let started = Instant::now();
    let result = pubr.publish(3, 3, b"{\"execution_pool\":\"shared\"}").await;
    let waited = started.elapsed();

    assert!(result.is_err(), "no writer at all must fail, not hang");
    assert!(
        waited >= Duration::from_secs(9),
        "it used the retry window before giving up ({waited:?})"
    );
    assert!(
        waited < Duration::from_secs(15),
        "and gave up at the deadline rather than retrying forever ({waited:?})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_healthy_publish_pays_nothing_for_the_wider_window() {
    // The window is a ceiling, not a cost — the common path must not slow down.
    let (obj, local) = (unique_dir("fast-obj"), unique_dir("fast-local"));
    let addr = free_addr().await;
    let _w = Writer::spawn(local.clone(), obj.clone(), addr);
    let pubr = publisher(addr);

    let started = Instant::now();
    for i in 0..20i64 {
        pubr.publish(i, i, b"{\"execution_pool\":\"shared\"}")
            .await
            .unwrap();
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "20 healthy publishes took {elapsed:?} — the retry path is not on the happy path"
    );

    for d in [&obj, &local] {
        let _ = std::fs::remove_dir_all(d);
    }
}
