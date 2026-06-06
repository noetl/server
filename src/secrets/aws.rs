//! AWS Secrets Manager provider (Secrets Wallet Phase 3.x, noetl/ai-meta#61).
//!
//! Resolves secret references against AWS Secrets Manager via the regional
//! JSON-over-POST endpoint `secretsmanager.<region>.amazonaws.com/`, action
//! `secretsmanager.GetSecretValue`, authenticated with hand-rolled **AWS
//! Signature Version 4** signing.  The dependency footprint stays small —
//! `hmac` + `sha2` + `hex` + the existing `reqwest` (rustls-tls) — instead of
//! pulling in the full `aws-sdk-secretsmanager` crate tree.
//!
//! ## Reference shape
//!
//! `[<region>:]<secret-id>[#<json-key>]`
//!
//! - bare `<secret-id>` ⇒ the entire `SecretString` is returned as the value.
//! - `#<json-key>` ⇒ the `SecretString` is parsed as JSON and the named key
//!   extracted (AWS's recommended convention for storing multi-field secrets).
//! - `<region>:` prefix ⇒ override the default region for this one lookup.
//!
//! ## Credentials
//!
//! For this round credentials come from the environment:
//! `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optional `AWS_SESSION_TOKEN`
//! (when set, included as the `X-Amz-Security-Token` header — this is the
//! EKS-IRSA / web-identity / federated path).  Region from `AWS_REGION` or
//! `AWS_DEFAULT_REGION`.  The IRSA STS `AssumeRoleWithWebIdentity` exchange
//! (token file → temporary creds) is a clearly-scoped follow-up.

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{SecretProvider, SecretRef, SecretValue};
use crate::error::{AppError, AppResult};

/// Stable provider id.
const PROVIDER: &str = "aws";

const SERVICE: &str = "secretsmanager";
const TARGET: &str = "secretsmanager.GetSecretValue";
const CONTENT_TYPE: &str = "application/x-amz-json-1.1";

type HmacSha256 = Hmac<Sha256>;

/// AWS Secrets Manager backend.
pub struct AwsSmSecretProvider {
    http: reqwest::Client,
    /// Override endpoint host; defaults to
    /// `secretsmanager.<region>.amazonaws.com`.  Useful for tests / mocks.
    endpoint_override: Option<String>,
    default_region: String,
    creds: StaticCredentials,
}

/// Static AWS credentials resolved from the environment at startup.
#[derive(Debug, Clone)]
struct StaticCredentials {
    access_key_id: String,
    secret_access_key: String,
    /// Set when running with temporary creds (IRSA / `aws sts assume-role`).
    session_token: Option<String>,
}

#[derive(Deserialize)]
struct GetSecretValueResponse {
    /// Concrete version id behind a stage label (`AWSCURRENT` default).
    #[serde(rename = "VersionId", default)]
    version_id: Option<String>,
    /// The secret payload — present for string-encoded secrets.  Binary
    /// secrets land in `SecretBinary` (base64) and are unsupported here.
    #[serde(rename = "SecretString", default)]
    secret_string: Option<String>,
}

/// Parsed AWS Secrets Manager reference: region override, secret id, optional
/// JSON key for multi-field secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRef {
    region: Option<String>,
    secret_id: String,
    json_key: Option<String>,
}

fn parse_ref(raw: &str) -> AppResult<ParsedRef> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AppError::Config(
            "aws secret ref: empty reference".to_string(),
        ));
    }
    // Optional `#<json-key>` suffix (split first — the `:` region separator
    // may legally appear in an ARN-shaped secret id).
    let (rest, json_key) = match raw.split_once('#') {
        Some((r, k)) if !k.is_empty() => (r, Some(k.to_string())),
        Some(_) => {
            return Err(AppError::Config(
                "aws secret ref: empty json key after '#'".to_string(),
            ));
        }
        None => (raw, None),
    };
    // Optional `<region>:` prefix.  An ARN is `arn:aws:secretsmanager:<region>:...`,
    // which already starts with `arn:` — leave it alone (use it as a full id).
    let (region, secret_id) = if rest.starts_with("arn:") {
        (None, rest.to_string())
    } else if let Some((maybe_region, id)) = rest.split_once(':') {
        // A bare region looks like `us-east-1` — letters, digits, dashes only.
        let looks_like_region = !maybe_region.is_empty()
            && maybe_region
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-');
        if looks_like_region && !id.is_empty() {
            (Some(maybe_region.to_string()), id.to_string())
        } else {
            (None, rest.to_string())
        }
    } else {
        (None, rest.to_string())
    };
    Ok(ParsedRef {
        region,
        secret_id,
        json_key,
    })
}

