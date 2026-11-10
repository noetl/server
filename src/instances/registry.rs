//! **The instance registry**: who is registered, what each can run, and whether its lease
//! is current.
//!
//! # Soft state, by design
//!
//! Records live in memory and are rebuilt by the instances themselves: registration is the
//! heartbeat (one idempotent signed call on a timer), so after a server restart every live
//! instance is back within one heartbeat interval. A lease registry needs no durable copy to
//! be correct. The durable record of *who knew whom* is the ehgraph projection (`graph.rs`).
//!
//! # Why not the D8 runtime registry
//!
//! `/api/runtime/register` (D8) keys on ehdb's `RuntimeKind` enum (server, gateway, ehdb,
//! worker, playbook, execution). A NoETL *instance* (a laptop CLI, an edge daemon, a whole
//! cluster) is none of these, and adding `cli` / `daemon` is an ehdb change that moves the
//! three ehdb pins together. This module reuses D8's lease rules ([`ttl_ms`] follows
//! `NOETL_RUNTIME_TTL_SECS`) without writing to D8. Mirroring into D8 once ehdb has the kinds
//! is a follow-up.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// Most instances held at once.
pub const MAX_INSTANCES_ENV: &str = "NOETL_INSTANCE_MAX";
const DEFAULT_MAX: usize = 10_000;
/// A record is forgotten this many TTLs after its last heartbeat.
const FORGET_AFTER_TTLS: i64 = 10;

/// How an instance runs NoETL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    /// `noetl run` as a long-lived process (a listener or `noetl instance serve`).
    Cli,
    /// `noetl subscribe`.
    Daemon,
    /// A server + worker cluster, registering through a sidecar.
    Distributed,
}

/// How work reaches the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Reach {
    /// It pulls its inbox. Needs no inbound connectivity. The default.
    #[default]
    Inbox,
    /// It also serves an endpoint the control plane could call. Advertised only; this
    /// release delivers every task through the inbox.
    Push,
}

/// What an instance says about itself. Every field is bounded; none may hold secret material.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub runtime_kind: RuntimeKind,
    /// The NoETL build, e.g. `noetl-cli/6.1.0`.
    pub version: String,
    /// A human label. Not an identity: two instances may share one.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Tool kinds this instance's runtime can execute (`shell`, `http`, `postgres`, ...).
    #[serde(default)]
    pub tool_kinds: Vec<String>,
    /// Playbook paths or `*`-globs it will run for others.
    #[serde(default)]
    pub playbooks: Vec<String>,
    /// Worker pools, for a distributed instance.
    #[serde(default)]
    pub pools: Vec<String>,
    #[serde(default)]
    pub reach: Reach,
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Will it take tasks from other instances? Off unless it says so.
    #[serde(default)]
    pub accepts_tasks: bool,
    /// Instances it has discovered or handed work to. Becomes `knows` edges in the graph.
    #[serde(default)]
    pub known_peers: Vec<String>,
}

const MAX_STR: usize = 256;
const MAX_LIST: usize = 256;

fn bounded(field: &str, s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > MAX_STR || s.chars().any(char::is_control) {
        return Err(format!(
            "{field}: each value must be 1..={MAX_STR} printable characters"
        ));
    }
    Ok(())
}

fn bounded_list(field: &str, items: &[String]) -> Result<(), String> {
    if items.len() > MAX_LIST {
        return Err(format!("{field}: at most {MAX_LIST} entries"));
    }
    items.iter().try_for_each(|s| bounded(field, s))
}

