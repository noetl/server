//! NoETL DSL v2 Types - Canonical Format
//!
//! Complete type definitions for NoETL playbooks:
//! - tool as ordered pipeline (list of labeled tasks) or single tool shorthand
//! - step.when for transition enable guard
//! - next[].when for conditional routing
//! - loop.spec.mode for iteration mode
//! - tool.eval for per-task flow control
//! - No case/when/then blocks (removed in canonical format)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Supported tool kinds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Http,
    Postgres,
    Duckdb,
    Ducklake,
    Python,
    Workbook,
    Playbook,
    Playbooks,
    Secrets,
    Iterator,
    Container,
    Script,
    Snowflake,
    Transfer,
    SnowflakeTransfer,
    Gcs,
    Gateway,
    Nats,
    Shell,
    Artifact,
    Noop,
    TaskSequence,
    Rhai,
}

impl std::fmt::Display for ToolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ToolKind::Http => "http",
            ToolKind::Postgres => "postgres",
            ToolKind::Duckdb => "duckdb",
            ToolKind::Ducklake => "ducklake",
            ToolKind::Python => "python",
            ToolKind::Workbook => "workbook",
            ToolKind::Playbook => "playbook",
            ToolKind::Playbooks => "playbooks",
            ToolKind::Secrets => "secrets",
            ToolKind::Iterator => "iterator",
            ToolKind::Container => "container",
            ToolKind::Script => "script",
            ToolKind::Snowflake => "snowflake",
            ToolKind::Transfer => "transfer",
            ToolKind::SnowflakeTransfer => "snowflake_transfer",
            ToolKind::Gcs => "gcs",
            ToolKind::Gateway => "gateway",
            ToolKind::Nats => "nats",
            ToolKind::Shell => "shell",
            ToolKind::Artifact => "artifact",
            ToolKind::Noop => "noop",
            ToolKind::TaskSequence => "task_sequence",
            ToolKind::Rhai => "rhai",
        };
        write!(f, "{}", s)
    }
}

// ============================================================================
// Eval Condition - Tool-level flow control
// ============================================================================

/// Eval condition for per-task flow control.
/// Evaluated after each tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalCondition {
    /// Jinja2 expression to evaluate.
    /// Access outcome object: outcome.status, outcome.error, outcome.result
    #[serde(default)]
    pub expr: Option<String>,

    /// Action to take: continue, retry, break, jump, fail
    #[serde(rename = "do")]
    pub action: String,

    /// Retry attempts (for do: retry).
    #[serde(default)]
    pub attempts: Option<i32>,

    /// Retry backoff strategy: linear or exponential.
    #[serde(default)]
    pub backoff: Option<String>,

    /// Retry delay in seconds.
    #[serde(default)]
    pub delay: Option<f64>,

    /// Variables to set (step-scoped).
    #[serde(default)]
    pub set_vars: Option<HashMap<String, serde_json::Value>>,

    /// Target step (for do: jump).
    #[serde(default)]
    pub target: Option<String>,
}

/// Else clause for eval conditions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalElse {
    /// Action to take.
    #[serde(rename = "do")]
    pub action: String,

    /// Variables to set.
    #[serde(default)]
    pub set_vars: Option<HashMap<String, serde_json::Value>>,
}

// ============================================================================
// Tool Specification
// ============================================================================

