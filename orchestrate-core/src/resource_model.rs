//! The resource and performance model an AI agent plans a playbook against.
//!
//! Published beside the grammar as `schema/resource-model.json` (and on
//! noetl.ai as `/dsl/resource-model.json`). The grammar says what a playbook
//! may contain; this says what it will cost to run: which steps hold a worker
//! slot, what limits stop them, how many slots exist, and what has actually
//! been measured.
//!
//! ## The honesty rule this file is built on
//!
//! Every figure carries a `label`:
//!
//! - `MEASURED` — observed on a cluster or a harness; `environment`, `date` and a
//!   public `source` say where. A measurement is a fact about the build and
//!   load it was taken under, not a promise about the next one.
//! - `DEFAULT` — a value written in code (env var + default), cited by
//!   repository and file. Deployments may override it.
//! - `DEPLOYED` — a value read off the production cluster on `as_of`. It is a
//!   snapshot of configuration, not of code, and changes without a release.
//! - `GUIDANCE` — a planning rule derived from the figures above; never a
//!   measurement.
//! - `TARGET` — a goal or a budget someone set; never a measurement.
//! - `NOT_YET_MEASURED` — the question is real and nobody has a number.
//!
//! A number with no label is a defect: the test below refuses it. Several
//! values here are read by noetl/worker or noetl/tools, not by this crate, so
//! they cannot be derived from code in this repository; their `source` names
//! the file they were read from at the stated commit.

use serde_json::{json, Value};

use crate::dsl::{
    AGENT_MAX_TOOL_CALLS_ENV, AGENT_TOOL_TIMEOUT_SECS_ENV, CATALOG_HOT_RELOAD_ENV,
    CATALOG_PINNING_ENV, CATALOG_URI_ALLOW_ENV, CATALOG_URI_FETCH_ENV, WASM_FUEL_ENV,
    WASM_MAX_MEMORY_MB_ENV, WASM_TIMEOUT_MS_ENV,
};

/// Bumped when the shape (not the figures) changes.
pub const RESOURCE_MODEL_VERSION: u32 = 1;
/// The date the non-code figures (measurements, cluster reads) were collected.
pub const RESOURCE_MODEL_AS_OF: &str = "2026-11-11";

/// The labels a figure may carry. Anything else fails the test.
pub const LABELS: [&str; 6] = [
    "MEASURED",
    "DEFAULT",
    "DEPLOYED",
    "GUIDANCE",
    "TARGET",
    "NOT_YET_MEASURED",
];

const W: &str = "noetl/worker@d1c9584";
const T: &str = "noetl/tools@v4.6.0 (the version noetl/worker pins)";
const SL: &str = "noetl/ai-meta wiki Sessions-Log";

fn src(repo: &str, path: &str) -> String {
    format!("{repo} {path}")
}

/// The resource model, as JSON. Pure and deterministic.
pub fn model() -> Value {
    json!({
        "model_version": RESOURCE_MODEL_VERSION,
        "as_of": RESOURCE_MODEL_AS_OF,
        "purpose": "Plan a playbook's time, slots, fuel, memory and tool calls before running it, then measure the run and refine.",
        "labels": {
            "MEASURED": "observed on a cluster or harness; read environment/date/source before reusing it",
            "DEFAULT": "a code default (env var + value); a deployment may override it",
            "DEPLOYED": "read off the production cluster on as_of; configuration, not code",
            "GUIDANCE": "a planning rule derived from the figures; not a measurement",
            "TARGET": "a goal or budget; not a measurement",
            "NOT_YET_MEASURED": "a real question with no number; do not invent one"
        },
        "read_first": [
            "Latency on NoETL is dominated by orchestration per step, not by the work a step does. Count steps and sub-playbook hops before estimating anything else.",
            "Measurements are dated and environment-specific. A figure from kind does not transfer to prod, and an older figure may predate a fix.",
            "Live values beat this file: a running server answers /api/dsl/version; a running worker's limits are its env; actual cost is in the events and metrics listed under metrics."
        ],
        "step_kinds": step_kinds(),
        "limits": limits(),
        "concurrency": concurrency(),
        "orchestration_overhead": orchestration_overhead(),
        "heuristics": heuristics(),
        "metrics": metrics(),
        "not_yet_measured": not_yet_measured(),
    })
}