impl Capabilities {
    pub fn validate(&self) -> Result<(), String> {
        bounded("version", &self.version)?;
        if let Some(n) = &self.name {
            bounded("name", n)?;
        }
        if let Some(s) = &self.site {
            bounded("site", s)?;
        }
        if self.labels.len() > 64 {
            return Err("labels: at most 64 entries".into());
        }
        for (k, v) in &self.labels {
            bounded("labels key", k)?;
            bounded("labels value", v)?;
        }
        bounded_list("tool_kinds", &self.tool_kinds)?;
        bounded_list("playbooks", &self.playbooks)?;
        bounded_list("pools", &self.pools)?;
        bounded_list("known_peers", &self.known_peers)?;
        if let Some(p) = self
            .known_peers
            .iter()
            .find(|p| !super::identity::is_instance_id(p))
        {
            return Err(format!("known_peers: `{p}` is not an instance id"));
        }
        if let Some(e) = &self.endpoint {
            let u = reqwest::Url::parse(e).map_err(|_| "endpoint: not a URL".to_string())?;
            if !matches!(u.scheme(), "https" | "http")
                || !u.username().is_empty()
                || u.password().is_some()
            {
                return Err("endpoint: must be http(s) and carry no credentials".into());
            }
        }
        Ok(())
    }
}

/// One registered instance.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstanceRecord {
    pub instance_id: String,
    /// Base64 ML-DSA-87 public key. The id is its fingerprint.
    pub public_key: String,
    #[serde(flatten)]
    pub capabilities: Capabilities,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    pub heartbeats: u64,
}

/// `*` matches any run of characters, including `/`. Anything else is literal.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == path;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !path.starts_with(first) || path.len() < first.len() + last.len() || !path.ends_with(last) {
        return false;
    }
    let mut rest = &path[first.len()..path.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// A discovery query. Every given filter must match.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    #[serde(default)]
    pub runtime_kind: Option<RuntimeKind>,
    #[serde(default)]
    pub tool_kind: Vec<String>,
    #[serde(default)]
    pub pool: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    /// `key=value` pairs.
    #[serde(default)]
    pub label: Vec<String>,
    /// A playbook path the instance must be willing to run.
    #[serde(default)]
    pub playbook: Option<String>,
    #[serde(default)]
    pub accepts_tasks: Option<bool>,
    /// Include instances whose lease has lapsed. Default false.
    #[serde(default)]
    pub include_stale: bool,
}

impl Query {
    pub fn matches(&self, r: &InstanceRecord) -> bool {
        let c = &r.capabilities;
        self.runtime_kind.is_none_or(|k| c.runtime_kind == k)
            && self
                .tool_kind
                .iter()
                .all(|t| c.tool_kinds.iter().any(|x| x == t))
            && self.pool.as_ref().is_none_or(|p| c.pools.contains(p))
            && self
                .site
                .as_ref()
                .is_none_or(|s| c.site.as_ref() == Some(s))
            && self.label.iter().all(|kv| match kv.split_once('=') {
                Some((k, v)) => c.labels.get(k).map(String::as_str) == Some(v),
                None => c.labels.contains_key(kv),
            })
            && self
                .playbook
                .as_ref()
                .is_none_or(|p| c.playbooks.iter().any(|g| glob_match(g, p)))
            && self.accepts_tasks.is_none_or(|a| c.accepts_tasks == a)
    }
}

/// A discovery answer row.
#[derive(Debug, Clone, Serialize)]
pub struct Found {
    #[serde(flatten)]
    pub record: InstanceRecord,
    pub live: bool,
    pub age_secs: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    Registered,
    Renewed,
}

pub struct Registry {
    records: HashMap<String, InstanceRecord>,
    pub ttl_ms: i64,
    pub max: usize,
}

/// The lease, from the same knob the D8 registry uses.
pub fn ttl_ms() -> i64 {
    (crate::runtime_registry::ttl_micros() / 1000) as i64
}

impl Registry {
    pub fn new(ttl_ms: i64, max: usize) -> Self {
        Registry {
            records: HashMap::new(),
            ttl_ms,
            max,
        }
    }

