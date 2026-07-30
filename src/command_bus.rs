//! **L1 T4 — the EHDB command bus (flag-gated).**
//!
//! Selects the transport that carries command notifications to workers, behind
//! [`NOETL_COMMAND_BUS`](CommandBusMode). Default `nats` leaves today's path
//! untouched. `ehdb` publishes each command to the per-shard EHDB writer (the
//! cutover). `shadow` publishes to **both** — NATS stays authoritative and
//! workers keep consuming it, while the same command is mirrored onto the EHDB
//! bus so a shadow consumer can verify parity before any flip.
//!
//! A command notification maps to a D1 [`EventRecord`]: `event_id` is the sort
//! key (monotonic → the single-writer ascending contract holds per shard),
//! `execution_id` is the shard key (`shard_for_execution` is byte-identical to
//! the server/worker `shard_for`), and the notification JSON is the payload —
//! the worker decodes it back and fetches full command details from the API,
//! exactly as it does off NATS today.
//!
//! The publisher is **lazy-connected**: it dials the writers on first publish
//! (and drops + redials on error), so the stateless server never hard-depends on
//! the writers being up at boot — matching how it tolerates NATS being absent.

use std::collections::BTreeMap;
use std::sync::Arc;

use ehdb_feed::PublishRouter;
use ehdb_l0::{D1EventLog, EventRecord};
use tokio::sync::Mutex;

/// Which transport carries command notifications (env `NOETL_COMMAND_BUS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommandBusMode {
    /// Publish to NATS only — today's path (default).
    #[default]
    Nats,
    /// Publish to the per-shard EHDB writer only — the cutover.
    Ehdb,
    /// Publish to both: NATS authoritative, EHDB mirrored for parity comparison.
    Shadow,
}

impl CommandBusMode {
    /// Parse the `NOETL_COMMAND_BUS` value; anything unrecognised is the safe
    /// default (`nats`).
    pub fn from_env_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "ehdb" => Self::Ehdb,
            "shadow" => Self::Shadow,
            _ => Self::Nats,
        }
    }

    /// Whether this mode publishes to the EHDB bus.
    pub fn publishes_ehdb(self) -> bool {
        matches!(self, Self::Ehdb | Self::Shadow)
    }

    /// Whether this mode publishes to NATS.
    pub fn publishes_nats(self) -> bool {
        matches!(self, Self::Nats | Self::Shadow)
    }
}

/// Parse `NOETL_COMMAND_BUS_WRITER_ADDRS` = `"0@host:port,1@host:port,..."` into
/// a shard→address map. The address is kept as a `host:port` **string**, not a
/// parsed `SocketAddr`, so a Kubernetes service DNS name
/// (`noetl-cmdbus-writer.noetl.svc.cluster.local:9100`) passes and is resolved
/// at connect time (finding #2, noetl/ai-meta#194). Entries missing an `@`, a
/// numeric shard, or a `:port` separator are skipped; an empty map means "no
/// writers configured".
pub fn parse_writer_addrs(spec: &str) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    for entry in spec.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((shard, addr)) = entry.split_once('@') else {
            continue;
        };
        let Ok(shard) = shard.trim().parse::<u32>() else {
            continue;
        };
        let addr = addr.trim();
        if addr.is_empty() || !addr.contains(':') {
            continue;
        }
        out.insert(shard, addr.to_string());
    }
    out
}

/// A lazily-connected EHDB command publisher over the per-shard writers.
///
/// The router is held behind the mutex only to *swap* it (lazy connect, redial
/// after a failure) — never across a publish. A publish clones the `Arc` out,
/// drops the guard, and then does its round-trip, so concurrent publishes run
/// concurrently. Holding the lock across the round-trip serialised the control
/// plane's whole command path behind one writer `fsync` each, which was the
/// dominant term in command dispatch latency (noetl/ai-meta#205); it also kept
/// the writer from ever seeing two records at once, so it could never
/// group-commit them.
pub struct EhdbCommandPublisher {
    shard_count: u32,
    addrs: BTreeMap<u32, String>,
    router: Mutex<Option<Arc<PublishRouter<D1EventLog>>>>,
}

impl EhdbCommandPublisher {
    /// A publisher routing over `shard_count` shards to the writers at `addrs`
    /// (`host:port` strings — DNS names resolved at connect time).
    pub fn new(shard_count: u32, addrs: BTreeMap<u32, String>) -> Self {
        Self {
            shard_count: shard_count.max(1),
            addrs,
            router: Mutex::new(None),
        }
    }

    /// Whether any writer address is configured.
    pub fn is_configured(&self) -> bool {
        !self.addrs.is_empty()
    }

    /// How many times a publish is attempted before it gives up. A writer
    /// restart breaks every socket the router holds, so the first attempt after
    /// one always fails; the retries carry the command across the gap while the
    /// replacement pod's endpoint appears (noetl/ai-meta#208).
    const PUBLISH_ATTEMPTS: u32 = 3;
    /// Pause between publish attempts — long enough for a pod swap's endpoint
    /// update, short enough that the API request behind this publish is not held
    /// up noticeably.
    const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

