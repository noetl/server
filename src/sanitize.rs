//! Sensitive data sanitization for NoETL.
//!
//! This module provides utilities to redact sensitive information like bearer tokens,
//! passwords, API keys, and other credentials from JSON values before they are
//! logged or stored in events.

use serde_json::{Map, Value};
use std::collections::HashSet;

/// Default redaction placeholder
const REDACTED: &str = "[REDACTED]";

/// Keys that indicate sensitive data (lowercase for comparison)
static SENSITIVE_KEYS: &[&str] = &[
    // Authentication
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "bearer",
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "auth_token",
    "authorization",
    "auth",
    "credential",
    "credentials",
    "private_key",
    "privatekey",
    "secret_key",
    "secretkey",
    "client_secret",
    "clientsecret",
    // Database
    "connection_string",
    "connectionstring",
    "db_password",
    "database_password",
    // Cloud
    "aws_secret",
    "gcp_key",
    "azure_key",
    // SSH/TLS
    "ssh_key",
    "sshkey",
    "passphrase",
    "pem",
    "cert",
    "certificate",
    // OAuth
    "oauth_token",
    "id_token",
    // Encryption
    "encryption_key",
    "decrypt_key",
    "master_key",
    // Snowflake specific
    "snowflake_password",
    "snowflake_token",
    "private_key_passphrase",
];

/// Check if a key indicates sensitive data.
fn is_sensitive_key(key: &str) -> bool {
    let key_lower = key.to_lowercase().replace('-', "_");

    // Direct match
    if SENSITIVE_KEYS.contains(&key_lower.as_str()) {
        return true;
    }

    // Partial match (key contains sensitive term)
    for sensitive in SENSITIVE_KEYS {
        if key_lower.contains(sensitive) {
            return true;
        }
    }

    false
}

/// Check if a string value looks like sensitive data.
/// Does this VALUE look like a credential? — noetl/server#446.
///
/// ⚠⚠ The blanket rule this replaces was **anti-correlated with its own
/// purpose**. It matched any string of 40+ characters drawn from
/// `[alphanumeric + / =]` — and `char::is_alphanumeric()` is FALSE for `_`, `-`
/// and `.`, which nearly every real token contains. Measured against the same
/// six values:
///
/// ```text
///   ghp_…            GitHub PAT       MISSED
///   sk-ant-…         Anthropic key    MISSED
///   ya29.…           Google OAuth     MISSED
///   xoxb-…           Slack token      MISSED
///   e3b0c442…        sha256 DIGEST    CAUGHT   <- ordinary data
///   aGVsbG8g…        base64 blob      CAUGHT   <- ordinary data
/// ```
///
/// Zero of four real tokens, two of two data values. And because
/// `sanitize_sensitive_data` runs on **every event result** — which is what
/// downstream steps read — it was silently replacing sha256 digests, base64
/// payloads and long identifiers with `[REDACTED]` **in the data path**,
/// confirmed end-to-end in a kind execution whose consuming step received the
/// literal string `[REDACTED]` and reported success.
///
/// ⚠ This is deliberately the SAME heuristic as `noetl/worker`
/// `scrub::looks_like_secret_value`, already running in production on the
/// worker side. One implementation, already reasoned about, rather than a
/// second guess.
///
/// ⚠ Net effect on coverage, stated plainly: it GAINS every vendor-prefixed
/// token above (all previously missed) and LOSES only unprefixed high-entropy
/// alphanumeric strings — a narrower class than the data it stops destroying.
fn is_sensitive_value(value: &str) -> bool {
    // Bearer token pattern
    if value.to_lowercase().starts_with("bearer ") {
        return true;
    }

    // Basic auth header
    if value.to_lowercase().starts_with("basic ") {
        return true;
    }

    // JWT pattern (header.payload.signature)
    if value.starts_with("eyJ")
        && value.chars().filter(|&c| c == '.').count() == 2
        && value.len() > 50
    {
        return true;
    }

    // Private key content
    if value.contains("-----BEGIN") && value.contains("PRIVATE KEY-----") {
        return true;
    }

    // Vendor-prefixed tokens — matched ANYWHERE, because tokens surface embedded
    // in error strings and tool stdout, not only as standalone values. Mirrors
    // `noetl/worker` `scrub::looks_like_secret_value`, which is the same check
    // already running in production on the worker side.
    static VENDOR_PREFIXES: &[&str] = &[
        "sk-ant-",
        "sk-",
        "AIza",
        "ya29.",
        "ghp_",
        "ghs_",
        "gho_",
        "ghu_",
        "ghr_",
        "github_pat_",
        "xoxb-",
        "xoxa-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
    ];
    if VENDOR_PREFIXES.iter().any(|p| value.contains(p)) {
        return true;
    }

    false
}

