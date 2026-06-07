//! AWS STS `AssumeRoleWithWebIdentity` dynamic-secret provider
//! (Secrets Wallet Phase 6d.1, noetl/ai-meta#61).
//!
//! Exchanges the EKS-projected ServiceAccount JWT (`AWS_WEB_IDENTITY_TOKEN_FILE`)
//! for short-lived AWS temporary credentials via STS.  The returned
//! `SecretValue` carries the issuer-reported `expires_at`, so the Phase 6d
//! cache-decision honours the deadline + the Phase 7c.3 background refresh
//! re-resolves before the credentials expire.
//!
//! ## Why no SigV4?
//!
//! `AssumeRoleWithWebIdentity` is one of STS's **anonymous** actions — the
//! `WebIdentityToken` IS the credential.  The request is a plain
//! form-urlencoded POST; no AWS_ACCESS_KEY_ID is required.  This is the
//! EKS IRSA bootstrap path: the pod has no static AWS creds, only its
//! projected K8s ServiceAccount token.
//!
//! ## Reference shape
//!
//! `[<region>:]<role-arn>[#<session-name>]`
//!
//! - `<role-arn>` — the IAM role to assume (overrides `AWS_ROLE_ARN` env).
//! - `<region>:` prefix — regional STS endpoint override
//!   (default `sts.<region>.amazonaws.com`).
//! - `#<session-name>` — optional `RoleSessionName`; defaults to
//!   `noetl-server`.
//!
//! When `name` is empty the role + region fall back to `AWS_ROLE_ARN` +
//! `AWS_REGION`; this is the typical IRSA shape where the keychain alias
//! just selects the provider and the credentials come from env.
//!
//! ## Returned value
//!
//! JSON object with three string fields:
//!
//! ```ignore
//! {
//!   "access_key_id":     "ASIA...",
//!   "secret_access_key": "...",
//!   "session_token":     "..."
//! }
//! ```
//!
//! The `version` field carries the STS-returned `AssumedRoleId`; the
//! `expires_at` field carries the `Expiration` timestamp (RFC3339).
//!
//! Phase 6d's `cache_decision` clamps the cache TTL to
//! `min(default_ttl, expires_at - now - safety_margin)` so the cache row
//! is evicted before the credentials expire; Phase 7c.3 spawns a
//! background re-resolve inside the refresh window.

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use std::sync::OnceLock;

use super::{SecretProvider, SecretRef, SecretValue};
use crate::error::{AppError, AppResult};

/// Stable provider id.
const PROVIDER: &str = "aws_sts";

/// STS API version pinned by AWS for `AssumeRoleWithWebIdentity`.
const STS_API_VERSION: &str = "2011-06-15";

/// Default `RoleSessionName` when the ref doesn't supply one.
const DEFAULT_SESSION_NAME: &str = "noetl-server";

/// Default token-file path EKS pod-identity webhook injects.
const DEFAULT_TOKEN_FILE: &str = "/var/run/secrets/eks.amazonaws.com/serviceaccount/token";

/// Default lifetime to request from STS (1 hour — STS's max for IRSA tokens
/// is 12h but most operators want the shorter window so caches refresh
/// more often).
const DEFAULT_DURATION_SECS: u32 = 3600;

/// AWS STS provider.
pub struct AwsStsProvider {
    http: reqwest::Client,
    /// Override endpoint host; defaults to `sts.<region>.amazonaws.com`.
    endpoint_override: Option<String>,
    default_region: String,
    default_role_arn: Option<String>,
    /// Filesystem path to the projected ServiceAccount JWT.  Re-read on
    /// every call (the projected token rotates every ~hour by default).
    token_file: String,
    duration_seconds: u32,
}

/// Parsed AWS STS reference: optional region override, optional explicit
/// role ARN, optional session name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRef {
    region: Option<String>,
    role_arn: Option<String>,
    session_name: Option<String>,
}

