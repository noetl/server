//! Catalog database model.
//!
//! The catalog stores registered playbooks, tools, and other resources
//! with version control support.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// Catalog entry representing a registered resource.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CatalogEntry {
    /// Unique catalog ID (snowflake-like ID)
    pub id: i64,

    /// Resource path (e.g., "tests/fixtures/playbooks/hello_world")
    pub path: String,

    /// Resource kind (e.g., "Playbook", "Tool", "Model")
    pub kind: String,

    /// Version number (auto-incremented per path).
    ///
    /// `i16` matches the Postgres `smallint` column.  Older Rust
    /// revisions used `i32`, which caused sqlx decode failures
    /// against the real schema — same drift as v2.1.3 (credentials
    /// data column) and v2.1.4 (executions timestamps).
    pub version: i16,

    /// Raw YAML content.
    ///
    /// ⚠⚠ `Option`, because the listing path deliberately selects
    /// `NULL::text AS content` when bodies are not requested (noetl/server#436).
    /// This was `String`, and that shipped a **500 on every `/api/catalog/list`
    /// call in prod**: sqlx refused the row with
    /// `decoding column "content": unexpected null; try decoding as an Option`.
    ///
    /// The tests that were supposed to catch it checked the generated SQL
    /// *string* and measured raw-SQL byte counts — neither ever decoded a row
    /// through `query_as::<_, CatalogEntry>`, which is the only place the
    /// mismatch exists. Same row-type/schema drift the `version: i16` comment
    /// above records twice already.
    ///
    /// Every read that genuinely needs the body still selects the real column,
    /// so it arrives as `Some`.
    pub content: Option<String>,

    /// Parsed layout/structure (JSON)
    #[sqlx(default)]
    pub layout: Option<serde_json::Value>,

    /// Parsed payload/workload (JSON)
    #[sqlx(default)]
    pub payload: Option<serde_json::Value>,

    /// Additional metadata (JSON)
    #[sqlx(default)]
    pub meta: Option<serde_json::Value>,

    /// Creation timestamp
    pub created_at: DateTime<Utc>,
}

/// Request to register a new catalog resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogRegisterRequest {
    /// YAML content (plain text or base64 encoded)
    pub content: String,

    /// Resource type (default: "Playbook")
    #[serde(default = "default_resource_type")]
    pub resource_type: String,
}

fn default_resource_type() -> String {
    "Playbook".to_string()
}

/// Bulk registration — N items in one call.
///
/// Bulk-loading the catalog was previously a shell loop of single POSTs (see
/// `repos/ops/automation/development/validate-*.sh`), which is 2,518 round trips
/// for a full load and has no per-item outcome a caller can act on.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogRegisterBatchRequest {
    pub items: Vec<CatalogRegisterRequest>,
}

/// One item's outcome. **Partial failure is first-class**: a single bad YAML
/// yields an error at its index and the rest still register — the same posture
/// `execute_batch` takes, and the reason a bulk load does not become
/// all-or-nothing on one malformed file.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogRegisterBatchItem {
    pub index: usize,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogRegisterBatchResponse {
    pub count: usize,
    pub registered: usize,
    pub failed: usize,
    pub results: Vec<CatalogRegisterBatchItem>,
}

/// Response after registering a catalog resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogRegisterResponse {
    /// Operation status
    pub status: String,

    /// Status message
    pub message: String,

    /// Resource path
    pub path: String,

    /// Version number (Postgres `smallint`).
    pub version: i16,

    /// Catalog ID
    pub catalog_id: String,

    /// Resource kind
    pub kind: String,
}

/// Request to list catalog entries.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CatalogEntriesRequest {
    /// Filter by resource type
    #[serde(default)]
    pub resource_type: Option<String>,

    /// Include soft-deleted (archived) entries (noetl/ai-meta#237).
    ///
    /// Defaults to false: an archived entry is retired, so the ordinary listing
    /// hides it. Set true to see what has been retired — the view an operator
    /// needs before restoring something.
    #[serde(default)]
    pub include_archived: bool,

    /// Return only the newest version of each path (noetl/server#436).
    ///
    /// Defaults to false, preserving the historical shape. On prod this is
    /// 2539 entries -> 346: a caller that wants "the current catalog" is
    /// otherwise paying for every superseded version of every playbook.
    #[serde(default)]
    pub latest_only: bool,

    /// Include the heavy body fields `content` and `layout`.
    ///
    /// ⚠ Defaults to **false**, which is a change to what this endpoint
    /// returns. Measured against prod, those two fields are **97.4%** of the
    /// response — `content` 55.9%, `layout` 41.5% — against 1.2% for the
    /// identity fields most callers actually list by.
    ///
    /// `payload` and `meta` are always included: together they are 1.0%, and
    /// the GUI's playbook search reads `payload.metadata.{name,description}`.
    /// Dropping them to save 1% would break a live caller to no purpose.
    ///
    /// The keys are still emitted, as `null` — the pydantic `CatalogEntry` wire
    /// shape has no `exclude_none`, and omitting them surfaced as DIFF lines in
    /// the noetl/ai-meta#49 parity harness. Fetch a body with
    /// `POST /api/catalog/resource`, which is the endpoint for it.
    #[serde(default)]
    pub include_content: bool,

    /// Maximum entries to return. Absent means all of them.
    ///
    /// ⚠ Deliberately NOT defaulted to a bounded page. See
    /// `CatalogEntries::total` for why a row cap is the wrong instrument here.
    #[serde(default)]
    pub limit: Option<i32>,

    /// Entries to skip, for paging. Absent means 0.
    #[serde(default)]
    pub offset: Option<i32>,
}

