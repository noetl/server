//! **The admission check** for a signed instance request: signature, then freshness,
//! audience, purpose, replay, and trust, in that order.
//!
//! The order matters. Signature first, so nothing below spends server state (the nonce
//! cache) on a request nobody signed. Trust last, so a refusal for an untrusted id still
//! proves the caller holds the key, and the log line is about a real instance.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::Value;

use super::identity::{self, SignedEnvelope};

/// Who may register. Default `allowlist`, with an empty list: nobody.
pub const TRUST_ENV: &str = "NOETL_INSTANCE_TRUST";
/// Comma-separated instance ids for `allowlist` trust. Ids are public key fingerprints, so
/// this is configuration, not a secret.
pub const TRUSTED_IDS_ENV: &str = "NOETL_INSTANCE_TRUSTED_IDS";
/// The audience every payload must name. Stops a request signed for one control plane being
/// replayed against another. Default `noetl`.
pub const AUDIENCE_ENV: &str = "NOETL_INSTANCE_AUDIENCE";
/// Allowed clock skew, seconds. Default 120.
pub const SKEW_ENV: &str = "NOETL_INSTANCE_SKEW_SECS";

/// Nonces remembered at once. Past this the gate refuses new requests rather than forgetting
/// a nonce still inside the window, which would reopen replay.
pub const MAX_NONCES: usize = 200_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// Any instance with a valid signature. For kind and local development only.
    Open,
    /// Only these ids.
    Allowlist(HashSet<String>),
}

impl Trust {
    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Self {
        match get(TRUST_ENV).as_deref().map(str::trim) {
            Some("open") => Trust::Open,
            _ => Trust::Allowlist(
                get(TRUSTED_IDS_ENV)
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|s| identity::is_instance_id(s))
                    .map(str::to_string)
                    .collect(),
            ),
        }
    }

    pub fn admits(&self, id: &str) -> bool {
        match self {
            Trust::Open => true,
            Trust::Allowlist(ids) => ids.contains(id),
        }
    }
}

/// The header every signed payload carries; `body` is the request proper.
#[derive(Debug, Deserialize)]
pub struct Header {
    pub v: u32,
    pub purpose: String,
    pub instance_id: String,
    pub audience: String,
    pub ts_ms: i64,
    pub nonce: String,
    #[serde(default)]
    pub body: Value,
}

/// An admitted request.
#[derive(Debug, Clone)]
pub struct Admitted {
    pub instance_id: String,
    pub public_key_b64: String,
    pub body: Value,
}

/// Why a request was refused, with the HTTP status it maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub reason: String,
}

fn refuse(status: u16, reason: impl Into<String>) -> Refusal {
    Refusal {
        status,
        reason: reason.into(),
    }
}

pub struct Gate {
    pub trust: Trust,
    pub audience: String,
    pub skew_ms: i64,
    nonces: Mutex<Nonces>,
}

#[derive(Default)]
struct Nonces {
    seen: HashMap<(String, String), i64>,
    order: VecDeque<(i64, String, String)>,
}

impl Gate {
    pub fn new(trust: Trust, audience: impl Into<String>, skew_ms: i64) -> Self {
        Gate {
            trust,
            audience: audience.into(),
            skew_ms,
            nonces: Mutex::new(Nonces::default()),
        }
    }

    pub fn from_env() -> Self {
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Self {
        let audience = get(AUDIENCE_ENV)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "noetl".to_string());
        let skew = get(SKEW_ENV)
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|s| (1..=3600).contains(s))
            .unwrap_or(120);
        Gate::new(Trust::from_env_with(&get), audience, skew * 1000)
    }

    /// Admit `env` for `purpose` at `now_ms`.
    pub fn admit(
        &self,
        env: &SignedEnvelope,
        purpose: &str,
        now_ms: i64,
    ) -> Result<Admitted, Refusal> {
        let signer = identity::verify(env).map_err(|e| refuse(401, e.to_string()))?;
        let h: Header = serde_json::from_str(&env.payload)
            .map_err(|e| refuse(400, format!("payload is not a request header: {e}")))?;
        if h.v != 1 {
            return Err(refuse(400, format!("unsupported payload version {}", h.v)));
        }
        if h.instance_id != signer {
            return Err(refuse(401, "instance_id does not match the signing key"));
        }
        if h.purpose != purpose {
            return Err(refuse(
                400,
                format!(
                    "payload purpose `{}` sent to the `{purpose}` route",
                    h.purpose
                ),
            ));
        }
        if h.audience != self.audience {
            return Err(refuse(
                401,
                format!(
                    "payload audience `{}` is not this control plane",
                    h.audience
                ),
            ));
        }
        if (h.ts_ms - now_ms).abs() > self.skew_ms {
            return Err(refuse(
                401,
                "ts_ms is outside the allowed clock skew; check the instance clock",
            ));
        }
        if h.nonce.len() < 16 || h.nonce.len() > 128 {
            return Err(refuse(400, "nonce must be 16..=128 characters"));
        }
        self.remember(&signer, &h.nonce, now_ms)?;
        if !self.trust.admits(&signer) {
            return Err(refuse(
                403,
                format!("instance {signer} is not trusted here; add it to {TRUSTED_IDS_ENV}"),
            ));
        }
        Ok(Admitted {
            instance_id: signer,
            public_key_b64: env.public_key.clone(),
            body: h.body,
        })
    }

    fn remember(&self, id: &str, nonce: &str, now_ms: i64) -> Result<(), Refusal> {
        let mut n = self.nonces.lock().unwrap_or_else(|p| p.into_inner());
        // Forget nonces older than twice the window: a replay that old fails the ts check.
        let horizon = now_ms - 2 * self.skew_ms;
        while let Some((ts, _, _)) = n.order.front() {
            if *ts >= horizon {
                break;
            }
            let (_, i, nn) = n.order.pop_front().expect("front exists");
            n.seen.remove(&(i, nn));
        }
        let key = (id.to_string(), nonce.to_string());
        if n.seen.contains_key(&key) {
            return Err(refuse(409, "nonce already used (replay)"));
        }
        if n.seen.len() >= MAX_NONCES {
            return Err(refuse(503, "replay cache is full; retry shortly"));
        }
        n.seen.insert(key.clone(), now_ms);
        n.order.push_back((now_ms, key.0, key.1));
        Ok(())
    }
}

