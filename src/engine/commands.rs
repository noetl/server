//! Command generation for workers.
//!
//! Generates command payloads that workers pick up and execute.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::playbook::types::{CursorClaim, Step, ToolCall, ToolDefinition, ToolSpec};
use crate::template::TemplateRenderer;

/// Command to be executed by a worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    /// Unique command ID (event_id).
    pub command_id: i64,
    /// Execution ID this command belongs to.
    pub execution_id: i64,
    /// Catalog ID for the playbook.
    pub catalog_id: i64,
    /// Parent event ID that triggered this command.
    pub parent_event_id: i64,
    /// Step name.
    pub step_name: String,
    /// Tool specification.
    pub tool: ToolCommand,
    /// Rendered context for the command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<HashMap<String, serde_json::Value>>,
    /// Additional metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Iterator metadata if this is part of a loop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iterator: Option<IteratorMetadata>,
}

/// Tool command specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCommand {
    /// Tool kind.
    pub kind: String,
    /// Tool configuration/arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// Timeout in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<i64>,
}

/// Iterator metadata for loop iterations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IteratorMetadata {
    /// Parent execution ID for the loop.
    pub parent_execution_id: i64,
    /// Iterator step name.
    pub iterator_step: String,
    /// Current iteration index.
    pub index: usize,
    /// Total number of iterations.
    pub total: usize,
    /// Current iteration item.
    pub item: serde_json::Value,
    /// Variable name for the item.
    pub item_var: String,
}

/// Builder for creating commands.
pub struct CommandBuilder {
    renderer: TemplateRenderer,
}

impl Default for CommandBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandBuilder {
    /// Create a new command builder.
    pub fn new() -> Self {
        Self {
            renderer: TemplateRenderer::new(),
        }
    }

    /// Build a command from a step definition.
    #[allow(clippy::too_many_arguments)]
    pub fn build_command(
        &self,
        command_id: i64,
        execution_id: i64,
        catalog_id: i64,
        parent_event_id: i64,
        step: &Step,
        context: &HashMap<String, serde_json::Value>,
        metadata: Option<&serde_json::Value>,
    ) -> AppResult<Command> {
        // Build a render context that includes `ctx` and `workload` namespace
        // aliases pointing at the flat dispatch context, mirroring Python's
        // `context["ctx"] = state.variables` + `context["workload"] = state.variables`
        // (commands.py:915-916).  Use entry().or_insert_with() so we don't
        // clobber existing bindings — in particular, execute.rs:453 already
        // inserts `workload` as the structured YAML workload block, which must
        // take precedence over the flat-dict alias for
        // `{{ workload.session_token }}` style templates.
        let mut render_ctx = context.clone();
        let ctx_value = serde_json::to_value(context).unwrap_or(serde_json::Value::Null);
        render_ctx
            .entry("ctx".to_string())
            .or_insert_with(|| ctx_value.clone());
        render_ctx
            .entry("workload".to_string())
            .or_insert_with(|| ctx_value);

        // Build tool command using the shimmed render context so that
        // {{ ctx.foo }} and {{ workload.foo }} templates resolve correctly.
        let tool_command = self.build_tool_from_definition(&step.tool, &render_ctx)?;

        // Persist the shimmed render context (with `ctx` and `workload`
        // namespace aliases) on the Command so the worker can resolve
        // `{{ ctx.X }}` and `{{ workload.X }}` templates in pipeline
        // `input:` blocks that render_pipeline_config preserved unrendered
        // for worker-side resolution.
        Ok(Command {
            command_id,
            execution_id,
            catalog_id,
            parent_event_id,
            step_name: step.step.clone(),
            tool: tool_command,
            context: Some(render_ctx),
            metadata: metadata.cloned(),
            iterator: None,
        })
    }