impl AwsSmSecretProvider {
    /// Resolve config from the environment.
    pub fn from_env() -> AppResult<Self> {
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| {
            AppError::Config(
                "AWS Secrets Manager: AWS_ACCESS_KEY_ID is not set (required for the `aws` \
                 secret provider)"
                    .to_string(),
            )
        })?;
        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| {
            AppError::Config(
                "AWS Secrets Manager: AWS_SECRET_ACCESS_KEY is not set (required for the \
                 `aws` secret provider)"
                    .to_string(),
            )
        })?;
        let session_token = std::env::var("AWS_SESSION_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let default_region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .map_err(|_| {
                AppError::Config(
                    "AWS Secrets Manager: no region (set AWS_REGION or AWS_DEFAULT_REGION, or \
                     prefix the secret ref with `<region>:`)"
                        .to_string(),
                )
            })?;
        let endpoint_override = std::env::var("NOETL_AWS_SM_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty());
        Ok(Self {
            http: reqwest::Client::builder()
                .build()
                .map_err(|e| AppError::Config(format!("aws secret provider: build client: {e}")))?,
            endpoint_override,
            default_region,
            creds: StaticCredentials {
                access_key_id,
                secret_access_key,
                session_token,
            },
        })
    }

    fn endpoint_for(&self, region: &str) -> String {
        if let Some(o) = &self.endpoint_override {
            o.trim_end_matches('/').to_string()
        } else {
            format!("https://secretsmanager.{region}.amazonaws.com")
        }
    }
}

#[async_trait]
impl SecretProvider for AwsSmSecretProvider {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn fetch(&self, secret: &SecretRef) -> AppResult<SecretValue> {
        let parsed = parse_ref(&secret.name)?;
        // Region precedence (most-specific wins):
        //   1. `<region>:` prefix inside the ref string (parsed).
        //   2. SecretRef.region — Secrets Wallet Phase 6a, set by the
        //      resolver from KeychainDef.region or NOETL_SERVER_REGION.
        //   3. SecretRef.project — legacy overload; kept for back-compat
        //      with pre-6a callers that stashed region into `project`.
        //   4. Provider's default_region from AWS_REGION env.
        let region = parsed
            .region
            .clone()
            .or_else(|| secret.region.clone().filter(|r| !r.is_empty()))
            .or_else(|| secret.project.clone())
            .unwrap_or_else(|| self.default_region.clone());
        let endpoint = self.endpoint_for(&region);

        // Body: `{"SecretId": "...", "VersionStage": "AWSCURRENT"}` (or a
        // specific VersionId when the caller provided one).
        let body_value = match &secret.version {
            Some(v) if !v.is_empty() => serde_json::json!({
                "SecretId": &parsed.secret_id,
                "VersionId": v,
            }),
            _ => serde_json::json!({
                "SecretId": &parsed.secret_id,
                "VersionStage": "AWSCURRENT",
            }),
        };
        let body = serde_json::to_string(&body_value).map_err(|e| {
            AppError::Config(format!("aws secret provider: serialize request body: {e}"))
        })?;

        // SigV4 sign + dispatch.
        let now = chrono::Utc::now();
        let signed = sign_request(
            &self.creds,
            &region,
            &endpoint,
            body.as_bytes(),
            now,
            SERVICE,
        )?;
        let mut req = self.http.post(&endpoint).body(body);
        for (k, v) in &signed.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| {
            AppError::Config(format!(
                "aws secret provider: POST {endpoint} for '{}': {e}",
                parsed.secret_id
            ))
        })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AppError::Config(format!(
                "aws secret provider: GetSecretValue '{}' returned {status}: {}",
                parsed.secret_id,
                text.chars().take(400).collect::<String>()
            )));
        }
        let body: GetSecretValueResponse = serde_json::from_str(&text).map_err(|e| {
            AppError::Config(format!(
                "aws secret provider: decode GetSecretValue response for '{}': {e}",
                parsed.secret_id
            ))
        })?;
        let raw = body.secret_string.ok_or_else(|| {
            AppError::Config(format!(
                "aws secret provider: secret '{}' has no SecretString (binary secrets \
                 unsupported)",
                parsed.secret_id
            ))
        })?;
        let value = if let Some(key) = &parsed.json_key {
            extract_json_key(&raw, key, &parsed.secret_id)?
        } else {
            raw
        };
        Ok(SecretValue {
            value,
            version: body.version_id,
            expires_at: None,
        })
    }
}

