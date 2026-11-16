//! Dead-data survey and archive rehearsal (noetl/ai-meta#308 / cleanup track).
//!
//! Read-only reporting plus a **rehearsal** that proves an archive-then-restore
//! round trip works, without touching a single live row.
//!
//! # Why this exists as a typed endpoint
//!
//! The obvious way to answer "how big is the dead data" is arbitrary SQL through
//! `POST /api/postgres/execute`. That endpoint takes any statement, is not
//! auth-gated, and is [noetl/ai-meta#312](https://github.com/noetl/ai-meta/issues/312).
//! A cleanup that begins by reaching for it would be using the very thing the
//! cleanup is supposed to make unnecessary. This is the narrow, gated,
//! read-only alternative: fixed queries, no caller-supplied SQL.
//!
//! # Nothing here deletes anything
//!
//! `report` reads counts. `rehearse` creates a scratch table, round-trips **one
//! synthetic row** through it, and drops the scratch table. Neither reads a live
//! row's contents nor writes to a live table. The destructive steps stay in the
//! runbook, owner-run.

use serde::Serialize;

/// Tables the cleanup track is about.
///
/// A closed list, in code, because the alternative is a caller naming a table —
/// which is `/api/postgres/execute` again with extra steps.
pub const SURVEYED: [&str; 3] = ["outbox", "projection", "execution"];

/// The scratch table the rehearsal uses. Named so it is obviously disposable and
/// obviously not a real archive.
pub const REHEARSAL_TABLE: &str = "noetl.__archive_rehearsal";

#[derive(Debug, Clone, Serialize)]
pub struct TableSurvey {
    pub table: String,
    pub exists: bool,
    pub rows: Option<i64>,
    /// `pg_total_relation_size`, which includes indexes and TOAST.
    ///
    /// ⚠ Returns 0 for a **partitioned parent**; the size lives on the
    /// partitions. Reported as-is rather than silently summed, so a 0 here is
    /// read as "ask the partitions", not "empty".
    pub total_bytes: Option<i64>,
    pub total_pretty: Option<String>,
    /// Set only where the table has a meaningful liveness column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_rows: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `GET /api/admin/dead-data/report` — the cleanup dry run.
///
/// Read-only. Counts and sizes only; no row contents leave the database.
pub async fn report_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
) -> crate::error::AppResult<axum::Json<serde_json::Value>> {
    let pool = state.pools.cluster();
    let mut out: Vec<TableSurvey> = Vec::new();

    for t in SURVEYED {
        let full = format!("noetl.{t}");
        let exists: Option<(bool,)> = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = 'noetl' AND table_name = $1)",
        )
        .bind(t)
        .fetch_optional(pool)
        .await?;
        let exists = exists.map(|(b,)| b).unwrap_or(false);
        if !exists {
            out.push(TableSurvey {
                table: full,
                exists: false,
                rows: None,
                total_bytes: None,
                total_pretty: None,
                live_rows: None,
                note: Some("table not present".into()),
            });
            continue;
        }

        // Fixed identifiers from SURVEYED, never from the request.
        let rows: Option<(i64,)> = sqlx::query_as(&format!("SELECT COUNT(*) FROM {full}"))
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
        let size: Option<(i64, String)> = sqlx::query_as(
            "SELECT pg_total_relation_size($1::regclass)::bigint, \
             pg_size_pretty(pg_total_relation_size($1::regclass))",
        )
        .bind(&full)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();

        // Per-table liveness, where the table has a column that expresses it.
        let (live_rows, note) = match t {
            "outbox" => {
                let unpublished: Option<(i64,)> = sqlx::query_as(&format!(
                    "SELECT COUNT(*) FROM {full} WHERE published_at IS NULL"
                ))
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
                (
                    unpublished.map(|(n,)| n),
                    Some(
                        "live_rows = unpublished. A non-zero value means the outbox is \
                         NOT dead and nothing may be dropped."
                            .into(),
                    ),
                )
            }
            "projection" => (
                None,
                Some(
                    "the DEAD table: noetl/ai-meta#265 found it has no Rust writer. The \
                     live store is noetl.projection_snapshot — do not confuse them."
                        .into(),
                ),
            ),
            "execution" => (
                None,
                Some(
                    "⚠ NOT dead. noetl/ai-meta#235 records that `status` is a frozen \
                     Python-era column, which is a different claim from the table being \
                     unused. Survey only."
                        .into(),
                ),
            ),
            _ => (None, None),
        };

        out.push(TableSurvey {
            table: full,
            exists: true,
            rows: rows.map(|(n,)| n),
            total_bytes: size.as_ref().map(|(b, _)| *b),
            total_pretty: size.map(|(_, p)| p),
            live_rows,
            note,
        });
    }

    Ok(axum::Json(serde_json::json!({
        "action": "admin.dead_data.report",
        "outcome": "ok",
        "read_only": true,
        "tables": out,
        "reminder": "This endpoint deletes nothing. The archive-then-drop steps are \
                     owner-run and live in playbooks/dead-data-cleanup/.",
    })))
}

