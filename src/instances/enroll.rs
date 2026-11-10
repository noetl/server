//! **Enrollment tokens**: how an instance becomes trusted without an operator editing
//! `NOETL_INSTANCE_TRUSTED_IDS` and restarting the server.
//!
//! On with `NOETL_INSTANCE_TRUST=enroll`. The static allowlist still applies; enrolled ids are
//! trusted in addition to it.
//!
//! 1. An operator mints a token at `POST /api/runtime/instances/enrollments` (behind the
//!    internal-token gate): `{label, ttl_secs?, uses?}` → `{token, label, expires_ms, uses}`.
//!    The token is shown once. The server keeps only its SHA-256.
//! 2. The new instance calls `POST /api/instances/enroll` with a signed envelope (purpose
//!    `enroll`) whose body is `{token}`. Every gate check applies except trust: the signature,
//!    audience, clock skew and nonce. So a token cannot be replayed by someone who saw the
//!    request, and it binds to the key that signed it.
//! 3. The token is spent (one use, unless minted with more), and the signer's id joins the
//!    trusted set. The instance then registers as usual.
//!
//! An operator revokes an id with `DELETE /api/runtime/instances/enrolled/{id}`; its next
//! request is refused, and its registration lapses within one TTL.
//!
//! With `NOETL_INSTANCE_STORE=postgres`, tokens (hashes) and enrolled ids live in
//! `noetl.instance_enroll_tokens` / `noetl.instance_enrolled` and survive restarts. Spending a
//! token there is a single conditional `UPDATE … RETURNING`, so two racing redemptions of a
//! one-use token cannot both succeed. Without the store they are in memory, and a restart
//! forgets them (the instances must enroll again).
//!
//! An invalid, expired or spent token gets the same answer, so the route does not reveal
//! which tokens exist. Tokens are 256-bit random values; guessing one is not a strategy.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::db::DbPool;

pub const DEFAULT_TTL_SECS: i64 = 60 * 60;
pub const MAX_TTL_SECS: i64 = 7 * 24 * 60 * 60;
pub const MAX_USES: i64 = 100;
pub const MAX_LABEL: usize = 128;

pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn new_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!("nie-{}", hex::encode(b))
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Minted {
    pub token: String,
    pub label: String,
    pub expires_ms: i64,
    pub uses: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Enrolled {
    pub instance_id: String,
    pub label: String,
    pub enrolled_ms: i64,
}

#[async_trait]
pub trait EnrollStore: Send + Sync {
    async fn save_token(
        &self,
        hash: &str,
        label: &str,
        expires_ms: i64,
        uses: i64,
    ) -> Result<(), String>;
    /// Spend one use. `Ok(Some(label))` when the token was valid, unexpired and unspent.
    async fn spend(&self, hash: &str, now_ms: i64) -> Result<Option<String>, String>;
    async fn add(&self, e: &Enrolled) -> Result<(), String>;
    async fn remove(&self, instance_id: &str) -> Result<bool, String>;
    async fn list(&self) -> Result<Vec<Enrolled>, String>;
}

/// Mint a token, store its hash, return it (the only time it is visible).
pub async fn mint(
    store: &dyn EnrollStore,
    label: &str,
    ttl_secs: Option<i64>,
    uses: Option<i64>,
    now_ms: i64,
) -> Result<Minted, String> {
    let label = label.trim();
    if label.is_empty() || label.len() > MAX_LABEL {
        return Err(format!("label must be 1..={MAX_LABEL} characters"));
    }
    let ttl = ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
    if !(60..=MAX_TTL_SECS).contains(&ttl) {
        return Err(format!("ttl_secs must be 60..={MAX_TTL_SECS}"));
    }
    let uses = uses.unwrap_or(1);
    if !(1..=MAX_USES).contains(&uses) {
        return Err(format!("uses must be 1..={MAX_USES}"));
    }
    let token = new_token();
    let expires_ms = now_ms + ttl * 1000;
    store
        .save_token(&token_hash(&token), label, expires_ms, uses)
        .await?;
    Ok(Minted {
        token,
        label: label.to_string(),
        expires_ms,
        uses,
    })
}

struct Token {
    label: String,
    expires_ms: i64,
    uses_left: i64,
}

/// In-memory enrollment state (no store).
#[derive(Default)]
pub struct MemEnroll {
    tokens: Mutex<HashMap<String, Token>>,
    enrolled: Mutex<HashMap<String, Enrolled>>,
}

#[async_trait]
impl EnrollStore for MemEnroll {
    async fn save_token(
        &self,
        hash: &str,
        label: &str,
        expires_ms: i64,
        uses: i64,
    ) -> Result<(), String> {
        let mut t = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        if t.len() >= 10_000 {
            return Err("too many outstanding enrollment tokens".into());
        }
        t.insert(
            hash.to_string(),
            Token {
                label: label.to_string(),
                expires_ms,
                uses_left: uses,
            },
        );
        Ok(())
    }

    async fn spend(&self, hash: &str, now_ms: i64) -> Result<Option<String>, String> {
        let mut t = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        t.retain(|_, v| v.expires_ms > now_ms && v.uses_left > 0);
        let Some(tok) = t.get_mut(hash) else {
            return Ok(None);
        };
        tok.uses_left -= 1;
        let label = tok.label.clone();
        if tok.uses_left == 0 {
            t.remove(hash);
        }
        Ok(Some(label))
    }

    async fn add(&self, e: &Enrolled) -> Result<(), String> {
        self.enrolled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(e.instance_id.clone(), e.clone());
        Ok(())
    }

    async fn remove(&self, instance_id: &str) -> Result<bool, String> {
        Ok(self
            .enrolled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(instance_id)
            .is_some())
    }

    async fn list(&self) -> Result<Vec<Enrolled>, String> {
        let mut v: Vec<Enrolled> = self
            .enrolled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect();
        v.sort_by_key(|e| e.enrolled_ms);
        Ok(v)
    }
}

/// Postgres enrollment state (`NOETL_INSTANCE_STORE=postgres`).
pub struct PgEnroll {
    pool: DbPool,
}

impl PgEnroll {
    pub async fn new(pool: DbPool) -> Result<Self, String> {
        for ddl in [
            r#"CREATE TABLE IF NOT EXISTS noetl.instance_enroll_tokens (
                   token_sha256 TEXT PRIMARY KEY,
                   label        TEXT NOT NULL,
                   expires_ms   BIGINT NOT NULL,
                   uses_left    BIGINT NOT NULL,
                   created_at   TIMESTAMPTZ NOT NULL DEFAULT now())"#,
            r#"CREATE TABLE IF NOT EXISTS noetl.instance_enrolled (
                   instance_id  TEXT PRIMARY KEY,
                   label        TEXT NOT NULL,
                   enrolled_ms  BIGINT NOT NULL)"#,
        ] {
            sqlx::query(ddl)
                .execute(&pool)
                .await
                .map_err(|e| format!("prepare enrollment tables: {e}"))?;
        }
        Ok(PgEnroll { pool })
    }
}

