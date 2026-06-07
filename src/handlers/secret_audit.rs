//! Secret-audit query endpoint (Secrets Wallet Phase 7b.2,
//! [`noetl/ai-meta#61`](https://github.com/noetl/ai-meta/issues/61)).
//!
//! `GET /api/internal/secret-audit?credential=...&execution_id=...&from=...&to=...&limit=...`.
//! Internal-only — same `/api/internal/*` gating as the other
//! peer-only endpoints.  Returns up to
//! [`crate::db::queries::secret_audit::QUERY_HARD_CAP`] rows
//! regardless of what the caller asks for, ordered newest-first.

use axum::{
    extract::{Query, State},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::queries::secret_audit::{self as queries, AuditQuery};
use crate::db::DbPool;
use crate::error::AppResult;
use crate::services::secret_audit::AuditEvent;

/// State for the audit-query endpoint — pool only; the query layer is
/// stateless.
#[derive(Clone)]
pub struct SecretAuditDeps {
    pub pool: DbPool,
}

/// Querystring shape.  All fields optional; the query SQL applies the
/// supplied filters.
#[derive(Debug, Deserialize, Default)]
pub struct AuditQueryParams {
    pub credential: Option<String>,
    pub execution_id: Option<i64>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AuditQueryResponse {
    pub rows: Vec<AuditEvent>,
}

/// `GET /api/internal/secret-audit`.
pub async fn query(
    State(deps): State<SecretAuditDeps>,
    Query(params): Query<AuditQueryParams>,
) -> AppResult<Json<AuditQueryResponse>> {
    let span = tracing::info_span!(
        "secret_audit.query",
        credential = params.credential.as_deref(),
        execution_id = params.execution_id,
        limit = params.limit,
    );
    let _g = span.enter();
    let rows = queries::query(
        &deps.pool,
        AuditQuery {
            credential: params.credential,
            execution_id: params.execution_id,
            from: params.from,
            to: params.to,
            limit: params.limit,
        },
    )
    .await?;
    Ok(Json(AuditQueryResponse { rows }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_params_round_trip_via_json() {
        // Lock against drift in the public field names — `axum`'s
        // querystring decoder uses the same serde derive surface.
        let v: AuditQueryParams = serde_json::from_str(
            r#"{"credential":"duffel_token","execution_id":42,"limit":50}"#,
        )
        .unwrap();
        assert_eq!(v.credential.as_deref(), Some("duffel_token"));
        assert_eq!(v.execution_id, Some(42));
        assert_eq!(v.limit, Some(50));
    }

    #[test]
    fn empty_object_decodes_to_all_none() {
        let v: AuditQueryParams = serde_json::from_str("{}").unwrap();
        assert!(v.credential.is_none());
        assert!(v.execution_id.is_none());
        assert!(v.from.is_none());
        assert!(v.to.is_none());
        assert!(v.limit.is_none());
    }
}
