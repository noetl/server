//! **L1 T4 — the EHDB command bus (flag-gated).**
//!
//! Selects the transport that carries command notifications to workers, behind
//! [`NOETL_COMMAND_BUS`](CommandBusMode).
//!
//! ⚠ **The cutover is done and NATS is deleted** (noetl/ai-meta#194 T5). Every
//! prod workload — server, user pool, and both system-pool shards — sets
//! `NOETL_COMMAND_BUS=ehdb` explicitly.
//!
//! ⚠ **The code default is still `Nats`**, which is now a transport that does
//! not exist. That is safe only because every deployment sets the variable. A
//! new workload rolled without it would default to a dead transport, and
//! "unset the flag" is no longer a rollback — it is an outage. Changing the
//! default is a behaviour change and is tracked separately rather than done
//! in passing.
//!
//! `shadow` mode published to both so a shadow consumer could verify parity
//! before the flip. It has no meaning now: there is no second bus to mirror
//! onto and nothing consuming NATS.
//!
//! A command notification maps to a D1 [`EventRecord`]: `event_id` is the sort
//! key (monotonic → the single-writer ascending contract holds per shard),
//! `execution_id` is the shard key (`shard_for_execution` is byte-identical to
//! the server/worker `shard_for`), and the notification JSON is the payload —
//! the worker decodes it back and fetches full command details from the API —
//! the same shape it used off NATS before the cutover.
//!
//! The publisher is **lazy-connected**: it dials the writers on first publish
//! (and drops + redials on error), so the stateless server never hard-depends on
//! the writers being up at boot. That tolerance was inherited from the NATS
//! client and matters more now, not less: the writer is the only bus.

use std::collections::BTreeMap;
use std::sync::Arc;

use ehdb_feed::PublishRouter;
use ehdb_l0::{D1EventLog, EventRecord};
use tokio::sync::Mutex;

/// Which transport carries command notifications (env `NOETL_COMMAND_BUS`).
///
/// There is deliberately **no `Default`**.  A default here would have to be a
/// transport, and the only transport that exists is EHDB — so defaulting would
/// mean guessing.  `NOETL_COMMAND_BUS` is required, and
/// [`CommandBusMode::from_env_value`] returns an error rather than choosing
/// (noetl/ai-meta#243).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandBusMode {
    /// Publish to NATS only.
    ///
    /// ⚠ NATS was deleted at T5.  The variant survives so that a stale
    /// `NOETL_COMMAND_BUS=nats` produces a specific, actionable error instead
    /// of a generic parse failure — it is not selectable.
    Nats,
    /// Publish to the per-shard EHDB writer only — the cutover.
    Ehdb,
    /// Publish to both: NATS authoritative, EHDB mirrored for parity comparison.
    Shadow,
}

