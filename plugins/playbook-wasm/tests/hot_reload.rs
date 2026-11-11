//! Compile → load → run → hot-reload, in a wasmtime host shaped like the
//! worker's (`worker/src/plugin.rs`): a module cache keyed by
//! `(path, version)` that reloads when the bytes' digest changes, plus the
//! fuel limit the worker does not set yet (RFC §7.2).
//!
//! Needs two built modules; `ci/hot_reload.sh` builds them and sets
//! `PBC_V1` / `PBC_V2`. Skipped otherwise.

use std::collections::HashMap;

use serde_json::{json, Value};
use wasmtime::{Config, Engine, Instance, Linker, Module, Store};

struct Host {
    engine: Engine,
    cache: HashMap<(String, u32), (String, Module)>,
    reloads: usize,
}

/// The worker keys a module by the SHA-256 of its bytes (`PluginKey.digest`).
fn sha(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

impl Host {
    fn new() -> Self {
        let mut cfg = Config::new();
        cfg.consume_fuel(true);
        Host {
            engine: Engine::new(&cfg).unwrap(),
            cache: HashMap::new(),
            reloads: 0,
        }
    }

    /// The worker's `ensure_loaded_by_ref`: same (path, version), new digest → reload.
    fn ensure(&mut self, path: &str, version: u32, bytes: &[u8]) -> &Module {
        let digest = sha(bytes);
        let key = (path.to_string(), version);
        let stale = self
            .cache
            .get(&key)
            .map(|(d, _)| d != &digest)
            .unwrap_or(true);
        if stale {
            if self.cache.contains_key(&key) {
                self.reloads += 1;
            }
            let m = Module::new(&self.engine, bytes).expect("module compiles");
            assert_eq!(
                m.imports().count(),
                0,
                "a compiled playbook must import nothing"
            );
            self.cache.insert(key.clone(), (digest, m));
        }
        &self.cache[&key].1
    }

    fn call(
        &self,
        module: &Module,
        entry: &str,
        input: &[u8],
        fuel: u64,
    ) -> Result<Vec<u8>, String> {
        let mut store = Store::new(&self.engine, ());
        store.set_fuel(fuel).unwrap();
        let instance = Linker::new(&self.engine)
            .instantiate(&mut store, module)
            .map_err(|e| e.to_string())?;
        call_packed(&mut store, &instance, entry, input)
    }
}

fn call_packed(
    store: &mut Store<()>,
    instance: &Instance,
    entry: &str,
    input: &[u8],
) -> Result<Vec<u8>, String> {
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or("no memory export")?;
    let packed = if entry == "digest" {
        let f = instance
            .get_typed_func::<(), i64>(&mut *store, "digest")
            .map_err(|e| e.to_string())?;
        f.call(&mut *store, ()).map_err(|e| e.to_string())?
    } else {
        let alloc = instance
            .get_typed_func::<u32, u32>(&mut *store, "alloc")
            .map_err(|e| e.to_string())?;
        let ptr = alloc
            .call(&mut *store, input.len() as u32)
            .map_err(|e| e.to_string())?;
        memory
            .write(&mut *store, ptr as usize, input)
            .map_err(|e| e.to_string())?;
        let run = instance
            .get_typed_func::<(u32, u32), i64>(&mut *store, entry)
            .map_err(|e| e.to_string())?;
        run.call(&mut *store, (ptr, input.len() as u32))
            .map_err(|e| format!("{e:#}"))?
    };
    let (ptr, len) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
    let mut out = vec![0u8; len];
    memory
        .read(&*store, ptr, &mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

fn yaml_digest(file: &str) -> String {
    let y =
        std::fs::read_to_string(format!("{}/examples/{file}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    noetl_orchestrate_core::catalog::playbook_digest(&y).unwrap()
}

#[test]
fn compile_load_run_and_hot_reload() {
    let (Ok(p1), Ok(p2)) = (std::env::var("PBC_V1"), std::env::var("PBC_V2")) else {
        eprintln!("skipped: PBC_V1/PBC_V2 not set (run ci/hot_reload.sh)");
        return;
    };
    let (v1, v2) = (std::fs::read(&p1).unwrap(), std::fs::read(&p2).unwrap());
    let mut host = Host::new();
    let fuel = 2_000_000_000;

    // v1: the module reports the catalog digest of the YAML it was compiled from.
    let m = host.ensure("compiled/sum", 1, &v1).clone();
    let d1 = String::from_utf8(host.call(&m, "digest", b"", fuel).unwrap()).unwrap();
    assert_eq!(d1, yaml_digest("sum.yaml"));
    let out: Value = serde_json::from_slice(
        &host
            .call(&m, "run", br#"{"workload":{"a":2,"b":3}}"#, fuel)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (out["status"].as_str(), &out["result"]["answer"]),
        (Some("COMPLETED"), &json!(50)),
        "{out}"
    );
    eprintln!(
        "v1 {d1}: answer={} rounds={} commands={}",
        out["result"]["answer"], out["rounds"], out["commands"]
    );

    // The same bytes again: no reload.
    host.ensure("compiled/sum", 1, &v1);
    assert_eq!(host.reloads, 0);

    // Hot reload: same (path, version), new bytes → new digest → new module.
    let m2 = host.ensure("compiled/sum", 1, &v2).clone();
    assert_eq!(host.reloads, 1);
    let d2 = String::from_utf8(host.call(&m2, "digest", b"", fuel).unwrap()).unwrap();
    assert_eq!(d2, yaml_digest("sum_v2.yaml"));
    assert_ne!(d1, d2);
    let out2: Value = serde_json::from_slice(
        &host
            .call(&m2, "run", br#"{"workload":{"a":2,"b":3}}"#, fuel)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(out2["result"]["answer"], json!(500), "{out2}");
    eprintln!(
        "v2 {d2}: answer={} (hot-reloaded, reloads={})",
        out2["result"]["answer"], host.reloads
    );

    // Deterministic: the same input yields the same bytes.
    assert_eq!(
        host.call(&m2, "run", br#"{"workload":{"a":4}}"#, fuel)
            .unwrap(),
        host.call(&m2, "run", br#"{"workload":{"a":4}}"#, fuel)
            .unwrap()
    );

    // Fuel bounds a run: a starved store traps instead of running away.
    let starved = host.call(&m2, "run", b"{}", 10_000);
    assert!(
        starved
            .as_ref()
            .is_err_and(|e| e.to_lowercase().contains("fuel")),
        "{starved:?}"
    );
}