/// Build the payload text an instance signs. Used by tests and by the CLI mirror.
pub fn payload(
    purpose: &str,
    instance_id: &str,
    audience: &str,
    ts_ms: i64,
    nonce: &str,
    body: Value,
) -> String {
    serde_json::json!({
        "v": 1,
        "purpose": purpose,
        "instance_id": instance_id,
        "audience": audience,
        "ts_ms": ts_ms,
        "nonce": nonce,
        "body": body,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::super::identity::InstanceKey;
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_762_000_000_000;

    fn signed(k: &InstanceKey, purpose: &str, ts: i64, nonce: &str) -> SignedEnvelope {
        k.sign(payload(
            purpose,
            k.id(),
            "noetl",
            ts,
            nonce,
            json!({"x": 1}),
        ))
    }

    fn open() -> Gate {
        Gate::new(Trust::Open, "noetl", 120_000)
    }

    #[test]
    fn a_fresh_signed_request_is_admitted_with_its_body() {
        let k = InstanceKey::from_seed([1; 32]);
        let a = open()
            .admit(
                &signed(&k, "register", NOW, "nonce-000000000001"),
                "register",
                NOW,
            )
            .unwrap();
        assert_eq!(a.instance_id, k.id());
        assert_eq!(a.body, json!({"x": 1}));
    }

    #[test]
    fn replay_is_refused_and_so_is_a_stale_or_future_request() {
        let k = InstanceKey::from_seed([1; 32]);
        let g = open();
        let env = signed(&k, "register", NOW, "nonce-000000000001");
        g.admit(&env, "register", NOW).unwrap();
        assert_eq!(g.admit(&env, "register", NOW + 1).unwrap_err().status, 409);
        let stale = signed(&k, "register", NOW - 121_000, "nonce-000000000002");
        assert_eq!(g.admit(&stale, "register", NOW).unwrap_err().status, 401);
        let future = signed(&k, "register", NOW + 121_000, "nonce-000000000003");
        assert_eq!(g.admit(&future, "register", NOW).unwrap_err().status, 401);
    }

    #[test]
    fn purpose_audience_and_claimed_id_are_bound_by_the_signature() {
        let k = InstanceKey::from_seed([1; 32]);
        let other = InstanceKey::from_seed([2; 32]);
        let g = open();
        // A register payload cannot be spent on the inbox route.
        let env = signed(&k, "register", NOW, "nonce-000000000001");
        assert_eq!(g.admit(&env, "inbox.claim", NOW).unwrap_err().status, 400);
        // Signed for another control plane.
        let env = k.sign(payload(
            "register",
            k.id(),
            "elsewhere",
            NOW,
            "nonce-000000000002",
            json!({}),
        ));
        assert_eq!(g.admit(&env, "register", NOW).unwrap_err().status, 401);
        // Claims to be someone else.
        let env = k.sign(payload(
            "register",
            other.id(),
            "noetl",
            NOW,
            "nonce-000000000003",
            json!({}),
        ));
        assert_eq!(g.admit(&env, "register", NOW).unwrap_err().status, 401);
    }

    #[test]
    fn the_default_trust_admits_nobody_and_the_allowlist_admits_only_its_ids() {
        let k = InstanceKey::from_seed([1; 32]);
        let none = Gate::from_env_with(|_| None);
        let env = signed(&k, "register", NOW, "nonce-000000000001");
        assert_eq!(none.admit(&env, "register", NOW).unwrap_err().status, 403);

        let id = k.id().to_string();
        let listed = Gate::from_env_with(move |key| match key {
            TRUSTED_IDS_ENV => Some(format!("ni-bogus, {id}")),
            _ => None,
        });
        listed.admit(&env, "register", NOW).unwrap();
    }

    #[test]
    fn old_nonces_are_forgotten_only_once_their_window_is_gone() {
        let k = InstanceKey::from_seed([1; 32]);
        let g = open();
        g.admit(
            &signed(&k, "register", NOW, "nonce-000000000001"),
            "register",
            NOW,
        )
        .unwrap();
        g.admit(
            &signed(&k, "register", NOW + 300_000, "nonce-000000000002"),
            "register",
            NOW + 300_000,
        )
        .unwrap();
        assert_eq!(
            g.nonces.lock().unwrap().seen.len(),
            1,
            "the first nonce aged out"
        );
    }
}