fn extract_json_key(payload: &str, key: &str, secret_id: &str) -> AppResult<String> {
    let v: serde_json::Value = serde_json::from_str(payload).map_err(|e| {
        AppError::Config(format!(
            "aws secret provider: secret '{secret_id}' SecretString is not JSON (ref \
             requested key '{key}'): {e}"
        ))
    })?;
    let field = v.get(key).ok_or_else(|| {
        AppError::Config(format!(
            "aws secret provider: secret '{secret_id}' has no JSON key '{key}'"
        ))
    })?;
    match field {
        serde_json::Value::String(s) => Ok(s.clone()),
        // Numbers / bools / null get stringified; objects/arrays are an error
        // (callers asked for a scalar credential field).
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        serde_json::Value::Null => Ok(String::new()),
        _ => Err(AppError::Config(format!(
            "aws secret provider: secret '{secret_id}' key '{key}' is not a scalar"
        ))),
    }
}

// ---------------------------------------------------------------------------
// SigV4
// ---------------------------------------------------------------------------

struct SignedRequest {
    headers: Vec<(String, String)>,
}

/// Build SigV4 Authorization + X-Amz-* headers for the given request.
fn sign_request(
    creds: &StaticCredentials,
    region: &str,
    endpoint: &str,
    body: &[u8],
    now: chrono::DateTime<chrono::Utc>,
    service: &str,
) -> AppResult<SignedRequest> {
    let host = host_of(endpoint)?;
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string(); // 20260606T210000Z
    let date_stamp = now.format("%Y%m%d").to_string(); // 20260606
    let body_sha = sha256_hex(body);

    // Canonical headers (lowercase keys, sorted by key, values trimmed).
    let mut canonical_headers: Vec<(String, String)> = vec![
        ("content-type".to_string(), CONTENT_TYPE.to_string()),
        ("host".to_string(), host.clone()),
        ("x-amz-content-sha256".to_string(), body_sha.clone()),
        ("x-amz-date".to_string(), amz_date.clone()),
        ("x-amz-target".to_string(), TARGET.to_string()),
    ];
    if let Some(t) = &creds.session_token {
        canonical_headers.push(("x-amz-security-token".to_string(), t.clone()));
    }
    canonical_headers.sort_by(|a, b| a.0.cmp(&b.0));

    let signed_headers_list: Vec<&str> =
        canonical_headers.iter().map(|(k, _)| k.as_str()).collect();
    let signed_headers = signed_headers_list.join(";");
    let canonical_headers_str = canonical_headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();

    // Canonical request: METHOD \n CanonicalURI \n CanonicalQuery \n
    //                    CanonicalHeaders \n SignedHeaders \n HashedPayload.
    let canonical_uri = "/";
    let canonical_query = "";
    let canonical_request = format!(
        "POST\n{canonical_uri}\n{canonical_query}\n{canonical_headers_str}\n{signed_headers}\n{body_sha}"
    );

    // String to sign.
    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    // Derive signing key + sign.
    let signing_key = derive_signing_key(&creds.secret_access_key, &date_stamp, region, service)?;
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes())?);

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    );

    // Final header set (case is preserved verbatim; AWS only cares about
    // case-insensitive matching).
    let mut headers: Vec<(String, String)> = vec![
        ("Authorization".to_string(), authorization),
        ("Content-Type".to_string(), CONTENT_TYPE.to_string()),
        ("X-Amz-Date".to_string(), amz_date),
        ("X-Amz-Target".to_string(), TARGET.to_string()),
        ("X-Amz-Content-Sha256".to_string(), body_sha),
    ];
    if let Some(t) = &creds.session_token {
        headers.push(("X-Amz-Security-Token".to_string(), t.clone()));
    }
    Ok(SignedRequest { headers })
}

