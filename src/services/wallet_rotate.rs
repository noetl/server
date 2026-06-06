//! Wallet KEK rotation service (Secrets Wallet Phase 7a.2,
//! [`noetl/ai-meta#61`](https://github.com/noetl/ai-meta/issues/61)).
//!
//! Orchestrates [`crate::crypto::EnvelopeCipher::rewrap_storage_string`]
//! across the `noetl.credential` + `noetl.keychain` tables.  Batched
//! cursor-keyed scan + per-row UPDATE so a crash mid-rotate leaves the
//! tables in a consistent intermediate state: the next rotation pass
//! resumes from `id > max_id_processed_so_far` without re-touching
//! already-rewrapped rows (the `should_refresh`-style skip path inside
//! `rewrap_storage_string` short-circuits those).
//!
//! The endpoint surface (`POST /api/internal/wallet/rotate-kek` +
//! `GET /api/internal/wallet/key-status`) lives in
//! `crate::handlers::wallet_rotate`; this module is the pure-service
//! layer the handler delegates to.

use std::collections::BTreeMap;

use crate::crypto::envelope::RewrapOutcome;
use crate::crypto::EnvelopeCipher;
use crate::db::queries::wallet_rotate as queries;
use crate::db::DbPool;
use crate::error::AppResult;

/// Default batch size when the caller doesn't supply one.  Conservative —
/// 100 keeps the per-batch transaction short enough that a crash loses
/// at most 100 unwritten rewraps, while amortising the per-batch
/// overhead of opening the cursor.
const DEFAULT_BATCH_SIZE: i64 = 100;

/// Default cap on the number of batches a single rotate-kek call
/// processes.  1000 × 100 = 100k rows max per request; an operator who
/// needs more re-runs the call.  Prevents an accidentally-huge request
/// from monopolising the server.
const DEFAULT_MAX_BATCHES: i64 = 1000;

/// Per-table summary returned by [`rotate_table`] / the endpoint.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RotateSummary {
    /// Rows the scan returned.
    pub processed: u64,
    /// Rows that were re-wrapped under the current KEK version.
    pub rewrapped: u64,
    /// Rows already on the current KEK version (no KMS call).
    pub skipped: u64,
    /// Rows the rewrap primitive errored on.  Most common cause:
    /// `failed_unwrap` (KMS deleted the historical KEK version — needs
    /// operator intervention) or `parse_error` (legacy / corrupt
    /// `data_encrypted` value; re-register the secret).  Failures DO
    /// NOT abort the pass — the cursor advances past the failed row so
    /// later rows still get rewrapped.
    pub failed: u64,
    /// Largest `id` the scan saw.  Caller passes this back as
    /// `after_id` on the next call to resume past the boundary.
    pub last_id: i64,
}

/// Per-table summary returned by [`key_status_table`].
pub type KeyStatusSummary = BTreeMap<String, i64>;

/// Service wrapper.  Holds the wallet cipher (which exposes the current
/// KEK version + the rewrap primitive) and the DB pool the queries hit.
#[derive(Clone)]
pub struct WalletRotateService {
    pool: DbPool,
    cipher: EnvelopeCipher,
}

impl WalletRotateService {
    pub fn new(pool: DbPool, cipher: EnvelopeCipher) -> Self {
        Self { pool, cipher }
    }

