//! In-server **shadow** of the `system/orchestrate` WASM plug-in
//! (noetl/ai-meta#108 slice 4).
//!
//! When `NOETL_ORCHESTRATE_PLUGIN_SHADOW=true` (and the binary is built with the
//! `orchestrate-shadow` feature), the orchestrator runs the plug-in *alongside*
//! the in-process drive on every evaluation: it hands the plug-in the **same**
//! `WorkflowState` the in-process `evaluate_state` just consumed and diffs the
//! emitted command sets. The in-process result stays authoritative — the shadow
//! only observes — so the live platform is untouched while we build
//! cutover confidence over the real workload.
//!
//! The diff is **command-set identity** (parsed `Value` equality), not raw
//! bytes: the drive's `context` maps serialize in insertion order, which differs
//! wasm32-vs-host (slice 2 finding). The scheduler deserializes commands anyway.
//!
//! Everything heavy is gated behind the `orchestrate-shadow` feature so the
//! production server doesn't carry `wasmtime` unless the shadow is wanted. The
//! public wrappers are always present and are cheap no-ops without the feature,
//! so call sites (`trigger_orchestrator_inner`) need no `cfg`.

use noetl_orchestrate_core::orchestrator::OrchestrationResult;
use noetl_orchestrate_core::playbook::Playbook;
use noetl_orchestrate_core::state::WorkflowState;

/// True when a compiled shadow host is loaded and ready to evaluate.
pub fn enabled() -> bool {
    #[cfg(feature = "orchestrate-shadow")]
    {
        imp::host().is_some()
    }
    #[cfg(not(feature = "orchestrate-shadow"))]
    {
        false
    }
}

/// Run the plug-in on `pre_state` (the state as it was **before** the in-process
/// `evaluate_state` mutated it) and diff its commands against `native`. Records a
/// `match`/`mismatch` metric and warns on divergence. Never panics, never
/// affects the live result — best-effort observation.
#[allow(unused_variables)]
pub fn shadow_diff(
    pre_state: WorkflowState,
    latest_ts: Option<chrono::DateTime<chrono::Utc>>,
    playbook: &Playbook,
    trigger_event_type: &str,
    native: &OrchestrationResult,
    execution_id: i64,
) {
    #[cfg(feature = "orchestrate-shadow")]
    imp::shadow_diff(
        pre_state,
        latest_ts,
        playbook,
        trigger_event_type,
        native,
        execution_id,
    );
}

/// Compile + install the shadow host from the plug-in's wasm bytes (typically
/// fetched from `noetl.plugin_module`). No-op without the feature.
#[allow(unused_variables)]
pub fn init(wasm: &[u8]) {
    #[cfg(feature = "orchestrate-shadow")]
    imp::init(wasm);
}

#[cfg(feature = "orchestrate-shadow")]
mod imp {
    use std::sync::OnceLock;

    use noetl_orchestrate_core::orchestrator::OrchestrationResult;
    use noetl_orchestrate_core::playbook::Playbook;
    use noetl_orchestrate_core::state::WorkflowState;
    use serde::Serialize;
    use tracing::{debug, warn};
    use wasmtime::{Engine, Instance, Module, Store};

    /// The compiled plug-in, instantiated fresh per call (the worker host's
    /// isolation model). `Engine`/`Module` are `Send + Sync`.
    pub struct ShadowHost {
        engine: Engine,
        module: Module,
    }

    static HOST: OnceLock<ShadowHost> = OnceLock::new();