fn step_kinds() -> Value {
    json!({
        "slot_rule": {
            "label": "DEFAULT",
            "text": "Every command holds one worker slot from claim until its tool returns, including case evaluation and its lifecycle event emits; a slot is released early only on a failed or refused claim. Waiting inside a tool (an awaited sub-playbook, a polled Kubernetes Job, an LLM call) keeps the slot.",
            "source": src(W, "src/worker.rs:748-754, 848-897 (permit moved into the spawned task)")
        },
        "no_global_timeout": {
            "label": "DEFAULT",
            "text": "The worker sets no wall-clock timeout per command. A step is bounded only by its own tool's timeout; kinds listed with timeout none can hold a slot indefinitely unless the step sets one.",
            "source": src(T, "src/registry.rs:241-259")
        },
        "retry_config_inert": {
            "label": "DEFAULT",
            "text": "ToolConfig.retry (max_retries 3, 500 ms initial, x2, 10 s max) is parsed but no executor applies it; do not budget for automatic tool retries.",
            "source": src(T, "src/registry.rs:36-68")
        },
        "kinds": [
            {
                "kind": "python",
                "execution": "subprocess",
                "holds_slot": "whole run",
                "timeout": { "value": null, "meaning": "none unless the step sets timeout_seconds", "label": "DEFAULT", "source": src(T, "src/tools/python.rs:637, 675-700") },
                "other_limits": [{ "what": "remote script-source loader", "value": 30, "unit": "s", "label": "DEFAULT", "source": src(T, "src/tools/python.rs:23") }],
                "memory_class": "the subprocess shares the worker pod's memory limit; stdout is collected whole",
                "measured": [
                    { "what": "test/simple_python playbook end to end on idle kind", "value": "0.66 s warm / 1.20 s cold", "label": "MEASURED", "environment": "kind", "date": "2026-05", "source": "noetl/ai-meta handoffs/archive/2026-05-23-gke-provision-validation/round-01-prompt.md" },
                    { "what": "provider dispatch moved from a kind: playbook child to inline python (HotelBeds hotels)", "value": "9.3 s -> 3.0 s", "label": "MEASURED", "environment": "prod", "date": "2026-08-24", "source": "noetl/travel wiki playbook-itinerary-planner (#286); ".to_owned() + SL + " 2026-08-24" }
                ]
            },
            {
                "kind": "http",
                "execution": "in-process (reqwest)",
                "holds_slot": "whole request",
                "timeout": { "value": 30, "unit": "s", "meaning": "client default; per-request timeout_seconds overrides it", "label": "DEFAULT", "source": src(T, "src/tools/http.rs:81, 167-168") },
                "other_limits": [{ "what": "response body cap", "value": null, "meaning": "none: the body is read in full", "label": "DEFAULT", "source": src(T, "src/tools/http.rs:249-259") }],
                "memory_class": "the response body is held in memory",
                "measured": []
            },
            {
                "kind": "postgres",
                "execution": "in-process (deadpool-postgres, a pool per connection string)",
                "holds_slot": "whole query",
                "timeout": { "value": null, "meaning": "none set by the tool", "label": "DEFAULT", "source": src(T, "src/tools/postgres.rs:202-224") },
                "measured": []
            },
            {
                "kind": "duckdb",
                "aliases": ["ducklake"],
                "execution": "in-process (spawn_blocking)",
                "holds_slot": "whole query",
                "timeout": { "value": null, "meaning": "none", "label": "DEFAULT", "source": src(T, "src/tools/duckdb.rs:360") },
                "memory_class": "runs inside the worker process: a large query spends the pod's memory limit",
                "measured": []
            },
            {
                "kind": "rhai",
                "execution": "in-process (spawn_blocking)",
                "holds_slot": "whole script",
                "timeout": { "value": null, "meaning": "none, and no operation limits are set", "label": "DEFAULT", "source": src(T, "src/tools/rhai.rs:46, 76-81") },
                "measured": []
            },
            {
                "kind": "wasm",
                "execution": "in-process (wasmtime); one module runs at a time per worker process (the dispatcher sits behind a mutex)",
                "holds_slot": "whole invocation, plus any wait for the per-process wasm lock",
                "timeout": { "env": WASM_TIMEOUT_MS_ENV, "value": 30_000, "unit": "ms", "label": "DEFAULT", "source": src(W, "src/plugin.rs:44, 132-137") },
                "other_limits": [
                    { "env": WASM_FUEL_ENV, "value": 10_000_000_000u64, "unit": "fuel units", "label": "DEFAULT", "source": src(W, "src/plugin.rs:41, 131") },
                    { "env": WASM_MAX_MEMORY_MB_ENV, "value": 1024, "unit": "MiB linear memory", "label": "DEFAULT", "source": src(W, "src/plugin.rs:47, 138-143") }
                ],
                "serialization_source": src(W, "src/executor/command.rs:163, 2415"),
                "measured": [
                    { "what": "first dispatch of a ~1.6 MB module on a fresh pod (Cranelift compile ~99%, fetch ~1%); later hops hit the digest cache, and the drive pool pre-compiles at boot behind /readyz", "value": "~1.5 s once", "label": "MEASURED", "environment": "kind (fresh system-pool pod)", "date": "2026-06", "source": "noetl/worker wiki deployment-specification (boot warmup)" }
                ]
            },
            {
                "kind": "mcp",
                "execution": "remote JSON-RPC over HTTP",
                "holds_slot": "whole call",
                "timeout": { "env": "NOETL_MCP_REQUEST_TIMEOUT_SECONDS", "value": 60, "unit": "s", "meaning": "used when the step sets no timeout; clamped to NOETL_WORKER_COMMAND_TIMEOUT_SECONDS (default 180 s, read only here)", "label": "DEFAULT", "source": src(T, "src/tools/mcp.rs:78-82, 782-810") },
                "measured": []
            },
            {
                "kind": "agent",
                "execution": "in-process loop; every model call is a remote HTTP request; mcp_servers / agents entries are answered inside the loop",
                "holds_slot": "the whole loop, including every model call and every awaited playbook-bound child",
                "timeout": { "value": 300, "unit": "s", "meaning": "model HTTP client timeout per request (ToolConfig.timeout default)", "label": "DEFAULT", "source": src(T, "src/tools/agent.rs:79-85") },
                "other_limits": [
                    { "what": "max_turns", "value": 8, "label": "DEFAULT", "source": src(T, "src/tools/agent.rs:429-433") },
                    { "what": "max_tokens per model call", "value": 4096, "label": "DEFAULT", "source": src(T, "src/tools/agent.rs:464") },
                    { "what": "mcp_servers[].timeout", "value": 60, "unit": "s", "meaning": "clamped 1-600", "label": "DEFAULT", "source": src(T, "src/tools/agent.rs:697-700") },
                    { "env": AGENT_MAX_TOOL_CALLS_ENV, "value": 32, "unit": "tool calls per agent step (hosted loop)", "label": "DEFAULT", "source": src(W, "src/executor/agent_host.rs:50, 94-96") },
                    { "env": AGENT_TOOL_TIMEOUT_SECS_ENV, "value": 180, "unit": "s per playbook-bound child", "label": "DEFAULT", "source": src(W, "src/executor/agent_host.rs:51, 97") }
                ],
                "durability": "the hosted loop lives in one command: if its worker dies the step restarts from turn 1 and children that already ran run again",
                "measured": [
                    { "what": "a hosted LLM (OpenAI / Anthropic) call inside a real playbook step", "value": null, "label": "NOT_YET_MEASURED" },
                    { "what": "candidate small language model, unoptimised local serving, 2 generations per turn", "value": "p50 9.2 s / p95 13.1 s per turn", "label": "MEASURED", "environment": "local", "source": "noetl/travel wiki Travel-SLM-Journey" }
                ]
            },
            {
                "kind": "playbook",
                "execution": "remote: POST /api/execute starts a separate child execution on whatever pool its steps target",
                "holds_slot": "only for the start request by default (async: returns {status, execution_id}); with return_result: true or return_step the parent's slot is held, polling every 2 s, until the child finishes",
                "timeout": { "value": 30, "unit": "s", "meaning": "start request; the await wait is the step's timeout, else 180 s (return_result) / 300 s (return_step)", "label": "DEFAULT", "source": src(T, "src/tools/playbook.rs:42-69, 237-242, 402-405, 433-488") },
                "deadlock_note": "an awaited child needs a free slot on its own pool while the parent holds one; if both target the same small pool, enough concurrent awaiting parents can occupy every slot",
                "measured": [
                    { "what": "orchestration cost of one kind: playbook hop, regardless of the work inside it", "value": "~9 s", "label": "MEASURED", "environment": "prod", "date": "2026-08-24", "source": "noetl/travel wiki playbook-itinerary-planner (#286); ".to_owned() + SL + " 2026-08-24" },
                    { "what": "child-playbook hop vs the same work inline (Firestore read: 369 ms of work took 11.4 s as a child; Places: 663 ms of work took 1.5 s inline)", "value": "~10-15 s per hop", "label": "MEASURED", "environment": "prod", "date": "2026-08", "source": "noetl/travel wiki playbook-itinerary-planner#latency" },
                    { "what": "paired A/B: provider children inlined into the planner as python (4 places turns each)", "value": "median 34.7 s -> 19.4 s (-44%)", "label": "MEASURED", "environment": "prod", "date": "2026-06-29", "source": SL.to_owned() + " 2026-06-29" }
                ]
            },
            {
                "kind": "script",
                "execution": "remote Kubernetes Job, polled inline every 2 s",
                "holds_slot": "until the Job finishes",
                "timeout": { "value": 600, "unit": "s", "meaning": "job.timeout default, also the Job's activeDeadlineSeconds", "label": "DEFAULT", "source": src(T, "src/tools/script.rs:110-121, 565-611") },
                "measured": []
            },
            {
                "kind": "container",
                "execution": "remote Kubernetes Job; returns at once with a pending callback",
                "holds_slot": "no: the slot is freed after the Job is created (the callback pattern)",
                "timeout": { "value": null, "meaning": "the Job's timeout_seconds; none by default", "label": "DEFAULT", "source": src(T, "src/tools/container.rs:12-21, 164") },
                "measured": []
            },
            {
                "kind": "snowflake",
                "execution": "remote REST",
                "holds_slot": "whole statement",
                "timeout": { "value": 60, "unit": "s", "meaning": "statement timeout sent in the request body; the HTTP client sets none", "label": "DEFAULT", "source": src(T, "src/tools/snowflake.rs:182-185, 435") },
                "measured": []
            },
            {
                "kind": "subscription",
                "execution": "in-process bounded drain (nats / pubsub / kafka / timer)",
                "holds_slot": "one bounded drain",
                "timeout": { "value": 1000, "unit": "ms", "meaning": "timeout_ms default, cap 5000; batch default 100, cap 1000", "label": "DEFAULT", "source": src(T, "src/tools/source/mod.rs:59-86") },
                "measured": []
            },
            {
                "kind": "task_sequence",
                "execution": "in-process; runs its sub-tasks in order through a fresh default registry (so no worker mcp override and no agent)",
                "holds_slot": "all sub-tasks",
                "timeout": { "value": null, "meaning": "per sub-task", "label": "DEFAULT", "source": src(T, "src/tools/task_sequence.rs:213-218") },
                "other_limits": [{ "env": "NOETL_TASK_SEQUENCE_MAX_ITERATIONS", "value": 1_000_000, "unit": "iterations", "label": "DEFAULT", "source": src(T, "src/tools/task_sequence.rs:253-257") }],
                "measured": []
            },
            {
                "kind": "shell",
                "execution": "subprocess",
                "holds_slot": "whole run",
                "timeout": { "value": null, "meaning": "none unless the step sets timeout_seconds", "label": "DEFAULT", "source": src(T, "src/tools/shell.rs:156-163") },
                "measured": []
            },
            {
                "kind": "transfer",
                "execution": "in-process (snowflake / postgres / http / duckdb)",
                "holds_slot": "whole transfer",
                "timeout": { "value": null, "meaning": "HTTP client sets none; chunk_size default 1000 rows", "label": "DEFAULT", "source": src(T, "src/tools/transfer.rs:262-264, 308") },
                "measured": []
            },
            {
                "kind": "noop",
                "execution": "in-process",
                "holds_slot": "briefly; it still pays the per-step orchestration cost",
                "measured": []
            }
        ],
        "kinds_note": "Profiled: the kinds whose execution was read in the worker and tools code. Also accepted by the parser and not profiled here: artifact, gateway, gcs, iterator, nats, playbooks, secrets, snowflake_transfer, workbook (see the tool-kind manifest); nothing is measured for them."
    })
}