    pub fn from_env() -> Self {
        let max = std::env::var(MAX_INSTANCES_ENV)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|v: &usize| *v > 0)
            .unwrap_or(DEFAULT_MAX);
        Registry::new(ttl_ms(), max)
    }

    /// Register or renew. Renewal replaces the capabilities: what an instance says now is
    /// what it can do now.
    pub fn upsert(
        &mut self,
        id: &str,
        public_key: &str,
        caps: Capabilities,
        now: i64,
    ) -> Result<Upsert, String> {
        caps.validate()?;
        if let Some(r) = self.records.get_mut(id) {
            r.capabilities = caps;
            r.last_seen_ms = now;
            r.heartbeats += 1;
            return Ok(Upsert::Renewed);
        }
        self.forget_expired(now);
        if self.records.len() >= self.max {
            return Err(format!(
                "the registry holds its maximum of {} instances",
                self.max
            ));
        }
        self.records.insert(
            id.to_string(),
            InstanceRecord {
                instance_id: id.to_string(),
                public_key: public_key.to_string(),
                capabilities: caps,
                first_seen_ms: now,
                last_seen_ms: now,
                heartbeats: 0,
            },
        );
        Ok(Upsert::Registered)
    }

    pub fn deregister(&mut self, id: &str) -> bool {
        self.records.remove(id).is_some()
    }

    pub fn get(&self, id: &str) -> Option<&InstanceRecord> {
        self.records.get(id)
    }

    pub fn is_live(&self, id: &str, now: i64) -> bool {
        self.records
            .get(id)
            .is_some_and(|r| now - r.last_seen_ms < self.ttl_ms)
    }

    /// Matching instances, freshest first.
    pub fn query(&self, q: &Query, now: i64) -> Vec<Found> {
        let mut out: Vec<Found> = self
            .records
            .values()
            .filter(|r| q.matches(r))
            .map(|r| Found {
                live: now - r.last_seen_ms < self.ttl_ms,
                age_secs: (now - r.last_seen_ms).max(0) / 1000,
                record: r.clone(),
            })
            .filter(|f| f.live || q.include_stale)
            .collect();
        out.sort_by(|a, b| {
            b.record
                .last_seen_ms
                .cmp(&a.record.last_seen_ms)
                .then_with(|| a.record.instance_id.cmp(&b.record.instance_id))
        });
        out
    }

    /// Drop records long past their lease. Returns how many went.
    pub fn forget_expired(&mut self, now: i64) -> usize {
        let horizon = now - FORGET_AFTER_TTLS * self.ttl_ms;
        let before = self.records.len();
        self.records.retain(|_, r| r.last_seen_ms >= horizon);
        before - self.records.len()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn caps(kind: RuntimeKind) -> Capabilities {
        Capabilities {
            runtime_kind: kind,
            version: "noetl-cli/6.1.0".into(),
            name: None,
            site: Some("home".into()),
            labels: BTreeMap::from([("arch".into(), "arm64".into())]),
            tool_kinds: vec!["shell".into(), "http".into()],
            playbooks: vec!["edge/*".into()],
            pools: vec![],
            reach: Reach::Inbox,
            endpoint: None,
            accepts_tasks: true,
            known_peers: vec![],
        }
    }

    const A: &str = "ni-0000000000000000000000000000000a";
    const B: &str = "ni-0000000000000000000000000000000b";

    #[test]
    fn registration_is_the_heartbeat_and_the_lease_lapses() {
        let mut r = Registry::new(1000, 10);
        assert_eq!(
            r.upsert(A, "pk", caps(RuntimeKind::Daemon), 0).unwrap(),
            Upsert::Registered
        );
        assert_eq!(
            r.upsert(A, "pk", caps(RuntimeKind::Daemon), 500).unwrap(),
            Upsert::Renewed
        );
        assert!(r.is_live(A, 1499));
        assert!(!r.is_live(A, 1500));
        assert!(r.query(&Query::default(), 1500).is_empty());
        let q = Query {
            include_stale: true,
            ..Query::default()
        };
        assert_eq!(r.query(&q, 1500).len(), 1);
        assert_eq!(r.forget_expired(500 + 10_001), 1);
    }

    #[test]
    fn every_filter_must_match() {
        let mut r = Registry::new(60_000, 10);
        r.upsert(A, "pk", caps(RuntimeKind::Daemon), 0).unwrap();
        let mut c = caps(RuntimeKind::Distributed);
        c.site = Some("gke".into());
        c.pools = vec!["shared".into()];
        c.labels.clear();
        c.tool_kinds = vec!["postgres".into(), "shell".into()];
        c.playbooks = vec!["*".into()];
        r.upsert(B, "pk", c, 1).unwrap();

        let ids = |q: Query| {
            r.query(&q, 2)
                .into_iter()
                .map(|f| f.record.instance_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(Query::default()), vec![B, A], "freshest first");
        assert_eq!(
            ids(Query {
                tool_kind: vec!["shell".into()],
                ..Default::default()
            }),
            vec![B, A]
        );
        assert_eq!(
            ids(Query {
                tool_kind: vec!["shell".into(), "postgres".into()],
                ..Default::default()
            }),
            vec![B]
        );
        assert_eq!(
            ids(Query {
                pool: Some("shared".into()),
                ..Default::default()
            }),
            vec![B]
        );
        assert_eq!(
            ids(Query {
                site: Some("home".into()),
                ..Default::default()
            }),
            vec![A]
        );
        assert_eq!(
            ids(Query {
                label: vec!["arch=arm64".into()],
                ..Default::default()
            }),
            vec![A]
        );
        assert_eq!(
            ids(Query {
                label: vec!["arch=x86".into()],
                ..Default::default()
            }),
            Vec::<String>::new()
        );
        assert_eq!(
            ids(Query {
                playbook: Some("edge/probe".into()),
                ..Default::default()
            }),
            vec![B, A]
        );
        assert_eq!(
            ids(Query {
                playbook: Some("core/x".into()),
                ..Default::default()
            }),
            vec![B]
        );
        assert_eq!(
            ids(Query {
                runtime_kind: Some(RuntimeKind::Daemon),
                ..Default::default()
            }),
            vec![A]
        );
    }

    #[test]
    fn capabilities_are_bounded_and_carry_no_free_form_material() {
        let mut c = caps(RuntimeKind::Cli);
        c.tool_kinds = vec!["x".repeat(MAX_STR + 1)];
        assert!(c.validate().is_err());
        let mut c = caps(RuntimeKind::Cli);
        c.known_peers = vec!["not-an-id".into()];
        assert!(c.validate().is_err());
        let mut c = caps(RuntimeKind::Cli);
        c.endpoint = Some("https://user:pw@example.com".into());
        assert!(c.validate().is_err());
        // Unknown fields are refused rather than stored: there is no catch-all a secret could
        // ride in on.
        let err = serde_json::from_value::<Capabilities>(serde_json::json!({
            "runtime_kind": "cli", "version": "v", "api_key": "x"
        }));
        assert!(err.is_err());
    }

    #[test]
    fn the_registry_is_capped() {
        let mut r = Registry::new(60_000, 1);
        r.upsert(A, "pk", caps(RuntimeKind::Cli), 0).unwrap();
        assert!(r.upsert(B, "pk", caps(RuntimeKind::Cli), 0).is_err());
        // A renewal is never refused by the cap.
        r.upsert(A, "pk", caps(RuntimeKind::Cli), 1).unwrap();
    }

    #[test]
    fn globs() {
        assert!(glob_match("*", "a/b"));
        assert!(glob_match("edge/*", "edge/probe"));
        assert!(glob_match("edge/*/run", "edge/x/y/run"));
        assert!(!glob_match("edge/*", "core/probe"));
        assert!(glob_match("a", "a"));
        assert!(!glob_match("a*b*c", "ab"));
        assert!(glob_match("a*b*c", "aXbYc"));
    }
}
