//! Cell endpoint registry handler ([noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104) Phase C).
//!
//! `GET /api/internal/cells` — the server-served read-side cell map the
//! resolve-by-URN path consults to turn a derived `(region, cell, shard)` into a
//! concrete placement. See [`crate::services::cell_registry`] for the contract
//! and the fail-safe miss behavior (OQ6).

use axum::{extract::State, Json};

use crate::services::cell_registry::CellRegistry;

/// Injected registry (built once from env at startup).
#[derive(Clone)]
pub struct CellRegistryDeps {
    pub registry: CellRegistry,
}

/// `GET /api/internal/cells` — return the cell endpoint map.
pub async fn list_cells(State(deps): State<CellRegistryDeps>) -> Json<CellRegistry> {
    crate::metrics::record_cell_registry_request();
    Json(deps.registry.clone())
}