fn parse_ref(raw: &str) -> ParsedRef {
    let raw = raw.trim();
    if raw.is_empty() {
        return ParsedRef {
            region: None,
            role_arn: None,
            session_name: None,
        };
    }
    // Optional `#<session-name>` suffix.
    let (rest, session_name) = match raw.split_once('#') {
        Some((r, s)) if !s.is_empty() => (r, Some(s.to_string())),
        _ => (raw, None),
    };
    // Optional `<region>:` prefix.  ARNs already start with `arn:`, which
    // we leave alone; a bare region looks like `us-east-1` (letters /
    // digits / dashes).
    let (region, role_arn) = if rest.is_empty() {
        (None, None)
    } else if rest.starts_with("arn:") {
        (None, Some(rest.to_string()))
    } else if let Some((maybe_region, role)) = rest.split_once(':') {
        let looks_like_region = !maybe_region.is_empty()
            && maybe_region
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-');
        if looks_like_region && role.starts_with("arn:") {
            (Some(maybe_region.to_string()), Some(role.to_string()))
        } else {
            (None, Some(rest.to_string()))
        }
    } else {
        (None, Some(rest.to_string()))
    };
    ParsedRef {
        region,
        role_arn,
        session_name,
    }
}

impl AwsStsProvider {
    /// Resolve config from the environment.
    ///
    /// Env vars (all optional unless noted):
    ///
    /// - `AWS_ROLE_ARN` — default role ARN.  Required only if the
    ///   keychain ref doesn't provide one.
    /// - `AWS_WEB_IDENTITY_TOKEN_FILE` — projected SA token path.
    ///   Defaults to the EKS pod-identity-webhook injection path.
    /// - `AWS_REGION` / `AWS_DEFAULT_REGION` — default STS region.
    ///   Defaults to `us-east-1` (STS's home region; works for global
    ///   STS too).
    /// - `NOETL_AWS_STS_DURATION_SECS` — credential lifetime to request
    ///   (default 3600).
    /// - `NOETL_AWS_STS_ENDPOINT_OVERRIDE` — explicit endpoint
    ///   (useful for tests / VPC endpoint overrides).
    pub fn from_env() -> AppResult<Self> {
        let default_region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());
        let default_role_arn = std::env::var("AWS_ROLE_ARN").ok();
        let token_file = std::env::var("AWS_WEB_IDENTITY_TOKEN_FILE")
            .unwrap_or_else(|_| DEFAULT_TOKEN_FILE.to_string());
        let duration_seconds = std::env::var("NOETL_AWS_STS_DURATION_SECS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(DEFAULT_DURATION_SECS);
        let endpoint_override = std::env::var("NOETL_AWS_STS_ENDPOINT_OVERRIDE").ok();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| AppError::Config(format!("aws_sts: http client build failed: {e}")))?;
        Ok(Self {
            http,
            endpoint_override,
            default_region,
            default_role_arn,
            token_file,
            duration_seconds,
        })
    }

    /// Build the STS endpoint URL for `region`.
    fn endpoint_for(&self, region: &str) -> String {
        if let Some(override_url) = &self.endpoint_override {
            return override_url.clone();
        }
        format!("https://sts.{region}.amazonaws.com/")
    }

    /// Build the form-urlencoded request body.  The token + role_arn are
    /// percent-encoded; everything else is plain ASCII so a single
    /// formatter suffices.
    fn build_body(
        role_arn: &str,
        session_name: &str,
        token: &str,
        duration_secs: u32,
    ) -> String {
        format!(
            "Action=AssumeRoleWithWebIdentity\
             &Version={ver}\
             &RoleArn={role}\
             &RoleSessionName={session}\
             &DurationSeconds={dur}\
             &WebIdentityToken={token}",
            ver = STS_API_VERSION,
            role = percent_encode(role_arn),
            session = percent_encode(session_name),
            dur = duration_secs,
            token = percent_encode(token),
        )
    }
}