/// Tool specification with tool.kind pattern.
/// All execution-specific fields live under tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool type.
    pub kind: ToolKind,

    /// Tool-level flow control (canonical format).
    /// Evaluated after tool execution.
    #[serde(default)]
    pub eval: Option<Vec<EvalEntry>>,

    /// Authentication configuration.
    #[serde(default)]
    pub auth: Option<serde_json::Value>,

    /// Libraries/dependencies.
    #[serde(default)]
    pub libs: Option<serde_json::Value>,

    /// Default arguments / inputs passed to the tool runtime.
    ///
    /// The canonical NoETL v10 playbook YAML writes this as
    /// `input:` at the step's `tool:` block.  The Rust internal
    /// name stays `args` because that's what the noetl-tools
    /// registry's `ToolConfig` consumes (PythonTool, ShellTool,
    /// etc. expose this to user code as `args` / `globals()` /
    /// shell env).  The serde alias means both forms decode into
    /// the same field; `input:` is the form playbooks should
    /// write, `args:` stays accepted for back-compat with
    /// existing fixtures.
    ///
    /// Without the alias, `input:` is silently dropped by serde,
    /// the worker's Python wrapper's `globals().update(args)`
    /// gets an empty dict, and any user code referencing the
    /// workload by name (e.g. `print(f"hello {message}")`) raises
    /// `NameError`.  See noetl/ai-meta#56 for the e2e finding.
    #[serde(default, alias = "input")]
    pub args: Option<serde_json::Value>,

    /// Python code (for python tool).
    #[serde(default)]
    pub code: Option<String>,

    /// URL (for http tool).
    #[serde(default)]
    pub url: Option<String>,

    /// HTTP method (for http tool).
    #[serde(default)]
    pub method: Option<String>,

    /// Query/SQL (for database tools).
    #[serde(default)]
    pub query: Option<String>,

    /// Command (for database/shell tools).
    #[serde(default)]
    pub command: Option<String>,

    /// Connection string or credential reference.
    #[serde(default)]
    pub connection: Option<String>,

    /// HTTP params.
    #[serde(default)]
    pub params: Option<HashMap<String, serde_json::Value>>,

    /// HTTP headers.
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,

    /// Output selection strategy (for result externalization).
    #[serde(default)]
    pub output_select: Option<serde_json::Value>,

    /// Additional tool-specific configuration.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Eval entry - can be a condition or an else clause.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EvalEntry {
    /// Conditional eval with expr.
    Condition(EvalCondition),
    /// Else clause (no condition).
    Else { r#else: EvalElse },
}

// ============================================================================
// Pipeline Task - Labeled tool in a pipeline
// ============================================================================

/// Pipeline task - a labeled tool in a task sequence.
/// Format: { label: { kind: ..., args: ..., eval: ... } }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineTask {
    /// Task label (for referencing with _prev).
    pub label: String,

    /// Tool specification.
    pub tool: ToolSpec,
}

/// Tool definition - can be single tool or pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolDefinition {
    /// Single tool (shorthand).
    Single(ToolSpec),

    /// Pipeline - list of labeled tasks.
    ///
    /// Two YAML shapes are accepted on input, both map to the same
    /// internal representation (label-as-key) before serialization
    /// for the worker's `task_sequence` consumer:
    ///
    /// **Flat / name-as-field** (the dominant v10 form in e2e
    /// fixtures: ~5 fixtures with this shape under
    /// `repos/e2e/fixtures/playbooks/`):
    /// ```yaml
    /// tool:
    /// - name: init_action
    ///   kind: python
    ///   input: { ... }
    ///   code: ...
    /// ```
    ///
    /// **Nested / label-as-key** (the existing Rust internal
    /// shape, also used in some hand-written fixtures):
    /// ```yaml
    /// tool:
    /// - init_action:
    ///     kind: python
    ///     input: { ... }
    ///     code: ...
    /// ```
    ///
    /// See `noetl/ai-meta#57` for the e2e finding that surfaced
    /// the flat form being rejected by the strict Rust decoder.
    Pipeline(Vec<PipelineItem>),
}