/// `POST /api/admin/dead-data/rehearse` — prove the archive round trip is real.
///
/// Creates a scratch table, writes **one synthetic row**, reads it back, compares
/// it, and drops the scratch table. It does not read or write a live row.
///
/// # What this actually establishes
///
/// That the server's role can `CREATE TABLE` in the `noetl` schema, write, read
/// back byte-identically, and `DROP`. That is the whole archive-before-drop
/// mechanism, exercised end to end. Without it, "we will archive first" is a
/// plan whose first step has never been run — and the server's role is known to
/// lack ownership on some tables (`must be owner of table event` appears in the
/// boot log), so this is a real question rather than a formality.
pub async fn rehearse_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
) -> crate::error::AppResult<axum::Json<serde_json::Value>> {
    let pool = state.pools.cluster();
    let mut steps: Vec<serde_json::Value> = Vec::new();
    let mut ok = true;
    let token = format!("rehearsal-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));

    macro_rules! step {
        ($name:expr, $res:expr) => {{
            match $res {
                Ok(v) => {
                    steps.push(serde_json::json!({"step": $name, "ok": true}));
                    Some(v)
                }
                Err(e) => {
                    ok = false;
                    steps.push(serde_json::json!({
                        "step": $name, "ok": false, "error": e.to_string()
                    }));
                    None
                }
            }
        }};
    }

    // Always drop first: a leftover from a failed run must not make this pass by
    // finding a table that already exists.
    let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {REHEARSAL_TABLE}"))
        .execute(pool)
        .await;

    step!(
        "create_archive_table",
        sqlx::query(&format!(
            "CREATE TABLE {REHEARSAL_TABLE} (id BIGINT PRIMARY KEY, payload TEXT NOT NULL)"
        ))
        .execute(pool)
        .await
    );
    if ok {
        step!(
            "write_sample",
            sqlx::query(&format!(
                "INSERT INTO {REHEARSAL_TABLE} (id, payload) VALUES (1, $1)"
            ))
            .bind(&token)
            .execute(pool)
            .await
        );
    }
    let mut round_trip = false;
    if ok {
        let read: Result<Option<(String,)>, _> =
            sqlx::query_as(&format!("SELECT payload FROM {REHEARSAL_TABLE} WHERE id = 1"))
                .fetch_optional(pool)
                .await;
        if let Some(v) = step!("read_back", read) {
            // The comparison is the point. A rehearsal that created a table and
            // never checked what came out would pass on a store that silently
            // dropped the write.
            round_trip = v.map(|(p,)| p) == Some(token.clone());
            steps.push(serde_json::json!({
                "step": "compare", "ok": round_trip,
                "detail": if round_trip { "byte-identical" } else { "MISMATCH — the restore path is not real" }
            }));
            if !round_trip {
                ok = false;
            }
        }
    }
    // Always attempt cleanup, even on failure: a rehearsal that leaves debris
    // behind is a cleanup task of its own.
    let dropped = sqlx::query(&format!("DROP TABLE IF EXISTS {REHEARSAL_TABLE}"))
        .execute(pool)
        .await
        .is_ok();
    steps.push(serde_json::json!({"step": "drop_scratch", "ok": dropped}));

    Ok(axum::Json(serde_json::json!({
        "action": "admin.dead_data.rehearse",
        "outcome": if ok && round_trip && dropped { "ok" } else { "failed" },
        "archive_path_writable": ok,
        "round_trip_verified": round_trip,
        "scratch_removed": dropped,
        "touched_live_rows": false,
        "steps": steps,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The surveyed set is fixed in code, so no caller can name a table.
    #[test]
    fn the_surveyed_tables_are_a_closed_set() {
        assert_eq!(SURVEYED, ["outbox", "projection", "execution"]);
        for t in SURVEYED {
            assert!(
                !t.contains(';') && !t.contains(' ') && !t.contains('"'),
                "{t} is interpolated into SQL; it must be a bare identifier"
            );
        }
    }

    /// The scratch table is obviously disposable and clearly not a real archive.
    #[test]
    fn the_rehearsal_table_is_obviously_scratch() {
        assert!(REHEARSAL_TABLE.starts_with("noetl.__"));
        assert!(REHEARSAL_TABLE.contains("rehearsal"));
        for t in SURVEYED {
            assert_ne!(
                REHEARSAL_TABLE,
                format!("noetl.{t}"),
                "the rehearsal must never name a surveyed table"
            );
        }
    }

    /// ⚠ Nothing in this module deletes from a surveyed table.
    ///
    /// Counts CODE with `//` comments stripped, so the prose above cannot
    /// satisfy the guard and deleting the prose cannot break it.
    #[test]
    fn this_module_never_deletes_from_a_live_table() {
        let src = include_str!("dead_data.rs");
        let code = src
            .split_once("\n#[cfg(test)]")
            .map(|(above, _)| above)
            .unwrap_or(src);
        let code: String = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for t in SURVEYED {
            for verb in ["DELETE FROM", "TRUNCATE", "DROP TABLE"] {
                let needle = format!("{verb} noetl.{t}");
                assert!(
                    !code.to_uppercase().contains(&needle.to_uppercase()),
                    "this module must never `{verb}` a surveyed table, found: {needle}"
                );
            }
        }
        // Positive control: the guard can see a destructive statement, and does
        // — the scratch table is dropped, on purpose.
        assert!(
            code.contains("DROP TABLE IF EXISTS"),
            "the guard is not reading the code it thinks it is"
        );
    }
}