#[async_trait]
impl SecretProvider for AwsStsProvider {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn fetch(&self, secret: &SecretRef) -> AppResult<SecretValue> {
        let parsed = parse_ref(&secret.name);
        let region = parsed
            .region
            .or_else(|| secret.region.clone())
            .unwrap_or_else(|| self.default_region.clone());
        let role_arn = parsed
            .role_arn
            .or_else(|| self.default_role_arn.clone())
            .ok_or_else(|| {
                AppError::Config(
                    "aws_sts: no RoleArn supplied (set AWS_ROLE_ARN or include the ARN \
                     in the keychain ref)"
                        .to_string(),
                )
            })?;
        let session_name = parsed
            .session_name
            .unwrap_or_else(|| DEFAULT_SESSION_NAME.to_string());

        // Re-read the projected SA token every call — kubelet rotates it.
        let token = tokio::fs::read_to_string(&self.token_file)
            .await
            .map_err(|e| {
                AppError::Config(format!(
                    "aws_sts: cannot read web-identity token from {}: {e}",
                    self.token_file
                ))
            })?;
        let token = token.trim();
        if token.is_empty() {
            return Err(AppError::Config(format!(
                "aws_sts: web-identity token file {} is empty",
                self.token_file
            )));
        }

        let endpoint = self.endpoint_for(&region);
        let body = Self::build_body(&role_arn, &session_name, token, self.duration_seconds);

        let resp = self
            .http
            .post(&endpoint)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("aws_sts: POST {endpoint} failed: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| AppError::Internal(format!("aws_sts: read body failed: {e}")))?;
        if !status.is_success() {
            return Err(AppError::Internal(format!(
                "aws_sts: STS returned HTTP {status}: {text}"
            )));
        }

        // STS returns XML by default; the `accept: application/json` header
        // ABOVE is only honoured by the modern STS endpoints — older / VPC
        // endpoints stick to XML.  Try JSON first (cheaper), fall back to
        // XML extraction.
        let parsed = parse_assume_role_response(&text).ok_or_else(|| {
            AppError::Internal(format!(
                "aws_sts: could not parse AssumeRoleWithWebIdentityResponse from STS \
                 response (status {status})"
            ))
        })?;

        let value = serde_json::to_string(&serde_json::json!({
            "access_key_id":     parsed.access_key_id,
            "secret_access_key": parsed.secret_access_key,
            "session_token":     parsed.session_token,
        }))
        .map_err(|e| AppError::Internal(format!("aws_sts: serialize creds: {e}")))?;

        Ok(SecretValue {
            value,
            version: parsed.assumed_role_id,
            expires_at: Some(parsed.expiration),
        })
    }
}

/// Minimal `AssumeRoleWithWebIdentityResult` parser.  STS responses are
/// tightly structured, so a few regex matches recover the four fields we
/// need without pulling an XML / JSON dep tree.  We DO accept JSON shape
/// too (modern STS endpoints support `accept: application/json`).
#[derive(Debug)]
struct AssumeRoleResult {
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
    expiration: chrono::DateTime<chrono::Utc>,
    assumed_role_id: Option<String>,
}

fn parse_assume_role_response(text: &str) -> Option<AssumeRoleResult> {
    // Try JSON shape first.  STS JSON wraps the result in
    // `AssumeRoleWithWebIdentityResponse.AssumeRoleWithWebIdentityResult.Credentials`.
    if text.trim_start().starts_with('{') {
        if let Some(r) = parse_assume_role_json(text) {
            return Some(r);
        }
    }
    parse_assume_role_xml(text)
}

#[derive(Deserialize)]
struct StsCredsJson {
    #[serde(rename = "AccessKeyId")]
    access_key_id: String,
    #[serde(rename = "SecretAccessKey")]
    secret_access_key: String,
    #[serde(rename = "SessionToken")]
    session_token: String,
    /// STS returns epoch seconds (float) under JSON.
    #[serde(rename = "Expiration")]
    expiration: serde_json::Value,
}

#[derive(Deserialize)]
struct StsAssumedRoleUserJson {
    #[serde(rename = "AssumedRoleId")]
    assumed_role_id: Option<String>,
}

#[derive(Deserialize)]
struct StsResultJson {
    #[serde(rename = "Credentials")]
    credentials: StsCredsJson,
    #[serde(rename = "AssumedRoleUser", default)]
    assumed_role_user: Option<StsAssumedRoleUserJson>,
}

#[derive(Deserialize)]
struct StsResponseJson {
    #[serde(rename = "AssumeRoleWithWebIdentityResponse")]
    outer: StsResponseInnerJson,
}

#[derive(Deserialize)]
struct StsResponseInnerJson {
    #[serde(rename = "AssumeRoleWithWebIdentityResult")]
    result: StsResultJson,
}

fn parse_assume_role_json(text: &str) -> Option<AssumeRoleResult> {
    let parsed: StsResponseJson = serde_json::from_str(text).ok()?;
    let creds = parsed.outer.result.credentials;
    let expiration = parse_expiration_json(&creds.expiration)?;
    Some(AssumeRoleResult {
        access_key_id: creds.access_key_id,
        secret_access_key: creds.secret_access_key,
        session_token: creds.session_token,
        expiration,
        assumed_role_id: parsed
            .outer
            .result
            .assumed_role_user
            .and_then(|u| u.assumed_role_id),
    })
}

fn parse_expiration_json(v: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(f) = v.as_f64() {
        // STS JSON gives epoch seconds (float).
        let secs = f as i64;
        let nanos = ((f - secs as f64) * 1e9) as u32;
        chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos)
    } else if let Some(s) = v.as_str() {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&chrono::Utc))
    } else {
        None
    }
}