fn limits() -> Value {
    json!([
        { "env": WASM_FUEL_ENV, "value": 10_000_000_000u64, "unit": "fuel units per invocation", "label": "DEFAULT", "source": src(W, "src/plugin.rs:41"), "note": "0 / off / none / unlimited removes the limit; an unparseable value keeps the default; a trip increments noetl_worker_plugin_limit_trips_total{limit}" },
        { "env": WASM_TIMEOUT_MS_ENV, "value": 30_000, "unit": "ms wall clock per invocation", "label": "DEFAULT", "source": src(W, "src/plugin.rs:44"), "note": "10 ms deadline granularity" },
        { "env": WASM_MAX_MEMORY_MB_ENV, "value": 1024, "unit": "MiB linear memory", "label": "DEFAULT", "source": src(W, "src/plugin.rs:47") },
        { "env": AGENT_MAX_TOOL_CALLS_ENV, "value": 32, "unit": "tool calls per agent step", "label": "DEFAULT", "source": src(W, "src/executor/agent_host.rs:50"), "note": "positive integers only; the model cannot raise it; exceeding it ends the step with agent.final refused: tool_call_budget" },
        { "env": AGENT_TOOL_TIMEOUT_SECS_ENV, "value": 180, "unit": "s per playbook-bound child", "label": "DEFAULT", "source": src(W, "src/executor/agent_host.rs:51") },
        { "env": "WORKER_MAX_CONCURRENT", "value": 4, "unit": "slots per worker process", "label": "DEFAULT", "source": src(W, "src/config.rs:115-118") },
        { "env": "NOETL_EVENT_RESULT_CONTEXT_MAX_BYTES", "value": 102_400, "unit": "bytes", "label": "DEFAULT", "source": src(W, "src/executor/command.rs:1843, 1857-1865"), "note": "a step result above it is spilled to the result store and replaced by a reference: a downstream read then costs a fetch" },
        { "env": "NOETL_COMMAND_CONTEXT_MAX_BYTES", "value": 524_288, "unit": "bytes", "label": "DEFAULT", "source": "noetl/server src/config/app.rs:242-246", "note": "a command context above it is offloaded as a __context_ref__" },
        { "what": "request body limit on /api/execute and /api/events", "value": 2_097_152, "unit": "bytes", "label": "DEFAULT", "source": "noetl/server src/main.rs:640-645 (the axum default; no explicit layer)" },
        { "what": "GET /api/executions/{id} events returned", "value": 2000, "unit": "most recent events", "label": "DEFAULT", "source": "noetl/server src/services/execution.rs:694" },
        { "env": "NOETL_EVENT_EMIT_OUTAGE_BUDGET_SECS", "value": 120, "unit": "s", "label": "DEFAULT", "source": src(W, "src/client/control_plane.rs:125, 133-140"), "note": "extra retry window for event emits during a server outage; the slot stays held" },
        { "env": "NOETL_WORKER_DRAIN_TIMEOUT_SECS", "value": 150, "unit": "s", "label": "DEFAULT", "source": src(W, "src/worker.rs:1139"), "note": "on SIGTERM a worker finishes in-flight commands within this budget" },
        {
            "what": "catalog URI fetch (source: references)",
            "label": "DEFAULT",
            "gates": [CATALOG_URI_FETCH_ENV, CATALOG_URI_ALLOW_ENV],
            "value": "off; the allowlist is empty by default, which fetches nothing (fail closed, HTTP 403)",
            "costs": [
                { "what": "http(s) fetch timeout, redirects disabled", "value": 15, "unit": "s", "label": "DEFAULT", "source": "noetl/server src/services/catalog_peer.rs:218-221" },
                { "what": "db:// connect / query timeout; the connection is opened for one read", "value": "10 s / 15 s", "label": "DEFAULT", "source": "noetl/server src/services/catalog_sources.rs:46-47" },
                { "what": "ehdb:// read: linear scan, 16 records per page, at most 4000 pages, 60 s per page", "value": "worst case 64,000 records scanned", "label": "DEFAULT", "source": "noetl/server src/services/catalog_sources.rs:49-50, 312, 320" },
                { "what": "fetched body cap (UTF-8 only)", "value": 1_048_576, "unit": "bytes", "label": "DEFAULT", "source": "noetl/server src/services/catalog_peer.rs:76" },
                { "what": "fetch cache", "value": "none: every locate fetches again; the imported body is deduplicated by digest", "label": "DEFAULT", "source": "noetl/server src/services/catalog_peer.rs:591-593, 863-867" },
                { "what": "retries", "value": "none", "label": "DEFAULT", "source": "noetl/server src/services/catalog_peer.rs:896-951" },
                { "what": "latency of a URI fetch", "value": null, "label": "NOT_YET_MEASURED" }
            ],
            "deployed": { "value": "off on prod (no catalog CAS / pinning / discovery / URI flags in the server env)", "label": "DEPLOYED", "source": "prod cluster read, as_of" }
        },
        {
            "what": "version pinning and hot reload",
            "label": "DEFAULT",
            "gates": [CATALOG_PINNING_ENV, CATALOG_HOT_RELOAD_ENV],
            "value": "off. With pinning on, a root execution records a catalog watermark before its first event and every bare-path lookup in its tree resolves at or below it; hot_reload only applies to paths the ROOT lists (at most 64 globs) and referenced explicitly as @latest",
            "cost": "pinning adds no fetch: resolution is a bounded catalog lookup. Without pinning, /api/execute reads the latest catalog row on every request (no cache)",
            "source": "noetl/server orchestrate-core/src/catalog.rs:338-353, 406-416, 505-529; src/handlers/execute.rs:687-699",
            "deployed": { "value": "off on prod", "label": "DEPLOYED", "source": "prod cluster read, as_of" }
        },
        {
            "what": "loops and back-edges (goto)",
            "label": "DEFAULT",
            "value": "No server cap on iterations, steps, back-edge repeats or sub-playbook depth. A back-edge re-dispatches its target until the arc's when: guard goes false; the playbook's own guard is the only bound.",
            "cost": "every iteration and every back-edge pass is a full step: it pays the per-step orchestration cost below and holds a slot",
            "parallel": "loop.spec.mode: parallel dispatches every item at once. loop.spec.max_in_flight is parsed but has no reader, so it does not throttle anything",
            "source": "noetl/server orchestrate-core/src/orchestrator.rs:150-198, 1119-1240, 1783-1820; src/playbook.rs:778-780"
        },
        { "env": "NOETL_RECONCILE_MAX_NOOPS", "value": 225, "unit": "consecutive no-progress polls at an 8 s tick (~30 min)", "label": "DEFAULT", "source": "noetl/server src/config/app.rs:702-726", "note": "how long a stuck execution is re-driven before the server stops" }
    ])
}

