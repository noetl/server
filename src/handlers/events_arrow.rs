//! `POST /api/events/batch` with `Content-Type: application/vnd.apache.arrow.file`
//! — the server half of blueprint § H.11.5 (R-1.4): ingest a local execution's
//! Feather event buffer, as shipped by `noetl push`, onto the EHDB event log.
//!
//! # Inert until used
//!
//! * The route is shared with the worker's JSON batch. [`batch_dispatch`]
//!   inspects the Content-Type only. Anything that is not the Arrow media type
//!   goes to `handlers::events::handle_batch_events` through the same `Json`
//!   extractor as before, so the JSON path, including its rejections, is
//!   byte-identical.
//! * The Arrow branch is off unless `NOETL_EVENTS_ARROW_INGEST=enabled`.
//!   Without the flag it answers **501** `unconfigured` and writes nothing.
//!
//! # What it writes, and what it deliberately does not
//!
//! A local execution never existed on this server: no `noetl.execution` row,
//! no catalog id, no command, no chain head. Writing its events into
//! `noetl.event` would hand the orchestrator, the sweeps and the chain
//! populator an execution they did not start. So this path writes **only** to
//! the EHDB event-log tier: the store `GET /api/ehdb/tiers/eventlog` reads and
//! `noetl provider --server` folds `provider_fact`s from. It goes through the
//! same worker hop the event-log mirror uses (`POST {NOETL_EHDB_WORKER_QUERY_URL}
//! /ehdb/tiers/eventlog`). It does not write `noetl.event`, call
//! `trigger_orchestrator`, link `chain_heads`, `shadow_append`, or touch
//! `certified_fold`.
//!
//! Each record is tagged `mirror_source: "local_push"`, so any tier reader can
//! tell an imported local execution from a mirrored server one.
//!
//! The append is synchronous. The response says how many records the tier
//! accepted, and `noetl push` only marks an execution pushed on a 2xx.

use std::io::Cursor;
use std::time::Duration;

use arrow_array::{Array, BinaryArray, Int64Array, RecordBatch, StringArray, TimestampMicrosecondArray};
use arrow_ipc::reader::FileReader;
use axum::{
    body::Bytes,
    extract::{FromRequest, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::sanitize::sanitize_sensitive_data;
use crate::state::AppState;

/// The § H.11.5 media type.
pub const MEDIA_TYPE: &str = "application/vnd.apache.arrow.file";
/// Opt-in flag. Anything but `enabled` / `true` / `1` leaves the branch inert.
pub const ENABLE_ENV: &str = "NOETL_EVENTS_ARROW_INGEST";
pub const EXECUTION_ID_HEADER: &str = "x-noetl-execution-id";
pub const EVENT_COUNT_HEADER: &str = "x-noetl-event-count";
/// Upper bound on one pushed file. § H.11.4 flushes at 4 MiB per chunk; a
/// local execution large enough to exceed this should be split, not buffered
/// into server memory.
pub const MAX_BYTES: usize = 64 * 1024 * 1024;
/// Records per worker append. The mirror's own batches are this order of size;
/// a larger one risks the relay's request timeout.
const APPEND_CHUNK: usize = 500;
const APPEND_TIMEOUT: Duration = Duration::from_secs(30);
/// Provenance stamped on every record this path writes.
pub const MIRROR_SOURCE: &str = "local_push";

pub fn enabled_from(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("enabled") | Some("true") | Some("1")
    )
}

fn enabled() -> bool {
    enabled_from(std::env::var(ENABLE_ENV).ok().as_deref())
}

/// True iff the request declares the Arrow file media type (parameters such as
/// `; charset=` are ignored).
pub fn is_arrow(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case(MEDIA_TYPE))
        .unwrap_or(false)
}

/// The route handler for `POST /api/events/batch`.
pub async fn batch_dispatch(State(state): State<AppState>, req: Request) -> Response {
    if !is_arrow(req.headers()) {
        // The original path, through the original extractor.
        return match Json::<crate::handlers::events::BatchEventRequest>::from_request(req, &state).await {
            Ok(body) => crate::handlers::events::handle_batch_events(State(state), body)
                .await
                .into_response(),
            Err(rejection) => rejection.into_response(),
        };
    }
    arrow_branch(enabled(), worker_base(), req).await
}

