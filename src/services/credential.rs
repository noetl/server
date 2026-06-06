//! Credential service for managing encrypted credentials.
//!
//! Credentials are stored under **envelope encryption** (Secrets Wallet
//! Phase 1, noetl/ai-meta#61): each record's data is sealed with a per-record
//! DEK that is wrapped by the KEK. The on-storage value is the self-describing
//! envelope JSON (see `crypto::envelope`). Forward-only — there is no legacy
//! single-master-key path; a pre-wallet record must be re-registered.

use std::collections::HashMap;

use chrono::Utc;

use crate::crypto::EnvelopeCipher;
use crate::db::DbPool;
use crate::db::models::{
    CredentialCreateRequest, CredentialEntry, CredentialFilter, CredentialListResponse,
    CredentialResponse, KeychainSetRequest,
};
use crate::db::queries::catalog as catalog_queries;
use crate::db::queries::credential as queries;
use crate::error::{AppError, AppResult};
use crate::playbook::types::Playbook;
use crate::secrets::{build_secret_provider, dynamic, resolve_keychain_entry_with_meta};
use crate::services::keychain::KeychainService;

/// TTL (seconds) for a keychain-resolved secret cached after a provider fetch.
/// Execution-scoped, so it's also cleaned up when the execution ends; the TTL
/// just bounds staleness if the underlying secret rotates mid-run.
const KEYCHAIN_CACHE_TTL_SECS: i64 = 600;

/// Cache scope for resolved keychain secrets: `local` keys the entry by
/// `{alias}:{catalog_id}:{execution_id}` (see `queries::keychain::build_cache_key`).
const KEYCHAIN_CACHE_SCOPE: &str = "local";

/// Service for credential operations.
#[derive(Clone)]
pub struct CredentialService {
    pool: DbPool,
    cipher: EnvelopeCipher,
    keychain: KeychainService,
}

impl CredentialService {
    /// Create a new credential service.
    ///
    /// `cipher` is the wallet's [`EnvelopeCipher`], built once at startup over
    /// the configured KEK provider (`crypto::build_envelope_cipher`) — local
    /// master key in dev, GCP Cloud KMS in production. `keychain` is the
    /// execution-scoped cache for provider-resolved secrets (Phase 3c).
    pub fn new(pool: DbPool, cipher: EnvelopeCipher, keychain: KeychainService) -> Self {
        Self {
            pool,
            cipher,
            keychain,
        }
    }

    /// Create or update a credential.
    pub async fn create_or_update(
        &self,
        request: CredentialCreateRequest,
    ) -> AppResult<CredentialResponse> {
        // Envelope-seal the data into the self-describing storage string
        // (UTF-8 JSON) for the TEXT `data_encrypted` column.
        let encrypted_data = self.cipher.seal_json_to_storage(&request.data).await?;

        // Check if credential already exists
        if let Some(existing) = queries::get_credential_by_name(&self.pool, &request.name).await? {
            // Update existing credential
            queries::update_credential(
                &self.pool,
                existing.id,
                &request.credential_type,
                &encrypted_data,
                request.meta.as_ref(),
                request.tags.as_deref(),
                request.description.as_deref(),
            )
            .await?;

            // Fetch updated credential
            let updated = queries::get_credential_by_id(&self.pool, existing.id)
                .await?
                .ok_or_else(|| {
                    AppError::Internal("Failed to fetch updated credential".to_string())
                })?;

            return Ok(self.entry_to_response(updated, None));
        }

        // Create new credential
        let id = queries::insert_credential(
            &self.pool,
            &request.name,
            &request.credential_type,
            &encrypted_data,
            request.meta.as_ref(),
            request.tags.as_deref(),
            request.description.as_deref(),
        )
        .await?;

        // Fetch created credential
        let created = queries::get_credential_by_id(&self.pool, id)
            .await?
            .ok_or_else(|| AppError::Internal("Failed to fetch created credential".to_string()))?;

        Ok(self.entry_to_response(created, None))
    }

