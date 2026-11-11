//! Catalog source resolvers for `db://` and `ehdb://` (RFC §3.3,
//! noetl/ai-meta#548). Both feed the same path as every other source URI:
//! [`catalog_peer::fetch_uri`](super::catalog_peer::fetch_uri) returns the body,
//! and the caller verifies it against any `#sha256:` pin, imports it under
//! `peer/source/<path>` (dedup by digest) and records the URI as a source.
//!
//! # `db://<credential-alias>/<schema>.<table>?path=<p>[&version=<n>]`
//!
//! A row in a Postgres table the operator owns. The alias names a credential in
//! this server's credential store (`POST /api/credentials`); its data carries
//! `host`, `port`, `user`, `password`, `database` (or `dbname`), or a
//! `connection_string`. The table must have columns `path` (text), `version`
//! (integer) and `content` (text). Without `version` the highest version for
//! the path is taken.
//!
//! Schema and table names are identifiers (`[A-Za-z_][A-Za-z0-9_]*`, at most
//! 63 bytes) and are quoted; the path and version are bound parameters. The
//! connection is opened for the one read and closed.
//!
//! # `ehdb://catalog/<catalog_id>`
//!
//! A registration record in the cluster's EHDB catalog tier (the log written by
//! `NOETL_CATALOG_LOG`, read through the worker relay at
//! `NOETL_EHDB_WORKER_QUERY_URL`). The record carries the registered content
//! and its `content_sha256`, which is checked before the body is returned. The
//! record is found by a paged scan of the tier, stopping at the first match:
//! cost grows with the log, and a keyed lookup is TARGET.

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use sqlx::Connection;

use crate::error::{AppError, AppResult};
use crate::services::CredentialService;

static CREDENTIALS: OnceLock<CredentialService> = OnceLock::new();

/// Give the resolvers this server's credential store (for `db://` aliases).
/// Called once at startup; later calls are ignored.
pub fn set_credentials(svc: CredentialService) {
    let _ = CREDENTIALS.set(svc);
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
/// Records per tier page (catalog records carry content; the frame cap is 1 MiB).
const EHDB_PAGE: usize = 16;
const EHDB_MAX_PAGES: usize = 4000;

/// A parsed `db://` source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbSource {
    pub alias: String,
    pub schema: String,
    pub table: String,
    pub path: String,
    pub version: Option<i64>,
}

fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `db://alias/schema.table?path=p[&version=n]` (a `#…` fragment is ignored;
/// the caller has already taken the digest pin from it).
pub fn parse_db(uri: &str) -> AppResult<DbSource> {
    let bad = |m: &str| AppError::Validation(format!("'{uri}': {m}"));
    let rest = uri
        .strip_prefix("db://")
        .ok_or_else(|| bad("not a db:// URI"))?;
    let rest = rest.split('#').next().unwrap_or(rest);
    let (loc, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (alias, relation) = loc
        .split_once('/')
        .ok_or_else(|| bad("expected db://<alias>/<schema>.<table>"))?;
    if alias.is_empty() || alias.contains('@') || alias.contains(':') {
        return Err(bad(
            "the authority is a credential alias, never user:pass@host",
        ));
    }
    let (schema, table) = relation
        .trim_end_matches('/')
        .split_once('.')
        .ok_or_else(|| bad("expected <schema>.<table>"))?;
    if !ident_ok(schema) || !ident_ok(table) {
        return Err(bad(
            "schema and table must be identifiers [A-Za-z_][A-Za-z0-9_]*",
        ));
    }
    let path = query_param(query, "path")
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| bad("?path=<playbook path> is required"))?;
    let version = match query_param(query, "version") {
        None => None,
        Some(v) => Some(
            v.parse::<i64>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| bad("version must be a positive integer"))?,
        ),
    };
    Ok(DbSource {
        alias: alias.to_string(),
        schema: schema.to_string(),
        table: table.to_string(),
        path,
        version,
    })
}

/// The SQL a [`DbSource`] runs (identifiers already validated).
pub fn db_query(src: &DbSource) -> String {
    let rel = format!("\"{}\".\"{}\"", src.schema, src.table);
    match src.version {
        Some(_) => {
            format!("SELECT content::text FROM {rel} WHERE path = $1 AND version = $2 LIMIT 1")
        }
        None => {
            format!("SELECT content::text FROM {rel} WHERE path = $1 ORDER BY version DESC LIMIT 1")
        }
    }
}

/// Connection options from a postgres credential's data.
pub fn connect_options(alias: &str, data: &Value) -> AppResult<sqlx::postgres::PgConnectOptions> {
    let bad = |m: &str| AppError::Validation(format!("credential '{alias}': {m}"));
    let s = |k: &str| {
        data.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
    };
    if let Some(cs) = s("connection_string") {
        return cs
            .parse::<sqlx::postgres::PgConnectOptions>()
            .map_err(|e| bad(&format!("connection_string: {e}")));
    }
    let host = s("host").ok_or_else(|| bad("needs host (or connection_string)"))?;
    let user = s("user")
        .or_else(|| s("username"))
        .ok_or_else(|| bad("needs user"))?;
    let database = s("database")
        .or_else(|| s("dbname"))
        .ok_or_else(|| bad("needs database"))?;
    let port = data
        .get("port")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|p| p.parse().ok()))
        })
        .unwrap_or(5432) as u16;
    let mut o = sqlx::postgres::PgConnectOptions::new()
        .host(host)
        .port(port)
        .username(user)
        .database(database);
    if let Some(pw) = s("password") {
        o = o.password(pw);
    }
    Ok(o)
}