fn concurrency() -> Value {
    json!({
        "slot_model": {
            "label": "DEFAULT",
            "text": "A worker process has WORKER_MAX_CONCURRENT slots (a semaphore). A slot is acquired before the worker pulls a command and released when that command's tool returns. A pool's capacity is replicas x slots; commands beyond it queue on the command bus.",
            "source": src(W, "src/worker.rs:313, 748-754")
        },
        "routing": {
            "label": "DEFAULT",
            "text": "A worker claims only its pool's subject (commands.<pool>.>); a step reaches a pool through the pool its command is published to. Orchestration of system/* drives runs on the system pool.",
            "source": src(W, "src/worker.rs:233-244")
        },
        "pools": [
            { "pool": "worker-rust-pool (user pool, Deployment noetl-worker-rust)", "replicas": "2 (KEDA 2-20)", "slots_per_replica": 4, "capacity_slots": "8 at min, 80 at max", "cpu": "250m request / 2 limit", "memory": "256Mi request / 768Mi limit", "label": "DEPLOYED", "source": "prod cluster read, as_of" },
            { "pool": "worker-system-pool (Deployments noetl-worker-system-pool + -shard1)", "replicas": "1 + 1, fixed (no autoscaler)", "slots_per_replica": 2, "capacity_slots": 4, "cpu": "750m request / 2 limit", "memory": "3Gi / 3Gi", "label": "DEPLOYED", "source": "prod cluster read, as_of" }
        ],
        "autoscaling": {
            "deployed": { "label": "DEPLOYED", "source": "prod cluster read, as_of", "scaled_objects": [
                { "target": "noetl-worker-rust", "min": 2, "max": 20, "trigger": "metrics-api: ehdb_feed_subject_lag{subject=\"commands.shared.shard.0\"} from the command-bus writer", "target_value": 2, "polling_interval_s": 10, "cooldown_s": 30, "rule": "replicas ~ ceil(lag / 2)" }
            ], "note": "ops/ci/manifests also declares a system-pool ScaledObject (3-10); it is not on the cluster, so the system pool does not scale" },
            "measured": [
                { "what": "user pool under 200 real executions (200/200 completed)", "value": "replicas 2 -> 4 -> 8 -> 16 -> 20 -> 2", "label": "MEASURED", "environment": "prod", "date": "2026-07-31", "source": "noetl/ai-meta wiki Home (#210)" },
                { "what": "time for the user pool to ramp under load on the EHDB trigger", "value": null, "label": "NOT_YET_MEASURED" },
                { "what": "GKE Autopilot node provisioning when a scale-up needs a new node", "value": "~60-90 s", "label": "MEASURED", "environment": "prod", "source": "noetl/ai-meta playbooks/load-test/README.md" }
            ]
        },
        "measured": [
            { "what": "command.issued -> command.claimed, pool not saturated", "value": "p50 138.5 / p95 156.4 / p99 181.0 ms (n=60)", "label": "MEASURED", "environment": "prod", "date": "2026-07-30", "source": SL.to_owned() + " 2026-07-30 (prod deploy)" },
            { "what": "same, with 90 commands offered in 1.2 s against 8 slots (2 replicas x 4)", "value": "p50 2123 / p99 3735 ms", "label": "MEASURED", "environment": "prod", "date": "2026-07-30", "source": SL.to_owned() + " 2026-07-30 (prod deploy)", "note": "this measures the pool queue, not the bus" },
            { "what": "POST /api/execute submit latency by client concurrency (server v3.100.4, 2 reps)", "value": "c=1 p50 120-150 ms; c=2 p50 140-150 ms; c>=4 bimodal with errors and 30 s timeouts", "label": "MEASURED", "environment": "prod", "date": "2026-09-03", "source": "noetl/ai-meta wiki Umbrella-Request-Latency", "note": "p95 at c=1 rests on single samples" },
            { "what": "sustained executions per second the platform can complete", "value": null, "label": "NOT_YET_MEASURED" }
        ]
    })
}

