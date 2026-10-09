//! Refuse secret **material** in a catalog entry — noetl/ai-meta#455 P5.
//!
//! A catalog entry names a secret by **reference**: `auth: "{{ db_credential }}"`, an
//! alias the keychain resolves at step-execution time. The material itself lives in the
//! keychain, envelope-sealed. That split is the whole secrets contract
//! (`agents/rules/execution-model.md`).
//!
//! ⚠⚠ Why this is a hard refusal and not a warning: the internal catalog is EHDB-backed,
//! and `noetl.event` is **append-only, immutable, and never purged**. A credential pasted
//! inline into a registered playbook is therefore not a mistake you can clean up — it is
//! in the log for the life of the deployment, and it also travels into `command.issued`
//! when the step dispatches (the shape of the keychain drive leak). Registration is the
//! last point at which the answer can still be "no".
//!
//! ## Design: conservative on purpose
//!
//! Detection fires only on **unambiguous** markers — a PEM header, a service-account JSON
//! with both of its load-bearing keys, a three-part JWT, a provider-prefixed token.
//! Entropy heuristics are deliberately absent. A check that cries wolf is worse than no
//! check, because it teaches people to skim past the one finding that is real; the first
//! draft of a sibling guard reported 5 false positives out of 6 and had to be rewritten.
//!
//! ## Design: the error never echoes the value
//!
//! ⚠⚠ A validator that quotes the offending string back into its error message writes the
//! secret to the log, which is the thing it exists to prevent. Findings carry the YAML
//! **path** and the marker **class** — never any part of the material, not even a prefix.

/// What kind of material was recognised. Named classes rather than a free-form message, so
/// an operator can act without the value ever being printed.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Marker {
    /// `-----BEGIN … PRIVATE KEY-----` and friends.
    PemBlock,
    /// A service-account JSON carrying both `private_key` and `client_email`.
    ServiceAccountJson,
    /// A three-part `eyJ…` JWT.
    Jwt,
    /// A provider-prefixed token (`AKIA…`, `ghp_…`, `xoxb-…`, …).
    ProviderToken,
}

impl Marker {
    pub fn as_str(self) -> &'static str {
        match self {
            Marker::PemBlock => "a PEM private-key or certificate block",
            Marker::ServiceAccountJson => "a service-account JSON key",
            Marker::Jwt => "a JWT",
            Marker::ProviderToken => "a provider-prefixed API token",
        }
    }
}

/// One recognised location. Carries the path and the class — never the value.
#[derive(Debug, Clone)]
pub struct Finding {
    pub path: String,
    pub marker: Marker,
}

/// What a scan looked at, alongside what it found.
///
/// ⭐ The denominator is part of the result, not a detail. A scan that examined nothing
/// reports the same "no findings" as a clean document, and this repo has shipped several
/// green checks computed over zero bytes. `scalars` is what makes the two distinguishable.
#[derive(Debug, Default)]
pub struct Scan {
    pub findings: Vec<Finding>,
    pub scalars: usize,
}

/// Does this string carry recognisable secret material?
pub fn classify(s: &str) -> Option<Marker> {
    // A reference is never material. Checked first so a template naming a credential
    // cannot be mistaken for one.
    let t = s.trim();
    if t.starts_with("{{") && t.ends_with("}}") {
        return None;
    }

    // ⭐ Checked BEFORE the bare PEM test, not after. Requiring a PEM inside the JSON
    // made this class unreachable when the PEM test ran first — a marker that exists and
    // can never fire, which is the defect this codebase keeps finding one level up. The
    // specific label is what the author needs: "a service-account key" is actionable in a
    // way "a PEM block" is not.
    // ⚠⚠ Both key names AND an actual PEM body. Measured against 494 real playbooks
    // (26,081 string values): requiring only the two key names produced exactly one
    // finding, and it was a FALSE POSITIVE that would have broken a legitimate
    // registration — `ops/automation/agents/mcp/vertex-ai.yaml` embeds Python that READS
    // a service-account credential (`info.get("private_key")`,
    // `info.get("client_email")`), so the code handling the credential names both keys
    // while containing no material at all. Any genuine service-account key JSON carries
    // the PEM inside `private_key`, so demanding it costs no true positives and removes
    // the whole class of credential-handling code.
    if s.contains("\"private_key\"") && s.contains("\"client_email\"") && s.contains("-----BEGIN")
    {
        return Some(Marker::ServiceAccountJson);
    }
    if s.contains("-----BEGIN") && s.contains("PRIVATE KEY") {
        return Some(Marker::PemBlock);
    }
    if s.contains("-----BEGIN CERTIFICATE") {
        return Some(Marker::PemBlock);
    }
    if looks_like_jwt(t) {
        return Some(Marker::Jwt);
    }
    if let Some(m) = provider_token(t) {
        return Some(m);
    }
    None
}

