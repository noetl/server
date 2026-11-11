//! Content-addressed catalog core: playbook digests, references, and the
//! execution-pinning resolver.
//!
//! Pure and sans-IO, so it builds for `wasm32-unknown-unknown` and is shared by
//! the server, the CLI and the WASM runner. The resolver does not read a store.
//! [`plan`] turns a request plus the execution's [`Pin`] into one [`Lookup`].
//! The caller runs that lookup against its own store (Postgres, memory, EHDB)
//! and passes the row back to [`finish`].
//!
//! Design: `ai-meta/docs/rfc/distributed-playbook-catalog.md` (noetl/ai-meta#548).
//!
//! # Invariants
//!
//! - **The digest is the identity.** [`playbook_digest`] hashes the YAML's JSON
//!   value with keys sorted ([`crate::dsl::digest`]), so formatting and comments
//!   never change it, while any value change does.
//! - **A pinned execution never sees a row registered after its root started.**
//!   [`Lookup::Pinned`] bounds the search by the root's `catalog_id` watermark.
//! - **`@latest` inside a pinned tree is refused unless hot-reload is allowed
//!   three ways** (server flag, the root's policy, the explicit `@latest`).
//!   The resolver never quietly downgrades to the pinned version, because then
//!   the author would never learn that `@latest` was not honoured.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// Prefix every playbook digest carries.
pub const DIGEST_PREFIX: &str = "sha256:";

/// Errors from the pure catalog core. Each one names what was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// The YAML did not parse, or has no JSON form (non-string keys, tags).
    BadContent(String),
    /// The reference string is not one of the accepted forms.
    BadRef(String),
    /// `@latest` was asked for inside a pinned tree without permission.
    HotReloadRefused { path: String, why: &'static str },
    /// The lookup found nothing.
    NotFound(String),
    /// A row was found but its body has a different digest than demanded.
    DigestMismatch { expected: String, actual: String },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::BadContent(m) => write!(f, "playbook content has no digest: {m}"),
            CatalogError::BadRef(m) => write!(f, "invalid playbook reference: {m}"),
            CatalogError::HotReloadRefused { path, why } => write!(
                f,
                "'{path}@latest' asked for hot reload inside a pinned execution, refused: {why}"
            ),
            CatalogError::NotFound(m) => write!(f, "playbook not found: {m}"),
            CatalogError::DigestMismatch { expected, actual } => {
                write!(f, "digest mismatch: expected {expected}, got {actual}")
            }
        }
    }
}

impl std::error::Error for CatalogError {}

// ---------------------------------------------------------------------------
// Digest
// ---------------------------------------------------------------------------

/// `sha256:<hex>` of the playbook's JSON value with keys sorted at every level.
///
/// Refuses YAML with no JSON form, so a digest is always reproducible from the
/// JSON a peer or a WASM module carries.
pub fn playbook_digest(yaml: &str) -> Result<String, CatalogError> {
    Ok(crate::dsl::digest(&playbook_value(yaml)?))
}

/// Parse YAML into the JSON value the digest is computed over.
pub fn playbook_value(yaml: &str) -> Result<Value, CatalogError> {
    let y: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|e| CatalogError::BadContent(e.to_string()))?;
    yaml_to_json(&y)
}

fn yaml_to_json(y: &serde_yaml::Value) -> Result<Value, CatalogError> {
    use serde_yaml::Value as Y;
    Ok(match y {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(*b),
        Y::Number(n) => {
            serde_json::to_value(n).map_err(|e| CatalogError::BadContent(e.to_string()))?
        }
        Y::String(s) => Value::String(s.clone()),
        Y::Sequence(items) => {
            Value::Array(items.iter().map(yaml_to_json).collect::<Result<_, _>>()?)
        }
        Y::Mapping(m) => {
            let mut out = serde_json::Map::new();
            for (k, v) in m {
                let key = match k {
                    Y::String(s) => s.clone(),
                    other => {
                        return Err(CatalogError::BadContent(format!(
                            "mapping key {other:?} is not a string"
                        )))
                    }
                };
                out.insert(key, yaml_to_json(v)?);
            }
            Value::Object(out)
        }
        Y::Tagged(t) => {
            return Err(CatalogError::BadContent(format!(
                "YAML tag {} has no JSON form",
                t.tag
            )))
        }
    })
}

