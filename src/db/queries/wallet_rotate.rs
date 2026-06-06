//! Wallet KEK rotation DB queries (Secrets Wallet Phase 7a.2,
//! [`noetl/ai-meta#61`](https://github.com/noetl/ai-meta/issues/61)).
//!
//! Two surfaces — both batched, both keyed by integer primary key so a
//! crash mid-rotate is recoverable by re-scanning from the last
//! committed id:
//!
//! - [`iter_credential_rows`] / [`iter_keychain_rows`] — pull a window of
//!   `(id, data_encrypted)` rows past a cursor `after_id`.
//! - [`update_credential_data`] / [`update_keychain_data`] — write the
//!   re-wrapped envelope string back to the same row.
//!
//! `key_status_*` queries return per-`kek_version` row counts.  The
//! version label is parsed out of the stored envelope JSON's `dek.kv`
//! field (Phase 1's `StoredDek` shape) — Postgres parses the JSON in
//! the query so we don't pay the round-trip to read every row.

use sqlx::Row;

use crate::db::DbPool;
use crate::error::AppResult;

/// One `(id, data_encrypted)` row from `noetl.credential` or
/// `noetl.keychain`.  The rotation job consumes a stream of these and
/// hands each through [`crate::crypto::EnvelopeCipher::rewrap_storage_string`].
#[derive(Debug, Clone)]
pub struct WalletRow {
    pub id: i64,
    pub data_encrypted: String,
}

/// Fetch up to `batch_size` `noetl.credential` rows with `id > after_id`,
/// ordered by `id ASC`.  Returns the rows in id order so the caller can
/// advance the cursor by tracking the largest id seen.
pub async fn iter_credential_rows(
    pool: &DbPool,
    after_id: i64,
    batch_size: i64,
) -> AppResult<Vec<WalletRow>> {
    let rows = sqlx::query(
        r#"
        SELECT id, data_encrypted
        FROM noetl.credential
        WHERE id > $1
        ORDER BY id ASC
        LIMIT $2
        "#,
    )
    .bind(after_id)
    .bind(batch_size)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| WalletRow {
            id: r.get::<i64, _>("id"),
            data_encrypted: r.get::<String, _>("data_encrypted"),
        })
        .collect())
}

/// Fetch up to `batch_size` `noetl.keychain` rows with `id > after_id`.
/// Same shape as [`iter_credential_rows`].
pub async fn iter_keychain_rows(
    pool: &DbPool,
    after_id: i64,
    batch_size: i64,
) -> AppResult<Vec<WalletRow>> {
    let rows = sqlx::query(
        r#"
        SELECT id, data_encrypted
        FROM noetl.keychain
        WHERE id > $1
        ORDER BY id ASC
        LIMIT $2
        "#,
    )
    .bind(after_id)
    .bind(batch_size)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| WalletRow {
            id: r.get::<i64, _>("id"),
            data_encrypted: r.get::<String, _>("data_encrypted"),
        })
        .collect())
}

/// Write the re-wrapped storage string back to `noetl.credential.data_encrypted`.
/// Single-row UPDATE; the caller wraps batches in a transaction.
pub async fn update_credential_data(
    pool: &DbPool,
    id: i64,
    new_data_encrypted: &str,
) -> AppResult<()> {
    sqlx::query(
        r#"
        UPDATE noetl.credential
        SET data_encrypted = $2
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(new_data_encrypted)
    .execute(pool)
    .await?;
    Ok(())
}

/// Write the re-wrapped storage string back to `noetl.keychain.data_encrypted`.
pub async fn update_keychain_data(
    pool: &DbPool,
    id: i64,
    new_data_encrypted: &str,
) -> AppResult<()> {
    sqlx::query(
        r#"
        UPDATE noetl.keychain
        SET data_encrypted = $2
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(new_data_encrypted)
    .execute(pool)
    .await?;
    Ok(())
}

/// Per-`kek_version` row counts on `noetl.credential`.  The version
/// string comes from the stored envelope's `dek.kv` field — Postgres
/// extracts the JSON path in the query.  Rows whose `data_encrypted`
/// isn't valid envelope JSON are bucketed under the label `"invalid"`
/// (rare; usually means a pre-Phase-1 legacy row that needs manual
/// re-registration).
pub async fn key_status_credential(pool: &DbPool) -> AppResult<Vec<(String, i64)>> {
    let rows = sqlx::query(
        r#"
        SELECT
          COALESCE(
            (data_encrypted::jsonb)->'dek'->>'kv',
            'invalid'
          ) AS kek_version,
          COUNT(*) AS n
        FROM noetl.credential
        GROUP BY kek_version
        ORDER BY kek_version
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("kek_version"),
                r.get::<i64, _>("n"),
            )
        })
        .collect())
}

/// Per-`kek_version` row counts on `noetl.keychain`.
pub async fn key_status_keychain(pool: &DbPool) -> AppResult<Vec<(String, i64)>> {
    let rows = sqlx::query(
        r#"
        SELECT
          COALESCE(
            (data_encrypted::jsonb)->'dek'->>'kv',
            'invalid'
          ) AS kek_version,
          COUNT(*) AS n
        FROM noetl.keychain
        GROUP BY kek_version
        ORDER BY kek_version
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("kek_version"),
                r.get::<i64, _>("n"),
            )
        })
        .collect())
}
