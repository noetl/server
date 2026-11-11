//! Compile step: validate the playbook and embed it. A playbook that is not
//! pure (RFC §7.2) fails the BUILD, so an impure module cannot exist.
use std::{env, fs, path::PathBuf};

fn main() {
    let src = env::var("NOETL_PBC_PLAYBOOK").unwrap_or_else(|_| {
        format!(
            "{}/examples/sum.yaml",
            env::var("CARGO_MANIFEST_DIR").unwrap()
        )
    });
    println!("cargo:rerun-if-env-changed=NOETL_PBC_PLAYBOOK");
    println!("cargo:rerun-if-changed={src}");
    let yaml = fs::read_to_string(&src).unwrap_or_else(|e| panic!("{src}: {e}"));
    let value = noetl_orchestrate_core::catalog::playbook_value(&yaml)
        .unwrap_or_else(|e| panic!("{src}: {e}"));
    if let Err(e) = noetl_orchestrate_core::catalog::purity(&value) {
        panic!("{src} cannot be compiled to WASM: {e}");
    }
    let digest = noetl_orchestrate_core::dsl::digest(&value);
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(
        out.join("playbook.json"),
        noetl_orchestrate_core::dsl::canonical_json(&value),
    )
    .unwrap();
    fs::write(out.join("digest.txt"), &digest).unwrap();
}