/// The Arrow branch with its configuration passed in (the flag and the worker
/// base are process env in production), so it is testable without `AppState`.
pub async fn arrow_branch(enabled: bool, worker_base: Option<String>, req: Request) -> Response {
    if !enabled {
        return refuse(
            StatusCode::NOT_IMPLEMENTED,
            "unconfigured",
            format!("{ENABLE_ENV} is not enabled on this server; Arrow event-buffer ingest is off"),
        );
    }
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BYTES).await {
        Ok(b) => b,
        Err(e) => {
            return refuse(
                StatusCode::PAYLOAD_TOO_LARGE,
                "rejected",
                format!("body unreadable or over {MAX_BYTES} bytes: {e}"),
            )
        }
    };
    ingest(worker_base, &parts.headers, bytes).await
}

fn refuse(status: StatusCode, outcome: &str, error: String) -> Response {
    (
        status,
        Json(json!({ "action": "events.batch.arrow", "outcome": outcome, "error": error })),
    )
        .into_response()
}

/// One decoded row of a pushed `events.feather`.
#[derive(Debug, Clone, PartialEq)]
pub struct PushedEvent {
    pub event_id: i64,
    pub execution_id: i64,
    pub event_type: String,
    pub step: String,
    pub status: String,
    pub created_at_us: i64,
    pub context: Value,
    pub result: Value,
    pub meta: Value,
    pub parent_event_id: Option<i64>,
    pub payload_ref: Option<String>,
}

fn col<'a, T: 'static>(b: &'a RecordBatch, name: &str) -> Result<&'a T, String> {
    b.column_by_name(name)
        .ok_or_else(|| format!("missing column `{name}`"))?
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| format!("column `{name}` has the wrong type"))
}

fn json_at(a: &BinaryArray, i: usize, name: &str) -> Result<Value, String> {
    if a.is_null(i) || a.value(i).is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(a.value(i)).map_err(|e| format!("row {i}: `{name}` is not JSON: {e}"))
}

/// Decode a Feather V2 file in the § H.11.3 schema. Columns are read by name.
pub fn decode(bytes: &[u8]) -> Result<Vec<PushedEvent>, String> {
    if bytes.len() < 6 || &bytes[..6] != b"ARROW1" {
        return Err("body is not a Feather V2 / Arrow IPC file (missing ARROW1 magic)".into());
    }
    let reader = FileReader::try_new(Cursor::new(bytes), None).map_err(|e| format!("open Arrow file: {e}"))?;
    let mut out = Vec::new();
    for batch in reader {
        let b = batch.map_err(|e| format!("read Arrow chunk: {e}"))?;
        let event_id = col::<Int64Array>(&b, "event_id")?;
        let execution_id = col::<Int64Array>(&b, "execution_id")?;
        let event_type = col::<StringArray>(&b, "event_type")?;
        let step = col::<StringArray>(&b, "step")?;
        let status = col::<StringArray>(&b, "status")?;
        let created_at = col::<TimestampMicrosecondArray>(&b, "created_at")?;
        let context = col::<BinaryArray>(&b, "context")?;
        let result = col::<BinaryArray>(&b, "result")?;
        let meta = col::<BinaryArray>(&b, "meta")?;
        let parent = col::<Int64Array>(&b, "parent_event_id")?;
        let payload_ref = col::<StringArray>(&b, "payload_ref")?;
        for i in 0..b.num_rows() {
            out.push(PushedEvent {
                event_id: event_id.value(i),
                execution_id: execution_id.value(i),
                event_type: event_type.value(i).to_string(),
                step: step.value(i).to_string(),
                status: status.value(i).to_string(),
                created_at_us: created_at.value(i),
                context: json_at(context, i, "context")?,
                result: json_at(result, i, "result")?,
                meta: json_at(meta, i, "meta")?,
                parent_event_id: (!parent.is_null(i)).then(|| parent.value(i)),
                payload_ref: (!payload_ref.is_null(i)).then(|| payload_ref.value(i).to_string()),
            });
        }
    }
    Ok(out)
}