/// True for `sha256:` followed by exactly 64 lowercase hex characters.
pub fn is_digest(s: &str) -> bool {
    s.strip_prefix(DIGEST_PREFIX).is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

/// Which version of a named playbook a reference asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionSel {
    /// No version given: the pinned version inside a tree, else latest.
    Default,
    /// `@latest`, explicitly. A hot-reload request inside a pinned tree.
    Latest,
    /// `@3`.
    Exact(i32),
    /// `@sha256:…`: this name, and it must have this body.
    Digest(String),
}

/// URI schemes a source reference may use (RFC §3.3).
pub const SOURCE_SCHEMES: &[&str] = &[
    "noetl",
    "gs",
    "s3",
    "az",
    "minio+http",
    "minio+https",
    "https",
    "http",
    "db",
    "ehdb",
    "file",
];

/// A parsed playbook reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlaybookRef {
    /// A catalog name with a version selector.
    Name { path: String, version: VersionSel },
    /// A bare body digest, under any name.
    Digest { digest: String },
    /// A source URI. `expect` is the `#sha256:` fragment, if given.
    Source {
        scheme: String,
        uri: String,
        expect: Option<String>,
    },
}

impl PlaybookRef {
    /// Parse one of the RFC §3.3 forms.
    pub fn parse(s: &str) -> Result<Self, CatalogError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(CatalogError::BadRef("empty".into()));
        }
        if is_digest(s) {
            return Ok(PlaybookRef::Digest {
                digest: s.to_string(),
            });
        }
        if s.starts_with(DIGEST_PREFIX) {
            return Err(CatalogError::BadRef(format!(
                "'{s}' is not sha256: + 64 lowercase hex"
            )));
        }
        if let Some((scheme, rest)) = s.split_once("://") {
            if !SOURCE_SCHEMES.contains(&scheme) {
                return Err(CatalogError::BadRef(format!(
                    "unknown scheme '{scheme}://'"
                )));
            }
            let (body, expect) = match rest.rsplit_once('#') {
                Some((b, frag)) => {
                    if !is_digest(frag) {
                        return Err(CatalogError::BadRef(format!(
                            "fragment '#{frag}' is not a sha256 digest"
                        )));
                    }
                    (b, Some(frag.to_string()))
                }
                None => (rest, None),
            };
            let authority = body.split('/').next().unwrap_or("");
            if authority.contains('@') && scheme != "noetl" {
                return Err(CatalogError::BadRef(
                    "credentials in a source URI are refused; use a keychain alias".into(),
                ));
            }
            if body.is_empty() || (scheme != "file" && authority.is_empty()) {
                return Err(CatalogError::BadRef(format!("'{s}' has no location")));
            }
            return Ok(PlaybookRef::Source {
                scheme: scheme.to_string(),
                uri: format!("{scheme}://{body}"),
                expect,
            });
        }
        let (path, version) = match s.split_once('@') {
            None => (s, VersionSel::Default),
            Some((p, "latest")) => (p, VersionSel::Latest),
            Some((p, v)) if is_digest(v) => (p, VersionSel::Digest(v.to_string())),
            Some((p, v)) => match v.parse::<i32>() {
                Ok(n) if n > 0 => (p, VersionSel::Exact(n)),
                _ => {
                    return Err(CatalogError::BadRef(format!(
                        "version '{v}' is not a positive number, 'latest' or a digest"
                    )))
                }
            },
        };
        validate_path(path)?;
        Ok(PlaybookRef::Name {
            path: path.to_string(),
            version,
        })
    }

    /// What `/api/execute` resolves: `path` may itself be a reference
    /// (`a/b@3`, `a/b@latest`, `sha256:…`), because the playbook tool's
    /// `version` field is numeric and cannot say `latest`. A version given both
    /// in `path` and in `version`/`digest` is refused, not merged.
    pub fn from_request(
        path: &str,
        version: Option<&Value>,
        digest: Option<&str>,
    ) -> Result<Self, CatalogError> {
        let given = |v: Option<&Value>| v.is_some_and(|v| !v.is_null());
        if path.contains('@') || path.starts_with(DIGEST_PREFIX) || path.contains("://") {
            if given(version) || digest.is_some() {
                return Err(CatalogError::BadRef(format!(
                    "'{path}' already names a version; drop version/digest or the '@' suffix"
                )));
            }
            return Self::parse(path);
        }
        Self::from_fields(path, version, digest)
    }

    /// Build from the fields `/api/execute` (and the playbook tool) send.
    /// `version` may be a number or `"latest"`; a `digest` wins over it.
    pub fn from_fields(
        path: &str,
        version: Option<&Value>,
        digest: Option<&str>,
    ) -> Result<Self, CatalogError> {
        validate_path(path)?;
        let version = match (digest, version) {
            (Some(d), _) if is_digest(d) => VersionSel::Digest(d.to_string()),
            (Some(d), _) => {
                return Err(CatalogError::BadRef(format!(
                    "digest '{d}' is not sha256: + 64 hex"
                )))
            }
            (None, None) | (None, Some(Value::Null)) => VersionSel::Default,
            (None, Some(Value::Number(n))) => match n.as_i64() {
                Some(v) if v > 0 && v <= i32::MAX as i64 => VersionSel::Exact(v as i32),
                _ => {
                    return Err(CatalogError::BadRef(format!(
                        "version {n} is not a positive integer"
                    )))
                }
            },
            (None, Some(Value::String(s))) if s == "latest" => VersionSel::Latest,
            (None, Some(Value::String(s))) => match s.parse::<i32>() {
                Ok(v) if v > 0 => VersionSel::Exact(v),
                _ => {
                    return Err(CatalogError::BadRef(format!(
                        "version '{s}' is not a positive number or 'latest'"
                    )))
                }
            },
            (None, Some(other)) => {
                return Err(CatalogError::BadRef(format!(
                    "version {other} is not a number or 'latest'"
                )))
            }
        };
        Ok(PlaybookRef::Name {
            path: path.to_string(),
            version,
        })
    }
}