/// Fetch a `db://` source body.
pub async fn fetch_db(uri: &str) -> AppResult<String> {
    let src = parse_db(uri)?;
    let creds = CREDENTIALS.get().ok_or_else(|| {
        AppError::Internal("db:// sources: the credential store is not available".into())
    })?;
    let cred = creds
        .get(&src.alias, true, None)
        .await
        .map_err(|e| match e {
            AppError::NotFound(_) => {
                AppError::Validation(format!("{uri}: credential alias '{}' not found", src.alias))
            }
            other => other,
        })?;
    let data = cred.data.unwrap_or(Value::Null);
    let opts = connect_options(&src.alias, &data)?;
    let mut conn = tokio::time::timeout(CONNECT_TIMEOUT, sqlx::PgConnection::connect_with(&opts))
        .await
        .map_err(|_| AppError::ExternalService(format!("{uri}: connect timed out")))?
        .map_err(|e| AppError::ExternalService(format!("{uri}: connect: {e}")))?;
    let sql = db_query(&src);
    let q = sqlx::query_scalar::<_, String>(&sql).bind(&src.path);
    let q = match src.version {
        Some(v) => q.bind(v),
        None => q,
    };
    let row = tokio::time::timeout(QUERY_TIMEOUT, q.fetch_optional(&mut conn))
        .await
        .map_err(|_| AppError::ExternalService(format!("{uri}: query timed out")))?
        .map_err(|e| AppError::ExternalService(format!("{uri}: {e}")))?;
    let _ = conn.close().await;
    row.ok_or_else(|| {
        AppError::NotFound(format!(
            "{uri}: no row for path '{}'{}",
            src.path,
            src.version
                .map(|v| format!(" version {v}"))
                .unwrap_or_default()
        ))
    })
}

/// The catalog id an `ehdb://catalog/<id>` names.
pub fn parse_ehdb(uri: &str) -> AppResult<i64> {
    let bad = |m: &str| AppError::Validation(format!("'{uri}': {m}"));
    let rest = uri
        .strip_prefix("ehdb://")
        .ok_or_else(|| bad("not an ehdb:// URI"))?;
    let rest = rest.split(['#', '?']).next().unwrap_or(rest);
    let (ns, id) = rest
        .split_once('/')
        .ok_or_else(|| bad("expected ehdb://catalog/<catalog_id>"))?;
    if ns != "catalog" {
        return Err(bad(&format!(
            "'{ns}' is not a resolvable EHDB namespace; only ehdb://catalog/<catalog_id>"
        )));
    }
    id.trim_end_matches('/')
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| bad("the catalog id must be a positive integer"))
}

/// From one tier page, the content of the record for `catalog_id`, checked
/// against its own `content_sha256`.
pub fn pick_record(records: &[Value], catalog_id: i64) -> Option<AppResult<String>> {
    let want = catalog_id.to_string();
    records.iter().find_map(|r| {
        // The relay wraps each record: the payload is a JSON string or object.
        let payload = r.get("payload").or_else(|| r.get("record")).unwrap_or(r);
        let rec: Value = match payload {
            Value::String(s) => serde_json::from_str(s).ok()?,
            other => other.clone(),
        };
        let id_matches = match rec.get("catalog_id") {
            Some(Value::String(s)) => *s == want,
            Some(Value::Number(n)) => n.as_i64() == Some(catalog_id),
            _ => false,
        };
        if !id_matches
            || rec.get("event_type").and_then(Value::as_str) != Some("catalog.registered")
        {
            return None;
        }
        let content = rec.get("content").and_then(Value::as_str)?.to_string();
        let claimed = rec
            .get("content_sha256")
            .and_then(Value::as_str)
            .unwrap_or("");
        let actual = crate::handlers::catalog_snapshot::sha256_hex(content.as_bytes());
        Some(if claimed == actual {
            Ok(content)
        } else {
            Err(AppError::ExternalService(format!(
                "ehdb://catalog/{catalog_id}: record content does not match its content_sha256"
            )))
        })
    })
}