/// One item in a pipeline.  Untagged so serde tries each variant
/// in declaration order and picks the first that decodes.
///
/// `Flat` is tried first because the v10 dominant form has a
/// top-level `kind:` field (a required field on `ToolSpec`); the
/// nested label-as-key form has no `kind:` at the top level
/// (the key IS the label), so it cleanly falls through to
/// `Nested`.  The serializer round-trips through the nested form
/// so the wire shape downstream (worker's `task_sequence` consumer)
/// stays unchanged.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PipelineItem {
    /// Flat shape: `{ name: "label", kind: "python", code: "...", ... }`.
    /// The pipeline-item label lives in the `name` field; the rest of
    /// the fields are an inline `ToolSpec`.  Stored via `ToolSpec.extra`
    /// (the `#[serde(flatten)] HashMap` catch-all), so no schema change
    /// is needed on ToolSpec itself.
    Flat(ToolSpec),

    /// Nested shape: `{ "label": { kind: "python", code: "...", ... } }`.
    /// Single-key map — the key is the label, the value is the spec.
    Nested(HashMap<String, ToolSpec>),
}

impl serde::Serialize for PipelineItem {
    /// Normalize on the wire to the nested shape so the worker's
    /// `task_sequence` consumer (which has historically seen only
    /// the nested shape) doesn't have to dual-decode.  The `Flat`
    /// form's pipeline-item label is read from `ToolSpec.extra["name"]`
    /// if present; missing/non-string `name` falls back to the empty
    /// string, which is the same fallback the downstream config has
    /// always used.
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            PipelineItem::Flat(spec) => {
                let label = spec
                    .extra
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut map = HashMap::new();
                map.insert(label, spec.clone());
                map.serialize(serializer)
            }
            PipelineItem::Nested(map) => map.serialize(serializer),
        }
    }
}

// ============================================================================
// Loop Configuration
// ============================================================================

/// Loop execution mode.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoopMode {
    #[default]
    Sequential,
    Parallel,
}

/// Loop runtime specification (canonical format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopSpec {
    /// Execution mode: sequential or parallel.
    #[serde(default)]
    pub mode: LoopMode,

    /// Maximum concurrent iterations in parallel mode.
    #[serde(default)]
    pub max_in_flight: Option<i32>,
}

/// Step-level loop configuration (canonical format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Loop {
    /// Jinja expression for collection to iterate over.
    #[serde(rename = "in")]
    pub in_expr: String,

    /// Variable name for each item.
    pub iterator: String,

    /// Loop spec with mode (canonical format).
    #[serde(default)]
    pub spec: Option<LoopSpec>,
}

// ============================================================================
// Next Transitions (Canonical v10 Format)
// ============================================================================

/// Arc specification for v10 next router format.
/// Canonical format: next.arcs[].when for conditional routing (Petri-net arcs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NextArc {
    /// Target step name.
    pub step: String,

    /// Transition guard expression (Jinja2).
    /// Evaluated by server after step completion.
    #[serde(default)]
    pub when: Option<String>,

    /// Arguments to pass to target step.
    #[serde(default)]
    pub args: Option<HashMap<String, serde_json::Value>>,
}

/// Next router spec for v10 format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NextRouterSpec {
    /// Routing mode: exclusive (first match) or inclusive (all matches).
    #[serde(default)]
    pub mode: Option<String>,
}

/// Canonical v10 next router format with spec and arcs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NextRouter {
    /// Router specification (mode: exclusive/inclusive).
    #[serde(default)]
    pub spec: Option<NextRouterSpec>,

    /// List of arcs (outgoing transitions).
    #[serde(default)]
    pub arcs: Vec<NextArc>,
}

/// Legacy target for transition (deprecated, use NextArc).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalNextTarget {
    /// Target step name.
    pub step: String,

    /// Transition guard expression (Jinja2).
    #[serde(default)]
    pub when: Option<String>,

    /// Arguments to pass to target step.
    #[serde(default)]
    pub args: Option<HashMap<String, serde_json::Value>>,
}

/// Next step specification - supports multiple formats for compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NextSpec {
    /// Canonical v10 router format: { spec: { mode: ... }, arcs: [...] }
    Router(NextRouter),

    /// List of step targets with optional when conditions (legacy canonical format).
    Targets(Vec<CanonicalNextTarget>),

    /// Single step name.
    Single(String),

    /// List of step names.
    List(Vec<String>),
}

// ============================================================================
// Step Specification
// ============================================================================