    /// Get a credential by identifier (ID or name).
    ///
    /// On a credential-store miss, when `execution_id` is supplied and
    /// `include_data` is set, the alias is resolved as a **keychain entry** of
    /// the execution's playbook (Secrets Wallet Phase 3b): a `provider:`-backed
    /// entry (e.g. `provider: gcp`) is fetched from its secret manager and
    /// returned. This is the `auth: "{{ alias }}"` path — the secret never
    /// becomes workflow step output.
    pub async fn get(
        &self,
        identifier: &str,
        include_data: bool,
        execution_id: Option<i64>,
    ) -> AppResult<CredentialResponse> {
        match self.find_credential(identifier).await {
            Ok(entry) => {
                let data = if include_data {
                    // `entry.data` is the self-describing envelope JSON from the
                    // TEXT column — unwrap the DEK + decrypt.
                    Some(self.cipher.open_storage_json(&entry.data).await?)
                } else {
                    None
                };
                Ok(self.entry_to_response(entry, data))
            }
            Err(AppError::NotFound(_)) if include_data => {
                if let Some(exec_id) = execution_id {
                    if let Some(data) = self.try_resolve_keychain(exec_id, identifier).await? {
                        return Ok(self.keychain_response(identifier, data));
                    }
                }
                Err(AppError::NotFound(format!(
                    "Credential '{}' not found",
                    identifier
                )))
            }
            Err(e) => Err(e),
        }
    }