fn validate_path(p: &str) -> Result<(), CatalogError> {
    if p.is_empty()
        || p.len() > 512
        || p.starts_with('/')
        || p.contains("..")
        || p.chars().any(|c| c.is_control() || c == '@' || c == '#')
    {
        return Err(CatalogError::BadRef(format!("'{p}' is not a catalog path")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pin and policy
// ---------------------------------------------------------------------------

/// The root playbook's hot-reload policy: `metadata.catalog.hot_reload` globs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinPolicy {
    #[serde(default)]
    pub hot_reload: Vec<String>,
}

impl PinPolicy {
    /// Read the policy from the playbook's JSON value. Anything malformed is
    /// ignored entry by entry: a typo can only make the policy narrower.
    pub fn from_playbook(v: &Value) -> Self {
        let hot_reload = v
            .pointer("/metadata/catalog/hot_reload")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.is_empty() && s.len() <= 512)
                    .take(64)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        PinPolicy { hot_reload }
    }

    pub fn allows(&self, path: &str) -> bool {
        self.hot_reload.iter().any(|g| glob_match(g, path))
    }
}

/// `*` matches any run of characters, including `/`. Nothing else is special.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || s.len() < first.len() + last.len() || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// Why a resolution produced the row it did. Stored on every pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Exact,
    Digest,
    Pinned,
    Latest,
    HotReload,
    Import,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Exact => "exact",
            Reason::Digest => "digest",
            Reason::Pinned => "pinned",
            Reason::Latest => "latest",
            Reason::HotReload => "hot_reload",
            Reason::Import => "import",
        }
    }
}

/// The inherited part of an execution's pin, which every resolution in the
/// tree uses: the root's watermark, the root's start time and the root's policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    pub root_execution_id: i64,
    /// `max(catalog_id)` when the root started.
    pub watermark: i64,
    /// Root start, ms since epoch. Versions archived before it are invisible.
    pub pinned_at_ms: i64,
    #[serde(default)]
    pub policy: PinPolicy,
}

/// A catalog row as the resolver sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub catalog_id: i64,
    pub path: String,
    pub version: i32,
    pub digest: String,
}

/// The one store query a resolution needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "lookup", rename_all = "snake_case")]
pub enum Lookup {
    /// `path` at exactly `version`, archived or not (an exact ask is honoured).
    Exact { path: String, version: i32 },
    /// The highest-version row with this body; if `path` is given, under that name.
    Digest {
        digest: String,
        path: Option<String>,
    },
    /// Highest version of `path` with `catalog_id <= watermark`, excluding rows
    /// archived before `pinned_at_ms`.
    Pinned {
        path: String,
        watermark: i64,
        pinned_at_ms: i64,
    },
    /// Highest non-archived version of `path` now.
    Latest { path: String },
    /// Not a catalog lookup: fetch from a source (Phase 2).
    Source {
        scheme: String,
        uri: String,
        expect: Option<String>,
    },
}

/// What a resolution yields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub entry: Entry,
    pub reason: Reason,
}