fn host_of(url: &str) -> AppResult<String> {
    let after_scheme = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let host = after_scheme.split(['/', '?']).next().unwrap_or("");
    if host.is_empty() {
        return Err(AppError::Config(format!(
            "aws secret provider: cannot parse host from endpoint '{url}'"
        )));
    }
    Ok(host.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> AppResult<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|e| AppError::Config(format!("aws secret provider: hmac init: {e}")))?;
    mac.update(msg);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn derive_signing_key(
    secret_access_key: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
) -> AppResult<Vec<u8>> {
    let k_date = hmac_sha256(
        format!("AWS4{secret_access_key}").as_bytes(),
        date_stamp.as_bytes(),
    )?;
    let k_region = hmac_sha256(&k_date, region.as_bytes())?;
    let k_service = hmac_sha256(&k_region, service.as_bytes())?;
    hmac_sha256(&k_service, b"aws4_request")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rfc_creds() -> StaticCredentials {
        // From the AWS SigV4 reference test (`AKIDEXAMPLE`).
        StaticCredentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        }
    }

    #[test]
    fn parse_ref_bare() {
        let p = parse_ref("prod/duffel-token").unwrap();
        assert_eq!(
            p,
            ParsedRef {
                region: None,
                secret_id: "prod/duffel-token".into(),
                json_key: None
            }
        );
    }

    #[test]
    fn parse_ref_with_json_key() {
        let p = parse_ref("prod/db#password").unwrap();
        assert_eq!(p.secret_id, "prod/db");
        assert_eq!(p.json_key.as_deref(), Some("password"));
        assert_eq!(p.region, None);
    }

    #[test]
    fn parse_ref_with_region_prefix() {
        let p = parse_ref("us-east-1:prod/duffel-token").unwrap();
        assert_eq!(p.region.as_deref(), Some("us-east-1"));
        assert_eq!(p.secret_id, "prod/duffel-token");
    }

    #[test]
    fn parse_ref_arn_keeps_full_id() {
        let arn = "arn:aws:secretsmanager:us-west-2:111122223333:secret:prod/db-abc123";
        let p = parse_ref(arn).unwrap();
        assert!(p.region.is_none()); // ARN already names the region
        assert_eq!(p.secret_id, arn);
    }

    #[test]
    fn parse_ref_region_and_json_key() {
        let p = parse_ref("eu-west-1:prod/db#username").unwrap();
        assert_eq!(p.region.as_deref(), Some("eu-west-1"));
        assert_eq!(p.secret_id, "prod/db");
        assert_eq!(p.json_key.as_deref(), Some("username"));
    }

    #[test]
    fn parse_ref_empty_or_bad() {
        assert!(parse_ref("").is_err());
        assert!(parse_ref("   ").is_err());
        assert!(parse_ref("foo#").is_err());
    }

    #[test]
    fn extract_json_key_string_scalar() {
        let v = extract_json_key(
            r#"{"username":"alice","password":"secret"}"#,
            "password",
            "s",
        )
        .unwrap();
        assert_eq!(v, "secret");
    }

    #[test]
    fn extract_json_key_number() {
        let v = extract_json_key(r#"{"port":5432}"#, "port", "s").unwrap();
        assert_eq!(v, "5432");
    }

    #[test]
    fn extract_json_key_missing_key() {
        let e = extract_json_key(r#"{"a":1}"#, "b", "s").unwrap_err();
        assert!(format!("{e:?}").contains("no JSON key 'b'"));
    }

    #[test]
    fn extract_json_key_not_json() {
        let e = extract_json_key("not json", "k", "s").unwrap_err();
        assert!(format!("{e:?}").contains("not JSON"));
    }

    #[test]
    fn sha256_hex_known_vector() {
        // Empty-string SHA-256 — the SigV4 reference value when no body.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn derive_signing_key_matches_aws_reference() {
        // AWS published reference: signing key for date=20150830,
        // region=us-east-1, service=iam, secret=wJalr…EXAMPLEKEY.
        // Reference final key bytes (hex) — verifies our chained HMACs.
        let creds = rfc_creds();
        let k =
            derive_signing_key(&creds.secret_access_key, "20150830", "us-east-1", "iam").unwrap();
        assert_eq!(
            hex::encode(k),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn sign_request_produces_authorization_with_credential_scope() {
        // The exact signature depends on the timestamp; we only verify the
        // structural shape — credential scope, signed headers list — is what
        // AWS expects.
        let creds = rfc_creds();
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-06T21:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let signed = sign_request(
            &creds,
            "us-east-1",
            "https://secretsmanager.us-east-1.amazonaws.com",
            br#"{"SecretId":"prod/x","VersionStage":"AWSCURRENT"}"#,
            now,
            SERVICE,
        )
        .unwrap();
        let auth = signed
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.clone())
            .expect("Authorization header");
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260606/us-east-1/secretsmanager/aws4_request,"));
        assert!(auth.contains(
            "SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-target,"
        ));
        assert!(auth.contains("Signature="));
        assert!(signed.headers.iter().any(|(k, _)| k == "X-Amz-Date"));
        assert!(signed.headers.iter().any(|(k, _)| k == "X-Amz-Target"));
    }

    #[test]
    fn sign_request_includes_security_token_when_set() {
        let mut creds = rfc_creds();
        creds.session_token = Some("FQo…temp-token".to_string());
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-06T21:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let signed = sign_request(
            &creds,
            "us-east-1",
            "https://secretsmanager.us-east-1.amazonaws.com",
            b"{}",
            now,
            SERVICE,
        )
        .unwrap();
        let auth = signed
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(auth.contains("x-amz-security-token"));
        assert!(
            signed
                .headers
                .iter()
                .any(|(k, _)| k == "X-Amz-Security-Token")
        );
    }

    #[test]
    fn host_of_strips_scheme_and_path() {
        assert_eq!(
            host_of("https://secretsmanager.us-east-1.amazonaws.com/x").unwrap(),
            "secretsmanager.us-east-1.amazonaws.com"
        );
        assert_eq!(host_of("http://localhost:9999").unwrap(), "localhost:9999");
        assert!(host_of("").is_err());
    }
}