/// Header + body consistency: the header names the execution every row
/// belongs to, and the event count matches. A mismatch is a client bug or a
/// truncated upload; either way nothing is written.
pub fn validate(headers: &HeaderMap, events: &[PushedEvent]) -> Result<i64, String> {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(str::trim);
    let execution_id: i64 = h(EXECUTION_ID_HEADER)
        .ok_or("X-NoETL-Execution-Id is required")?
        .parse()
        .map_err(|_| "X-NoETL-Execution-Id is not an integer")?;
    let count: usize = h(EVENT_COUNT_HEADER)
        .ok_or("X-NoETL-Event-Count is required")?
        .parse()
        .map_err(|_| "X-NoETL-Event-Count is not an integer")?;
    if count != events.len() {
        return Err(format!("X-NoETL-Event-Count is {count} but the file holds {} event(s)", events.len()));
    }
    if let Some(e) = events.iter().find(|e| e.execution_id != execution_id) {
        return Err(format!(
            "event {} belongs to execution {}, not the header's {execution_id}",
            e.event_id, e.execution_id
        ));
    }
    Ok(execution_id)
}

/// The tier record for one pushed event: the identifying projection the
/// comparator reads (`event_id`, `event_type`, `step`, `status`), the row
/// shape `mirror_payload` writes, and `mirror_source: "local_push"`.
/// `catalog_id` is null because a local execution has none. Secret-bearing
/// fields are sanitised exactly as the JSON batch path sanitises them.
pub fn tier_record(e: &PushedEvent) -> Value {
    let created_at = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(e.created_at_us)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true));
    json!({
        "event_id": e.event_id,
        "event_type": e.event_type,
        "step": e.step,
        "status": e.status,
        "execution_id": e.execution_id,
        "catalog_id": null,
        "node_id": e.step,
        "node_name": e.step,
        "node_type": null,
        "parent_event_id": e.parent_event_id,
        "prev_event_id": null,
        "parent_execution_id": null,
        "result": sanitize_sensitive_data(&e.result),
        "meta": sanitize_sensitive_data(&e.meta),
        "error": null,
        "worker_id": null,
        "created_at": created_at,
        "context": sanitize_sensitive_data(&e.context),
        "payload_ref": e.payload_ref,
        "mirror_source": MIRROR_SOURCE,
        "mirror_payload_version": crate::handlers::ehdb_eventlog_mirror::MIRROR_PAYLOAD_VERSION,
    })
}

fn worker_base() -> Option<String> {
    std::env::var(crate::handlers::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

async fn ingest(worker_base: Option<String>, headers: &HeaderMap, bytes: Bytes) -> Response {
    let events = match decode(&bytes) {
        Ok(e) => e,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, "rejected", e),
    };
    let execution_id = match validate(headers, &events) {
        Ok(id) => id,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, "rejected", e),
    };
    if events.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({"status": "ok", "action": "events.batch.arrow", "execution_id": execution_id.to_string(), "appended": 0})),
        )
            .into_response();
    }
    let Some(base) = worker_base else {
        return refuse(
            StatusCode::NOT_IMPLEMENTED,
            "unconfigured",
            format!(
                "{} is not set; there is no EHDB event-log tier to append to",
                crate::handlers::ehdb::WORKER_QUERY_URL_ENV
            ),
        );
    };
    let url = format!("{base}/ehdb/tiers/eventlog");
    let records: Vec<String> = events.iter().map(|e| tier_record(e).to_string()).collect();

    let mut appended = 0u64;
    for (n, chunk) in records.chunks(APPEND_CHUNK).enumerate() {
        let body = json!({ "execution_id": execution_id.to_string(), "records": chunk });
        let resp = match http().post(&url).json(&body).timeout(APPEND_TIMEOUT).send().await {
            Ok(r) => r,
            Err(e) => {
                return partial(StatusCode::BAD_GATEWAY, execution_id, appended, n, format!("tier append failed: {e}"))
            }
        };
        let status = resp.status();
        let detail: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            // Relay the worker's 501 (mirror source not `server`) as-is: it
            // is a configuration answer, not a gateway failure.
            let code = if status == StatusCode::NOT_IMPLEMENTED { status } else { StatusCode::BAD_GATEWAY };
            return partial(code, execution_id, appended, n, format!("tier answered {status}: {detail}"));
        }
        appended += detail
            .get("appended")
            .and_then(Value::as_u64)
            .unwrap_or(chunk.len() as u64);
    }
    tracing::info!(target: "noetl_server::events_arrow", execution_id, events = events.len(), appended,
        "local event buffer ingested onto the EHDB event log");
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "action": "events.batch.arrow",
            "execution_id": execution_id.to_string(),
            "events": events.len(),
            "appended": appended,
            "tier": "eventlog",
            "mirror_source": MIRROR_SOURCE,
        })),
    )
        .into_response()
}