/// Response containing list of catalog entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntries {
    /// List of catalog entries
    pub entries: Vec<CatalogEntryResponse>,

    /// How many entries matched the filters, BEFORE `limit`/`offset`.
    ///
    /// ⚠ This is the honesty field. A paged response that just returns rows
    /// cannot be distinguished by the caller from a complete one, so a client
    /// that asks for 50 and gets 50 has no way to know it is looking at a
    /// truncated catalog. `total` says what it did not get.
    ///
    /// ⚠ Why there is no default page size or hard cap here, unlike
    /// `/api/executions`: a row cap does not bound this response. Entry sizes
    /// are wildly uneven — the largest single prod entry is **509 KB**, so even
    /// a 100-row page can exceed 50 MB. What actually bounds it is
    /// `include_content`, which is 97.4% of the bytes and drops **no records**.
    /// Bounding the default by rows instead would silently truncate the GUI's
    /// playbook picker, which lists the whole catalog, while still shipping
    /// megabytes.
    #[serde(default)]
    pub total: i64,

    /// True when `limit`/`offset` meant this response is a window onto `total`.
    #[serde(default)]
    pub truncated: bool,
}

/// Catalog entry response.
///
/// Optional JSON-bodied fields (`content`, `layout`, `payload`,
/// `meta`) serialize as explicit `null` (not omitted) to match
/// the Python pydantic `CatalogEntry` wire shape — pydantic v2
/// has no `exclude_none` config on that model, so it always
/// emits the keys.  Omitting them on the Rust side surfaced as
/// DIFF lines in the noetl/ai-meta#49 Phase A parity harness;
/// same `null`-vs-omit pattern as the `UiSchemaField` fix in
/// v2.2.0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntryResponse {
    /// Catalog ID
    pub catalog_id: String,

    /// Resource path
    pub path: String,

    /// Resource kind
    pub kind: String,

    /// Version number (Postgres `smallint`).
    pub version: i16,

    /// Raw YAML content
    pub content: Option<String>,

    /// Parsed layout/structure (JSON)
    pub layout: Option<serde_json::Value>,

    /// Parsed payload/workload (JSON)
    pub payload: Option<serde_json::Value>,

    /// Additional metadata (JSON)
    pub meta: Option<serde_json::Value>,

    /// Creation timestamp
    pub created_at: DateTime<Utc>,
}

impl From<CatalogEntry> for CatalogEntryResponse {
    fn from(entry: CatalogEntry) -> Self {
        Self {
            catalog_id: entry.id.to_string(),
            path: entry.path,
            kind: entry.kind,
            version: entry.version,
            content: entry.content,
            layout: entry.layout,
            payload: entry.payload,
            meta: entry.meta,
            created_at: entry.created_at,
        }
    }
}

/// Request to get a specific catalog resource.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CatalogEntryRequest {
    /// Direct catalog entry ID
    #[serde(default)]
    pub catalog_id: Option<String>,

    /// Resource path
    #[serde(default)]
    pub path: Option<String>,

    /// Version identifier (number or "latest")
    #[serde(default)]
    pub version: Option<String>,
}

/// `POST /api/catalog/delete` body (noetl/ai-meta#237).
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogDeleteRequest {
    /// Catalog path to remove.
    pub path: String,
    /// When present, remove only this version; when absent, remove every
    /// version of `path`. Required to be explicit — there is deliberately no
    /// "delete the latest" shorthand, because the latest version is the one
    /// executions resolve to.
    #[serde(default)]
    pub version: Option<i16>,
}

/// One removed row, echoed back so the caller can audit what went.
#[derive(Debug, Clone, Serialize)]
pub struct DeletedCatalogEntry {
    pub catalog_id: String,
    pub version: i16,
}

/// `POST /api/catalog/delete` response.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogDeleteResponse {
    pub status: String,
    pub message: String,
    pub path: String,
    /// Every row removed. Empty when nothing matched — deleting an absent
    /// entry is a no-op, not an error.
    pub deleted: Vec<DeletedCatalogEntry>,
    pub count: usize,
}