    /// Publish one command notification onto the EHDB bus. `execution_id` routes
    /// the shard; `event_id` is the sort key; `payload` is the notification JSON.
    /// Returns the writer-assigned sort key. Lazily (re)connects the router.
    ///
    /// **Retries across a writer restart (noetl/ai-meta#208).** The router holds
    /// live sockets to the shard writers, so a writer pod that goes away breaks
    /// every publish in flight on it — and the first publish afterwards, which
    /// only discovers the socket is dead by using it. Dropping the router made the
    /// *next* command redial, but the command that hit the broken socket was
    /// simply lost: `command.issued` was already durable in the event log, nothing
    /// ever reached the bus, and the caller got a 500. Observed in kind on a
    /// routine writer rollout as `EHDB publish failed: early eof` plus one
    /// execution stuck with an issued-but-never-claimed command. A command
    /// dropped this way is only recovered by the orphaned-command guardrail
    /// (noetl/ai-meta#171), and after T5 there is no NATS to fall back to.
    ///
    /// So a failed attempt now redials and publishes again, up to
    /// [`PUBLISH_ATTEMPTS`](Self::PUBLISH_ATTEMPTS). A retry can only ever
    /// *duplicate* a command — if the record was appended but its ack was lost,
    /// the retry appends a second copy — and duplicate delivery is already what
    /// the bus's `ack_wait` redelivery produces, so the worker's claim path
    /// dedupes it (the second claimer is told the command is already claimed).
    /// Losing the command has no such safety net, so at-least-once is the right
    /// trade here.
    pub async fn publish(
        &self,
        execution_id: i64,
        event_id: i64,
        payload: &[u8],
    ) -> Result<u64, String> {
        let record = EventRecord::new(
            event_id as u64,
            execution_id.to_string(),
            String::new(),
            String::from_utf8_lossy(payload).into_owned(),
        );
        let mut last_err = String::new();
        for attempt in 1..=Self::PUBLISH_ATTEMPTS {
            let router = match self.router().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = e;
                    if attempt < Self::PUBLISH_ATTEMPTS {
                        tokio::time::sleep(Self::RETRY_BACKOFF).await;
                    }
                    continue;
                }
            };
            match router.publish(&record).await {
                Ok(seq) => {
                    if attempt > 1 {
                        tracing::info!(
                            execution_id,
                            event_id,
                            attempt,
                            "EHDB command published after redialing the writer"
                        );
                    }
                    return Ok(seq);
                }
                Err(e) => {
                    last_err = format!("EHDB publish failed: {e}");
                    // Drop the router so the next attempt redials (writer
                    // restarted, rolled, or a shard moved) — but only if it is
                    // still the one that failed, so a redial another task already
                    // completed is not thrown away.
                    let mut guard = self.router.lock().await;
                    if guard.as_ref().is_some_and(|r| Arc::ptr_eq(r, &router)) {
                        *guard = None;
                    }
                    drop(guard);
                    if attempt < Self::PUBLISH_ATTEMPTS {
                        tracing::warn!(
                            execution_id,
                            event_id,
                            attempt,
                            error = %last_err,
                            "EHDB publish failed; redialing the writer and retrying"
                        );
                        tokio::time::sleep(Self::RETRY_BACKOFF).await;
                    }
                }
            }
        }
        Err(last_err)
    }

    /// The live router, connecting it if this is the first use or the previous
    /// one was dropped after a failure.
    async fn router(&self) -> Result<Arc<PublishRouter<D1EventLog>>, String> {
        let mut guard = self.router.lock().await;
        if guard.is_none() {
            let router = PublishRouter::<D1EventLog>::connect(self.shard_count, self.addrs.clone())
                .await
                .map_err(|e| format!("EHDB writer connect failed: {e}"))?;
            *guard = Some(Arc::new(router));
        }
        Ok(Arc::clone(guard.as_ref().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing_defaults_to_nats() {
        assert_eq!(CommandBusMode::from_env_value("nats"), CommandBusMode::Nats);
        assert_eq!(CommandBusMode::from_env_value("EHDB"), CommandBusMode::Ehdb);
        assert_eq!(
            CommandBusMode::from_env_value(" Shadow "),
            CommandBusMode::Shadow
        );
        assert_eq!(
            CommandBusMode::from_env_value("garbage"),
            CommandBusMode::Nats
        );
        assert_eq!(CommandBusMode::default(), CommandBusMode::Nats);
        assert!(CommandBusMode::Shadow.publishes_ehdb() && CommandBusMode::Shadow.publishes_nats());
        assert!(CommandBusMode::Ehdb.publishes_ehdb() && !CommandBusMode::Ehdb.publishes_nats());
        assert!(!CommandBusMode::Nats.publishes_ehdb() && CommandBusMode::Nats.publishes_nats());
    }

    #[test]
    fn writer_addr_parsing() {
        let m = parse_writer_addrs("0@127.0.0.1:9100, 1@127.0.0.1:9101 ,bad,2@10.0.0.5:9100");
        assert_eq!(m.len(), 3);
        assert_eq!(m[&0], "127.0.0.1:9100");
        assert_eq!(m[&1], "127.0.0.1:9101");
        assert_eq!(m[&2], "10.0.0.5:9100");
        assert!(parse_writer_addrs("").is_empty());
    }

    #[test]
    fn writer_addr_parsing_accepts_dns_names() {
        // Finding #2 (noetl/ai-meta#194): a K8s service DNS name is NOT a
        // parseable `SocketAddr`, but must be kept for resolution at connect.
        let m =
            parse_writer_addrs("0@noetl-cmdbus-writer.noetl.svc.cluster.local:9100,1@writer-1:9100");
        assert_eq!(m.len(), 2);
        assert_eq!(m[&0], "noetl-cmdbus-writer.noetl.svc.cluster.local:9100");
        assert_eq!(m[&1], "writer-1:9100");
        // A host with no `:port` separator is rejected.
        assert!(parse_writer_addrs("0@nohost").is_empty());
    }
}