fn orchestration_overhead() -> Value {
    json!({
        "summary": "Each step pays a fixed orchestration cost (command dispatch, claim, and three lifecycle event emits) on top of its work; each kind: playbook hop pays a whole child execution's worth.",
        "measured": [
            { "what": "fixed cost per step in the travel itinerary planner; events spaced a near-uniform 0.8-1.2 s apart", "value": "~4.5 s per step", "label": "MEASURED", "environment": "prod", "date": "2026-08", "source": "noetl/travel wiki playbook-itinerary-planner#latency" },
            { "what": "share of a planner turn that is orchestration (63 events, ~15-17 s of work in a ~76-107 s turn)", "value": "~82%", "label": "MEASURED", "environment": "prod", "date": "2026-08", "source": "noetl/travel wiki playbook-itinerary-planner#latency" },
            { "what": "10-hop probe playbook: share of a turn in two command-dispatch gaps per hop (~510 ms p50 each)", "value": "79%", "label": "MEASURED", "environment": "prod", "date": "2026-08-18", "source": SL.to_owned() + " 2026-08-18" },
            { "what": "server event ingest per emit: command.started / command.completed vs call.done (three emits per hop)", "value": "~336 ms / ~1021 ms on prod, ~40 ms on kind", "label": "MEASURED", "environment": "prod and kind", "source": "noetl/server src/metrics.rs (doc comment on noetl_event_ingest_phase_seconds)" },
            { "what": "prod smoke turns through the gateway: travel planner turn / ranch concierge turn", "value": "17.0-21.2 s / 6.4-7.0 s", "label": "MEASURED", "environment": "prod", "date": "2026-11-04", "source": SL.to_owned() + " 2026-11-04" },
            { "what": "first drive of a fresh system-pool pod after a roll (state builder replays the feed)", "value": "drives stall ~7-10 min", "label": "MEASURED", "environment": "prod", "date": "2026-11-03", "source": SL.to_owned() + " 2026-11-03 (early)" },
            { "what": "command-bus writer restart (lease election)", "value": "failover window 21 s; plan for a 20-60 s write outage", "label": "MEASURED", "environment": "prod", "date": "2026-11-04", "source": "noetl/worker wiki cmdbus-writer-tier-service" }
        ],
        "caveat": "Per-step cost varies with load and with the server build: several of these figures predate later dispatch fixes. Measure on the target cluster at the target load before committing to a budget."
    })
}