    /// Build a command for a loop iteration.
    #[allow(clippy::too_many_arguments)]
    pub fn build_iteration_command(
        &self,
        command_id: i64,
        execution_id: i64,
        catalog_id: i64,
        parent_event_id: i64,
        step: &Step,
        context: &HashMap<String, serde_json::Value>,
        iterator: IteratorMetadata,
    ) -> AppResult<Command> {
        // Build context with iterator variables.
        // Insert both at the top level (so `{{ num }}` works) AND
        // under an `iter` namespace map (so `{{ iter.num }}` works).
        // Playbooks use both conventions; the `iter.` prefix is the
        // documented DSL shape for v10 loop steps.
        let mut iter_context = context.clone();
        iter_context.insert(iterator.item_var.clone(), iterator.item.clone());
        iter_context.insert("_index".to_string(), serde_json::json!(iterator.index));
        iter_context.insert("_total".to_string(), serde_json::json!(iterator.total));

        // Build the `iter` namespace map so `{{ iter.<var> }}`,
        // `{{ iter._index }}`, `{{ iter._total }}` all resolve.
        let mut iter_ns = serde_json::Map::new();
        iter_ns.insert(iterator.item_var.clone(), iterator.item.clone());
        iter_ns.insert("_index".to_string(), serde_json::json!(iterator.index));
        iter_ns.insert("_total".to_string(), serde_json::json!(iterator.total));
        iter_context.insert("iter".to_string(), serde_json::Value::Object(iter_ns));

        // Add `ctx` + `workload` namespace shims AFTER the iterator-var
        // insertions so the iterator value is also visible through
        // {{ ctx.<item_var> }}.  Same idempotency guard as build_command —
        // don't clobber any pre-existing binding.
        let mut render_ctx = iter_context.clone();
        let ctx_value = serde_json::to_value(&iter_context).unwrap_or(serde_json::Value::Null);
        render_ctx
            .entry("ctx".to_string())
            .or_insert_with(|| ctx_value.clone());
        render_ctx
            .entry("workload".to_string())
            .or_insert_with(|| ctx_value);

        // Build tool command from definition with iterator context (plus shims)
        let mut tool_command = self.build_tool_from_definition(&step.tool, &render_ctx)?;

        // Phase D R3b-2: also inject the iteration variables into
        // the tool's `args` map.  The worker's Python tool exposes
        // `args` keys as Python globals via `globals().update(args)`
        // — without this, a `step.loop` over `items: [1,2,3]` with
        // a Python tool referencing `item` / `_index` / `_total`
        // raises `NameError: name 'item' is not defined` on the
        // worker side.  Other tool kinds (shell, http, duckdb)
        // also benefit from getting the iter vars in `args` for the
        // same Jinja-rendering reason — keys with templates like
        // `{{ item }}` rendered earlier resolve correctly, but raw
        // references in tool-specific runtimes (Python globals,
        // shell `$item`, etc.) need the literal binding.  Safe for
        // tools without an `args` convention — the field just goes
        // unused.
        if let Some(serde_json::Value::Object(cfg)) = tool_command.config.as_mut() {
            let args_entry = cfg
                .entry("args".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let serde_json::Value::Object(args) = args_entry {
                args.insert(iterator.item_var.clone(), iterator.item.clone());
                args.insert("_index".to_string(), serde_json::json!(iterator.index));
                args.insert("_total".to_string(), serde_json::json!(iterator.total));
            }
        }

        // Persist the shimmed render context so the worker can resolve
        // `{{ ctx.X }}` / `{{ workload.X }}` templates in pipeline
        // `input:` blocks (same rationale as build_command).
        Ok(Command {
            command_id,
            execution_id,
            catalog_id,
            parent_event_id,
            step_name: step.step.clone(),
            tool: tool_command,
            context: Some(render_ctx),
            metadata: None,
            iterator: Some(iterator),
        })
    }

    /// Build the claim command for one frame of a `mode: cursor` loop
    /// (noetl/ai-meta#100).  Runs the `loop.cursor.claim` SQL as a normal
    /// tool command (default `postgres`); its `RETURNING` rows are the frame
    /// the orchestrator fans the step body over.  `__frame_max_rows` is
    /// injected so the claim's `LIMIT {{ __frame_max_rows }}` resolves.  The
    /// command carries `metadata.cursor = {phase: "claim", frame}` so the
    /// orchestrator can recognise the completion and continue the loop.
    #[allow(clippy::too_many_arguments)]
    pub fn build_cursor_claim_command(
        &self,
        execution_id: i64,
        catalog_id: i64,
        step: &Step,
        cursor: &CursorClaim,
        context: &HashMap<String, serde_json::Value>,
        frame_index: i64,
        max_rows: i64,
    ) -> AppResult<Command> {
        // Inject __frame_max_rows + ctx/workload shims into the render context
        // so the claim SQL's templates resolve (execution_id, ctx.*, workload.*,
        // __frame_max_rows).
        let mut render_ctx = context.clone();
        render_ctx.insert(
            "__frame_max_rows".to_string(),
            serde_json::json!(max_rows),
        );
        let ctx_value = serde_json::to_value(&render_ctx).unwrap_or(serde_json::Value::Null);
        render_ctx
            .entry("ctx".to_string())
            .or_insert_with(|| ctx_value.clone());
        render_ctx
            .entry("workload".to_string())
            .or_insert_with(|| ctx_value);

        let claim_sql = self.renderer.render(&cursor.claim, &render_ctx)?;

        let mut config = serde_json::Map::new();
        config.insert("command".to_string(), serde_json::Value::String(claim_sql));
        if let Some(auth) = &cursor.auth {
            config.insert("auth".to_string(), serde_json::Value::String(auth.clone()));
        }
        let tool_command = ToolCommand {
            kind: cursor.kind.clone(),
            config: Some(serde_json::Value::Object(config)),
            timeout: None,
        };

        let metadata = serde_json::json!({
            "cursor": { "phase": "claim", "step": step.step, "frame": frame_index }
        });

        Ok(Command {
            command_id: 0,
            execution_id,
            catalog_id,
            parent_event_id: 0,
            step_name: step.step.clone(),
            tool: tool_command,
            context: Some(render_ctx),
            metadata: Some(metadata),
            iterator: None,
        })
    }

    /// Build a tool command from a tool definition (single or pipeline).
    fn build_tool_from_definition(
        &self,
        tool: &ToolDefinition,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<ToolCommand> {
        match tool {
            ToolDefinition::Single(spec) => self.build_tool_command(spec, context),
            ToolDefinition::Pipeline(tasks) => {
                // For pipelines, create a task_sequence tool command.
                // Render server-resolvable templates (credential
                // aliases, execution_id, step variables, etc.) now,
                // but preserve `set:` and `input:` blocks verbatim —
                // they contain runtime-only expressions (`{{ output }}`,
                // context vars from prior items' `set:`) that the
                // worker's task_sequence tool evaluates per-item.
                let pipeline_config = serde_json::to_value(tasks).ok();
                let config = if let Some(cfg) = pipeline_config {
                    Some(render_pipeline_config(&self.renderer, &cfg, context)?)
                } else {
                    None
                };

                Ok(ToolCommand {
                    kind: "task_sequence".to_string(),
                    config,
                    timeout: None,
                })
            }
        }
    }

    /// Build a tool command from a single tool spec.
    fn build_tool_command(
        &self,
        tool: &ToolSpec,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<ToolCommand> {
        // Get kind as string
        let kind = tool.kind.to_string();

        // Build config from tool spec using ToolCall
        let tool_call = ToolCall::from_spec(tool);
        let config_value = serde_json::to_value(&tool_call.config).ok();

        // Render any templates in the config
        let config = if let Some(cfg) = config_value {
            Some(self.renderer.render_value(&cfg, context)?)
        } else {
            None
        };

        Ok(ToolCommand {
            kind,
            config,
            timeout: None,
        })
    }

    /// Build a command for a playbook call (nested execution).
    #[allow(clippy::too_many_arguments)]
    pub fn build_playbook_call(
        &self,
        command_id: i64,
        execution_id: i64,
        catalog_id: i64,
        parent_event_id: i64,
        step_name: &str,
        playbook_path: &str,
        playbook_version: Option<&str>,
        args: Option<&serde_json::Value>,
        context: &HashMap<String, serde_json::Value>,
    ) -> Command {
        let config = serde_json::json!({
            "path": playbook_path,
            "version": playbook_version.unwrap_or("latest"),
            "args": args.cloned().unwrap_or(serde_json::Value::Null),
        });

        Command {
            command_id,
            execution_id,
            catalog_id,
            parent_event_id,
            step_name: step_name.to_string(),
            tool: ToolCommand {
                kind: "playbook".to_string(),
                config: Some(config),
                timeout: None,
            },
            context: Some(context.clone()),
            metadata: None,
            iterator: None,
        }
    }

    /// Build a noop command (for steps without tools).
    pub fn build_noop_command(
        &self,
        command_id: i64,
        execution_id: i64,
        catalog_id: i64,
        parent_event_id: i64,
        step_name: &str,
        context: &HashMap<String, serde_json::Value>,
    ) -> Command {
        Command {
            command_id,
            execution_id,
            catalog_id,
            parent_event_id,
            step_name: step_name.to_string(),
            tool: ToolCommand {
                kind: "noop".to_string(),
                config: None,
                timeout: None,
            },
            context: Some(context.clone()),
            metadata: None,
            iterator: None,
        }
    }
}

/// Render a pipeline (task_sequence) config, preserving `set:` and
/// `input:`/`args:` blocks that the worker's task_sequence tool
/// evaluates at runtime.
///
/// The pipeline config is an array of single-key objects:
/// `[{"label": {"kind": ..., "set": {...}, "args": {...}, ...}}, ...]`
///
/// For each item the function:
/// 1. Extracts `set` and `args` from the inner spec (these contain
///    runtime-only templates like `{{ output }}` and inter-item
///    context vars).
/// 2. Renders everything else (credential aliases, execution_id,
///    step variables — all resolvable at command-build time).
/// 3. Restores `set` verbatim and `args` as `input` (the key name
///    the worker's task_sequence reads; serde serialized `input:`
///    from YAML as `args` because that's ToolSpec's field name).
fn render_pipeline_config(
    renderer: &TemplateRenderer,
    config: &serde_json::Value,
    context: &HashMap<String, serde_json::Value>,
) -> AppResult<serde_json::Value> {
    let arr = match config.as_array() {
        Some(a) => a,
        None => return renderer.render_value(config, context),
    };

    let mut result = Vec::with_capacity(arr.len());
    for item in arr {
        let obj = match item.as_object() {
            Some(o) => o,
            None => {
                result.push(renderer.render_value(item, context)?);
                continue;
            }
        };

        let mut rendered_item = serde_json::Map::new();
        for (label, spec) in obj {
            let spec_obj = match spec.as_object() {
                Some(o) => o,
                None => {
                    rendered_item
                        .insert(label.clone(), renderer.render_value(spec, context)?);
                    continue;
                }
            };

            // Stash runtime-only keys before rendering.
            // These contain template expressions that reference
            // `output` (tool result data) or pipeline-internal
            // variables set by previous steps' `set:` blocks,
            // which don't exist at server-side render time —
            // they're only available worker-side after tool
            // execution.
            let set_block = spec_obj.get("set").cloned();
            let args_block = spec_obj.get("args").cloned();
            let spec_block = spec_obj.get("spec").cloned();
            let command_block = spec_obj.get("command").cloned();

            // Build a spec without runtime-only keys.  `spec`
            // carries `policy.rules[].then.set` whose templates
            // (e.g. `{{ output.data.counter }}`) must be
            // evaluated worker-side, not server-side.
            // `command` is also deferred because pipeline steps
            // can reference variables set by previous steps'
            // `set:` blocks (e.g. `{{ iter.processed_item.item_name }}`
            // in a postgres command after a python step that sets it).
            let filtered: serde_json::Map<String, serde_json::Value> = spec_obj
                .iter()
                .filter(|(k, _)| {
                    k.as_str() != "set"
                        && k.as_str() != "args"
                        && k.as_str() != "spec"
                        && k.as_str() != "command"
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let rendered =
                renderer.render_value(&serde_json::Value::Object(filtered), context)?;
            let mut rendered_spec = match rendered {
                serde_json::Value::Object(m) => m,
                other => {
                    rendered_item.insert(label.clone(), other);
                    continue;
                }
            };

            // Restore `set:` verbatim — evaluated post-execution
            // by task_sequence with `output` in scope.
            if let Some(set) = set_block {
                rendered_spec.insert("set".to_string(), set);
            }

            // Restore `args` as `input` — the key the worker's
            // task_sequence reads.  ToolSpec serializes the
            // `#[serde(alias = "input")]` field as `args`; we
            // rename it back so forward-only resolution works.
            if let Some(args) = args_block {
                rendered_spec.insert("input".to_string(), args);
            }

            // Restore `spec` verbatim — policy rules inside it
            // contain `{{ output.data.* }}` templates resolved
            // by the worker's task_sequence after execution.
            if let Some(spec) = spec_block {
                rendered_spec.insert("spec".to_string(), spec);
            }

            // Restore `command` verbatim — pipeline steps may
            // reference variables set by previous steps' `set:`
            // blocks (e.g. `{{ iter.processed_item.item_name }}`).
            // The worker's task_sequence renders these after each
            // step completes and the `set:` variables are applied.
            if let Some(cmd) = command_block {
                rendered_spec.insert("command".to_string(), cmd);
            }

            rendered_item
                .insert(label.clone(), serde_json::Value::Object(rendered_spec));
        }
        result.push(serde_json::Value::Object(rendered_item));
    }
    Ok(serde_json::Value::Array(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playbook::types::ToolKind;

    #[test]
    fn test_command_serialization() {
        let mut context = HashMap::new();
        context.insert("key".to_string(), serde_json::json!("value"));

        let command = Command {
            command_id: 12345,
            execution_id: 67890,
            catalog_id: 11111,
            parent_event_id: 22222,
            step_name: "test_step".to_string(),
            tool: ToolCommand {
                kind: "http".to_string(),
                config: Some(serde_json::json!({
                    "url": "https://example.com",
                    "method": "GET"
                })),
                timeout: Some(30),
            },
            context: Some(context),
            metadata: None,
            iterator: None,
        };

        let json = serde_json::to_string(&command).unwrap();
        assert!(json.contains("test_step"));
        assert!(json.contains("http"));
        assert!(json.contains("example.com"));
    }

    #[test]
    fn test_build_command() {
        let builder = CommandBuilder::new();
        let step = Step {
            step: "test_step".to_string(),
            desc: None,
            spec: None,
            when: None,
            args: None,
            vars: None,
            set_vars: None,
            r#loop: None,
            tool: ToolDefinition::Single(Box::new(ToolSpec {
                kind: ToolKind::Http,
                auth: None,
                libs: None,
                args: None,
                code: None,
                url: Some("https://{{ host }}/api".to_string()),
                method: Some("GET".to_string()),
                query: None,
                command: None,
                connection: None,
                params: None,
                headers: None,
                eval: None,
                output_select: None,
                extra: HashMap::new(),
            })),
            next: None,
        };

        let mut context = HashMap::new();
        context.insert("host".to_string(), serde_json::json!("example.com"));

        let command = builder
            .build_command(1, 2, 3, 4, &step, &context, None)
            .unwrap();

        assert_eq!(command.step_name, "test_step");
        assert_eq!(command.tool.kind, "http");

        // Check that template was rendered
        let config = command.tool.config.unwrap();
        assert_eq!(
            config.get("url").and_then(|v| v.as_str()),
            Some("https://example.com/api")
        );
    }

    #[test]
    fn test_build_iteration_command() {
        let builder = CommandBuilder::new();
        let step = Step {
            step: "process_item".to_string(),
            desc: None,
            spec: None,
            when: None,
            args: None,
            vars: None,
            set_vars: None,
            r#loop: None,
            tool: ToolDefinition::Single(Box::new(ToolSpec {
                kind: ToolKind::Python,
                auth: None,
                libs: None,
                args: None,
                code: Some("return {'item': '{{ item }}'}".to_string()),
                url: None,
                method: None,
                query: None,
                command: None,
                connection: None,
                params: None,
                headers: None,
                eval: None,
                output_select: None,
                extra: HashMap::new(),
            })),
            next: None,
        };

        let context = HashMap::new();
        let iterator = IteratorMetadata {
            parent_execution_id: 100,
            iterator_step: "loop_step".to_string(),
            index: 2,
            total: 5,
            item: serde_json::json!("test_value"),
            item_var: "item".to_string(),
        };

        let command = builder
            .build_iteration_command(1, 2, 3, 4, &step, &context, iterator)
            .unwrap();

        assert!(command.iterator.is_some());
        let iter = command.iterator.as_ref().unwrap();
        assert_eq!(iter.index, 2);
        assert_eq!(iter.total, 5);

        // Phase D R3b-2: tool.config.args must carry the iteration
        // variables so the worker's Python tool sees them as globals.
        let tool_cfg = command.tool.config.as_ref().expect("tool config present");
        let args = tool_cfg.get("args").expect("args injected");
        assert_eq!(args.get("item"), Some(&serde_json::json!("test_value")));
        assert_eq!(args.get("_index"), Some(&serde_json::json!(2)));
        assert_eq!(args.get("_total"), Some(&serde_json::json!(5)));
    }

    #[test]
    fn test_build_noop_command() {
        let builder = CommandBuilder::new();
        let mut context = HashMap::new();
        context.insert("key".to_string(), serde_json::json!("value"));

        let command = builder.build_noop_command(1, 2, 3, 4, "noop_step", &context);

        assert_eq!(command.tool.kind, "noop");
        assert!(command.tool.config.is_none());
    }

    #[test]
    fn test_build_pipeline_command() {
        let builder = CommandBuilder::new();

        // Create a pipeline with multiple tasks
        let mut fetch_task = HashMap::new();
        fetch_task.insert(
            "fetch".to_string(),
            ToolSpec {
                kind: ToolKind::Http,
                auth: None,
                libs: None,
                args: None,
                code: None,
                url: Some("https://api.example.com".to_string()),
                method: Some("GET".to_string()),
                query: None,
                command: None,
                connection: None,
                params: None,
                headers: None,
                eval: None,
                output_select: None,
                extra: HashMap::new(),
            },
        );

        let mut transform_task = HashMap::new();
        transform_task.insert(
            "transform".to_string(),
            ToolSpec {
                kind: ToolKind::Python,
                auth: None,
                libs: None,
                args: None,
                code: Some("result = {'processed': True}".to_string()),
                url: None,
                method: None,
                query: None,
                command: None,
                connection: None,
                params: None,
                headers: None,
                eval: None,
                output_select: None,
                extra: HashMap::new(),
            },
        );

        let step = Step {
            step: "pipeline_step".to_string(),
            desc: None,
            spec: None,
            when: None,
            args: None,
            vars: None,
            set_vars: None,
            r#loop: None,
            tool: ToolDefinition::Pipeline(vec![
                crate::playbook::types::PipelineItem::Nested(fetch_task),
                crate::playbook::types::PipelineItem::Nested(transform_task),
            ]),
            next: None,
        };

        let context = HashMap::new();

        let command = builder
            .build_command(1, 2, 3, 4, &step, &context, None)
            .unwrap();

        assert_eq!(command.step_name, "pipeline_step");
        assert_eq!(command.tool.kind, "task_sequence");
        assert!(command.tool.config.is_some());
    }

    // ---- ctx / workload namespace shim tests (noetl/ai-meta#74) ----

    fn make_http_step_with_url(url: &str) -> Step {
        Step {
            step: "test_step".to_string(),
            desc: None,
            spec: None,
            when: None,
            args: None,
            vars: None,
            set_vars: None,
            r#loop: None,
            tool: ToolDefinition::Single(Box::new(ToolSpec {
                kind: ToolKind::Http,
                auth: None,
                libs: None,
                args: None,
                code: None,
                url: Some(url.to_string()),
                method: Some("GET".to_string()),
                query: None,
                command: None,
                connection: None,
                params: None,
                headers: None,
                eval: None,
                output_select: None,
                extra: HashMap::new(),
            })),
            next: None,
        }
    }

    /// `{{ ctx.foo }}` resolves to the flat dispatch context value for `foo`.
    #[test]
    fn test_build_command_exposes_ctx_namespace() {
        let builder = CommandBuilder::new();
        let step = make_http_step_with_url("https://example.com/{{ ctx.foo }}");

        let mut context = HashMap::new();
        context.insert("foo".to_string(), serde_json::json!(42));

        let command = builder
            .build_command(1, 2, 3, 4, &step, &context, None)
            .unwrap();

        let config = command.tool.config.unwrap();
        assert_eq!(
            config.get("url").and_then(|v| v.as_str()),
            Some("https://example.com/42"),
            "{{ ctx.foo }} should resolve to 42 via the ctx namespace shim"
        );
        // The persisted context MUST contain the shim keys so the worker
        // can resolve `{{ ctx.X }}` templates in pipeline `input:` blocks
        // that render_pipeline_config preserved unrendered.
        let persisted = command.context.unwrap();
        assert!(
            persisted.contains_key("ctx"),
            "persisted context must carry ctx shim for worker-side pipeline input rendering"
        );
    }

    /// `{{ workload.foo }}` resolves to the flat dispatch context value for `foo`
    /// when no structured workload block was pre-populated.
    #[test]
    fn test_build_command_exposes_workload_namespace() {
        let builder = CommandBuilder::new();
        let step = make_http_step_with_url("https://example.com/{{ workload.foo }}");

        let mut context = HashMap::new();
        context.insert("foo".to_string(), serde_json::json!(42));

        let command = builder
            .build_command(1, 2, 3, 4, &step, &context, None)
            .unwrap();

        let config = command.tool.config.unwrap();
        assert_eq!(
            config.get("url").and_then(|v| v.as_str()),
            Some("https://example.com/42"),
            "{{ workload.foo }} should resolve to 42 via the workload namespace shim"
        );
    }

    /// When the incoming context already has `workload` (the structured YAML
    /// workload block, inserted by execute.rs:453), the shim must NOT clobber it.
    #[test]
    fn test_build_command_preserves_existing_workload() {
        let builder = CommandBuilder::new();
        let step = make_http_step_with_url("https://example.com/{{ workload.session_token }}");

        let mut context = HashMap::new();
        // Simulate what execute.rs:453 does: insert the structured workload object.
        context.insert(
            "workload".to_string(),
            serde_json::json!({ "session_token": "abc123" }),
        );
        context.insert("foo".to_string(), serde_json::json!(99));

        let command = builder
            .build_command(1, 2, 3, 4, &step, &context, None)
            .unwrap();

        let config = command.tool.config.unwrap();
        assert_eq!(
            config.get("url").and_then(|v| v.as_str()),
            Some("https://example.com/abc123"),
            "existing workload.session_token must survive — shim must not clobber"
        );
    }

    /// Flat top-level keys are still accessible alongside `{{ ctx.foo }}`.
    #[test]
    fn test_build_command_preserves_flat_top_level_keys() {
        let builder = CommandBuilder::new();
        // Template uses both a flat key and a ctx-namespaced key.
        let step = make_http_step_with_url("https://{{ host }}/{{ ctx.path }}");

        let mut context = HashMap::new();
        context.insert("host".to_string(), serde_json::json!("example.com"));
        context.insert("path".to_string(), serde_json::json!("api/v1"));

        let command = builder
            .build_command(1, 2, 3, 4, &step, &context, None)
            .unwrap();

        let config = command.tool.config.unwrap();
        assert_eq!(
            config.get("url").and_then(|v| v.as_str()),
            Some("https://example.com/api/v1"),
            "flat top-level key {{ host }} and ctx-namespaced {{ ctx.path }} must both resolve"
        );
    }

    /// In a loop iteration, the iterator variable is visible through
    /// `{{ ctx.<item_var> }}` because the shim is applied AFTER the
    /// iterator insertions.
    #[test]
    fn test_build_iteration_command_ctx_includes_iterator_var() {
        let builder = CommandBuilder::new();
        let step = make_http_step_with_url("https://example.com/{{ ctx.num }}");

        let context = HashMap::new();
        let iterator = IteratorMetadata {
            parent_execution_id: 100,
            iterator_step: "loop_step".to_string(),
            index: 0,
            total: 3,
            item: serde_json::json!(42),
            item_var: "num".to_string(),
        };

        let command = builder
            .build_iteration_command(1, 2, 3, 4, &step, &context, iterator)
            .unwrap();

        let config = command.tool.config.unwrap();
        assert_eq!(
            config.get("url").and_then(|v| v.as_str()),
            Some("https://example.com/42"),
            "{{ ctx.num }} must resolve to 42 via the ctx shim in an iteration command"
        );
        // The persisted iter_context MUST contain the shim keys so the
        // worker can resolve `{{ ctx.X }}` templates in pipeline `input:`.
        let persisted = command.context.unwrap();
        assert!(
            persisted.contains_key("ctx"),
            "persisted iter_context must carry ctx shim for worker-side pipeline input rendering"
        );
    }
}