/// A JWT: three dot-separated base64url segments, the first decoding to a JSON header —
/// which in practice means it starts `eyJ`.
///
/// ⚠ Length-gated. `eyJ.a.b` is not a credential, and matching it would fire on anything
/// vaguely dotted.
fn looks_like_jwt(t: &str) -> bool {
    if !t.starts_with("eyJ") || t.len() < 80 {
        return false;
    }
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '='))
        })
}

/// Provider-prefixed tokens, each gated on the length the provider actually issues so a
/// prose mention of the prefix does not trip it.
fn provider_token(t: &str) -> Option<Marker> {
    let aws = |p: &str| {
        t.len() == 20
            && t.starts_with(p)
            && t[4..]
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    };
    if aws("AKIA") || aws("ASIA") {
        return Some(Marker::ProviderToken);
    }
    for (prefix, min) in [
        ("ghp_", 36),
        ("gho_", 36),
        ("github_pat_", 40),
        ("xoxb-", 40),
        ("xoxp-", 40),
        ("sk-ant-", 40),
        ("glpat-", 20),
        ("AIza", 39),
    ] {
        if t.starts_with(prefix) && t.len() >= min && !t.contains(' ') {
            return Some(Marker::ProviderToken);
        }
    }
    None
}

/// Walk a YAML document, recognising material anywhere in it.
///
/// ⚠ Deliberately NOT restricted to secret-sounding keys. Material under `description` or
/// `workload.notes` lands in the append-only log exactly as material under `auth` does,
/// and a key-name allowlist would be a guess about which names are secret-bearing — the
/// kind of narrow idiom that made an earlier read-set scan miss 96 of 152 variables.
pub fn scan(yaml: &serde_yaml::Value) -> Scan {
    let mut out = Scan::default();
    walk(yaml, "$", &mut out);
    out
}

fn walk(v: &serde_yaml::Value, path: &str, out: &mut Scan) {
    match v {
        serde_yaml::Value::String(s) => {
            out.scalars += 1;
            if let Some(m) = classify(s) {
                out.findings.push(Finding {
                    path: path.to_string(),
                    marker: m,
                });
            }
        }
        serde_yaml::Value::Sequence(items) => {
            for (i, item) in items.iter().enumerate() {
                walk(item, &format!("{path}[{i}]"), out);
            }
        }
        serde_yaml::Value::Mapping(map) => {
            for (k, val) in map {
                let key = k.as_str().unwrap_or("?");
                walk(val, &format!("{path}.{key}"), out);
            }
        }
        // Numbers, bools, null and tagged values cannot carry a credential string.
        _ => {}
    }
}