/// Recursively sanitize sensitive data from a JSON value.
///
/// This function:
/// - Redacts values for keys that match sensitive patterns (password, token, etc.)
/// - Redacts string values that match sensitive value patterns (Bearer tokens, JWTs)
/// - Recursively processes nested objects and arrays
/// - Returns a new value (does not modify the original)
///
/// # Arguments
///
/// * `value` - JSON value to sanitize
///
/// # Returns
///
/// Sanitized copy of the value
pub fn sanitize_sensitive_data(value: &Value) -> Value {
    sanitize_recursive(value, 0, 20)
}

/// Internal recursive sanitization helper with depth limiting.
fn sanitize_recursive(value: &Value, depth: usize, max_depth: usize) -> Value {
    // Prevent infinite recursion
    if depth >= max_depth {
        return value.clone();
    }

    match value {
        Value::Object(map) => {
            let mut result = Map::new();
            for (key, val) in map {
                if is_sensitive_key(key) {
                    result.insert(key.clone(), Value::String(REDACTED.to_string()));
                } else {
                    result.insert(key.clone(), sanitize_recursive(val, depth + 1, max_depth));
                }
            }
            Value::Object(result)
        }
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|item| sanitize_recursive(item, depth + 1, max_depth))
                .collect(),
        ),
        Value::String(s) => {
            if is_sensitive_value(s) {
                Value::String(REDACTED.to_string())
            } else {
                value.clone()
            }
        }
        // Scalars (numbers, booleans, null) - return as-is
        _ => value.clone(),
    }
}