fn heuristics() -> Value {
    json!([
        { "label": "GUIDANCE", "rule": "Estimate wall clock as steps x per_step_overhead + sum(step work) + hops x hop_overhead, where hops are kind: playbook children. On prod today use ~4.5 s per step and ~9-15 s per hop (orchestration_overhead.measured), and re-measure: the overhead, not the work, decides the total." },
        { "label": "GUIDANCE", "rule": "Prefer an inline python step to a kind: playbook child when the child only wraps one call: each hop measured ~9-15 s of orchestration on prod, and inlining cut a planner turn by 44%." },
        { "label": "GUIDANCE", "rule": "Count slots, not steps, for concurrency: a parallel loop dispatches every item at once and max_in_flight does not throttle it, so N items need N free slots on the target pool or the rest queue. The prod user pool has 8 slots at minimum and 80 at maximum; the system pool has 4." },
        { "label": "GUIDANCE", "rule": "An awaited child (return_result / return_step) or a hosted agent's playbook tool holds the parent's slot while the child needs another. Budget two slots per awaiting parent and avoid sending both to a small pool." },
        { "label": "GUIDANCE", "rule": "Bound every loop and back-edge with a when: guard on an explicit counter: the server enforces no iteration cap, and each pass is a full step." },
        { "label": "GUIDANCE", "rule": "Set timeout_seconds on python, shell, postgres, duckdb and rhai steps that call anything slow: those kinds have no default timeout and the worker has no per-command one." },
        { "label": "GUIDANCE", "rule": "For kind: wasm, plan inside the defaults (30 s, 10e9 fuel, 1024 MiB) and expect one module at a time per worker process: concurrent wasm steps on one pod serialize. Read fuel actually spent from the worker's debug log; no metric reports it." },
        { "label": "GUIDANCE", "rule": "For kind: agent, the tool-call budget (32) and child timeout (180 s) bound a hosted loop: worst case is about 32 x 180 s = 96 min of one held slot. Size max_turns and the tool list so the expected path uses a few calls, and read agent.final for the real count." },
        { "label": "GUIDANCE", "rule": "Keep step results under 100 KiB when a downstream step reads them: larger results are spilled to the result store and every read becomes a fetch." },
        { "label": "GUIDANCE", "rule": "Choose a pool by where the work must run (credentials, network, memory) first; then check its slot capacity against the plan's peak parallelism. Bursts above capacity on the user pool trigger KEDA scale-up, which can add node provisioning time (~60-90 s) on Autopilot." },
        { "label": "TARGET", "rule": "Orchestrator-driven resume of a hosted agent loop after a worker dies (today the step restarts from turn 1).", "source": "noetl/server orchestrate-core dsl-manifest worker_runtime.agent.playbook_tools.not_durable" }
    ])
}

