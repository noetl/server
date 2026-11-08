//! The committed DSL knowledge-base artifacts must be exactly what the code
//! generates (ai-meta RFC `dsl-knowledge-base.md`, Phase 1).
//!
//! The artifacts exist so docs, release assets and offline generators can use
//! the grammar without running a server. An artifact that can be edited by hand
//! and still pass is hand-maintained documentation with extra steps, so this
//! test regenerates both and fails on any byte difference. A grammar change
//! that forgets to regenerate fails here, naming the command that fixes it.
#![cfg(feature = "json-schema")]

use noetl_orchestrate_core::dsl;
use noetl_orchestrate_core::playbook::ToolKindGates;

const REGEN: &str = "cargo run -p noetl-orchestrate-core --example emit_dsl --features json-schema";

fn check(file: &str, generated: String) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("schema")
        .join(file);
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} missing ({e}); run `{REGEN}`", path.display()));
    assert!(
        committed == generated,
        "orchestrate-core/schema/{file} is stale: the playbook types changed but the \
         committed artifact did not. Run `{REGEN}` and commit the result."
    );
}

#[test]
fn committed_schema_is_what_the_types_generate() {
    check(
        "playbook.schema.json",
        dsl::pretty_canonical(&dsl::playbook_schema(ToolKindGates::OFF)),
    );
}

#[test]
fn committed_manifest_is_what_the_registry_generates() {
    check(
        "dsl-manifest.json",
        dsl::pretty_canonical(&dsl::manifest(ToolKindGates::OFF)),
    );
}

/// Negative control: the comparison must be able to fail.
#[test]
fn the_freshness_check_can_fail() {
    let real = dsl::pretty_canonical(&dsl::playbook_schema(ToolKindGates::OFF));
    let mcp_on = dsl::pretty_canonical(&dsl::playbook_schema(ToolKindGates {
        mcp: true,
        agent: false,
    }));
    assert_ne!(real, mcp_on, "opening a gate must change the artifact");
}