/// Plan the lookup for `r` inside an execution tree pinned by `pin`.
///
/// `hot_reload_enabled` is the server flag (`NOETL_CATALOG_HOT_RELOAD`).
pub fn plan(
    r: &PlaybookRef,
    pin: Option<&Pin>,
    hot_reload_enabled: bool,
) -> Result<(Lookup, Reason), CatalogError> {
    Ok(match r {
        PlaybookRef::Digest { digest } => (
            Lookup::Digest {
                digest: digest.clone(),
                path: None,
            },
            Reason::Digest,
        ),
        PlaybookRef::Source {
            scheme,
            uri,
            expect,
        } => (
            Lookup::Source {
                scheme: scheme.clone(),
                uri: uri.clone(),
                expect: expect.clone(),
            },
            Reason::Import,
        ),
        PlaybookRef::Name { path, version } => match version {
            VersionSel::Exact(v) => (
                Lookup::Exact {
                    path: path.clone(),
                    version: *v,
                },
                Reason::Exact,
            ),
            VersionSel::Digest(d) => (
                Lookup::Digest {
                    digest: d.clone(),
                    path: Some(path.clone()),
                },
                Reason::Digest,
            ),
            VersionSel::Default => match pin {
                Some(p) => (
                    Lookup::Pinned {
                        path: path.clone(),
                        watermark: p.watermark,
                        pinned_at_ms: p.pinned_at_ms,
                    },
                    Reason::Pinned,
                ),
                None => (Lookup::Latest { path: path.clone() }, Reason::Latest),
            },
            VersionSel::Latest => match pin {
                None => (Lookup::Latest { path: path.clone() }, Reason::Latest),
                Some(_) if !hot_reload_enabled => {
                    return Err(CatalogError::HotReloadRefused {
                        path: path.clone(),
                        why: "NOETL_CATALOG_HOT_RELOAD is off on this server",
                    })
                }
                Some(p) if !p.policy.allows(path) => return Err(CatalogError::HotReloadRefused {
                    path: path.clone(),
                    why: "the root playbook's metadata.catalog.hot_reload does not list this path",
                }),
                Some(_) => (Lookup::Latest { path: path.clone() }, Reason::HotReload),
            },
        },
    })
}

/// Check the row a lookup returned and produce the resolution.
pub fn finish(
    lookup: &Lookup,
    reason: Reason,
    found: Option<Entry>,
) -> Result<Resolution, CatalogError> {
    let entry = found.ok_or_else(|| CatalogError::NotFound(describe(lookup)))?;
    if let Lookup::Digest { digest, .. } = lookup {
        if &entry.digest != digest {
            return Err(CatalogError::DigestMismatch {
                expected: digest.clone(),
                actual: entry.digest,
            });
        }
    }
    Ok(Resolution { entry, reason })
}

/// Human description of a lookup, for errors and logs.
pub fn describe(l: &Lookup) -> String {
    match l {
        Lookup::Exact { path, version } => format!("{path}@{version}"),
        Lookup::Digest {
            digest,
            path: Some(p),
        } => format!("{p}@{digest}"),
        Lookup::Digest { digest, path: None } => digest.clone(),
        Lookup::Pinned {
            path, watermark, ..
        } => format!("{path} (pinned at catalog_id <= {watermark})"),
        Lookup::Latest { path } => format!("{path}@latest"),
        Lookup::Source { uri, .. } => uri.clone(),
    }
}

// ---------------------------------------------------------------------------
// Discovery keys (RFC §4.2) and imports (§4.4)
// ---------------------------------------------------------------------------

/// The three bloom keys an entry is advertised under: `p:<path>`,
/// `v:<path>@<version>`, `d:<digest>`.
pub fn advert_keys(path: &str, version: i32, digest: &str) -> [String; 3] {
    [
        format!("p:{path}"),
        format!("v:{path}@{version}"),
        format!("d:{digest}"),
    ]
}

/// The bloom key a reference is probed with, or `None` when a reference names
/// its source (it is fetched directly, never probed).
pub fn probe_key(r: &PlaybookRef) -> Option<String> {
    match r {
        PlaybookRef::Digest { digest } => Some(format!("d:{digest}")),
        PlaybookRef::Name { path, version } => Some(match version {
            VersionSel::Exact(v) => format!("v:{path}@{v}"),
            VersionSel::Digest(d) => format!("d:{d}"),
            VersionSel::Default | VersionSel::Latest => format!("p:{path}"),
        }),
        PlaybookRef::Source { .. } => None,
    }
}