impl CommandBusMode {
    /// Parse the `NOETL_COMMAND_BUS` value.  Required — there is no default.
    ///
    /// Before noetl/ai-meta#243 an unset or unrecognised value silently became
    /// `Nats`, which T5 deleted.  The server then started cleanly, published
    /// nothing, and every execution stalled with no error anywhere.  Each
    /// failure mode now names itself.
    pub fn from_env_value(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ehdb" => Ok(Self::Ehdb),
            "shadow" => Ok(Self::Shadow),
            "nats" => Err(
                "NOETL_COMMAND_BUS=nats selects a transport that no longer exists — NATS was \
                 removed at T5 (noetl/ai-meta#212). Set NOETL_COMMAND_BUS=ehdb."
                    .to_string(),
            ),
            "" => Err(
                "NOETL_COMMAND_BUS is required and unset. There is no default: the only \
                 transport is EHDB, and guessing would mean starting a server that publishes \
                 nothing while looking healthy. Set NOETL_COMMAND_BUS=ehdb."
                    .to_string(),
            ),
            other => Err(format!(
                "NOETL_COMMAND_BUS={other:?} is not a known transport. Valid values: ehdb, \
                 shadow. (nats was removed at T5.)"
            )),
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

    /// How long a publish keeps retrying before it gives up — sized to span a
    /// writer **pod restart**, not just a broken socket.
    ///
    /// The first cut of this retry (3 attempts × 250 ms) covered ~0.5 s of
    /// retrying, which is enough to redial a socket the writer closed cleanly but
    /// far short of a pod swap. The measured prod gap is ~2.7 s from the writer's
    /// SIGTERM to a re-dialable replacement — terminate, reschedule, reopen the
    /// durable log, rebind the ingest listener, endpoint propagate — and a rollout
    /// under load or a cold image pull is longer. So the window closed while the
    /// writer was still coming back and two `POST /api/execute` calls returned 500
    /// during the restart: fail-closed, no silent loss, but not transparent, which
    /// is the bar for a bus that has no NATS behind it after T5.
    ///
    /// 10 s covers the observed gap with room for a slow reschedule. It is a
    /// ceiling, not a cost: a healthy publish still returns on the first attempt,
    /// and the only request that waits is one that would otherwise have failed.
    const PUBLISH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
    /// First pause between publish attempts. Short, so a transient broken socket
    /// (the common case — the writer is already back) costs ~100 ms rather than a
    /// quarter second.
    const RETRY_BACKOFF_INITIAL: std::time::Duration = std::time::Duration::from_millis(100);
    /// Ceiling on the exponential backoff, so a long gap is still probed ~every
    /// second rather than sleeping through the writer's return.
    const RETRY_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_millis(1_000);
    /// Hard cap on attempts — a backstop against a pathological zero-cost failure
    /// spinning inside the deadline. With the backoff schedule above, the deadline
    /// is what actually ends the loop.
    const PUBLISH_ATTEMPTS: u32 = 32;

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
    /// So a failed attempt redials and publishes again, with exponential backoff,
    /// until [`PUBLISH_DEADLINE`](Self::PUBLISH_DEADLINE) elapses — a window sized
    /// to span a writer pod restart rather than just a broken socket. A retry can
    /// only ever
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
        let started = std::time::Instant::now();
        let mut last_err = String::new();
        let mut backoff = Self::RETRY_BACKOFF_INITIAL;
        for attempt in 1..=Self::PUBLISH_ATTEMPTS {
            let router = match self.router().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = e;
                    if !Self::sleep_before_retry(started, &mut backoff).await {
                        break;
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
                            waited_ms = started.elapsed().as_millis() as u64,
                            "EHDB command published after redialing the writer"
                        );
                    }
                    return Ok(seq);
                }
                Err(e) => {
                    last_err = format!("EHDB publish failed: {e}");
                    crate::metrics::record_ehdb_command_publish_failed("attempt");
                    // Drop the router so the next attempt redials (writer
                    // restarted, rolled, or a shard moved) — but only if it is
                    // still the one that failed, so a redial another task already
                    // completed is not thrown away.
                    let mut guard = self.router.lock().await;
                    if guard.as_ref().is_some_and(|r| Arc::ptr_eq(r, &router)) {
                        *guard = None;
                    }
                    drop(guard);
                    tracing::warn!(
                        execution_id,
                        event_id,
                        attempt,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        error = %last_err,
                        "EHDB publish failed; redialing the writer and retrying"
                    );
                    if !Self::sleep_before_retry(started, &mut backoff).await {
                        break;
                    }
                }
            }
        }
        // Countable, not just loggable: this is the dispatch path, and a command
        // that is never published is never claimed — the execution stops with no
        // terminal event to notice it (noetl/ai-meta#208).
        crate::metrics::record_ehdb_command_publish_failed("gave_up");
        tracing::error!(
            execution_id,
            event_id,
            elapsed_ms = started.elapsed().as_millis() as u64,
            error = %last_err,
            "EHDB publish gave up after the retry window"
        );
        Err(last_err)
    }

    /// Wait out the backoff before the next publish attempt, doubling it (capped
    /// at [`RETRY_BACKOFF_MAX`](Self::RETRY_BACKOFF_MAX)). Returns `false` when
    /// the [`PUBLISH_DEADLINE`](Self::PUBLISH_DEADLINE) leaves no room for another
    /// attempt, so the caller stops instead of sleeping past it — the deadline is
    /// the total wall-clock the caller's request can be held, not a per-attempt
    /// budget.
    async fn sleep_before_retry(
        started: std::time::Instant,
        backoff: &mut std::time::Duration,
    ) -> bool {
        let remaining = match Self::PUBLISH_DEADLINE.checked_sub(started.elapsed()) {
            Some(r) => r,
            None => return false,
        };
        let nap = (*backoff).min(remaining);
        if nap.is_zero() {
            return false;
        }
        tokio::time::sleep(nap).await;
        *backoff = (*backoff * 2).min(Self::RETRY_BACKOFF_MAX);
        true
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

    /// Every failure mode must NAME itself.
    ///
    /// This test replaces `mode_parsing_defaults_to_nats`, whose name was the
    /// defect: an unset or unrecognised value became `Nats`, a transport T5
    /// deleted.  The server then started cleanly, published nothing, and
    /// stalled every execution with no error anywhere (noetl/ai-meta#243).
    #[test]
    fn mode_parsing_is_required_and_every_failure_is_named() {
        // The two live transports.
        assert_eq!(
            CommandBusMode::from_env_value("EHDB"),
            Ok(CommandBusMode::Ehdb)
        );
        assert_eq!(
            CommandBusMode::from_env_value(" Shadow "),
            Ok(CommandBusMode::Shadow)
        );

        // Unset is an error, not a guess — there is no safe default when the
        // only transport is EHDB.
        let e = CommandBusMode::from_env_value("").unwrap_err();
        assert!(e.contains("required"), "unset must say so: {e}");
        assert!(e.contains("ehdb"), "and must name the fix: {e}");

        // A stale `nats` gets its OWN message rather than a generic parse
        // error, because that is the value a pre-T5 manifest actually carries.
        let e = CommandBusMode::from_env_value("nats").unwrap_err();
        assert!(e.contains("no longer exists"), "nats must be specific: {e}");

        // Anything else names what it saw.
        let e = CommandBusMode::from_env_value("garbage").unwrap_err();
        assert!(e.contains("garbage"), "must echo the bad value: {e}");

        assert!(CommandBusMode::Shadow.publishes_ehdb() && CommandBusMode::Shadow.publishes_nats());
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
        let m = parse_writer_addrs(
            "0@noetl-cmdbus-writer.noetl.svc.cluster.local:9100,1@writer-1:9100",
        );
        assert_eq!(m.len(), 2);
        assert_eq!(m[&0], "noetl-cmdbus-writer.noetl.svc.cluster.local:9100");
        assert_eq!(m[&1], "writer-1:9100");
        // A host with no `:port` separator is rejected.
        assert!(parse_writer_addrs("0@nohost").is_empty());
    }
}