/// Fetch an `ehdb://catalog/<id>` body from the catalog tier.
pub async fn fetch_ehdb(http: &reqwest::Client, uri: &str) -> AppResult<String> {
    let id = parse_ehdb(uri)?;
    let base = std::env::var(crate::handlers::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Validation(format!(
                "{uri}: {} is unset, so the EHDB catalog tier cannot be read",
                crate::handlers::ehdb::WORKER_QUERY_URL_ENV
            ))
        })?;
    let tier = crate::handlers::catalog_log::TIER;
    let mut after: u64 = 0;
    let mut page = EHDB_PAGE;
    for _ in 0..EHDB_MAX_PAGES {
        let url = if after == 0 {
            format!("{base}/ehdb/tiers/{tier}?limit={page}")
        } else {
            format!("{base}/ehdb/tiers/{tier}?limit={page}&after={after}")
        };
        let resp = http
            .get(&url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| AppError::ExternalService(format!("{uri}: {e}")))?;
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            // One page over the relay's frame cap: halve and retry.
            if text.contains("exceeds the") && text.contains("cap") && page > 1 {
                page = (page / 2).max(1);
                continue;
            }
            return Err(AppError::ExternalService(format!(
                "{uri}: tier read failed: {}",
                text.chars().take(200).collect::<String>()
            )));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| AppError::ExternalService(format!("{uri}: {e}")))?;
        if let Some(status) = body.get("status").and_then(Value::as_str) {
            if status == "disabled" || status == "guard_refused" {
                return Err(AppError::ExternalService(format!(
                    "{uri}: the EHDB catalog tier is {status} on this cluster"
                )));
            }
        }
        let recs = body
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if recs.is_empty() {
            break;
        }
        if let Some(found) = pick_record(&recs, id) {
            return found;
        }
        let next = recs
            .iter()
            .filter_map(|r| r.get("global_sequence").and_then(Value::as_u64))
            .max()
            .unwrap_or(0);
        if recs.len() < page || next <= after {
            break;
        }
        after = next;
    }
    Err(AppError::NotFound(format!(
        "{uri}: no catalog.registered record for catalog_id {id} in the EHDB catalog tier"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn db_uris_parse_and_refuse_by_name() {
        let s = parse_db("db://ext_pg/pub_lib.playbooks?path=a/b&version=3#sha256:x").unwrap();
        assert_eq!(
            s,
            DbSource {
                alias: "ext_pg".into(),
                schema: "pub_lib".into(),
                table: "playbooks".into(),
                path: "a/b".into(),
                version: Some(3)
            }
        );
        assert_eq!(parse_db("db://a/s.t?path=x%2Fy").unwrap().path, "x/y");
        for (u, why) in [
            ("db://u:p@h/s.t?path=a", "credential alias"),
            ("db://a/s.t", "path="),
            ("db://a/st?path=a", "<schema>.<table>"),
            ("db://a/s.t;drop?path=a", "identifiers"),
            ("db://a/s.\"t\"?path=a", "identifiers"),
            ("db://a/s.t?path=a&version=0", "positive"),
            ("db://a/s.t?path=a&version=x", "positive"),
        ] {
            let e = parse_db(u).unwrap_err().to_string();
            assert!(e.contains(why), "{u}: {e}");
        }
    }

    #[test]
    fn db_queries_bind_values_and_quote_identifiers() {
        let mut s = parse_db("db://a/lib.pb?path=x").unwrap();
        assert_eq!(
            db_query(&s),
            "SELECT content::text FROM \"lib\".\"pb\" WHERE path = $1 ORDER BY version DESC LIMIT 1"
        );
        s.version = Some(2);
        assert!(db_query(&s).ends_with("WHERE path = $1 AND version = $2 LIMIT 1"));
    }

    #[test]
    fn credential_data_shapes() {
        assert!(connect_options(
            "a",
            &json!({"host":"h","user":"u","database":"d","port":"5433","password":"p"})
        )
        .is_ok());
        assert!(connect_options("a", &json!({"host":"h","username":"u","dbname":"d"})).is_ok());
        assert!(
            connect_options("a", &json!({"connection_string":"postgres://u@h:5432/d"})).is_ok()
        );
        assert!(connect_options("a", &json!({"user":"u"}))
            .unwrap_err()
            .to_string()
            .contains("needs host"));
    }

    #[test]
    fn ehdb_uris() {
        assert_eq!(parse_ehdb("ehdb://catalog/42").unwrap(), 42);
        assert_eq!(parse_ehdb("ehdb://catalog/42#sha256:abc").unwrap(), 42);
        assert!(parse_ehdb("ehdb://events/1")
            .unwrap_err()
            .to_string()
            .contains("namespace"));
        assert!(parse_ehdb("ehdb://catalog/x").is_err());
        assert!(parse_ehdb("ehdb://catalog/0").is_err());
    }

    #[test]
    fn ehdb_records_are_matched_and_checked() {
        let content = "kind: Playbook\n";
        let sha = crate::handlers::catalog_snapshot::sha256_hex(content.as_bytes());
        let rec = |id: &str, sha: &str| {
            json!({"global_sequence": 7, "payload": json!({
                "event_type": "catalog.registered", "catalog_id": id,
                "content": content, "content_sha256": sha,
            }).to_string()})
        };
        let page = vec![rec("41", &sha), rec("42", &sha)];
        assert_eq!(pick_record(&page, 42).unwrap().unwrap(), content);
        assert!(pick_record(&page, 43).is_none());
        let tampered = vec![rec("42", "00")];
        assert!(pick_record(&tampered, 42).unwrap().is_err());
    }
}