/// Where a peer's playbook is imported locally: `peer/<instance-id>/<path>`, so
/// an imported copy can never shadow a local path of the same name.
pub fn import_path(instance_id: &str, path: &str) -> String {
    format!("peer/{instance_id}/{path}")
}

/// A `noetl://<instance>/<path>[@<version>]` source, split. `local` is this
/// catalog.
pub fn split_noetl_uri(uri: &str) -> Result<(String, PlaybookRef), CatalogError> {
    let rest = uri
        .strip_prefix("noetl://")
        .ok_or_else(|| CatalogError::BadRef(format!("'{uri}' is not a noetl:// URI")))?;
    let (instance, reference) = rest
        .split_once('/')
        .ok_or_else(|| CatalogError::BadRef(format!("'{uri}' names no playbook")))?;
    if instance.is_empty() {
        return Err(CatalogError::BadRef(format!("'{uri}' names no instance")));
    }
    let r = PlaybookRef::parse(reference)?;
    if matches!(r, PlaybookRef::Source { .. }) {
        return Err(CatalogError::BadRef(format!("'{uri}' nests a URI")));
    }
    Ok((instance.to_string(), r))
}

/// Verify a fetched body against the digest it must have (if any) and return
/// its digest. The digest of what was fetched is what gets pinned.
pub fn verify_fetched(content: &str, expect: Option<&str>) -> Result<String, CatalogError> {
    let actual = playbook_digest(content)?;
    match expect {
        Some(e) if e != actual => Err(CatalogError::DigestMismatch {
            expected: e.to_string(),
            actual,
        }),
        _ => Ok(actual),
    }
}

// ---------------------------------------------------------------------------
// In-memory store (tests, the CLI, the WASM runner)
// ---------------------------------------------------------------------------

/// A row with the fields the in-memory store needs to answer every [`Lookup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemRow {
    pub entry: Entry,
    pub archived_at_ms: Option<i64>,
}

/// An in-memory catalog that answers [`Lookup`]s the way the Postgres store must.
/// The server's SQL is tested against the same cases (`tests/catalog_cas_*.rs`).
#[derive(Debug, Clone, Default)]
pub struct MemoryCatalog {
    pub rows: Vec<MemRow>,
}

impl MemoryCatalog {
    pub fn lookup(&self, l: &Lookup) -> Option<Entry> {
        let best = |it: &mut dyn Iterator<Item = &MemRow>| {
            it.max_by_key(|r| (r.entry.version, r.entry.catalog_id))
                .map(|r| r.entry.clone())
        };
        match l {
            Lookup::Exact { path, version } => self
                .rows
                .iter()
                .find(|r| &r.entry.path == path && r.entry.version == *version)
                .map(|r| r.entry.clone()),
            Lookup::Digest { digest, path } => best(&mut self.rows.iter().filter(|r| {
                &r.entry.digest == digest && path.as_ref().is_none_or(|p| &r.entry.path == p)
            })),
            Lookup::Pinned {
                path,
                watermark,
                pinned_at_ms,
            } => best(&mut self.rows.iter().filter(|r| {
                &r.entry.path == path
                    && r.entry.catalog_id <= *watermark
                    && r.archived_at_ms.is_none_or(|a| a > *pinned_at_ms)
            })),
            Lookup::Latest { path } => best(
                &mut self
                    .rows
                    .iter()
                    .filter(|r| &r.entry.path == path && r.archived_at_ms.is_none()),
            ),
            Lookup::Source { .. } => None,
        }
    }