/// Step-level behavior configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StepSpec {
    /// Next evaluation mode: exclusive (first match) or inclusive (all matches).
    #[serde(default)]
    pub next_mode: Option<String>,

    /// Step timeout (e.g., "30s", "5m").
    #[serde(default)]
    pub timeout: Option<String>,

    /// Error handling: fail, continue, or retry.
    #[serde(default)]
    pub on_error: Option<String>,
}

// ============================================================================
// Step Definition (Canonical Format)
// ============================================================================

/// Workflow step with canonical format control flow.
///
/// Canonical step structure:
/// - step: name (unique identifier)
/// - desc: description
/// - spec: step behavior (next_mode, timeout, on_error)
/// - when: transition enable guard (evaluated by server on input token)
/// - loop: optional loop wrapper with spec.mode
/// - tool: ordered pipeline (list of labeled tasks) OR single tool shorthand
/// - next: outgoing arcs with optional when conditions for routing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// Step name (unique identifier).
    pub step: String,

    /// Step description.
    #[serde(default)]
    pub desc: Option<String>,

    /// Step behavior configuration.
    #[serde(default)]
    pub spec: Option<StepSpec>,

    /// Transition enable guard (canonical format).
    /// Jinja2 expression evaluated by server before step runs.
    #[serde(default)]
    pub when: Option<String>,

    /// Input arguments for this step.
    #[serde(default)]
    pub args: Option<HashMap<String, serde_json::Value>>,

    /// Variables to extract from step result.
    #[serde(default)]
    pub vars: Option<HashMap<String, serde_json::Value>>,

    /// Loop configuration with spec.mode.
    #[serde(default)]
    pub r#loop: Option<Loop>,

    /// Tool configuration - single tool or pipeline.
    pub tool: ToolDefinition,

    /// Next step(s) with optional when conditions for routing.
    #[serde(default)]
    pub next: Option<NextSpec>,
}

// ============================================================================
// Workbook and Keychain
// ============================================================================

/// Reusable task definition in workbook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbookTask {
    /// Task name.
    pub name: String,

    /// Tool configuration.
    pub tool: ToolSpec,

    /// Optional sink configuration.
    #[serde(default)]
    pub sink: Option<serde_json::Value>,
}

/// Keychain entry for credential/token definitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeychainDef {
    /// Keychain entry name.
    pub name: String,

    /// Credential reference.
    #[serde(default)]
    pub credential: Option<String>,

    /// Token type.
    #[serde(default)]
    pub token_type: Option<String>,

    /// Scope type.
    #[serde(default)]
    pub scope: Option<String>,

    /// Auto-renew flag.
    #[serde(default)]
    pub auto_renew: bool,

    /// Additional configuration.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

// ============================================================================
// Playbook Metadata
// ============================================================================

/// Playbook metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    /// Playbook name (required).
    pub name: String,

    /// Resource path.
    #[serde(default)]
    pub path: Option<String>,

    /// Description.
    #[serde(default)]
    pub description: Option<String>,

    /// Labels for filtering.
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,

    /// Additional metadata.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

// ============================================================================
// Playbook Definition
// ============================================================================

/// Complete workflow definition (v2 canonical format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playbook {
    /// API version (noetl.io/v2).
    #[serde(rename = "apiVersion")]
    pub api_version: String,

    /// Resource kind (Playbook).
    pub kind: String,

    /// Metadata (name, path, labels).
    pub metadata: Metadata,

    /// Global workflow variables.
    #[serde(default)]
    pub workload: Option<serde_json::Value>,

    /// Top-level variables (canonical format).
    #[serde(default)]
    pub vars: Option<HashMap<String, serde_json::Value>>,

    /// Keychain definitions for credentials and tokens.
    #[serde(default)]
    pub keychain: Option<Vec<KeychainDef>>,

    /// Reusable tasks.
    #[serde(default)]
    pub workbook: Option<Vec<WorkbookTask>>,

    /// Workflow steps.
    pub workflow: Vec<Step>,
}