/// A failure after zero or more chunks landed. Says how many did, so a retry
/// (`noetl push --force`) is an informed decision rather than a blind one.
fn partial(status: StatusCode, execution_id: i64, appended: u64, chunk: usize, error: String) -> Response {
    (
        status,
        Json(json!({
            "action": "events.batch.arrow",
            "outcome": if appended == 0 { "failed" } else { "partial" },
            "execution_id": execution_id.to_string(),
            "appended": appended,
            "failed_chunk": chunk,
            "error": error,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::ArrayRef;
    use arrow_ipc::writer::FileWriter;
    use arrow_schema::{DataType, Field, Schema, TimeUnit};
    use axum::http::HeaderValue;
    use std::sync::Arc;

    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("event_id", DataType::Int64, false),
            Field::new("execution_id", DataType::Int64, false),
            Field::new("event_type", DataType::Utf8, false),
            Field::new("step", DataType::Utf8, false),
            Field::new("status", DataType::Utf8, false),
            Field::new("created_at", DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())), false),
            Field::new("context", DataType::Binary, false),
            Field::new("result", DataType::Binary, false),
            Field::new("meta", DataType::Binary, false),
            Field::new("parent_event_id", DataType::Int64, true),
            Field::new("payload_ref", DataType::Utf8, true),
        ]))
    }

    /// A file exactly as the CLI writes it: two chunks.
    fn feather(eid: i64, rows: &[(&str, Value)]) -> Vec<u8> {
        let mut buf = Vec::new();
        let s = schema();
        let mut w = FileWriter::try_new(&mut buf, &s).unwrap();
        for (k, chunk) in rows.chunks(1).enumerate() {
            let bin = |v: &Value| Arc::new(BinaryArray::from_iter_values([serde_json::to_vec(v).unwrap()])) as ArrayRef;
            let cols: Vec<ArrayRef> = vec![
                Arc::new(Int64Array::from(vec![1000 + k as i64])),
                Arc::new(Int64Array::from(vec![eid])),
                Arc::new(StringArray::from(vec![chunk[0].0])),
                Arc::new(StringArray::from(vec!["record_state"])),
                Arc::new(StringArray::from(vec!["COMPLETED"])),
                Arc::new(TimestampMicrosecondArray::from(vec![1_762_500_000_000_000]).with_timezone("UTC")),
                bin(&json!({})),
                bin(&chunk[0].1),
                bin(&json!({"password": "hunter2"})),
                Arc::new(Int64Array::from(vec![None::<i64>])),
                Arc::new(StringArray::from(vec![None::<&str>])),
            ];
            w.write(&RecordBatch::try_new(s.clone(), cols).unwrap()).unwrap();
        }
        w.finish().unwrap();
        drop(w);
        buf
    }

    fn headers(eid: &str, count: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static(MEDIA_TYPE));
        h.insert(EXECUTION_ID_HEADER, HeaderValue::from_str(eid).unwrap());
        h.insert(EVENT_COUNT_HEADER, HeaderValue::from_str(count).unwrap());
        h
    }

    fn fact() -> Value {
        json!({"data": {"provider_fact": {"urn": "k8s://kind-noetl/log-analytics/core/ConfigMap/x",
            "provider": "kubernetes", "service": "core", "resource_type": "ConfigMap", "verb": "ensure",
            "stack": "log-analytics", "outcome": "changed", "execution_id": 7, "desired": {}}}})
    }

    #[test]
    fn flag_is_off_unless_explicitly_enabled() {
        for off in [None, Some(""), Some("off"), Some("false"), Some("server")] {
            assert!(!enabled_from(off), "{off:?}");
        }
        for on in ["enabled", "true", "1", " Enabled "] {
            assert!(enabled_from(Some(on)), "{on}");
        }
    }

    #[test]
    fn only_the_arrow_media_type_is_diverted() {
        let mut h = HeaderMap::new();
        assert!(!is_arrow(&h), "no content-type stays on the JSON path");
        h.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        assert!(!is_arrow(&h));
        h.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.apache.arrow.stream"));
        assert!(!is_arrow(&h), "the IPC *stream* type is not the § H.11.5 contract");
        h.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static("Application/Vnd.Apache.Arrow.File; x=1"));
        assert!(is_arrow(&h));
    }

    #[test]
    fn decodes_multi_chunk_files_and_validates_headers() {
        let bytes = feather(42, &[("provider.fact", fact()), ("step.exit", json!({"data": 1}))]);
        let ev = decode(&bytes).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].result, fact());
        assert_eq!(validate(&headers("42", "2"), &ev).unwrap(), 42);
        assert!(validate(&headers("42", "3"), &ev).unwrap_err().contains("Event-Count"));
        assert!(validate(&headers("43", "2"), &ev).unwrap_err().contains("not the header"));
        assert!(validate(&HeaderMap::new(), &ev).is_err());
    }

    /// A file written by the real CLI (`noetl buffer`, noetl/cli R-1.4): one
    /// `step.exit` and one `provider.fact`. Guards the cross-repo contract.
    #[test]
    fn decodes_a_file_written_by_the_noetl_cli() {
        let bytes = include_bytes!("../../tests/fixtures/cli_local_buffer_events.feather");
        let ev = decode(bytes).unwrap();
        assert_eq!(ev.iter().map(|e| e.event_type.as_str()).collect::<Vec<_>>(), ["step.exit", "provider.fact"]);
        assert_eq!(ev[1].result["data"]["provider_fact"]["urn"], "k8s://x");
        let eid = ev[0].execution_id.to_string();
        assert_eq!(validate(&headers(&eid, "2"), &ev).unwrap(), ev[0].execution_id);
    }

    #[test]
    fn rejects_non_arrow_and_wrong_schema() {
        assert!(decode(b"{\"events\":[]}").unwrap_err().contains("ARROW1"));
        let s = Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)]));
        let mut buf = Vec::new();
        {
            let mut w = FileWriter::try_new(&mut buf, &s).unwrap();
            w.write(&RecordBatch::try_new(s.clone(), vec![Arc::new(Int64Array::from(vec![1])) as ArrayRef]).unwrap())
                .unwrap();
            w.finish().unwrap();
        }
        assert!(decode(&buf).unwrap_err().contains("missing column"));
    }

    #[test]
    fn tier_record_is_foldable_tagged_and_sanitised() {
        let ev = decode(&feather(42, &[("provider.fact", fact())])).unwrap();
        let r = tier_record(&ev[0]);
        // The comparator's identifying projection.
        for k in ["event_id", "event_type", "step", "status"] {
            assert!(!r[k].is_null(), "{k}");
        }
        assert_eq!(r["mirror_source"], MIRROR_SOURCE);
        assert_eq!(r["catalog_id"], Value::Null, "a local execution has no catalog id");
        // The provider fold's first probe path is result.data.provider_fact.
        assert_eq!(r["result"]["data"]["provider_fact"]["verb"], "ensure");
        assert!(!r.to_string().contains("hunter2"), "meta is sanitised like the JSON batch path");
        assert_eq!(r["created_at"], "2025-11-07T07:20:00.000000Z");
    }

    /// A stand-in for the worker's `POST /ehdb/tiers/eventlog`: records every
    /// body it receives and answers `status`.
    async fn fake_worker(status: u16) -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
        use axum::routing::post;
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let s2 = seen.clone();
        let app = axum::Router::new().route(
            "/ehdb/tiers/eventlog",
            post(move |Json(b): Json<Value>| {
                let s2 = s2.clone();
                async move {
                    let n = b["records"].as_array().map(|a| a.len()).unwrap_or(0);
                    s2.lock().unwrap().push(b);
                    (StatusCode::from_u16(status).unwrap(), Json(json!({"outcome": "ok", "appended": n})))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn request(eid: &str, count: &str, body: Vec<u8>) -> Request {
        let mut r = Request::new(axum::body::Body::from(body));
        *r.headers_mut() = headers(eid, count);
        r
    }

    async fn body_json(r: Response) -> (StatusCode, Value) {
        let st = r.status();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn flag_off_answers_501_and_contacts_nothing() {
        let (base, seen) = fake_worker(200).await;
        let bytes = feather(42, &[("provider.fact", fact())]);
        let (st, b) = body_json(arrow_branch(false, Some(base), request("42", "1", bytes)).await).await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(b["outcome"], "unconfigured");
        assert!(seen.lock().unwrap().is_empty(), "inert: nothing was appended");
    }

    #[tokio::test]
    async fn enabled_appends_every_event_to_the_tier_in_order() {
        let (base, seen) = fake_worker(200).await;
        let bytes = feather(42, &[("provider.fact", fact()), ("step.exit", json!({"data": 2}))]);
        let (st, b) = body_json(arrow_branch(true, Some(base), request("42", "2", bytes)).await).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["appended"], 2);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["execution_id"], "42");
        let recs: Vec<Value> = seen[0]["records"].as_array().unwrap().iter()
            .map(|r| serde_json::from_str(r.as_str().unwrap()).unwrap()).collect();
        assert_eq!(recs[0]["event_type"], "provider.fact");
        assert_eq!(recs[1]["event_type"], "step.exit");
        assert!(recs.iter().all(|r| r["mirror_source"] == MIRROR_SOURCE));
    }

    #[tokio::test]
    async fn bad_upload_writes_nothing_and_worker_501_is_relayed() {
        let (base, seen) = fake_worker(200).await;
        let bytes = feather(42, &[("provider.fact", fact())]);
        let (st, _) = body_json(arrow_branch(true, Some(base.clone()), request("42", "9", bytes.clone())).await).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "count mismatch");
        let (st, _) = body_json(arrow_branch(true, None, request("42", "1", bytes.clone())).await).await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED, "no worker base configured");
        assert!(seen.lock().unwrap().is_empty());
        let (base501, _) = fake_worker(501).await;
        let (st, b) = body_json(arrow_branch(true, Some(base501), request("42", "1", bytes)).await).await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED, "worker mirror source not `server` → relayed: {b}");
        assert_eq!(b["appended"], 0);
    }

    /// The JSON path must stay on the original handler. Source-level guard:
    /// the route points at the dispatcher, and the dispatcher's non-Arrow arm
    /// calls `handle_batch_events` through the original `Json` extractor
    /// before any Arrow or flag logic runs.
    #[test]
    fn json_path_is_routed_to_the_original_handler_first() {
        let me = include_str!("events_arrow.rs");
        let f = &me[me.find("pub async fn batch_dispatch").unwrap()..];
        let f = &f[..f.find("\n}\n").unwrap()];
        let json_arm = f.find("handle_batch_events").expect("JSON arm present");
        assert!(json_arm < f.find("enabled()").unwrap(), "flag check must not gate the JSON path");
        assert!(json_arm < f.find("arrow_branch(").unwrap());
        let main = include_str!("../main.rs");
        assert!(main.contains("post(handlers::events_arrow::batch_dispatch)"));
        assert!(!main.contains("post(handlers::events::handle_batch_events)"));
    }

    /// The import path writes nowhere but the tier: no `noetl.event`, no
    /// orchestration, no chain, no shadow (see the module docs).
    #[test]
    fn ingest_has_no_orchestration_or_postgres_side_effects() {
        let me = include_str!("events_arrow.rs");
        let body = &me[..me.find("#[cfg(test)]").unwrap()];
        let code: String = body.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
        for forbidden in [
            "trigger_orchestrator(",
            "INSERT INTO",
            "chain_heads",
            "shadow_append(",
            "certified_fold::",
            "emit_events(",
            "pools.pool_for",
        ] {
            assert!(!code.contains(forbidden), "events_arrow must not call `{forbidden}`");
        }
    }
}
