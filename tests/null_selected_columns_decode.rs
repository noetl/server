//! A column selected as a typed NULL must decode into an `Option` field.
//!
//! ⚠⚠ This exists because the absence of it took `/api/catalog/list` down in
//! production with a 500 on every call:
//!
//! ```text
//! Database error: error occurred while decoding column "content":
//!   unexpected null; try decoding as an `Option`
//! ```
//!
//! noetl/server#436 made the listing path select `NULL::text AS content` so the
//! bodies never leave Postgres. The row struct's field was `String`. sqlx
//! refuses that row at decode time — but *only* against a real database.
//!
//! The tests that were supposed to catch it did not, and it is worth being
//! precise about why, because each looked adequate:
//!
//! * the unit tests asserted the generated **SQL string** contained
//!   `NULL::text AS content` — true, and irrelevant to decoding;
//! * the pre-merge proof loaded the real 2539-row prod catalog into a throwaway
//!   Postgres and compared **raw SQL** byte counts and row identity — real data,
//!   real database, and it never went through `query_as::<_, CatalogEntry>`,
//!   which is the only place the mismatch lives.
//!
//! Both measured something true. Neither measured the decode. This file checks
//! the pairing directly, with no database, so it runs in CI where a live-DB test
//! would not.
//!
//! The same drift is recorded twice already in `CatalogEntry`'s own comments
//! (`version: i16` after an `i32` decode failure, plus the credentials and
//! executions columns). It is this codebase's recurring failure, not a one-off.

const QUERIES: &str = include_str!("../src/db/queries/catalog.rs");
const MODELS: &str = include_str!("../src/db/models/catalog.rs");

/// Source with comment lines removed — this crate documents the hazard in prose,
/// and a matcher that reads prose as code reports the warning as the defect.
fn code_only(src: &str) -> String {
    src.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("///") && !t.starts_with('*')
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `NULL::<ty> AS <column>` the query builder can emit.
fn null_selected_columns(src: &str) -> Vec<String> {
    let code = code_only(src);
    let mut out = Vec::new();
    let mut rest = code.as_str();
    while let Some(i) = rest.find("NULL::") {
        let tail = &rest[i..];
        if let Some(as_at) = tail.find(" AS ") {
            let after = &tail[as_at + 4..];
            let name: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                out.push(name);
            }
        }
        rest = &rest[i + 6..];
    }
    out.sort();
    out.dedup();
    out
}

/// Is `field` declared as an `Option<..>` on the row struct?
fn field_is_optional(models: &str, field: &str) -> bool {
    let code = code_only(models);
    let start = code
        .find("pub struct CatalogEntry {")
        .expect("the CatalogEntry row struct");
    let body = &code[start..start + code[start..].find("\n}").expect("struct body")];
    body.lines()
        .filter_map(|l| l.trim().strip_prefix("pub "))
        .find_map(|l| l.strip_prefix(&format!("{field}: ")))
        .map(|ty| ty.trim_start().starts_with("Option<"))
        .unwrap_or(false)
}

/// ⭐ The pairing that production enforces at runtime, checked at build time.
#[test]
fn every_null_selected_column_decodes_as_an_option() {
    let cols = null_selected_columns(QUERIES);
    assert!(
        !cols.is_empty(),
        "no `NULL::<ty> AS <col>` found in the catalog queries — either the \
         projection was removed (then delete this guard) or the matcher broke \
         (then a green result here means nothing)"
    );

    let bad: Vec<&String> = cols
        .iter()
        .filter(|c| !field_is_optional(MODELS, c))
        .collect();

    assert!(
        bad.is_empty(),
        "these columns are selected as a typed NULL but their `CatalogEntry` \
         field is NOT an Option: {bad:?}\n\n\
         sqlx refuses such a row at decode time with \"unexpected null; try \
         decoding as an `Option`\", and it does so only against a real \
         database — so unit tests over the generated SQL, and even a \
         throwaway-Postgres proof that runs raw SQL, both pass while every \
         request 500s.\n\n\
         Either make the field `Option<..>` or stop selecting NULL for it."
    );
}

/// Positive control: the matchers must find what they look for, and must not
/// fire on prose describing it.
#[test]
fn the_matchers_can_actually_fail() {
    let planted = r#"fn q() { "SELECT NULL::text AS content, NULL::jsonb AS layout FROM t" }"#;
    assert_eq!(
        null_selected_columns(planted),
        vec!["content".to_string(), "layout".to_string()],
        "the NULL-column matcher cannot see a plain projection"
    );

    let discussed = "/// we select NULL::text AS content here\nfn q() {}";
    assert!(
        null_selected_columns(discussed).is_empty(),
        "the matcher counts comments as code"
    );

    let models =
        "pub struct CatalogEntry {\n    pub content: String,\n    pub layout: Option<Value>,\n}";
    assert!(
        !field_is_optional(models, "content"),
        "a String field must read as non-Option"
    );
    assert!(
        field_is_optional(models, "layout"),
        "an Option field must read as Option"
    );
}