    pub fn host() -> Option<&'static ShadowHost> {
        HOST.get()
    }

    pub fn init(wasm: &[u8]) {
        if HOST.get().is_some() {
            return;
        }
        let engine = Engine::default();
        match Module::new(&engine, wasm) {
            Ok(module) => {
                let _ = HOST.set(ShadowHost { engine, module });
                tracing::info!(bytes = wasm.len(), "orchestrate shadow host loaded");
            }
            Err(e) => warn!(error = %e, "orchestrate shadow: failed to compile plug-in; disabled"),
        }
    }

    /// The plug-in's `OrchestrateStateInput` JSON shape, built server-side (the
    /// plug-in crate is not a dependency, so we mirror the contract here).
    #[derive(Serialize)]
    struct StateInput<'a> {
        state: &'a WorkflowState,
        #[serde(skip_serializing_if = "Option::is_none")]
        latest_ts: Option<chrono::DateTime<chrono::Utc>>,
        playbook: &'a Playbook,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger_event_type: Option<&'a str>,
    }

    pub fn shadow_diff(
        pre_state: WorkflowState,
        latest_ts: Option<chrono::DateTime<chrono::Utc>>,
        playbook: &Playbook,
        trigger_event_type: &str,
        native: &OrchestrationResult,
        execution_id: i64,
    ) {
        let Some(host) = HOST.get() else { return };

        let input = StateInput {
            state: &pre_state,
            latest_ts,
            playbook,
            trigger_event_type: Some(trigger_event_type),
        };
        let input_bytes = match serde_json::to_vec(&input) {
            Ok(b) => b,
            Err(e) => {
                warn!(execution_id, error = %e, "orchestrate shadow: encode input failed");
                return;
            }
        };

        let plugin_bytes = match host.run_state(&input_bytes) {
            Ok(b) => b,
            Err(e) => {
                warn!(execution_id, error = %e, "orchestrate shadow: plug-in invocation failed");
                return;
            }
        };

        // Semantic command-set identity (parsed Value eq tolerates non-canonical
        // context-map key order — slice 2 finding).
        let native_v = serde_json::to_value(&native.commands).unwrap_or(serde_json::Value::Null);
        let plugin_v: serde_json::Value = match serde_json::from_slice(&plugin_bytes) {
            Ok(v) => v,
            Err(e) => {
                warn!(execution_id, error = %e, "orchestrate shadow: decode plug-in output failed");
                return;
            }
        };
        if let Some(err) = plugin_v.get("error") {
            warn!(execution_id, plugin_error = %err, "orchestrate shadow: plug-in returned an error envelope");
            crate::metrics::record_orchestrate_shadow("error");
            return;
        }
        let plugin_cmds = plugin_v.get("commands").cloned().unwrap_or(serde_json::Value::Null);

        if native_v == plugin_cmds {
            crate::metrics::record_orchestrate_shadow("match");
            debug!(
                execution_id,
                commands = native.commands.len(),
                "orchestrate shadow: match"
            );
        } else {
            crate::metrics::record_orchestrate_shadow("mismatch");
            warn!(
                execution_id,
                trigger = trigger_event_type,
                native_commands = native.commands.len(),
                "orchestrate shadow: MISMATCH — plug-in commands diverge from in-process drive"
            );
        }
    }

    impl ShadowHost {
        /// Invoke the plug-in's `run_state` export over the worker host's
        /// data-plane ABI: `alloc(len)` → write input → `run_state(ptr,len)` →
        /// unpack `(out_ptr<<32)|out_len` → read output. Fresh `Store`/`Instance`
        /// per call.
        fn run_state(&self, input: &[u8]) -> Result<Vec<u8>, String> {
            let mut store = Store::new(&self.engine, ());
            let instance = Instance::new(&mut store, &self.module, &[])
                .map_err(|e| format!("instantiate: {e}"))?;
            let memory = instance
                .get_memory(&mut store, "memory")
                .ok_or("missing export: memory")?;
            let alloc = instance
                .get_typed_func::<i32, i32>(&mut store, "alloc")
                .map_err(|_| "missing export: alloc".to_string())?;
            let run_state = instance
                .get_typed_func::<(i32, i32), i64>(&mut store, "run_state")
                .map_err(|_| "missing export: run_state".to_string())?;

            let len = i32::try_from(input.len()).map_err(|_| "input exceeds i32".to_string())?;
            let in_ptr = alloc.call(&mut store, len).map_err(|e| format!("alloc: {e}"))?;
            memory
                .write(&mut store, in_ptr as usize, input)
                .map_err(|e| format!("write: {e}"))?;
            let packed = run_state
                .call(&mut store, (in_ptr, len))
                .map_err(|e| format!("run_state: {e}"))?;
            let out_ptr = ((packed >> 32) & 0xffff_ffff) as usize;
            let out_len = (packed & 0xffff_ffff) as usize;
            let mut out = vec![0u8; out_len];
            memory
                .read(&store, out_ptr, &mut out)
                .map_err(|e| format!("read: {e}"))?;
            Ok(out)
        }
    }
}
