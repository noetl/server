//! Regenerate the committed DSL knowledge-base artifacts (ai-meta RFC
//! `dsl-knowledge-base.md`, Phase 1).
//!
//! ```text
//! cargo run -p noetl-orchestrate-core --example emit_dsl --features json-schema
//! ```
//!
//! Writes `orchestrate-core/schema/{playbook.schema.json,dsl-manifest.json,resource-model.json}`
//! under the DEFAULT gates (every flag-gated kind off) — the vocabulary a
//! server with no arming flags serves. `tests/dsl_artifacts.rs` fails when the
//! committed files differ from what this would write.

use noetl_orchestrate_core::dsl;
use noetl_orchestrate_core::playbook::ToolKindGates;
use noetl_orchestrate_core::resource_model;

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema");
    std::fs::create_dir_all(&dir).expect("create schema dir");
    let gates = ToolKindGates::OFF;
    let schema = dsl::playbook_schema(gates);
    let manifest = dsl::manifest(gates);
    std::fs::write(
        dir.join("playbook.schema.json"),
        dsl::pretty_canonical(&schema),
    )
    .expect("write schema");
    std::fs::write(
        dir.join("dsl-manifest.json"),
        dsl::pretty_canonical(&manifest),
    )
    .expect("write manifest");
    let model = resource_model::model();
    std::fs::write(
        dir.join("resource-model.json"),
        dsl::pretty_canonical(&model),
    )
    .expect("write resource model");
    println!("schema   {}", dsl::digest(&schema));
    println!("manifest {}", dsl::digest(&manifest));
    println!("resource {}", dsl::digest(&model));
}