#[async_trait]
impl EnrollStore for PgEnroll {
    async fn save_token(
        &self,
        hash: &str,
        label: &str,
        expires_ms: i64,
        uses: i64,
    ) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO noetl.instance_enroll_tokens (token_sha256, label, expires_ms, uses_left) VALUES ($1, $2, $3, $4)",
        )
        .bind(hash)
        .bind(label)
        .bind(expires_ms)
        .bind(uses)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn spend(&self, hash: &str, now_ms: i64) -> Result<Option<String>, String> {
        let row: Option<(String,)> = sqlx::query_as(
            r#"UPDATE noetl.instance_enroll_tokens
                  SET uses_left = uses_left - 1
                WHERE token_sha256 = $1 AND uses_left > 0 AND expires_ms > $2
            RETURNING label"#,
        )
        .bind(hash)
        .bind(now_ms)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        // Housekeeping: spent and expired tokens are useless; drop them.
        let _ = sqlx::query(
            "DELETE FROM noetl.instance_enroll_tokens WHERE uses_left <= 0 OR expires_ms <= $1",
        )
        .bind(now_ms)
        .execute(&self.pool)
        .await;
        Ok(row.map(|(l,)| l))
    }

    async fn add(&self, e: &Enrolled) -> Result<(), String> {
        sqlx::query(
            r#"INSERT INTO noetl.instance_enrolled (instance_id, label, enrolled_ms) VALUES ($1, $2, $3)
               ON CONFLICT (instance_id) DO UPDATE SET label = EXCLUDED.label, enrolled_ms = EXCLUDED.enrolled_ms"#,
        )
        .bind(&e.instance_id)
        .bind(&e.label)
        .bind(e.enrolled_ms)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn remove(&self, instance_id: &str) -> Result<bool, String> {
        let r = sqlx::query("DELETE FROM noetl.instance_enrolled WHERE instance_id = $1")
            .bind(instance_id)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(r.rows_affected() > 0)
    }

    async fn list(&self) -> Result<Vec<Enrolled>, String> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT instance_id, label, enrolled_ms FROM noetl.instance_enrolled ORDER BY enrolled_ms",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|(instance_id, label, enrolled_ms)| Enrolled {
                instance_id,
                label,
                enrolled_ms,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_token_is_single_use_expires_and_is_stored_hashed() {
        let s = MemEnroll::default();
        let m = mint(&s, "edge-1", Some(60), None, 0).await.unwrap();
        assert!(m.token.starts_with("nie-") && m.token.len() == 68);
        assert!(
            s.tokens.lock().unwrap().keys().all(|k| k != &m.token),
            "only the hash is kept"
        );
        assert_eq!(
            s.spend(&token_hash(&m.token), 1).await.unwrap().as_deref(),
            Some("edge-1")
        );
        assert_eq!(
            s.spend(&token_hash(&m.token), 2).await.unwrap(),
            None,
            "spent"
        );
        let m2 = mint(&s, "edge-2", Some(60), Some(2), 0).await.unwrap();
        assert!(s.spend(&token_hash(&m2.token), 1).await.unwrap().is_some());
        assert_eq!(
            s.spend(&token_hash(&m2.token), 60_000).await.unwrap(),
            None,
            "expired"
        );
        assert_eq!(s.spend(&token_hash("nie-nope"), 1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn mint_bounds() {
        let s = MemEnroll::default();
        assert!(mint(&s, "", None, None, 0).await.is_err());
        assert!(mint(&s, "x", Some(10), None, 0).await.is_err());
        assert!(mint(&s, "x", Some(MAX_TTL_SECS + 1), None, 0)
            .await
            .is_err());
        assert!(mint(&s, "x", None, Some(0), 0).await.is_err());
        assert!(mint(&s, "x", None, Some(MAX_USES + 1), 0).await.is_err());
    }
}