    /// Resolve `alias` as a keychain entry of the execution's playbook.
    ///
    /// Returns `Ok(Some(data))` when `alias` is a `provider:`-backed keychain
    /// entry that resolved; `Ok(None)` when it is not a provider-backed entry
    /// (the caller surfaces the original not-found); `Err(_)` when the entry
    /// exists but its provider fetch failed.
    async fn try_resolve_keychain(
        &self,
        execution_id: i64,
        alias: &str,
    ) -> AppResult<Option<serde_json::Value>> {
        // execution_id → (catalog_id, workload) from the start event.
        let info: Option<(i64, Option<serde_json::Value>)> = sqlx::query_as(
            r#"
            SELECT catalog_id, context->'workload' as workload
            FROM noetl.event
            WHERE execution_id = $1
              AND event_type IN ('playbook.initialized', 'playbook_started')
            LIMIT 1
            "#,
        )
        .bind(execution_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some((catalog_id, workload)) = info else {
            return Ok(None);
        };

        // Cache read (Phase 3c): an earlier step in this execution may have
        // already resolved + cached this alias — skip the provider fetch.
        // Best-effort: a cache error degrades to a fresh resolution, it must
        // never fail the credential lookup.
        match self
            .keychain
            .get(catalog_id, alias, Some(execution_id), KEYCHAIN_CACHE_SCOPE)
            .await
        {
            Ok(c) if c.status == "found" => {
                if let Some(data) = c.data {
                    tracing::debug!(execution_id, alias, "keychain.cache_hit");
                    return Ok(Some(data));
                }
            }
            Ok(_) => {} // not_found / expired → resolve fresh
            Err(e) => {
                tracing::warn!(execution_id, alias, error = %e, "keychain.cache_read failed; resolving fresh")
            }
        }

        // catalog_id → playbook YAML → parse.
        let Some(entry) = catalog_queries::get_catalog_by_id(&self.pool, catalog_id).await? else {
            return Ok(None);
        };
        let playbook: Playbook = match serde_yaml::from_str(&entry.content) {
            Ok(pb) => pb,
            Err(e) => {
                tracing::warn!(execution_id, error = %e, "keychain resolve: playbook parse failed");
                return Ok(None);
            }
        };

        // Only provider-backed keychain entries resolve here.
        let Some(kc) = playbook.find_keychain(alias) else {
            return Ok(None);
        };
        let Some(provider_id) = kc.provider.as_deref() else {
            return Ok(None);
        };

        let workload_map: HashMap<String, serde_json::Value> = workload
            .as_ref()
            .and_then(|w| w.as_object())
            .map(|m| m.clone().into_iter().collect())
            .unwrap_or_default();

        let provider = build_secret_provider(provider_id)?;
        tracing::info!(
            execution_id,
            alias,
            provider = provider_id,
            "keychain.resolve"
        );
        let (data, expires_at) =
            resolve_keychain_entry_with_meta(kc, &workload_map, &*provider).await?;

        // Phase 6d — honour the issuer-reported expiry when one was
        // supplied.  The decision helper returns CacheFor(secs) for
        // normal-case secrets, and SkipCacheAlreadyExpired for tokens
        // whose deadline already passed (or sits inside the operator's
        // safety margin) — caching something already dead would force
        // the next worker fetch into a 401.  The Phase-3c cache write
        // is best-effort, so the skip path just logs and proceeds with
        // the live response.
        let now = Utc::now();
        let cache_decision = dynamic::effective_cache_ttl(
            expires_at,
            std::time::Duration::from_secs(KEYCHAIN_CACHE_TTL_SECS as u64),
            now,
        );
        if let Some(exp) = expires_at {
            let remaining = (exp - now).num_seconds().max(0) as f64;
            crate::metrics::record_secret_dynamic_ttl(remaining);
        }
        match cache_decision {
            dynamic::CacheDecision::CacheFor(ttl_secs) => {
                let set_req = KeychainSetRequest {
                    data: data.clone(),
                    scope_type: KEYCHAIN_CACHE_SCOPE.to_string(),
                    execution_id: Some(execution_id),
                    expires_at,
                    expires_in: Some(ttl_secs as i64),
                    auto_renew: false,
                    renew_config: None,
                };
                if let Err(e) = self.keychain.set(catalog_id, alias, set_req).await {
                    tracing::warn!(execution_id, alias, error = %e, "keychain.cache_write failed");
                }
            }
            dynamic::CacheDecision::SkipCacheAlreadyExpired => {
                tracing::warn!(
                    execution_id,
                    alias,
                    "keychain.cache_skip: issuer expires_at already in the past or within safety margin"
                );
                crate::metrics::record_secret_cache_skip("already_expired");
            }
        }

        Ok(Some(data))
    }

    /// Build a response for a credential resolved from a keychain provider
    /// (not a stored credential — the value is fetched live + masked downstream).
    fn keychain_response(&self, alias: &str, data: serde_json::Value) -> CredentialResponse {
        let now = Utc::now();
        CredentialResponse {
            id: "0".to_string(),
            name: alias.to_string(),
            credential_type: "keychain".to_string(),
            meta: None,
            tags: None,
            description: Some("resolved from keychain provider".to_string()),
            data: Some(data),
            created_at: now,
            updated_at: now,
        }
    }

    /// List credentials with optional filtering.
    pub async fn list(
        &self,
        credential_type: Option<&str>,
        search: Option<&str>,
    ) -> AppResult<CredentialListResponse> {
        let entries = queries::list_credentials(&self.pool, credential_type, search).await?;

        let items: Vec<CredentialResponse> = entries
            .into_iter()
            .map(|e| self.entry_to_response(e, None))
            .collect();

        let filter = if credential_type.is_some() || search.is_some() {
            Some(CredentialFilter {
                credential_type: credential_type.map(|s| s.to_string()),
                q: search.map(|s| s.to_string()),
            })
        } else {
            None
        };

        Ok(CredentialListResponse { items, filter })
    }

    /// Delete a credential by identifier.
    pub async fn delete(&self, identifier: &str) -> AppResult<String> {
        // Find the credential first to get the ID
        let entry = self.find_credential(identifier).await?;
        let id = entry.id;

        // Delete by ID
        let deleted = queries::delete_credential_by_id(&self.pool, id).await?;

        if deleted {
            Ok(id.to_string())
        } else {
            Err(AppError::Internal(
                "Failed to delete credential".to_string(),
            ))
        }
    }

    /// Find a credential by identifier (ID or name).
    async fn find_credential(&self, identifier: &str) -> AppResult<CredentialEntry> {
        // Try to parse as ID first
        if let Ok(id) = identifier.parse::<i64>() {
            if let Some(entry) = queries::get_credential_by_id(&self.pool, id).await? {
                return Ok(entry);
            }
        }

        // Try to find by name
        queries::get_credential_by_name(&self.pool, identifier)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Credential '{}' not found", identifier)))
    }

    /// Convert a credential entry to a response.
    fn entry_to_response(
        &self,
        entry: CredentialEntry,
        data: Option<serde_json::Value>,
    ) -> CredentialResponse {
        CredentialResponse {
            id: entry.id.to_string(),
            name: entry.name,
            credential_type: entry.credential_type,
            meta: entry.meta,
            tags: entry.tags,
            description: entry.description,
            data,
            created_at: entry.created_at,
            updated_at: entry.updated_at,
        }
    }
}