/// The message an author sees. Paths and classes only.
pub fn describe(scan: &Scan) -> String {
    let mut parts: Vec<String> = scan
        .findings
        .iter()
        .map(|f| format!("{} looks like {}", f.path, f.marker.as_str()))
        .collect();
    parts.dedup();
    format!(
        "secret material must not be registered in the catalog: {}. \
         Store it in the keychain and reference it by alias instead \
         (e.g. auth: \"{{{{ my_credential }}}}\"). \
         (scanned {} string values; noetl/ai-meta#455 P5 — the catalog is EHDB-backed and \
         noetl.event is append-only and never purged, so an inline credential cannot be \
         removed later.)",
        parts.join("; "),
        scan.scalars
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn y(s: &str) -> serde_yaml::Value {
        serde_yaml::from_str(s).unwrap()
    }

    /// The reference form — the shape every catalog entry is supposed to use — must pass.
    /// First, because a guard that rejects the correct usage is worse than none.
    #[test]
    fn a_credential_referenced_by_alias_is_accepted() {
        let doc = y("workflow:\n  - step: q\n    auth: \"{{ db_credential }}\"\n    kind: postgres\n");
        let s = scan(&doc);
        assert!(
            s.findings.is_empty(),
            "a templated alias was refused: {:?}",
            s.findings
        );
        // ⭐ Denominator: the scan must have looked at something. A pass computed over
        // zero scalars is indistinguishable from a broken walker.
        assert!(s.scalars >= 3, "scanned only {} scalars", s.scalars);
    }

    /// Plain alias strings, provider reference URIs and ordinary prose all pass.
    #[test]
    fn references_and_prose_are_not_material() {
        for v in [
            "db_credential",
            "gcp:projects/p/secrets/s/versions/1",
            "arn:aws:secretsmanager:us-east-1:1:secret:x",
            "see the AKIA key in the keychain",
            "set GITHUB_TOKEN (a ghp_ prefixed token) in the keychain",
            "private_key",
            "{{ secret }}",
            "eyJ.a.b",
            "",
        ] {
            assert_eq!(classify(v), None, "{v:?} was wrongly flagged as material");
        }
    }

    /// ⚠⚠ Each class must actually fire. A guard that has never produced a positive is
    /// indistinguishable from one that cannot.
    #[test]
    fn every_marker_class_fires_on_real_material_shapes() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCA\n-----END RSA PRIVATE KEY-----";
        assert_eq!(classify(pem), Some(Marker::PemBlock));
        assert_eq!(
            classify("-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----"),
            Some(Marker::PemBlock)
        );
        // A realistic service-account key: the PEM lives inside `private_key`.
        let sa = r#"{"type":"service_account","private_key":"-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n","client_email":"a@b.iam"}"#;
        assert_eq!(classify(sa), Some(Marker::ServiceAccountJson));
        let jwt = format!("eyJ{}.eyJ{}.{}", "a".repeat(30), "b".repeat(30), "c".repeat(30));
        assert_eq!(classify(&jwt), Some(Marker::Jwt));
        assert_eq!(classify("AKIAIOSFODNN7EXAMPLE"), Some(Marker::ProviderToken));
        // ⚠⚠ Reachability of the SPECIFIC class, not merely "something fired". Requiring a
        // PEM inside the JSON put this class behind the bare-PEM test, where it could
        // never be reached; a plain `is_some()` here would have passed on PemBlock and
        // hidden that.
        assert_ne!(
            classify(sa),
            Some(Marker::PemBlock),
            "the service-account class is shadowed by the bare PEM test and can never fire"
        );
        assert_eq!(classify(&format!("ghp_{}", "a".repeat(36))), Some(Marker::ProviderToken));
        assert_eq!(classify(&format!("xoxb-{}", "1".repeat(40))), Some(Marker::ProviderToken));
    }

    /// ⚠⚠ The exact false positive a real-population sweep found, kept as a regression
    /// guard. Code that CONSUMES a service-account credential names both of its keys; it
    /// is not material, and refusing it would have broken a legitimate registration
    /// (`ops/automation/agents/mcp/vertex-ai.yaml`). This is the single finding out of
    /// 26,081 string values across 494 playbooks, and it was wrong.
    #[test]
    fn code_that_reads_a_service_account_credential_is_not_material() {
        let code = "info = json.loads(raw)\n\
                    private_key = info.get(\"private_key\")\n\
                    client_email = info.get(\"client_email\")\n\
                    if not private_key or not client_email:\n\
                        raise VertexMcpError(\"missing private_key or client_email\")\n";
        assert_eq!(
            classify(code),
            None,
            "credential-handling code was flagged as material; a guard that refuses the \
             correct usage is worse than no guard"
        );
    }

    /// ⚠⚠ The error must never contain the material. This is the property that makes the
    /// guard safe to run at all: a validator that echoes the value writes the secret into
    /// the log, which is exactly what it exists to prevent.
    #[test]
    fn the_message_never_echoes_the_material() {
        let secret = format!("ghp_{}", "Z".repeat(36));
        let doc = y(&format!("workflow:\n  - step: s\n    token: \"{secret}\"\n"));
        let s = scan(&doc);
        assert_eq!(s.findings.len(), 1, "the planted token was not found");
        let msg = describe(&s);
        assert!(
            !msg.contains(&secret),
            "the error echoed the whole secret: {msg}"
        );
        // Not even a prefix: a 12-char slice of a token is still a disclosure.
        assert!(
            !msg.contains(&secret[..12]),
            "the error echoed a prefix of the secret: {msg}"
        );
        // It must still be actionable — the path and the class.
        assert!(msg.contains("$.workflow[0].token"), "no path in: {msg}");
        assert!(msg.contains("provider-prefixed"), "no class in: {msg}");
    }

    /// Material anywhere counts, not only under secret-sounding keys — it reaches the
    /// append-only log either way.
    #[test]
    fn material_outside_an_auth_field_is_still_refused() {
        let doc = y("description: |\n  example: -----BEGIN PRIVATE KEY-----\n  MIIB\n");
        let s = scan(&doc);
        assert_eq!(s.findings.len(), 1, "material under description was missed");
        assert_eq!(s.findings[0].path, "$.description");
    }

    /// Nested structures are reached. A walker that stops at the first level would report
    /// a confident clean on every realistic playbook.
    #[test]
    fn the_walker_reaches_nested_sequences_and_mappings() {
        let doc = y(
            "a:\n  b:\n    - c:\n        d: \"AKIAIOSFODNN7EXAMPLE\"\n",
        );
        let s = scan(&doc);
        assert_eq!(s.findings.len(), 1, "nested material was missed");
        assert_eq!(s.findings[0].path, "$.a.b[0].c.d");
    }
}

