//! Execution management API handlers.
//!
//! Handles listing, status, cancellation, and finalization of executions.

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::services::execution::{ExecutionFilter, ExecutionService};

/// Query parameters for listing executions.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListExecutionsQuery {
    pub catalog_id: Option<i64>,
    pub path: Option<String>,
    pub status: Option<String>,
    pub limit: Option<i32>,
    pub offset: Option<i32>,
}

/// Response for cancellation check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancellationCheckResponse {
    pub execution_id: i64,
    pub is_cancelled: bool,
}

/// Request for finalizing an execution.
#[derive(Debug, Clone, Deserialize)]
pub struct FinalizeRequest {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response for finalization.
#[derive(Debug, Clone, Serialize)]
pub struct FinalizeResponse {
    pub execution_id: i64,
    pub status: String,
    pub message: String,
}

/// List executions.
///
/// GET /api/executions
pub async fn list(
    State(service): State<ExecutionService>,
    Query(query): Query<ListExecutionsQuery>,
) -> Result<axum::response::Response, AppError> {
    let filter = ExecutionFilter {
        catalog_id: query.catalog_id,
        path: query.path,
        status: query.status,
        limit: query.limit,
        offset: query.offset,
    };

    // noetl/ai-meta#255 — the cap has to be VISIBLE.  `?limit=300` used to return
    // exactly 100 rows with no error, no warning and no marker: a response
    // indistinguishable from "there are only 100".  Anyone paginating by "ask for
    // more than I expect and see whether I got fewer" got a wrong answer.
    let paging = crate::services::execution::resolve_paging(query.limit, query.offset)
        .map_err(AppError::BadRequest)?;
    let executions = service.list(&filter).await?;

    // Headers rather than an envelope: the body stays a bare JSON array, so every
    // existing caller — the SPA, the gateway, scripts — keeps working.  Changing
    // the shape to signal truncation would break them all to fix a reporting gap.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "x-noetl-limit-applied",
        paging.limit.to_string().parse().expect("ascii digits"),
    );
    if paging.capped {
        headers.insert(
            "x-noetl-limit-capped",
            "true".parse().expect("ascii literal"),
        );
        crate::metrics::record_executions_limit(true);
        tracing::info!(
            requested = paging.requested_limit,
            applied = paging.limit,
            "GET /api/executions limit capped (noetl/ai-meta#255)"
        );
    } else {
        crate::metrics::record_executions_limit(false);
    }
    // `has_more` is deliberately "the page came back full", not a COUNT(*): the
    // count would cost a second aggregate over the same window, and a full page is
    // exactly the condition under which a caller must ask again.  It can be a
    // false positive on an exact multiple, which is the safe direction — it says
    // "check again", never "you have everything" when you do not.
    let full_page = executions.len() as i32 >= paging.limit && paging.limit > 0;
    headers.insert(
        "x-noetl-has-more",
        if full_page { "true" } else { "false" }
            .parse()
            .expect("ascii literal"),
    );

    Ok((headers, Json(executions)).into_response())
}

/// Get execution details.
///
/// GET /api/executions/{execution_id}
pub async fn get(
    State(service): State<ExecutionService>,
    Path(execution_id): Path<i64>,
) -> Result<Json<crate::services::execution::ExecutionDetail>, AppError> {
    let execution = service.get(execution_id).await?;
    Ok(Json(execution))
}

/// Get execution status.
///
/// GET /api/executions/{execution_id}/status
pub async fn get_status(
    State(service): State<ExecutionService>,
    Path(execution_id): Path<i64>,
) -> Result<Json<crate::services::execution::ExecutionStatus>, AppError> {
    let status = service.get_status(execution_id).await?;
    Ok(Json(status))
}

/// Cancel an execution.
///
/// POST /api/executions/{execution_id}/cancel
pub async fn cancel(
    State(service): State<ExecutionService>,
    Path(execution_id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    service.cancel(execution_id).await?;
    Ok(Json(serde_json::json!({
        "execution_id": execution_id,
        "status": "cancelled",
        "message": "Execution cancellation requested"
    })))
}

/// Check if an execution is cancelled.
///
/// GET /api/executions/{execution_id}/cancellation-check
pub async fn cancellation_check(
    State(service): State<ExecutionService>,
    Path(execution_id): Path<i64>,
) -> Result<Json<CancellationCheckResponse>, AppError> {
    let is_cancelled = service.is_cancelled(execution_id).await?;
    Ok(Json(CancellationCheckResponse {
        execution_id,
        is_cancelled,
    }))
}

/// Finalize an execution.
///
/// POST /api/executions/{execution_id}/finalize
pub async fn finalize(
    State(service): State<ExecutionService>,
    Path(execution_id): Path<i64>,
    Json(request): Json<FinalizeRequest>,
) -> Result<Json<FinalizeResponse>, AppError> {
    service
        .finalize(execution_id, &request.status, request.error.as_deref())
        .await?;

    Ok(Json(FinalizeResponse {
        execution_id,
        status: request.status.clone(),
        message: format!("Execution finalized with status: {}", request.status),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_query_default() {
        let query = ListExecutionsQuery::default();
        assert!(query.catalog_id.is_none());
        assert!(query.limit.is_none());
    }

    #[test]
    fn test_cancellation_response_serialization() {
        let response = CancellationCheckResponse {
            execution_id: 12345,
            is_cancelled: true,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("12345"));
        assert!(json.contains("true"));
    }

    #[test]
    fn test_finalize_response_serialization() {
        let response = FinalizeResponse {
            execution_id: 12345,
            status: "COMPLETED".to_string(),
            message: "Execution finalized".to_string(),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("COMPLETED"));
    }
}