    /// Plan, look up, finish.
    pub fn resolve(
        &self,
        r: &PlaybookRef,
        pin: Option<&Pin>,
        hot_reload: bool,
    ) -> Result<Resolution, CatalogError> {
        let (l, reason) = plan(r, pin, hot_reload)?;
        finish(&l, reason, self.lookup(&l))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PB: &str = "apiVersion: noetl.io/v2\nkind: Playbook\nmetadata:\n  name: a\n  path: a/b\nworkflow:\n  - step: start\n    tool: {kind: noop}\n";

    fn d(n: u8) -> String {
        format!("sha256:{}", format!("{n:02x}").repeat(32))
    }

    fn row(id: i64, path: &str, v: i32, digest: &str, archived: Option<i64>) -> MemRow {
        MemRow {
            entry: Entry {
                catalog_id: id,
                path: path.into(),
                version: v,
                digest: digest.into(),
            },
            archived_at_ms: archived,
        }
    }

    fn pin(watermark: i64, hot: &[&str]) -> Pin {
        Pin {
            root_execution_id: 1,
            watermark,
            pinned_at_ms: 1_000,
            policy: PinPolicy {
                hot_reload: hot.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    #[test]
    fn digest_ignores_formatting_and_comments_but_not_values() {
        let reformatted = "# a comment\nkind: Playbook\napiVersion: 'noetl.io/v2'\nworkflow:\n- tool: {kind: noop}\n  step: start\nmetadata: {path: a/b, name: a}\n";
        assert_eq!(
            playbook_digest(PB).unwrap(),
            playbook_digest(reformatted).unwrap()
        );
        let changed = PB.replace("noop", "rhai");
        assert_ne!(
            playbook_digest(PB).unwrap(),
            playbook_digest(&changed).unwrap()
        );
        assert!(is_digest(&playbook_digest(PB).unwrap()));
    }

    #[test]
    fn digest_is_the_dsl_digest_of_the_json_value() {
        let v = playbook_value(PB).unwrap();
        assert_eq!(playbook_digest(PB).unwrap(), crate::dsl::digest(&v));
    }

    #[test]
    fn digest_refuses_yaml_with_no_json_form() {
        assert!(matches!(
            playbook_digest("? [1, 2]\n: x\n"),
            Err(CatalogError::BadContent(_))
        ));
        assert!(matches!(
            playbook_digest("a: !custom 1\n"),
            Err(CatalogError::BadContent(_))
        ));
        assert!(matches!(
            playbook_digest("a: [\n"),
            Err(CatalogError::BadContent(_))
        ));
    }

    #[test]
    fn refs_parse_every_documented_form() {
        use PlaybookRef as R;
        assert_eq!(
            R::parse("a/b").unwrap(),
            R::Name {
                path: "a/b".into(),
                version: VersionSel::Default
            }
        );
        assert_eq!(
            R::parse("a/b@3").unwrap(),
            R::Name {
                path: "a/b".into(),
                version: VersionSel::Exact(3)
            }
        );
        assert_eq!(
            R::parse("a/b@latest").unwrap(),
            R::Name {
                path: "a/b".into(),
                version: VersionSel::Latest
            }
        );
        assert_eq!(
            R::parse(&format!("a/b@{}", d(1))).unwrap(),
            R::Name {
                path: "a/b".into(),
                version: VersionSel::Digest(d(1))
            }
        );
        assert_eq!(R::parse(&d(2)).unwrap(), R::Digest { digest: d(2) });
        assert_eq!(
            R::parse(&format!("gs://bkt/pb/a.yaml#{}", d(3))).unwrap(),
            R::Source {
                scheme: "gs".into(),
                uri: "gs://bkt/pb/a.yaml".into(),
                expect: Some(d(3))
            }
        );
        assert_eq!(
            R::parse("noetl://ni-0123456789abcdef0123456789abcdef/a/b@2").unwrap(),
            R::Source {
                scheme: "noetl".into(),
                uri: "noetl://ni-0123456789abcdef0123456789abcdef/a/b@2".into(),
                expect: None
            }
        );
        for s in [
            "s3://b/k",
            "az://c/k",
            "minio+http://host:9000/b/k",
            "https://h/p.yaml",
            "db://pg_alias/s.t?path=a",
            "ehdb://catalog/42",
            "file:///tmp/a.yaml",
        ] {
            assert!(matches!(R::parse(s), Ok(R::Source { .. })), "{s}");
        }
    }

    #[test]
    fn refs_refuse_malformed_and_credentialed_input() {
        for s in [
            "",
            "a/b@0",
            "a/b@-1",
            "a/b@x",
            "sha256:abc",
            "ftp://h/x",
            "s3://user:pw@b/k",
            "/abs/path",
            "a/../b",
            "gs://",
            "https://h/p#sha256:zz",
        ] {
            assert!(PlaybookRef::parse(s).is_err(), "{s} should be refused");
        }
    }

    #[test]
    fn from_request_accepts_a_reference_in_path_but_not_twice() {
        use PlaybookRef as R;
        assert_eq!(
            R::from_request("a@latest", None, None).unwrap(),
            R::Name {
                path: "a".into(),
                version: VersionSel::Latest
            }
        );
        assert_eq!(
            R::from_request("a@3", Some(&Value::Null), None).unwrap(),
            R::Name {
                path: "a".into(),
                version: VersionSel::Exact(3)
            }
        );
        assert_eq!(
            R::from_request(&d(9), None, None).unwrap(),
            R::Digest { digest: d(9) }
        );
        assert!(R::from_request("a@3", Some(&json!(4)), None).is_err());
        assert!(R::from_request("a@3", None, Some(&d(1))).is_err());
        assert_eq!(
            R::from_request("a", Some(&json!(4)), None).unwrap(),
            R::Name {
                path: "a".into(),
                version: VersionSel::Exact(4)
            }
        );
    }

    #[test]
    fn from_fields_honours_version_and_digest() {
        use PlaybookRef as R;
        assert_eq!(
            R::from_fields("a", Some(&json!(2)), None).unwrap(),
            R::Name {
                path: "a".into(),
                version: VersionSel::Exact(2)
            }
        );
        assert_eq!(
            R::from_fields("a", Some(&json!("2")), None).unwrap(),
            R::Name {
                path: "a".into(),
                version: VersionSel::Exact(2)
            }
        );
        assert_eq!(
            R::from_fields("a", Some(&json!("latest")), None).unwrap(),
            R::Name {
                path: "a".into(),
                version: VersionSel::Latest
            }
        );
        assert_eq!(
            R::from_fields("a", None, Some(&d(4))).unwrap(),
            R::Name {
                path: "a".into(),
                version: VersionSel::Digest(d(4))
            }
        );
        assert_eq!(
            R::from_fields("a", Some(&Value::Null), None).unwrap(),
            R::Name {
                path: "a".into(),
                version: VersionSel::Default
            }
        );
        assert!(R::from_fields("a", Some(&json!(0)), None).is_err());
        assert!(R::from_fields("a", Some(&json!(true)), None).is_err());
        assert!(R::from_fields("a", None, Some("sha256:nope")).is_err());
    }

    fn store() -> MemoryCatalog {
        MemoryCatalog {
            rows: vec![
                row(100, "child", 1, &d(1), None),
                row(200, "child", 2, &d(2), None),
                // registered after the root started (watermark 250):
                row(300, "child", 3, &d(3), None),
                row(110, "gone", 1, &d(5), Some(500)), // archived before the pin (1000)
                row(120, "late", 1, &d(6), Some(2_000)), // archived after the pin
                row(130, "alias", 1, &d(2), None),     // same body as child@2
            ],
        }
    }

    #[test]
    fn a_pinned_child_does_not_see_versions_registered_after_the_root_started() {
        let s = store();
        let r = s
            .resolve(
                &PlaybookRef::parse("child").unwrap(),
                Some(&pin(250, &[])),
                false,
            )
            .unwrap();
        assert_eq!((r.entry.version, r.reason), (2, Reason::Pinned));
        let unpinned = s
            .resolve(&PlaybookRef::parse("child").unwrap(), None, false)
            .unwrap();
        assert_eq!(
            (unpinned.entry.version, unpinned.reason),
            (3, Reason::Latest)
        );
    }

    #[test]
    fn archive_never_breaks_a_running_execution_but_hides_rows_archived_before_it() {
        let s = store();
        assert!(matches!(
            s.resolve(
                &PlaybookRef::parse("gone").unwrap(),
                Some(&pin(250, &[])),
                false
            ),
            Err(CatalogError::NotFound(_))
        ));
        assert_eq!(
            s.resolve(
                &PlaybookRef::parse("late").unwrap(),
                Some(&pin(250, &[])),
                false
            )
            .unwrap()
            .entry
            .version,
            1
        );
        // Unpinned, both are archived now.
        assert!(s
            .resolve(&PlaybookRef::parse("late").unwrap(), None, false)
            .is_err());
    }

    #[test]
    fn exact_and_digest_requests_win_over_the_pin() {
        let s = store();
        let p = pin(250, &[]);
        let r = s
            .resolve(&PlaybookRef::parse("child@3").unwrap(), Some(&p), false)
            .unwrap();
        assert_eq!((r.entry.version, r.reason), (3, Reason::Exact));
        let r = s
            .resolve(&PlaybookRef::parse(&d(3)).unwrap(), Some(&p), false)
            .unwrap();
        assert_eq!((r.entry.catalog_id, r.reason), (300, Reason::Digest));
        let named = s
            .resolve(
                &PlaybookRef::parse(&format!("alias@{}", d(2))).unwrap(),
                Some(&p),
                false,
            )
            .unwrap();
        assert_eq!(named.entry.catalog_id, 130);
        assert!(s
            .resolve(
                &PlaybookRef::parse(&format!("alias@{}", d(1))).unwrap(),
                Some(&p),
                false
            )
            .is_err());
    }

    #[test]
    fn latest_inside_a_pinned_tree_needs_all_three_controls() {
        let s = store();
        let r = PlaybookRef::parse("child@latest").unwrap();
        // 1. flag off
        assert!(matches!(
            s.resolve(&r, Some(&pin(250, &["child"])), false),
            Err(CatalogError::HotReloadRefused { .. })
        ));
        // 2. flag on, policy does not list it
        assert!(matches!(
            s.resolve(&r, Some(&pin(250, &["other/*"])), true),
            Err(CatalogError::HotReloadRefused { .. })
        ));
        // 3. all three
        let ok = s.resolve(&r, Some(&pin(250, &["chi*"])), true).unwrap();
        assert_eq!((ok.entry.version, ok.reason), (3, Reason::HotReload));
        // Without `@latest` the policy alone changes nothing.
        let pinned = s
            .resolve(
                &PlaybookRef::parse("child").unwrap(),
                Some(&pin(250, &["child"])),
                true,
            )
            .unwrap();
        assert_eq!(pinned.entry.version, 2);
        // A root execution asking @latest is plain latest.
        assert_eq!(s.resolve(&r, None, false).unwrap().reason, Reason::Latest);
    }

    #[test]
    fn policy_reads_globs_from_metadata_and_ignores_junk() {
        let v = json!({"metadata": {"catalog": {"hot_reload": ["ops/*", 7, "", "x/y"]}}});
        let p = PinPolicy::from_playbook(&v);
        assert_eq!(p.hot_reload, vec!["ops/*", "x/y"]);
        assert!(p.allows("ops/a/b") && p.allows("x/y") && !p.allows("x/y/z"));
        assert_eq!(
            PinPolicy::from_playbook(&json!({"metadata": {}})),
            PinPolicy::default()
        );
    }

    #[test]
    fn advert_and_probe_keys_agree() {
        let keys = advert_keys("a/b", 3, &d(1));
        let probe = |s: &str| probe_key(&PlaybookRef::parse(s).unwrap()).unwrap();
        assert!(keys.contains(&probe("a/b")));
        assert!(keys.contains(&probe("a/b@latest")));
        assert!(keys.contains(&probe("a/b@3")));
        assert!(keys.contains(&probe(&d(1))));
        assert!(keys.contains(&probe(&format!("x/y@{}", d(1)))));
        assert!(!keys.contains(&probe("a/b@4")));
        assert_eq!(probe_key(&PlaybookRef::parse("gs://b/k").unwrap()), None);
    }

    #[test]
    fn noetl_uris_split_into_instance_and_reference() {
        let (i, r) = split_noetl_uri("noetl://ni-abc/a/b@2").unwrap();
        assert_eq!(
            (i.as_str(), r),
            (
                "ni-abc",
                PlaybookRef::Name {
                    path: "a/b".into(),
                    version: VersionSel::Exact(2)
                }
            )
        );
        let (_, r) = split_noetl_uri(&format!("noetl://local/{}", d(2))).unwrap();
        assert_eq!(r, PlaybookRef::Digest { digest: d(2) });
        assert!(split_noetl_uri("noetl:///a").is_err());
        assert!(split_noetl_uri("noetl://ni-x").is_err());
        assert!(split_noetl_uri("noetl://ni-x/gs://b/k").is_err());
        assert_eq!(import_path("ni-x", "a/b"), "peer/ni-x/a/b");
    }

    #[test]
    fn fetched_bodies_are_verified() {
        let good = playbook_digest(PB).unwrap();
        assert_eq!(verify_fetched(PB, Some(&good)).unwrap(), good);
        assert_eq!(verify_fetched(PB, None).unwrap(), good);
        assert!(matches!(
            verify_fetched(PB, Some(&d(7))),
            Err(CatalogError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn glob_semantics() {
        assert!(glob_match("a/*", "a/b/c"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(!glob_match("a*b*c", "aXcYb"));
        assert!(!glob_match("ab*ba", "aba"));
        assert!(glob_match("exact", "exact") && !glob_match("exact", "exactly"));
    }

    #[test]
    fn source_lookups_are_never_answered_by_the_catalog() {
        let r = PlaybookRef::parse("gs://b/k.yaml").unwrap();
        let (l, reason) = plan(&r, None, false).unwrap();
        assert_eq!(reason, Reason::Import);
        assert!(store().lookup(&l).is_none());
    }
}