impl Playbook {
    /// Check if workflow has a start step.
    pub fn has_start_step(&self) -> bool {
        self.workflow.iter().any(|s| s.step == "start")
    }

    /// Get a step by name.
    pub fn get_step(&self, name: &str) -> Option<&Step> {
        self.workflow.iter().find(|s| s.step == name)
    }

    /// Get all step names.
    pub fn step_names(&self) -> Vec<&str> {
        self.workflow.iter().map(|s| s.step.as_str()).collect()
    }

    /// Get the resource path.
    pub fn path(&self) -> Option<&str> {
        self.metadata.path.as_deref()
    }

    /// Get the playbook name.
    pub fn name(&self) -> &str {
        &self.metadata.name
    }
}

// ============================================================================
// Command Specification (Canonical Format)
// ============================================================================

/// Command-level behavior configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommandSpec {
    /// Next evaluation mode: exclusive (first match) or inclusive (all matches).
    #[serde(default)]
    pub next_mode: Option<String>,
}

/// Next target info for command routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NextTargetInfo {
    /// Target step name.
    pub step: String,

    /// Transition guard expression.
    #[serde(default)]
    pub when: Option<String>,

    /// Arguments to pass.
    #[serde(default)]
    pub args: Option<HashMap<String, serde_json::Value>>,
}

// ============================================================================
// Tool Call and Command Models
// ============================================================================

/// Tool invocation details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool kind.
    pub kind: ToolKind,

    /// Tool-specific configuration.
    #[serde(default)]
    pub config: HashMap<String, serde_json::Value>,
}

impl ToolCall {
    /// Create from a ToolSpec.
    pub fn from_spec(spec: &ToolSpec) -> Self {
        let mut config = spec.extra.clone();

        if let Some(ref auth) = spec.auth {
            config.insert("auth".to_string(), auth.clone());
        }
        if let Some(ref libs) = spec.libs {
            config.insert("libs".to_string(), libs.clone());
        }
        if let Some(ref args) = spec.args {
            config.insert("args".to_string(), args.clone());
        }
        if let Some(ref code) = spec.code {
            config.insert("code".to_string(), serde_json::Value::String(code.clone()));
        }
        if let Some(ref url) = spec.url {
            config.insert("url".to_string(), serde_json::Value::String(url.clone()));
        }
        if let Some(ref method) = spec.method {
            config.insert(
                "method".to_string(),
                serde_json::Value::String(method.clone()),
            );
        }
        if let Some(ref query) = spec.query {
            config.insert(
                "query".to_string(),
                serde_json::Value::String(query.clone()),
            );
        }
        if let Some(ref command) = spec.command {
            config.insert(
                "command".to_string(),
                serde_json::Value::String(command.clone()),
            );
        }
        if let Some(ref connection) = spec.connection {
            config.insert(
                "connection".to_string(),
                serde_json::Value::String(connection.clone()),
            );
        }
        if let Some(ref params) = spec.params {
            config.insert(
                "params".to_string(),
                serde_json::to_value(params).unwrap_or_default(),
            );
        }
        if let Some(ref headers) = spec.headers {
            config.insert(
                "headers".to_string(),
                serde_json::to_value(headers).unwrap_or_default(),
            );
        }
        if let Some(ref eval) = spec.eval {
            config.insert(
                "eval".to_string(),
                serde_json::to_value(eval).unwrap_or_default(),
            );
        }
        if let Some(ref output_select) = spec.output_select {
            config.insert("output_select".to_string(), output_select.clone());
        }

        Self {
            kind: spec.kind.clone(),
            config,
        }
    }
}

