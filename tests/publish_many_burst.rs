//! noetl/ai-meta#561 fused ingest — `publish_many` puts a batch's frames on the
//! one pipelined connection before awaiting any ack, so the writer group-commits
//! them. Driven against a real ehdb ingest listener (same harness as
//! `publish_retry_window.rs`).


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
async fn a_burst_lands_in_item_order_and_beats_per_row_round_trips() {
    let (obj, local) = (unique_dir("burst-obj"), unique_dir("burst-local"));
    let addr = free_addr().await;
    let _w = Writer::spawn(local.clone(), obj.clone(), addr);
    let pubr = publisher(addr);
    pubr.publish(7, 1, b"{\"warm\":1}").await.unwrap();

    const N: i64 = 12;
    let payloads: Vec<Vec<u8>> = (0..N).map(|i| format!("{{\"i\":{i}}}").into_bytes()).collect();

    let t = Instant::now();
    let mut serial = Vec::new();
    for (i, p) in payloads.iter().enumerate() {
        serial.push(pubr.publish(7, 100 + i as i64, p).await.unwrap());
    }
    let serial_t = t.elapsed();

    let items: Vec<(i64, &[u8])> = payloads
        .iter()
        .enumerate()
        .map(|(i, p)| (200 + i as i64, p.as_slice()))
        .collect();
    let t = Instant::now();
    let burst = pubr.publish_many(7, &items).await.unwrap();
    let burst_t = t.elapsed();

    assert_eq!(burst.len(), N as usize);
    assert!(
        burst.windows(2).all(|w| w[0] < w[1]),
        "writer-assigned keys must follow item order: {burst:?}"
    );
    assert!(burst[0] > *serial.last().unwrap(), "after the earlier rows");
    eprintln!("serial {N} rows: {serial_t:?}; one burst: {burst_t:?}");

    for d in [&obj, &local] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_with_no_writer_falls_back_to_the_retry_loop_and_keeps_order() {
    let (obj, local) = (unique_dir("fb-obj"), unique_dir("fb-local"));
    let addr = free_addr().await;
    let pubr = publisher(addr);
    // The writer appears 1.5 s after the burst starts: the burst cannot dial,
    // so every row goes through the per-row retry loop — in order.
    let (o2, l2) = (obj.clone(), local.clone());
    let late = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(1_500));
        Writer::spawn(l2, o2, addr)
    });
    let payloads: Vec<Vec<u8>> = (0..5).map(|i| format!("{{\"i\":{i}}}").into_bytes()).collect();
    let items: Vec<(i64, &[u8])> = payloads
        .iter()
        .enumerate()
        .map(|(i, p)| (300 + i as i64, p.as_slice()))
        .collect();
    let seqs = pubr.publish_many(9, &items).await.expect("rides out the gap");
    let _w = late.await.unwrap();
    assert_eq!(seqs.len(), 5);
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");

    for d in [&obj, &local] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn the_batch_path_is_flag_gated_and_keeps_the_per_row_loop() {
    let src = include_str!("../src/handlers/event_write.rs");
    assert!(src.contains("state.config.event_publish_batch && rows.len() > 1"));
    assert!(src.contains("for row in rows.iter().filter(|_| !batched)"));
}

#[test]
fn step_events_are_fused_ahead_of_command_issued_only_under_the_flag() {
    let ev = include_str!("../src/handlers/events.rs");
    assert!(ev.contains("let fuse = state.config.event_publish_batch;"));
    assert!(ev.contains("if fuse {\n            fused.push(ev);\n            continue;\n        }"));
    let ex = include_str!("../src/handlers/execute.rs");
    // orchestration events first, then command.issued — the unfused order
    assert!(ex.contains("let event_rows: Vec<crate::handlers::event_write::EventRow> = fused_events\n        .into_iter()\n        .chain(prepared.iter()"));
    // the command row's issuing event is the one the unfused head would hold
    assert!(ex.contains("let issuing_event = match fused_events.last() {\n        Some(last) => last.event_id,"));
}