fn xml_tag_re(tag: &str) -> &'static Regex {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static CACHE: OnceLock<Mutex<HashMap<String, &'static Regex>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap();
    if let Some(r) = guard.get(tag) {
        return r;
    }
    // (?s) DOTALL so a multiline body parses; non-greedy capture.
    let pattern = format!(r"(?s)<{tag}>(.*?)</{tag}>", tag = regex::escape(tag));
    let leaked: &'static Regex = Box::leak(Box::new(
        Regex::new(&pattern).expect("regex should compile"),
    ));
    guard.insert(tag.to_string(), leaked);
    leaked
}

fn xml_extract(text: &str, tag: &str) -> Option<String> {
    xml_tag_re(tag)
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
}

fn parse_assume_role_xml(text: &str) -> Option<AssumeRoleResult> {
    let access_key_id = xml_extract(text, "AccessKeyId")?;
    let secret_access_key = xml_extract(text, "SecretAccessKey")?;
    let session_token = xml_extract(text, "SessionToken")?;
    let expiration_raw = xml_extract(text, "Expiration")?;
    let expiration = chrono::DateTime::parse_from_rfc3339(&expiration_raw)
        .ok()?
        .with_timezone(&chrono::Utc);
    let assumed_role_id = xml_extract(text, "AssumedRoleId");
    Some(AssumeRoleResult {
        access_key_id,
        secret_access_key,
        session_token,
        expiration,
        assumed_role_id,
    })
}