/// Command to be executed by worker (canonical format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    /// Execution identifier.
    pub execution_id: String,

    /// Step name.
    pub step: String,

    /// Tool invocation details.
    pub tool: ToolCall,

    /// Step input arguments.
    #[serde(default)]
    pub args: Option<HashMap<String, serde_json::Value>>,

    /// Full render context for Jinja2 templates.
    #[serde(default)]
    pub render_context: HashMap<String, serde_json::Value>,

    /// Pipeline tasks (for task_sequence execution).
    #[serde(default)]
    pub pipeline: Option<Vec<HashMap<String, serde_json::Value>>>,

    /// Next targets with optional when conditions for routing.
    #[serde(default)]
    pub next_targets: Option<Vec<NextTargetInfo>>,

    /// Command behavior specification.
    #[serde(default)]
    pub spec: Option<CommandSpec>,

    /// Attempt number for retries.
    #[serde(default = "default_attempt")]
    pub attempt: i32,

    /// Command priority (higher = more urgent).
    #[serde(default)]
    pub priority: i32,

    /// Retry backoff delay in seconds.
    #[serde(default)]
    pub backoff: Option<f64>,

    /// Maximum retry attempts.
    #[serde(default)]
    pub max_attempts: Option<i32>,

    /// Initial retry delay in seconds.
    #[serde(default)]
    pub retry_delay: Option<f64>,

    /// Retry backoff strategy.
    #[serde(default)]
    pub retry_backoff: Option<String>,

    /// Additional metadata.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