/// Guards that the scan is on the path registration actually takes.
///
/// ⚠⚠ Kept deliberately distinct from "the function exists". A sibling hook shipped to
/// prod gated correctly on a path prod's traffic never traverses, and the structural tests
/// covering it passed the whole time (noetl/ai-meta#326). Asserting a call site is
/// necessary and NOT sufficient — the reachability half of this guard is the prod
/// measurement recorded on noetl/ai-meta#455, run against the live catalog.
#[cfg(test)]
mod wiring {
    const SRC: &str = include_str!("catalog.rs");

    #[test]
    fn registration_scans_for_material() {
        assert!(
            SRC.contains("secret_material::scan("),
            "catalog registration must scan for secret material, or inline credentials \
             reach the append-only log where they cannot be removed"
        );
    }

    /// ⚠⚠ The scan must run BEFORE the row is written. Scanning afterwards would persist
    /// the material and then report an error — the log keeps it either way, so the order
    /// IS the guarantee.
    #[test]
    fn the_scan_precedes_the_write() {
        let scan_at = SRC
            .find("secret_material::scan(")
            .expect("the scan call exists");
        let write_at = SRC
            .find("queries::get_next_version(")
            .expect("the version/write step exists");
        assert!(
            scan_at < write_at,
            "the scan runs after the write begins: material would be persisted and only \
             then refused, and noetl.event never forgets it"
        );
    }

    /// The refusal must be a hard error, not a log line.
    #[test]
    fn a_finding_refuses_the_registration() {
        let at = SRC.find("secret_material::scan(").unwrap();
        let window = &SRC[at..at + 500];
        assert!(
            window.contains("return Err(") && window.contains("findings.is_empty()"),
            "a finding must refuse the registration; warning and proceeding puts the \
             credential in the log anyway: {window}"
        );
    }
}