/// Minimal application/x-www-form-urlencoded percent encoding.  Encodes
/// every byte outside the unreserved set [A-Za-z0-9-._~].  We don't reach
/// for `urlencoding` or `percent-encoding` since this is the only consumer
/// in the file and the rules are stable.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // parse_ref
    // -----------------------------------------------------------------

    #[test]
    fn parse_ref_empty_is_all_none() {
        let p = parse_ref("");
        assert_eq!(
            p,
            ParsedRef {
                region: None,
                role_arn: None,
                session_name: None
            }
        );
    }

    #[test]
    fn parse_ref_bare_arn_is_role_only() {
        let p = parse_ref("arn:aws:iam::123:role/noetl");
        assert_eq!(
            p,
            ParsedRef {
                region: None,
                role_arn: Some("arn:aws:iam::123:role/noetl".to_string()),
                session_name: None,
            }
        );
    }

    #[test]
    fn parse_ref_region_prefix_splits() {
        let p = parse_ref("eu-west-1:arn:aws:iam::123:role/noetl");
        assert_eq!(
            p,
            ParsedRef {
                region: Some("eu-west-1".to_string()),
                role_arn: Some("arn:aws:iam::123:role/noetl".to_string()),
                session_name: None,
            }
        );
    }

    #[test]
    fn parse_ref_session_suffix() {
        let p = parse_ref("arn:aws:iam::123:role/noetl#my-session");
        assert_eq!(
            p,
            ParsedRef {
                region: None,
                role_arn: Some("arn:aws:iam::123:role/noetl".to_string()),
                session_name: Some("my-session".to_string()),
            }
        );
    }

    #[test]
    fn parse_ref_full_combo() {
        let p = parse_ref("us-east-1:arn:aws:iam::999:role/noetl#etl-session");
        assert_eq!(
            p,
            ParsedRef {
                region: Some("us-east-1".to_string()),
                role_arn: Some("arn:aws:iam::999:role/noetl".to_string()),
                session_name: Some("etl-session".to_string()),
            }
        );
    }

    #[test]
    fn parse_ref_non_region_colon_left_alone() {
        // Some random string with a colon that isn't a region-prefix shape
        // shouldn't get split.  Use a non-region first half + non-arn second
        // half — our heuristic treats this as "no region", whole string is
        // the role.
        let p = parse_ref("not_a_region:something");
        // `not_a_region` looks region-shaped (alnum + underscore is NOT
        // matched, underscore fails the alphanumeric+dash check), so
        // role_arn is the full string.
        assert_eq!(p.region, None);
        assert_eq!(p.role_arn.as_deref(), Some("not_a_region:something"));
    }

    // -----------------------------------------------------------------
    // build_body
    // -----------------------------------------------------------------

    #[test]
    fn build_body_percent_encodes_role_arn() {
        let body = AwsStsProvider::build_body(
            "arn:aws:iam::123:role/test-role",
            "noetl-server",
            "eyJhbGciOi.JWT.payload",
            900,
        );
        // The `:` and `/` in the ARN must be encoded.
        assert!(body.contains("RoleArn=arn%3Aaws%3Aiam%3A%3A123%3Arole%2Ftest-role"));
        // Underscores and hyphens stay literal.
        assert!(body.contains("RoleSessionName=noetl-server"));
        // The JWT's `.` separators stay literal (unreserved set).
        assert!(body.contains("WebIdentityToken=eyJhbGciOi.JWT.payload"));
        assert!(body.contains("DurationSeconds=900"));
        assert!(body.contains("Action=AssumeRoleWithWebIdentity"));
        assert!(body.contains("Version=2011-06-15"));
    }

    // -----------------------------------------------------------------
    // endpoint_for
    // -----------------------------------------------------------------

    fn provider_with_region(region: &str) -> AwsStsProvider {
        AwsStsProvider {
            http: reqwest::Client::new(),
            endpoint_override: None,
            default_region: region.to_string(),
            default_role_arn: None,
            token_file: "/dev/null".to_string(),
            duration_seconds: 3600,
        }
    }

    #[test]
    fn endpoint_for_uses_region() {
        let p = provider_with_region("us-east-1");
        assert_eq!(p.endpoint_for("eu-west-1"), "https://sts.eu-west-1.amazonaws.com/");
    }

    #[test]
    fn endpoint_for_honours_override() {
        let mut p = provider_with_region("us-east-1");
        p.endpoint_override = Some("http://mock-sts.test/".to_string());
        assert_eq!(p.endpoint_for("eu-west-1"), "http://mock-sts.test/");
    }

    // -----------------------------------------------------------------
    // XML response parser
    // -----------------------------------------------------------------

    const XML_RESPONSE: &str = r#"<AssumeRoleWithWebIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleWithWebIdentityResult>
    <SubjectFromWebIdentityToken>system:serviceaccount:default:noetl</SubjectFromWebIdentityToken>
    <AssumedRoleUser>
      <Arn>arn:aws:sts::123:assumed-role/test/eks-session</Arn>
      <AssumedRoleId>AROAEXAMPLEID:eks-session</AssumedRoleId>
    </AssumedRoleUser>
    <Credentials>
      <SessionToken>FwoGZXIvYXdzEAA</SessionToken>
      <SecretAccessKey>wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY</SecretAccessKey>
      <Expiration>2026-06-07T03:00:00Z</Expiration>
      <AccessKeyId>ASIAEXAMPLEACCESSKEY</AccessKeyId>
    </Credentials>
    <Provider>arn:aws:iam::123:oidc-provider/oidc.eks.us-east-1.amazonaws.com/id/EXAMPLED539D4633E5</Provider>
  </AssumeRoleWithWebIdentityResult>
  <ResponseMetadata>
    <RequestId>example-request-id</RequestId>
  </ResponseMetadata>