fn metrics() -> Value {
    json!({
        "events": [
            { "type": "call.done", "fields": "result.context: status, duration_ms, error, exit_code, child_execution_id; an agent step's result data carries usage {input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens}, turns, model, stop_reason", "source": src(W, "src/executor/command.rs:1097-1110") + "; " + &src(T, "src/result.rs:62-135, src/tools/agent.rs:838-851") },
            { "type": "command.started / command.completed / command.failed", "fields": "created_at on each event: the difference is the step's wall clock including slot wait after claim", "source": src(W, "src/executor/command.rs:545-557, 1352-1361") },
            { "type": "agent.turn", "fields": "turn, outcome (tool_calls | final), tool_calls [{id, name}]", "source": src(W, "src/executor/agent_host.rs:226-254") },
            { "type": "agent.tool_call", "fields": "turn, tool_use_id, tool, binding, playbook, child_execution_id, is_error, duration_ms, input and result previews (2 KiB)", "source": src(W, "src/executor/agent_host.rs:307-321") },
            { "type": "agent.final", "fields": "turns, tool_calls, child_execution_ids, duration_ms; or refused: tool_call_budget", "source": src(W, "src/executor/agent_host.rs:231-240, 259-262") }
        ],
        "events_note": "agent.* events come only from the hosted loop (NOETL_WORKER_AGENT_PLAYBOOK_TOOLS=true). Token usage is in call.done result data, not on agent.* events.",
        "worker_prometheus": [
            { "name": "noetl_worker_dispatch_duration_seconds", "type": "histogram", "labels": "tool_kind", "meaning": "full dispatch per command: tool + case evaluation + lifecycle emits; buckets 0.01-120 s" },
            { "name": "noetl_worker_concurrent_dispatches", "type": "gauge", "meaning": "slots in use on this worker" },
            { "name": "noetl_worker_command_pickup_phase_seconds", "type": "histogram", "labels": "phase (park | outcome)", "meaning": "claim latency split" },
            { "name": "noetl_worker_event_emit_duration_seconds", "type": "histogram", "labels": "event_type" },
            { "name": "noetl_worker_plugin_load_seconds", "type": "histogram", "labels": "phase (fetch | compile)", "meaning": "wasm cold start" },
            { "name": "noetl_worker_plugin_limit_trips_total", "type": "counter", "labels": "limit (fuel | timeout | memory)" },
            { "name": "noetl_worker_result_store_put_duration_seconds", "type": "histogram", "meaning": "cost of spilling an over-budget result" },
            { "name": "noetl_worker_build_info", "type": "gauge", "labels": "version", "meaning": "which worker build answered; absence of a newer metric means that build predates it" }
        ],
        "worker_prometheus_source": src(W, "src/metrics.rs") + ", served on WORKER_METRICS_BIND, else 0.0.0.0:$PORT, else 0.0.0.0:9090",
        "server_prometheus": [
            { "name": "noetl_event_ingest_duration_seconds", "type": "histogram", "labels": "event_type" },
            { "name": "noetl_event_ingest_phase_seconds", "type": "histogram", "labels": "phase", "meaning": "where inside event ingest the time goes" },
            { "name": "noetl_orchestrate_in_flight_executions / noetl_orchestrate_oldest_seconds", "type": "gauge" },
            { "name": "noetl_execute_outcomes_total", "type": "counter", "labels": "entry, outcome" }
        ],
        "server_prometheus_source": "noetl/server src/metrics.rs, served at GET /metrics",
        "apis": [
            { "route": "GET /api/executions/{id}", "returns": "started_at, completed_at and up to 2000 most recent events with created_at; no duration field (subtract)" },
            { "route": "GET /api/executions/{id}/status", "returns": "progress step counts only, no times" }
        ],
        "not_available": [
            "no per-execution or per-step duration histogram on the server",
            "no metric for wasm fuel consumed (debug log only)",
            "no metric for token usage (call.done result data only)",
            "no per-tool memory metric"
        ]
    })
}