    /// Rotate one table.  `table` ∈ `{credential, keychain}`.  Pulls
    /// batches via the matching query, runs each row's stored envelope
    /// through `rewrap_storage_string`, and UPDATEs in place.  Stops
    /// at `max_batches` or when a batch returns < `batch_size` rows
    /// (end-of-table sentinel).
    pub async fn rotate_table(
        &self,
        table: WalletTable,
        batch_size: Option<i64>,
        max_batches: Option<i64>,
    ) -> AppResult<RotateSummary> {
        let batch_size = batch_size.unwrap_or(DEFAULT_BATCH_SIZE).max(1);
        let max_batches = max_batches.unwrap_or(DEFAULT_MAX_BATCHES).max(1);
        let mut summary = RotateSummary::default();
        let mut after_id: i64 = 0;
        for _ in 0..max_batches {
            let rows = match table {
                WalletTable::Credential => {
                    queries::iter_credential_rows(&self.pool, after_id, batch_size).await?
                }
                WalletTable::Keychain => {
                    queries::iter_keychain_rows(&self.pool, after_id, batch_size).await?
                }
            };
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                summary.processed += 1;
                summary.last_id = row.id;
                match self.cipher.rewrap_storage_string(&row.data_encrypted).await {
                    Ok(RewrapOutcome::Skipped { .. }) => {
                        summary.skipped += 1;
                        crate::metrics::record_wallet_rotate(table.as_label(), "skipped");
                    }
                    Ok(RewrapOutcome::Rewrapped {
                        new_storage_string, ..
                    }) => {
                        let update_result = match table {
                            WalletTable::Credential => {
                                queries::update_credential_data(
                                    &self.pool,
                                    row.id,
                                    &new_storage_string,
                                )
                                .await
                            }
                            WalletTable::Keychain => {
                                queries::update_keychain_data(
                                    &self.pool,
                                    row.id,
                                    &new_storage_string,
                                )
                                .await
                            }
                        };
                        match update_result {
                            Ok(()) => {
                                summary.rewrapped += 1;
                                crate::metrics::record_wallet_rotate(
                                    table.as_label(),
                                    "rewrapped",
                                );
                            }
                            Err(e) => {
                                summary.failed += 1;
                                crate::metrics::record_wallet_rotate(
                                    table.as_label(),
                                    "failed_write",
                                );
                                tracing::warn!(
                                    table = %table.as_label(),
                                    id = row.id,
                                    error = %e,
                                    "wallet_rotate.update failed"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        // Bucket the failure by error category — the
                        // metric distinguishes between "KMS issue"
                        // (failed_unwrap / failed_wrap; alert-worthy)
                        // and "data issue" (parse_error; operator
                        // chases the row).
                        summary.failed += 1;
                        let status = classify_failure(&e);
                        crate::metrics::record_wallet_rotate(table.as_label(), status);
                        tracing::warn!(
                            table = %table.as_label(),
                            id = row.id,
                            status = %status,
                            error = %e,
                            "wallet_rotate.rewrap failed"
                        );
                    }
                }
            }
            after_id = summary.last_id;
            // Short batch = end of table.
            if (rows.len() as i64) < batch_size {
                break;
            }
        }
        Ok(summary)
    }

    /// Per-version row counts for `table`.
    pub async fn key_status_table(&self, table: WalletTable) -> AppResult<KeyStatusSummary> {
        let pairs = match table {
            WalletTable::Credential => queries::key_status_credential(&self.pool).await?,
            WalletTable::Keychain => queries::key_status_keychain(&self.pool).await?,
        };
        Ok(pairs.into_iter().collect())
    }
}

/// Which wallet-owned table to scan.
#[derive(Debug, Clone, Copy)]
pub enum WalletTable {
    Credential,
    Keychain,
}

impl WalletTable {
    pub fn as_label(self) -> &'static str {
        match self {
            WalletTable::Credential => "credential",
            WalletTable::Keychain => "keychain",
        }
    }
}

/// Heuristic mapping from an `AppError` produced by
/// `rewrap_storage_string` to the metric status label.  Hard-coded
/// substring matches against `to_string()` because the upstream errors
/// don't carry a structured kind — this is purely for the metric
/// breakdown (the underlying error message still goes to the log line).
fn classify_failure(err: &crate::error::AppError) -> &'static str {
    let s = err.to_string();
    if s.contains("not a wallet envelope") || s.contains("envelope") && s.contains("base64") {
        "parse_error"
    } else if s.contains("cannot unwrap") || s.contains("unwrap") {
        "failed_unwrap"
    } else if s.contains("wrap") {
        "failed_wrap"
    } else {
        "failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_table_labels() {
        assert_eq!(WalletTable::Credential.as_label(), "credential");
        assert_eq!(WalletTable::Keychain.as_label(), "keychain");
    }

    #[test]
    fn classify_failure_buckets() {
        use crate::error::AppError;
        // Parse error — the rewrap couldn't decode the storage string
        // at all (forward-only contract violated).
        assert_eq!(
            classify_failure(&AppError::Encryption(
                "not a wallet envelope record: bad input".to_string()
            )),
            "parse_error"
        );
        // Unwrap failure — KMS doesn't have the historical key version.
        assert_eq!(
            classify_failure(&AppError::Encryption(
                "LocalDevKms cannot unwrap a DEK from provider 'gcp-kms'".to_string()
            )),
            "failed_unwrap"
        );
        // Wrap failure — KMS reachability for the current version.
        assert_eq!(
            classify_failure(&AppError::Encryption("kms wrap call failed".to_string())),
            "failed_wrap"
        );
        // Anything else falls through to the generic bucket.
        assert_eq!(
            classify_failure(&AppError::Internal("unknown".to_string())),
            "failed"
        );
    }

    #[test]
    fn rotate_summary_defaults_zero() {
        let s = RotateSummary::default();
        assert_eq!(s.processed, 0);
        assert_eq!(s.rewrapped, 0);
        assert_eq!(s.skipped, 0);
        assert_eq!(s.failed, 0);
        assert_eq!(s.last_id, 0);
    }
}