fn default_attempt() -> i32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_playbook() {
        let yaml = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: test_playbook
  path: test/simple
workflow:
  - step: start
    tool:
      kind: python
      code: |
        result = {"status": "ok"}
    next:
      - step: end
  - step: end
    tool:
      kind: python
      code: |
        result = {"status": "done"}
"#;

        let playbook: Playbook = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(playbook.api_version, "noetl.io/v2");
        assert_eq!(playbook.kind, "Playbook");
        assert_eq!(playbook.name(), "test_playbook");
        assert!(playbook.has_start_step());
        assert_eq!(playbook.workflow.len(), 2);
    }

    #[test]
    fn test_parse_playbook_with_loop_spec() {
        let yaml = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: loop_test
workload:
  items: [1, 2, 3]
workflow:
  - step: start
    loop:
      in: "{{ workload.items }}"
      iterator: item
      spec:
        mode: sequential
    tool:
      kind: python
      code: |
        result = {"item": input_data.get("item")}
    args:
      item: "{{ item }}"
"#;

        let playbook: Playbook = serde_yaml::from_str(yaml).unwrap();
        let step = playbook.get_step("start").unwrap();
        assert!(step.r#loop.is_some());
        let loop_config = step.r#loop.as_ref().unwrap();
        assert_eq!(loop_config.iterator, "item");
        assert!(loop_config.spec.is_some());
        assert_eq!(
            loop_config.spec.as_ref().unwrap().mode,
            LoopMode::Sequential
        );
    }

    #[test]
    fn test_parse_playbook_with_next_when() {
        let yaml = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: routing_test
workflow:
  - step: start
    tool:
      kind: python
      code: |
        result = {"value": 10}
    next:
      - step: high
        when: "{{ start.value > 5 }}"
      - step: low
        when: "{{ start.value <= 5 }}"
  - step: high
    tool:
      kind: python
      code: |
        result = {"path": "high"}
  - step: low
    tool:
      kind: python
      code: |
        result = {"path": "low"}
"#;

        let playbook: Playbook = serde_yaml::from_str(yaml).unwrap();
        let step = playbook.get_step("start").unwrap();
        assert!(step.next.is_some());

        if let Some(NextSpec::Targets(targets)) = &step.next {
            assert_eq!(targets.len(), 2);
            assert_eq!(targets[0].step, "high");
            assert_eq!(targets[0].when, Some("{{ start.value > 5 }}".to_string()));
            assert_eq!(targets[1].step, "low");
        } else {
            panic!("Expected NextSpec::Targets");
        }
    }

    #[test]
    fn test_parse_playbook_with_v10_router_format() {
        let yaml = r#"
apiVersion: noetl.io/v10
kind: Playbook
metadata:
  name: v10_routing_test
workflow:
  - step: start
    tool:
      kind: python
      code: |
        result = {"value": 10}
    next:
      spec:
        mode: exclusive
      arcs:
        - step: high
          when: "{{ start.value > 5 }}"
        - step: low
          when: "{{ start.value <= 5 }}"
  - step: high
    tool:
      kind: python
      code: |
        result = {"path": "high"}
  - step: low
    tool:
      kind: python
      code: |
        result = {"path": "low"}
"#;

        let playbook: Playbook = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(playbook.api_version, "noetl.io/v10");
        let step = playbook.get_step("start").unwrap();
        assert!(step.next.is_some());

        if let Some(NextSpec::Router(router)) = &step.next {
            assert!(router.spec.is_some());
            assert_eq!(
                router.spec.as_ref().unwrap().mode,
                Some("exclusive".to_string())
            );
            assert_eq!(router.arcs.len(), 2);
            assert_eq!(router.arcs[0].step, "high");
            assert_eq!(
                router.arcs[0].when,
                Some("{{ start.value > 5 }}".to_string())
            );
            assert_eq!(router.arcs[1].step, "low");
        } else {
            panic!("Expected NextSpec::Router, got {:?}", step.next);
        }
    }

    #[test]
    fn test_parse_playbook_with_step_when() {
        let yaml = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: guard_test
workflow:
  - step: conditional
    when: "{{ workload.enabled }}"
    tool:
      kind: python
      code: |
        result = {"status": "ran"}
"#;

        let playbook: Playbook = serde_yaml::from_str(yaml).unwrap();
        let step = playbook.get_step("conditional").unwrap();
        assert_eq!(step.when, Some("{{ workload.enabled }}".to_string()));
    }

    #[test]
    fn test_parse_playbook_with_pipeline() {
        let yaml = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: pipeline_test
workflow:
  - step: fetch_transform
    tool:
      - fetch:
          kind: http
          url: "https://api.example.com/data"
          method: GET
      - transform:
          kind: python
          args:
            data: "{{ _prev }}"
          code: |
            result = {"processed": True}
    next:
      - step: end
  - step: end
    tool:
      kind: noop
"#;

        let playbook: Playbook = serde_yaml::from_str(yaml).unwrap();
        let step = playbook.get_step("fetch_transform").unwrap();

        if let ToolDefinition::Pipeline(tasks) = &step.tool {
            assert_eq!(tasks.len(), 2);
            // Nested form (label-as-key) — the existing back-compat shape.
            match &tasks[0] {
                PipelineItem::Nested(map) => assert!(map.contains_key("fetch")),
                PipelineItem::Flat(_) => panic!("Expected Nested form for label-as-key YAML"),
            }
            match &tasks[1] {
                PipelineItem::Nested(map) => assert!(map.contains_key("transform")),
                PipelineItem::Flat(_) => panic!("Expected Nested form for label-as-key YAML"),
            }
        } else {
            panic!("Expected ToolDefinition::Pipeline");
        }
    }

    #[test]
    fn test_parse_pipeline_with_name_as_field_shape() {
        // noetl/ai-meta#57 — the v10 canonical fixtures write
        // pipeline items in flat form with `name:` as a field
        // (not as the outer label-as-key map).  Both shapes must
        // decode into a Pipeline of the right cardinality with
        // labels addressable downstream.
        let yaml = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: pipeline_flat
workflow:
  - step: fetch_transform
    tool:
      - name: fetch
        kind: http
        url: "https://api.example.com"
      - name: transform
        kind: python
        code: "result = {'ok': True}"
"#;
        let playbook: Playbook = serde_yaml::from_str(yaml).unwrap();
        let step = playbook.get_step("fetch_transform").unwrap();
        if let ToolDefinition::Pipeline(tasks) = &step.tool {
            assert_eq!(tasks.len(), 2);
            // Flat form — pipeline-item label lives in spec.extra["name"].
            for (i, expected_label) in [(0usize, "fetch"), (1usize, "transform")] {
                match &tasks[i] {
                    PipelineItem::Flat(spec) => {
                        let name = spec
                            .extra
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        assert_eq!(name, expected_label);
                    }
                    PipelineItem::Nested(_) => {
                        panic!("Expected Flat form for name-as-field YAML")
                    }
                }
            }
        } else {
            panic!("Expected ToolDefinition::Pipeline");
        }
    }

    #[test]
    fn test_parse_tool_with_eval() {
        let yaml = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: eval_test
workflow:
  - step: fetch
    tool:
      kind: http
      url: "https://api.example.com/data"
      eval:
        - expr: "{{ outcome.error.retryable }}"
          do: retry
          attempts: 3
          backoff: exponential
          delay: 1.0
        - expr: "{{ outcome.status == 'error' }}"
          do: fail
        - else:
            do: continue
    next:
      - step: end
  - step: end
    tool:
      kind: noop
"#;

        let playbook: Playbook = serde_yaml::from_str(yaml).unwrap();
        let step = playbook.get_step("fetch").unwrap();

        if let ToolDefinition::Single(spec) = &step.tool {
            assert!(spec.eval.is_some());
            let eval = spec.eval.as_ref().unwrap();
            assert_eq!(eval.len(), 3);
        } else {
            panic!("Expected ToolDefinition::Single");
        }
    }

    #[test]
    fn test_tool_call_from_spec() {
        let spec = ToolSpec {
            kind: ToolKind::Python,
            eval: None,
            auth: None,
            libs: None,
            args: None,
            code: Some("return {}".to_string()),
            url: None,
            method: None,
            query: None,
            command: None,
            connection: None,
            params: None,
            headers: None,
            output_select: None,
            extra: HashMap::new(),
        };

        let call = ToolCall::from_spec(&spec);
        assert_eq!(call.kind, ToolKind::Python);
        assert!(call.config.contains_key("code"));
    }

    #[test]
    fn test_tool_spec_accepts_input_alias_for_args() {
        // Canonical NoETL v10 playbook YAML writes `input:` on the
        // tool block.  Without the serde alias, this is silently
        // dropped and the worker's Python wrapper's
        // `globals().update(args)` gets an empty dict.  See
        // noetl/ai-meta#56 — surfaced via hello_world e2e on the
        // Rust-only stack: `NameError: name 'message' is not
        // defined` because `input: { message: "{{ message }}" }`
        // never reached the wrapper.
        let yaml = r#"
kind: python
input:
  message: "Hello World"
  count: 42
code: |
  print(f"hello {message}")
"#;
        let spec: ToolSpec = serde_yaml::from_str(yaml).unwrap();
        let args = spec.args.clone().expect("input alias should decode into args");
        assert_eq!(args.get("message").and_then(|v| v.as_str()), Some("Hello World"));
        assert_eq!(args.get("count").and_then(|v| v.as_i64()), Some(42));

        let call = ToolCall::from_spec(&spec);
        let call_args = call
            .config
            .get("args")
            .expect("ToolCall::from_spec should propagate args");
        assert_eq!(call_args.get("message").and_then(|v| v.as_str()), Some("Hello World"));
    }

    #[test]
    fn test_tool_spec_accepts_args_field_directly() {
        // Back-compat: existing fixtures that use `args:` keep
        // working alongside the new `input:` alias.
        let yaml = r#"
kind: python
args:
  x: 10
code: "print(x * 2)"
"#;
        let spec: ToolSpec = serde_yaml::from_str(yaml).unwrap();
        let args = spec.args.expect("args field decodes");
        assert_eq!(args.get("x").and_then(|v| v.as_i64()), Some(10));
    }

    #[test]
    fn test_step_names() {
        let yaml = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: test
workflow:
  - step: start
    tool:
      kind: python
      code: ""
  - step: process
    tool:
      kind: python
      code: ""
  - step: end
    tool:
      kind: python
      code: ""
"#;

        let playbook: Playbook = serde_yaml::from_str(yaml).unwrap();
        let names = playbook.step_names();
        assert_eq!(names, vec!["start", "process", "end"]);
    }
}
