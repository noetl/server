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
use std::net::SocketAddr;

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
/// a shard→address map. Entries that don't parse are skipped (logged by the
/// caller); an empty map means "no writers configured".
pub fn parse_writer_addrs(spec: &str) -> BTreeMap<u32, SocketAddr> {
    let mut out = BTreeMap::new();
    for entry in spec.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((shard, addr)) = entry.split_once('@') else {
            continue;
        };
        let (Ok(shard), Ok(addr)) = (
            shard.trim().parse::<u32>(),
            addr.trim().parse::<SocketAddr>(),
        ) else {
            continue;
        };
        out.insert(shard, addr);
    }
    out
}

/// A lazily-connected EHDB command publisher over the per-shard writers.
pub struct EhdbCommandPublisher {
    shard_count: u32,
    addrs: BTreeMap<u32, SocketAddr>,
    router: Mutex<Option<PublishRouter<D1EventLog>>>,
}

impl EhdbCommandPublisher {
    /// A publisher routing over `shard_count` shards to the writers at `addrs`.
    pub fn new(shard_count: u32, addrs: BTreeMap<u32, SocketAddr>) -> Self {
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

    /// Publish one command notification onto the EHDB bus. `execution_id` routes
    /// the shard; `event_id` is the sort key; `payload` is the notification JSON.
    /// Returns the writer-assigned sort key. Lazily (re)connects the router.
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
        let mut guard = self.router.lock().await;
        if guard.is_none() {
            let router = PublishRouter::<D1EventLog>::connect(self.shard_count, self.addrs.clone())
                .await
                .map_err(|e| format!("EHDB writer connect failed: {e}"))?;
            *guard = Some(router);
        }
        match guard.as_mut().unwrap().publish(&record).await {
            Ok(seq) => Ok(seq),
            Err(e) => {
                // Drop the router so the next publish redials (writer restarted,
                // rolled, or a shard moved).
                *guard = None;
                Err(format!("EHDB publish failed: {e}"))
            }
        }
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
        assert_eq!(m[&0], "127.0.0.1:9100".parse().unwrap());
        assert_eq!(m[&1], "127.0.0.1:9101".parse().unwrap());
        assert_eq!(m[&2], "10.0.0.5:9100".parse().unwrap());
        assert!(parse_writer_addrs("").is_empty());
    }
}