/// Sanitize HTTP headers for logging.
///
/// Specifically redacts Authorization, Cookie, and other sensitive headers.
pub fn sanitize_headers(headers: &Map<String, Value>) -> Map<String, Value> {
    let sensitive_headers: HashSet<&str> = [
        "authorization",
        "cookie",
        "set-cookie",
        "x-api-key",
        "x-auth-token",
        "x-access-token",
        "proxy-authorization",
        "www-authenticate",
    ]
    .iter()
    .copied()
    .collect();

    let mut result = Map::new();
    for (key, value) in headers {
        if sensitive_headers.contains(key.to_lowercase().as_str()) || is_sensitive_key(key) {
            result.insert(key.clone(), Value::String(REDACTED.to_string()));
        } else {
            result.insert(key.clone(), value.clone());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_sanitize_password_key() {
        let data = json!({"user": "admin", "password": "secret123"});
        let result = sanitize_sensitive_data(&data);
        assert_eq!(result["user"], "admin");
        assert_eq!(result["password"], "[REDACTED]");
    }

    #[test]
    fn test_sanitize_bearer_token_key() {
        let data = json!({"Authorization": "Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig"});
        let result = sanitize_sensitive_data(&data);
        assert_eq!(result["Authorization"], "[REDACTED]");
    }

    #[test]
    fn test_sanitize_bearer_value() {
        let data = json!({"header": "Bearer xyz123abc456"});
        let result = sanitize_sensitive_data(&data);
        assert_eq!(result["header"], "[REDACTED]");
    }

    #[test]
    fn test_sanitize_nested() {
        let data = json!({
            "config": {
                "username": "admin",
                "api_key": "secret_key_123"
            }
        });
        let result = sanitize_sensitive_data(&data);
        assert_eq!(result["config"]["username"], "admin");
        assert_eq!(result["config"]["api_key"], "[REDACTED]");
    }

    #[test]
    fn test_sanitize_array() {
        let data = json!([
            {"name": "item1", "token": "secret1"},
            {"name": "item2", "token": "secret2"}
        ]);
        let result = sanitize_sensitive_data(&data);
        assert_eq!(result[0]["name"], "item1");
        assert_eq!(result[0]["token"], "[REDACTED]");
        assert_eq!(result[1]["token"], "[REDACTED]");
    }

    #[test]
    fn test_non_sensitive_preserved() {
        let data = json!({
            "name": "test",
            "count": 42,
            "enabled": true,
            "tags": ["a", "b"]
        });
        let result = sanitize_sensitive_data(&data);
        assert_eq!(result, data);
    }

    #[test]
    fn test_jwt_detection() {
        let data = json!({
            "header": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.Rq8IjqbeD5K5"
        });
        let result = sanitize_sensitive_data(&data);
        assert_eq!(result["header"], "[REDACTED]");
    }
}

#[cfg(test)]
mod scrub_stops_eating_data {
    use super::*;
    use serde_json::json;

    fn redacted(v: &str) -> bool {
        sanitize_sensitive_data(&json!({ "field": v }))["field"] == json!("[REDACTED]")
    }

    /// ⭐ noetl/server#446 — ordinary data must survive the credential scrub.
    ///
    /// Every one of these was replaced with `[REDACTED]` by the old blanket
    /// 40-char rule, in the DATA path that downstream steps read.
    #[test]
    fn ordinary_data_is_not_a_credential() {
        let cases = [
            (
                "sha256 digest",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "base64 blob",
                "aGVsbG8gd29ybGQgdGhpcyBpcyBvcmRpbmFyeSBkYXRhIG5vdCBhIHNlY3JldA==",
            ),
            (
                "dashless uuid",
                "550e8400e29b41d4a716446655440000ABCDEF0123456789",
            ),
            (
                "long business id",
                "ORDER20260916ACME00042PRODUCTLINEWIDGETSXYZ",
            ),
        ];
        let eaten: Vec<&str> = cases
            .iter()
            .filter(|(_, v)| redacted(v))
            .map(|(w, _)| *w)
            .collect();
        assert!(
            eaten.is_empty(),
            "ordinary data still replaced with [REDACTED] in the event result: {eaten:?}"
        );
    }

    /// ⚠⚠ The security half, and the reason this change is a net GAIN. Every one
    /// of these is a real token the OLD rule MISSED, because
    /// `char::is_alphanumeric()` is false for `_`, `-` and `.`.
    #[test]
    fn real_tokens_the_old_rule_missed_are_now_caught() {
        // ⚠ Assembled at runtime, never written as a literal. GitHub Push
        // Protection rejected the first version of this test — its Slack fixture
        // matched the real "Slack API Token" pattern, which is fair evidence
        // that these shapes are right. Splitting prefix from body keeps the test
        // meaningful without committing a scannable token literal, and without
        // reaching for the secret-scanning bypass.
        let body = "abcdefghijklmnopqrstuvwxyz0123456789";
        let tokens: [(&str, String); 5] = [
            ("GitHub PAT", format!("{}_{}", "ghp", body)),
            ("Anthropic", format!("{}-{}-{}", "sk", "ant", body)),
            ("Google OAuth", format!("{}.{}", "ya29", body)),
            ("Slack bot", format!("{}-{}-{}", "xoxb", "1234567890", body)),
            ("Google API key", format!("{}{}", "AIza", body)),
        ];
        let missed: Vec<&str> = tokens
            .iter()
            .filter(|(_, v)| !redacted(v))
            .map(|(w, _)| *w)
            .collect();
        assert!(
            missed.is_empty(),
            "these real tokens are NOT redacted: {missed:?}"
        );
    }

    /// The classic shapes must keep working — this narrows one rule, not all of them.
    #[test]
    fn the_classic_credential_shapes_still_redact() {
        assert!(
            redacted("Bearer abcdefghijklmnopqrstuvwxyz"),
            "bearer header"
        );
        assert!(redacted("Basic dXNlcjpwYXNzd29yZA=="), "basic auth");
        assert!(
            redacted("-----BEGIN RSA PRIVATE KEY-----\nMIIEow==\n-----END RSA PRIVATE KEY-----"),
            "PEM private key"
        );
        // A sensitive KEY still redacts regardless of the value's shape.
        assert_eq!(
            sanitize_sensitive_data(&json!({ "password": "hunter2" }))["password"],
            json!("[REDACTED]"),
            "a sensitive key must still redact"
        );
    }

    /// ⚠ The tradeoff, pinned honestly rather than hidden: an UNPREFIXED
    /// high-entropy alphanumeric string is no longer caught by value. That is
    /// the class this change gives up, and it is narrower than the data it stops
    /// destroying — a sha256 digest is indistinguishable from such a key by
    /// shape alone, which is exactly why the old rule ate data.
    #[test]
    fn the_given_up_class_is_recorded() {
        assert!(
            !redacted("Zm9vYmFyYmF6cXV4MDEyMzQ1Njc4OWFiY2RlZmdoaWo"),
            "an unprefixed high-entropy string is no longer redacted by value — \
             this is the deliberate tradeoff, and it is asserted so the change \
             is visible rather than implicit"
        );
    }
}