</AssumeRoleWithWebIdentityResponse>
"#;

    #[test]
    fn parse_xml_extracts_credentials_and_expiration() {
        let r = parse_assume_role_response(XML_RESPONSE).expect("parse");
        assert_eq!(r.access_key_id, "ASIAEXAMPLEACCESSKEY");
        assert_eq!(r.secret_access_key, "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY");
        assert_eq!(r.session_token, "FwoGZXIvYXdzEAA");
        assert_eq!(r.assumed_role_id.as_deref(), Some("AROAEXAMPLEID:eks-session"));
        // Expiration becomes a real DateTime<Utc>.
        assert_eq!(
            r.expiration,
            chrono::DateTime::parse_from_rfc3339("2026-06-07T03:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)
        );
    }

    #[test]
    fn parse_xml_returns_none_on_missing_fields() {
        let truncated = r#"<AssumeRoleWithWebIdentityResponse>
          <AssumeRoleWithWebIdentityResult>
            <Credentials>
              <AccessKeyId>ASIA</AccessKeyId>
            </Credentials>
          </AssumeRoleWithWebIdentityResult>
        </AssumeRoleWithWebIdentityResponse>"#;
        assert!(parse_assume_role_response(truncated).is_none());
    }

    // -----------------------------------------------------------------
    // JSON response parser
    // -----------------------------------------------------------------

    #[test]
    fn parse_json_extracts_credentials() {
        let json = r#"{
            "AssumeRoleWithWebIdentityResponse": {
                "AssumeRoleWithWebIdentityResult": {
                    "Credentials": {
                        "AccessKeyId": "ASIAEX",
                        "SecretAccessKey": "secret",
                        "SessionToken": "token",
                        "Expiration": 1780118400.0
                    },
                    "AssumedRoleUser": {
                        "AssumedRoleId": "AROAEX:session",
                        "Arn": "arn:aws:sts::123:assumed-role/x/session"
                    }
                }
            }
        }"#;
        let r = parse_assume_role_response(json).expect("parse");
        assert_eq!(r.access_key_id, "ASIAEX");
        assert_eq!(r.secret_access_key, "secret");
        assert_eq!(r.session_token, "token");
        assert_eq!(r.assumed_role_id.as_deref(), Some("AROAEX:session"));
        // 1780118400 -> 2026-05-30T16:00:00Z (epoch seconds float).
        assert_eq!(r.expiration.timestamp(), 1780118400);
    }

    #[test]
    fn parse_json_accepts_iso_string_expiration() {
        let json = r#"{
            "AssumeRoleWithWebIdentityResponse": {
                "AssumeRoleWithWebIdentityResult": {
                    "Credentials": {
                        "AccessKeyId": "ASIA",
                        "SecretAccessKey": "s",
                        "SessionToken": "t",
                        "Expiration": "2026-06-07T03:00:00Z"
                    },
                    "AssumedRoleUser": null
                }
            }
        }"#;
        let r = parse_assume_role_response(json).expect("parse");
        assert_eq!(
            r.expiration,
            chrono::DateTime::parse_from_rfc3339("2026-06-07T03:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)
        );
        assert!(r.assumed_role_id.is_none());
    }

    // -----------------------------------------------------------------
    // percent_encode
    // -----------------------------------------------------------------

    #[test]
    fn percent_encode_preserves_unreserved() {
        assert_eq!(
            percent_encode("ABCabc123-_.~"),
            "ABCabc123-_.~"
        );
    }

    #[test]
    fn percent_encode_escapes_specials() {
        assert_eq!(
            percent_encode("arn:aws/iam::123"),
            "arn%3Aaws%2Fiam%3A%3A123"
        );
        // Spaces become %20.
        assert_eq!(percent_encode("a b"), "a%20b");
    }
}
