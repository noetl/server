//! Cell endpoint registry ([noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104) Phase C, RFC §4.3).
//!
//! The derivable §7 physical key carries `region`/`cell`/`shard` segments. A
//! consumer that knows `(tenant, project, execution_id)` **derives** the home
//! shard with zero central lookup (`ResultCoordinates::shard_key`); the only
//! thing that needs a registry is turning a **cell** into a concrete
//! placement/bucket/endpoint. Phase B seeded that single-cell on the *write*
//! side from worker env (`CellSeed`); Phase C generalizes it to a
//! **server-served** map the *read* path consults, so a multi-cell grid can grow
//! without changing names.
//!
//! `GET /api/internal/cells` returns the map. Today it is a single-cell seed
//! (`default_cell`) derived from the same `NOETL_RESULT_CELL*` env the materializer
//! reads, so the read-side placement matches the write-side placement byte for
//! byte. Multi-cell entries and shard→cell routing are an additive follow-up.
//!
//! **Miss behavior (OQ6 — resolved fail-safe).** The registry never fails an
//! execution. If a consumer can't reach the registry, or a cell can't be
//! resolved, the resolve-by-URN read path falls back to the authoritative
//! `noetl.result_store` / inline result and increments a fallback metric — never
//! a hard failure, never silent data loss. The registry is an optimization seam,
//! not a single point of failure.

use serde::Serialize;

use crate::services::object_backend::ObjectBackend;

/// One cell's placement + physical object-store binding.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CellEntry {
    /// Cell id (the `cell=` §7 segment), e.g. `local-0`.
    pub cell: String,
    /// Deployment env (the `env=` §7 segment), e.g. `dev` / `prod`.
    pub env: String,
    /// Region (the `region=` §7 segment), e.g. `local` / `usw2`.
    pub region: String,
    /// Object-store provider backing this cell (`gcs` / `postgres`).
    pub provider: String,
    /// Bucket (empty for the Postgres backend).
    pub bucket: String,
    /// Object-store endpoint (empty for the Postgres backend).
    pub endpoint: String,
}

/// The cell endpoint registry — single-cell seed today, multi-cell ready.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CellRegistry {
    /// Shard space size; the §7 `shard=` segment is `shard_key % shard_count`.
    pub shard_count: u32,
    /// The cell every shard homes on in the single-cell seed.
    pub default_cell: String,
    /// Known cells (one today).
    pub cells: Vec<CellEntry>,
}

impl CellRegistry {
    /// Build from the same env the worker materializer's `CellSeed` reads, plus
    /// the object-store backend (for `provider`/`bucket`/`endpoint`), so the
    /// read-side placement matches the write-side §7 key exactly.
    pub fn from_env(backend: &ObjectBackend) -> Self {
        let env = std::env::var("NOETL_RESULT_CELL_ENV").unwrap_or_else(|_| "dev".to_string());
        let region =
            std::env::var("NOETL_RESULT_CELL_REGION").unwrap_or_else(|_| "local".to_string());
        let cell = std::env::var("NOETL_RESULT_CELL").unwrap_or_else(|_| "local-0".to_string());
        let shard_count = std::env::var("NOETL_RESULT_SHARD_COUNT")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(256)
            .max(1);
        let (provider, bucket, endpoint) = match backend {
            ObjectBackend::Postgres => ("postgres".to_string(), String::new(), String::new()),
            ObjectBackend::Gcs(_) => (
                "gcs".to_string(),
                std::env::var("NOETL_OBJECT_STORE_GCS_BUCKET").unwrap_or_default(),
                std::env::var("NOETL_OBJECT_STORE_GCS_ENDPOINT").unwrap_or_default(),
            ),
        };
        let entry = CellEntry {
            cell: cell.clone(),
            env,
            region,
            provider,
            bucket,
            endpoint,
        };
        CellRegistry {
            shard_count,
            default_cell: cell,
            cells: vec![entry],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Single sequential test (shared process env → parallel tests would race).
    #[test]
    fn from_env_seed_and_overrides() {
        let keys = [
            "NOETL_RESULT_CELL_ENV",
            "NOETL_RESULT_CELL_REGION",
            "NOETL_RESULT_CELL",
            "NOETL_RESULT_SHARD_COUNT",
        ];
        for k in keys {
            std::env::remove_var(k);
        }
        // Defaults mirror the worker `CellSeed::from_env` defaults exactly.
        let reg = CellRegistry::from_env(&ObjectBackend::Postgres);
        assert_eq!(reg.shard_count, 256);
        assert_eq!(reg.default_cell, "local-0");
        assert_eq!(reg.cells.len(), 1);
        let c = &reg.cells[0];
        assert_eq!(c.cell, "local-0");
        assert_eq!(c.env, "dev");
        assert_eq!(c.region, "local");
        assert_eq!(c.provider, "postgres");

        // Overrides flow through.
        std::env::set_var("NOETL_RESULT_CELL_ENV", "prod");
        std::env::set_var("NOETL_RESULT_CELL_REGION", "usw2");
        std::env::set_var("NOETL_RESULT_CELL", "usw2-a");
        std::env::set_var("NOETL_RESULT_SHARD_COUNT", "512");
        let reg = CellRegistry::from_env(&ObjectBackend::Postgres);
        assert_eq!(reg.shard_count, 512);
        assert_eq!(reg.default_cell, "usw2-a");
        assert_eq!(reg.cells[0].env, "prod");
        assert_eq!(reg.cells[0].region, "usw2");

        for k in keys {
            std::env::remove_var(k);
        }
    }
}