fn not_yet_measured() -> Value {
    json!([
        "per-step latency of http, duckdb, postgres and mcp steps on prod",
        "latency of a hosted LLM call inside a real playbook step",
        "sustained executions per second on prod",
        "worker slot occupancy over time",
        "user-pool KEDA ramp time on the EHDB trigger, and scale-down time on prod",
        "catalog URI fetch latency",
        "unsaturated prod command-bus latency after 2026-07-30"
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk every object; any object with a numeric or string `value` must carry
    /// a known label (directly or from the nearest labelled ancestor).
    fn check(v: &Value, inherited: Option<&str>, path: &str, bad: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                let label = m.get("label").and_then(Value::as_str).or(inherited);
                if let Some(l) = m.get("label").and_then(Value::as_str) {
                    if !LABELS.contains(&l) {
                        bad.push(format!("{path}: unknown label {l}"));
                    }
                }
                if m.contains_key("value") && label.is_none() {
                    bad.push(format!("{path}: value without a label"));
                }
                if label == Some("MEASURED") && m.contains_key("value") && !m.contains_key("source")
                {
                    bad.push(format!("{path}: MEASURED figure without a source"));
                }
                if label == Some("NOT_YET_MEASURED") && m.get("value").is_some_and(|v| !v.is_null())
                {
                    bad.push(format!("{path}: NOT_YET_MEASURED carries a number"));
                }
                if let Some(e) = m.get("env") {
                    let ok = e.as_str().is_some_and(|e| {
                        !e.is_empty()
                            && e.chars()
                                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    });
                    if !ok {
                        bad.push(format!("{path}: env must name an env var (use environment for where a figure was measured)"));
                    }
                }
                for (k, c) in m {
                    check(c, label, &format!("{path}.{k}"), bad);
                }
            }
            Value::Array(a) => {
                for (i, c) in a.iter().enumerate() {
                    check(c, inherited, &format!("{path}[{i}]"), bad);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn every_figure_is_labelled_and_measured_ones_are_sourced() {
        let mut bad = Vec::new();
        check(&model(), None, "$", &mut bad);
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// Negative control: the walker must be able to fail.
    #[test]
    fn the_label_check_can_fail() {
        let mut bad = Vec::new();
        check(
            &json!({"x": {"value": 1}, "y": {"value": 2, "label": "MEASURED"}, "z": {"value": 3, "label": "GUESS"}}),
            None,
            "$",
            &mut bad,
        );
        assert_eq!(bad.len(), 3, "{bad:#?}");
    }

    #[test]
    fn wasm_and_agent_limits_match_the_worker_runtime_section() {
        let lim = model()["limits"].clone();
        let get = |env: &str| {
            lim.as_array()
                .unwrap()
                .iter()
                .find(|l| l["env"] == env)
                .unwrap_or_else(|| panic!("{env} missing"))["value"]
                .clone()
        };
        let wr = crate::dsl::worker_runtime();
        for l in wr["wasm_limits"]["limits"].as_array().unwrap() {
            assert_eq!(get(l["env"].as_str().unwrap()), l["default"]);
        }
        for b in wr["agent"]["budgets"].as_array().unwrap() {
            assert_eq!(get(b["env"].as_str().unwrap()), b["default"]);
        }
    }

    #[test]
    fn step_kinds_are_known_tool_kinds() {
        let known: Vec<String> = crate::dsl::tool_kinds(crate::playbook::ToolKindGates::OFF)
            .into_iter()
            .map(|k| k.name)
            .collect();
        for k in model()["step_kinds"]["kinds"].as_array().unwrap() {
            let name = k["kind"].as_str().unwrap();
            assert!(known.iter().any(|n| n == name), "{name} is not a tool kind");
        }
    }
}
