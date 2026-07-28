//! Tool composition for agentkit.
//!
//! This crate provides [`ComposeTool`], a tool that runs a script in a
//! pluggable execution backend and lets that script call the current AgentKit
//! tool catalog. The default backend is sandboxed Lua ([`LuaBackend`]); the
//! optional `runlet` feature adds [`RunletBackend`], which executes
//! [Runlet](https://github.com/danielkov/runlet) programs instead. The
//! optional `toon` feature adds [`ResultEncoding::Toon`] for compact compose
//! results.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use agentkit_core::{
    MetadataMap, SessionId, ToolCallId, ToolOutput, ToolResultPart, TurnCancellation, TurnId,
};
use agentkit_tools_core::{
    ApprovalRequest, PermissionCode, PermissionDenial, Tool, ToolAnnotations, ToolCatalogEvent,
    ToolContext, ToolError, ToolExecutionOutcome, ToolExecutionScope, ToolInterruption, ToolName,
    ToolRegistry, ToolRequest, ToolResult, ToolSource, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

mod lua;
#[cfg(feature = "runlet")]
mod runlet_backend;

pub use lua::LuaBackend;
#[cfg(feature = "runlet")]
pub use runlet_backend::RunletBackend;

pub const COMPOSE_TOOL_NAME: &str = "compose";

/// Metadata key set on an approval interrupt to identify the nested tool call
/// inside the parent compose run that produced it.
pub const COMPOSE_CHILD_CALL_ID_METADATA_KEY: &str = "agentkit.compose.child_call_id";

/// Creates a [`ToolRegistry`] pre-populated with [`ComposeTool`].
pub fn registry() -> ToolRegistry {
    registry_with_config(ComposeConfig::default())
}

/// Creates a [`ToolRegistry`] pre-populated with [`ComposeTool`] using `config`.
pub fn registry_with_config(config: ComposeConfig) -> ToolRegistry {
    ToolRegistry::new().with(ComposeTool::new(config))
}

/// How the final compose result is encoded for the model transcript.
///
/// Intermediate (nested) tool results never enter the transcript either way;
/// this only affects the value the script returns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResultEncoding {
    /// Compact JSON as a structured tool output.
    #[default]
    Json,
    /// TOON (Token-Oriented Object Notation) text. Uniform object lists
    /// render as a header plus one row per element, which is substantially
    /// smaller than JSON for the list-shaped values compose scripts tend to
    /// return. Falls back to JSON if the value cannot be TOON-encoded.
    #[cfg(feature = "toon")]
    Toon,
}

/// Configuration for [`ComposeTool`].
#[derive(Clone, Debug)]
pub struct ComposeConfig {
    pub max_script_bytes: usize,
    pub max_nested_tool_calls: usize,
    pub max_result_bytes: usize,
    /// Backend-specific execution budget. The Lua backend enforces this as a
    /// VM instruction count; the Runlet backend ignores it because Runlet
    /// programs terminate structurally.
    pub max_instruction_count: u64,
    pub allow_recursive_compose: bool,
    pub allowed_tools: Option<BTreeSet<ToolName>>,
    pub result_encoding: ResultEncoding,
}

impl Default for ComposeConfig {
    fn default() -> Self {
        Self {
            max_script_bytes: 64 * 1024,
            max_nested_tool_calls: 64,
            max_result_bytes: 1024 * 1024,
            max_instruction_count: 1_000_000,
            allow_recursive_compose: false,
            allowed_tools: None,
            result_encoding: ResultEncoding::default(),
        }
    }
}

impl ComposeConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_script_bytes(mut self, value: usize) -> Self {
        self.max_script_bytes = value;
        self
    }

    pub fn with_max_nested_tool_calls(mut self, value: usize) -> Self {
        self.max_nested_tool_calls = value;
        self
    }

    pub fn with_max_result_bytes(mut self, value: usize) -> Self {
        self.max_result_bytes = value;
        self
    }

    pub fn with_max_instruction_count(mut self, value: u64) -> Self {
        self.max_instruction_count = value;
        self
    }

    pub fn allow_recursive_compose(mut self, value: bool) -> Self {
        self.allow_recursive_compose = value;
        self
    }

    pub fn with_result_encoding(mut self, value: ResultEncoding) -> Self {
        self.result_encoding = value;
        self
    }

    pub fn with_allowed_tools<I>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = ToolName>,
    {
        self.allowed_tools = Some(names.into_iter().collect());
        self
    }

    fn allows(&self, name: &ToolName) -> bool {
        if !self.allow_recursive_compose && name.0 == COMPOSE_TOOL_NAME {
            return false;
        }
        self.allowed_tools
            .as_ref()
            .is_none_or(|allowed| allowed.contains(name))
    }
}

/// Terminal outcome of a compose backend run that did not complete.
pub enum ComposeOutcome {
    Interrupted(ToolInterruption),
    Failed(ToolError),
}

/// Identifies a nested call for replay across approval interrupts.
///
/// Backends that dispatch sequentially use [`CallKey::Sequential`]: the
/// dispatcher keys the call by its ordinal in the run and enforces that fresh
/// calls append in order. Backends that dispatch concurrently supply a stable
/// content-addressed key via [`CallKey::Operation`] so replay is independent
/// of scheduling order.
#[derive(Clone, Debug)]
pub enum CallKey {
    Sequential,
    Operation(String),
}

/// Error surface of [`ChildDispatcher::call`].
#[derive(Debug)]
pub enum DispatchError {
    /// The child requires approval; the compose run must surface the
    /// interruption and can be resumed once the approval is granted.
    Interrupted(ToolInterruption),
    Failed(ToolError),
}

#[derive(Clone, Debug, Default)]
struct ComposeRunState {
    records: BTreeMap<String, ChildRecord>,
    sequential_len: usize,
    pending: BTreeMap<String, PendingChild>,
}

#[derive(Clone, Debug)]
struct ChildRecord {
    name: ToolName,
    input: Value,
    output: Value,
}

#[derive(Clone, Debug)]
struct PendingChild {
    name: ToolName,
    input: Value,
    approval_id: String,
}

/// Executes nested tool calls on behalf of a compose backend.
///
/// Owns everything that is backend-agnostic about a compose run: the nested
/// call budget, the allowlist, cancellation checks, replay of already
/// completed children after an approval interrupt, and pending-approval
/// bookkeeping.
#[derive(Clone)]
pub struct ChildDispatcher {
    config: ComposeConfig,
    states: Arc<Mutex<BTreeMap<ToolCallId, ComposeRunState>>>,
    scope: ToolExecutionScope,
    parent_call_id: ToolCallId,
    session_id: SessionId,
    turn_id: TurnId,
    approved_request: Option<ApprovalRequest>,
    call_counter: Arc<AtomicUsize>,
    cancellation: Option<TurnCancellation>,
}

impl ChildDispatcher {
    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.is_cancelled())
    }

    /// Dispatches one nested tool call, replaying it from a previous attempt
    /// of this compose run when the same call already completed before an
    /// approval interrupt.
    pub async fn call(
        &self,
        key: CallKey,
        tool_name: ToolName,
        child_input: Value,
    ) -> Result<Value, DispatchError> {
        if self.is_cancelled() {
            return Err(DispatchError::Failed(ToolError::Cancelled));
        }
        let ordinal = self.call_counter.fetch_add(1, Ordering::SeqCst);
        if ordinal >= self.config.max_nested_tool_calls {
            return Err(DispatchError::Failed(ToolError::ExecutionFailed(format!(
                "compose exceeded {} nested tool calls",
                self.config.max_nested_tool_calls
            ))));
        }
        if !self.config.allows(&tool_name) {
            return Err(DispatchError::Failed(ToolError::PermissionDenied(
                PermissionDenial {
                    code: PermissionCode::CustomPolicyDenied,
                    message: format!("compose cannot call tool {}", tool_name.0),
                    metadata: MetadataMap::new(),
                },
            )));
        }

        let (record_key, sequential) = match &key {
            CallKey::Sequential => (format!("seq:{ordinal}"), true),
            CallKey::Operation(id) => (format!("op:{id}"), false),
        };

        let replayed = {
            let states = self.states.lock().await;
            states
                .get(&self.parent_call_id)
                .and_then(|state| state.records.get(&record_key))
                .map(|record| {
                    (
                        record.name.clone(),
                        record.input.clone(),
                        record.output.clone(),
                    )
                })
        };
        if let Some((recorded_name, recorded_input, recorded_output)) = replayed {
            if recorded_name == tool_name && recorded_input == child_input {
                return Ok(recorded_output);
            }
            return Err(DispatchError::Failed(ToolError::ExecutionFailed(format!(
                "compose replay diverged at nested tool call {record_key}"
            ))));
        }

        let child_suffix = match &key {
            CallKey::Sequential => ordinal.to_string(),
            CallKey::Operation(id) => id.chars().take(12).collect(),
        };
        let child_call_id = ToolCallId::new(format!(
            "{}:compose:{child_suffix}",
            self.parent_call_id.0.as_str()
        ));
        let child_request = ToolRequest {
            call_id: child_call_id.clone(),
            tool_name: tool_name.clone(),
            input: child_input.clone(),
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            metadata: MetadataMap::new(),
        };

        let is_approved_pending = {
            let states = self.states.lock().await;
            let pending = states
                .get(&self.parent_call_id)
                .and_then(|state| state.pending.get(&record_key));
            pending.is_some_and(|pending| {
                pending.name == tool_name
                    && pending.input == child_input
                    && self
                        .approved_request
                        .as_ref()
                        .is_some_and(|approval| approval.id.0 == pending.approval_id)
            })
        };

        let outcome = if is_approved_pending {
            let approval = self.approved_request.as_ref().ok_or_else(|| {
                DispatchError::Failed(ToolError::Internal(
                    "missing compose approval request".into(),
                ))
            })?;
            self.scope
                .execute_approved_child(child_request, approval)
                .await
        } else {
            self.scope.execute_child(child_request).await
        };

        match outcome {
            ToolExecutionOutcome::Completed(result) => {
                let output =
                    tool_output_to_json(result.result.output).map_err(DispatchError::Failed)?;
                {
                    let mut states = self.states.lock().await;
                    let run_state = states.entry(self.parent_call_id.clone()).or_default();
                    if sequential {
                        if run_state.sequential_len != ordinal {
                            return Err(DispatchError::Failed(ToolError::ExecutionFailed(
                                format!("compose replay cannot append nested tool call {ordinal}"),
                            )));
                        }
                        run_state.sequential_len += 1;
                    }
                    run_state.records.insert(
                        record_key.clone(),
                        ChildRecord {
                            name: tool_name,
                            input: child_input,
                            output: output.clone(),
                        },
                    );
                    run_state.pending.remove(&record_key);
                }
                Ok(output)
            }
            ToolExecutionOutcome::Interrupted(ToolInterruption::ApprovalRequired(mut approval)) => {
                approval.metadata.insert(
                    COMPOSE_CHILD_CALL_ID_METADATA_KEY.into(),
                    Value::String(child_call_id.0.clone()),
                );
                {
                    let mut states = self.states.lock().await;
                    let run_state = states.entry(self.parent_call_id.clone()).or_default();
                    run_state.pending.insert(
                        record_key,
                        PendingChild {
                            name: tool_name,
                            input: child_input,
                            approval_id: approval.id.0.clone(),
                        },
                    );
                }
                Err(DispatchError::Interrupted(
                    ToolInterruption::ApprovalRequired(approval),
                ))
            }
            ToolExecutionOutcome::FailedBeforeInvocation(error)
            | ToolExecutionOutcome::Failed(error) => Err(DispatchError::Failed(error)),
        }
    }
}

/// One compose execution handed to a backend.
pub struct BackendRun {
    pub script: String,
    pub input: Value,
    pub visible_specs: Vec<ToolSpec>,
    pub dispatcher: ChildDispatcher,
    pub config: ComposeConfig,
    pub cancellation: Option<TurnCancellation>,
}

/// A script execution engine for [`ComposeTool`].
#[async_trait]
pub trait ComposeBackend: Send + Sync {
    /// Short engine name, e.g. `"lua"` or `"runlet"`.
    fn name(&self) -> &'static str;

    /// Full compose tool description, including language guidance and the
    /// rendered child catalog shapes (see [`render_catalog_shapes`]).
    fn description(&self, catalog: Option<&[ToolSpec]>) -> String;

    /// Description of the `script` input property.
    fn script_description(&self) -> &'static str;

    /// Runs `script` to completion and returns its JSON result. Nested tool
    /// calls must go through `run.dispatcher`.
    async fn execute(&self, run: BackendRun) -> Result<Value, ComposeOutcome>;
}

/// Appended to the compose description when [`ResultEncoding::Toon`] is
/// active, so the model can read the encoded result without guessing.
#[cfg(feature = "toon")]
const TOON_RESULT_NOTE: &str = "\n\nThe compose result is returned as TOON \
    (Token-Oriented Object Notation), not JSON: nested fields are `key: value` \
    lines indented two spaces; a list of uniform objects renders as \
    `name[N]{field1,field2}:` followed by one comma-separated row per element; \
    other lists render as `name[N]:` with `- ` items or a single \
    comma-separated line of scalars.";

/// Renders child output schemas for inclusion in a backend description.
///
/// Shapes render as compact type notation rather than raw JSON Schema: the
/// schema keyword `items` reads as a property to models trained on
/// `{ items: [...] }` API conventions, and `string[]` cannot be misread that
/// way.
pub fn render_catalog_shapes(catalog: &[ToolSpec]) -> String {
    let mut out = String::from(
        "\n\nReturn shapes of callable tools (input schemas are already provided by the \
         top-level tool catalog). `T[]` is a list of T — iterate it directly; `field?` may be \
         absent; `{ [key]: T }` has arbitrary keys; `\"a\" | \"b\"` is one of exactly those \
         strings:\n",
    );
    for spec in catalog {
        out.push_str("\n- ");
        out.push_str(spec.name.0.as_str());
        out.push_str(": ");
        match spec.output_schema.as_ref() {
            Some(schema) => out.push_str(&type_notation(schema, 0)),
            None => out.push_str("<undocumented>"),
        }
    }
    out
}

/// Compact type notation for a JSON Schema, mirroring the notation runlet's
/// analyzer uses in diagnostics. Lossy by design: bounds, formats, and
/// descriptions are dropped; unrepresentable constructs widen to `any`.
fn type_notation(schema: &Value, depth: usize) -> String {
    const MAX_DEPTH: usize = 6;
    let Some(object) = schema.as_object() else {
        return "any".to_string();
    };
    if let Some(variants) = object
        .get("oneOf")
        .or_else(|| object.get("anyOf"))
        .and_then(Value::as_array)
    {
        return variants
            .iter()
            .map(|v| type_notation(v, depth + 1))
            .collect::<Vec<_>>()
            .join(" | ");
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
    }
    match object.get("type").and_then(Value::as_str) {
        Some("array") => {
            let items = object
                .get("items")
                .map(|items| type_notation(items, depth + 1))
                .unwrap_or_else(|| "any".to_string());
            if items.contains(' ') && !items.starts_with('{') {
                format!("({items})[]")
            } else {
                format!("{items}[]")
            }
        }
        Some("object") => {
            if depth >= MAX_DEPTH {
                return "object".to_string();
            }
            let required: Vec<&str> = object
                .get("required")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            match object.get("properties").and_then(Value::as_object) {
                Some(properties) if !properties.is_empty() => {
                    let mut fields = properties
                        .iter()
                        .map(|(name, prop)| {
                            let optional = if required.contains(&name.as_str()) {
                                ""
                            } else {
                                "?"
                            };
                            // Author-written documentation is load-bearing
                            // (formats like "HH:MM UTC" exist nowhere else) —
                            // only schema keywords are lossy to drop.
                            let comment = prop
                                .get("description")
                                .and_then(Value::as_str)
                                .map(|d| format!(" /* {d} */"))
                                .unwrap_or_default();
                            format!(
                                "{}{optional}: {}{comment}",
                                serde_json::to_string(name)
                                    .expect("object property name serializes"),
                                type_notation(prop, depth + 1)
                            )
                        })
                        .collect::<Vec<_>>();
                    fields.sort();
                    format!("{{ {} }}", fields.join(", "))
                }
                _ => match object.get("additionalProperties") {
                    Some(additional) if additional.is_object() => {
                        format!("{{ [key]: {} }}", type_notation(additional, depth + 1))
                    }
                    _ => "object".to_string(),
                },
            }
        }
        Some("integer") => "int".to_string(),
        Some("number") => "number".to_string(),
        Some("string") => "string".to_string(),
        Some("boolean") => "bool".to_string(),
        Some("null") => "null".to_string(),
        _ => "any".to_string(),
    }
}

/// Tool that executes composition scripts over the current tool catalog.
#[derive(Clone)]
pub struct ComposeTool {
    spec: ToolSpec,
    config: ComposeConfig,
    backend: Arc<dyn ComposeBackend>,
    states: Arc<Mutex<BTreeMap<ToolCallId, ComposeRunState>>>,
    sources: Vec<Arc<dyn ToolSource>>,
    spec_cache: Arc<ComposeSpecCache>,
}

/// Memoizes the rendered compose spec between catalog changes.
///
/// The compose description enumerates every child's output schema, so
/// rebuilding it on each `specs()` call (once per model step) is wasted work
/// for static catalogs. The cache is invalidated when a child source reports
/// catalog events through [`ToolSource::drain_catalog_events`] — the same
/// signal the agent loop uses to refresh the model-visible catalog, so the
/// cached spec can never be staler than what the model already sees.
struct ComposeSpecCache {
    dirty: AtomicBool,
    spec: StdMutex<Option<ToolSpec>>,
}

impl ComposeSpecCache {
    fn empty() -> Self {
        Self {
            dirty: AtomicBool::new(false),
            spec: StdMutex::new(None),
        }
    }
}

impl ComposeTool {
    /// Builds a compose tool with no child catalog source. The tool description
    /// stays generic; the model has to use `tools()` at runtime to discover
    /// what's available. Prefer [`wrap`](Self::wrap) when possible — the
    /// model writes correct scripts on the first try when it sees concrete
    /// input/output schemas at planning time.
    pub fn new(config: ComposeConfig) -> Self {
        Self::build(config, Arc::new(LuaBackend), Vec::new())
    }

    /// Wraps a source of child tools. The resulting [`ToolSource`] still
    /// advertises every child tool individually to the model AND adds the
    /// `compose` entry whose description enumerates each child's output schema.
    /// Child tool lookups and catalog events continue to delegate to the live
    /// source, so dynamic catalogs stay reactive.
    ///
    /// ```rust
    /// use agentkit_core::{ToolOutput, ToolResultPart};
    /// use agentkit_tool_compose::{ComposeConfig, ComposeTool};
    /// use agentkit_tools_core::{ToolError, ToolRegistry, ToolResult, ToolSource};
    /// use agentkit_tools_derive::tool;
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(JsonSchema, Deserialize)]
    /// struct EchoInput { message: String }
    ///
    /// /// Echo the input back as the tool result.
    /// #[tool(read_only)]
    /// async fn echo(input: EchoInput) -> Result<ToolResult, ToolError> {
    ///     Ok(ToolResult::new(ToolResultPart::success(
    ///         "call",
    ///         ToolOutput::text(input.message),
    ///     )))
    /// }
    ///
    /// let tool_source = ComposeTool::wrap(ToolRegistry::new().with(echo))
    ///     .with_config(ComposeConfig::new().with_max_instruction_count(12_000));
    ///
    /// // Model sees both `echo` and `compose`; compose's description
    /// // enumerates echo's input/output schemas.
    /// let names: Vec<String> = tool_source.specs().into_iter().map(|s| s.name.0).collect();
    /// assert!(names.iter().any(|n| n == "compose"));
    /// assert!(names.iter().any(|n| n == "echo"));
    /// ```
    pub fn wrap(source: impl ToolSource + 'static) -> Self {
        Self::new(ComposeConfig::default()).with_source(source)
    }

    /// Adds another child source to this compose source.
    ///
    /// To make a source's tools dispatchable through `tool(name, input)`
    /// without advertising them individually to the model (and without
    /// enumerating their schemas in the compose description), wrap it with
    /// [`ToolSource::unadvertised`] first.
    pub fn with_source(mut self, source: impl ToolSource + 'static) -> Self {
        self.sources.push(Arc::new(source));
        // Fresh cache: clones of the pre-`with_source` tool must not share
        // a slot with the extended catalog.
        self.spec_cache = Arc::new(ComposeSpecCache::empty());
        self.spec = self.cached_compose_spec();
        self
    }

    /// Replaces the configuration and rebuilds the compose tool description so
    /// it reflects the new permission filter.
    pub fn with_config(self, config: ComposeConfig) -> Self {
        Self::build(config, self.backend, self.sources)
    }

    /// Replaces the execution backend and rebuilds the compose tool
    /// description so it reflects the backend's language.
    pub fn with_backend(self, backend: impl ComposeBackend + 'static) -> Self {
        Self::build(self.config, Arc::new(backend), self.sources)
    }

    fn build(
        config: ComposeConfig,
        backend: Arc<dyn ComposeBackend>,
        sources: Vec<Arc<dyn ToolSource>>,
    ) -> Self {
        let mut tool = Self {
            spec: Self::base_spec(&config, backend.as_ref(), None),
            config,
            backend,
            states: Arc::new(Mutex::new(BTreeMap::new())),
            sources,
            spec_cache: Arc::new(ComposeSpecCache::empty()),
        };
        tool.spec = tool.cached_compose_spec();
        tool
    }

    fn base_spec(
        config: &ComposeConfig,
        backend: &dyn ComposeBackend,
        catalog: Option<&[ToolSpec]>,
    ) -> ToolSpec {
        let filtered: Option<Vec<ToolSpec>> = catalog.map(|snap| {
            snap.iter()
                .filter(|spec| config.allows(&spec.name))
                .cloned()
                .collect()
        });
        let mut description = backend.description(filtered.as_deref());
        #[cfg(feature = "toon")]
        if config.result_encoding == ResultEncoding::Toon {
            description.push_str(TOON_RESULT_NOTE);
        }
        ToolSpec::new(
            COMPOSE_TOOL_NAME,
            description,
            json!({
                "type": "object",
                "properties": {
                    "script": {
                        "type": "string",
                        "description": backend.script_description()
                    },
                    "input": {
                        "description": "Optional JSON value exposed to the script as global input."
                    }
                },
                "required": ["script"],
                "additionalProperties": false
            }),
        )
        .with_annotations(ToolAnnotations::new())
    }

    fn compose_spec(&self) -> ToolSpec {
        let catalog = self.child_specs();
        Self::base_spec(&self.config, self.backend.as_ref(), Some(&catalog))
    }

    /// Returns the compose spec, recomputing it only when a child source
    /// has reported catalog changes since the last call (see
    /// [`ComposeSpecCache`]).
    fn cached_compose_spec(&self) -> ToolSpec {
        let mut slot = self
            .spec_cache
            .spec
            .lock()
            .expect("compose spec cache poisoned");
        if self.spec_cache.dirty.swap(false, Ordering::AcqRel) || slot.is_none() {
            *slot = Some(self.compose_spec());
        }
        slot.clone().expect("compose spec cache filled above")
    }

    fn child_specs(&self) -> Vec<ToolSpec> {
        let mut seen = BTreeSet::new();
        let mut specs = Vec::new();
        for source in &self.sources {
            for spec in source.specs() {
                if seen.insert(spec.name.clone()) {
                    specs.push(spec);
                }
            }
        }
        specs
    }

    fn visible_specs(&self, scope: &ToolExecutionScope) -> Vec<ToolSpec> {
        scope
            .executor
            .specs()
            .into_iter()
            .filter(|spec| self.config.allows(&spec.name))
            .collect()
    }
}

impl ToolSource for ComposeTool {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut seen = BTreeSet::new();
        let mut specs = Vec::new();
        let compose_spec = self.cached_compose_spec();
        seen.insert(compose_spec.name.clone());
        specs.push(compose_spec);
        for spec in self.child_specs() {
            if seen.insert(spec.name.clone()) {
                specs.push(spec);
            }
        }
        specs
    }

    fn get(&self, name: &ToolName) -> Option<Arc<dyn Tool>> {
        if name.0.as_str() == COMPOSE_TOOL_NAME {
            return Some(Arc::new(self.clone()));
        }
        self.sources.iter().find_map(|source| source.get(name))
    }

    fn drain_catalog_events(&self) -> Vec<ToolCatalogEvent> {
        let mut events: Vec<ToolCatalogEvent> = self
            .sources
            .iter()
            .flat_map(|source| source.drain_catalog_events())
            .collect();
        if !events.is_empty() {
            self.spec_cache.dirty.store(true, Ordering::Release);
            let mut event = ToolCatalogEvent::new(COMPOSE_TOOL_NAME);
            event.changed.push(COMPOSE_TOOL_NAME.into());
            events.push(event);
        }
        events
    }
}

#[derive(Debug, Deserialize)]
struct ComposeInput {
    script: String,
    #[serde(default)]
    input: Value,
}

#[async_trait]
impl Tool for ComposeTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn current_spec(&self) -> Option<ToolSpec> {
        Some(self.cached_compose_spec())
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        match self.invoke_outcome(request, ctx).await {
            ToolExecutionOutcome::Completed(result) => Ok(result),
            ToolExecutionOutcome::Interrupted(_) => Err(ToolError::Internal(
                "compose produced an approval interrupt through invoke".into(),
            )),
            ToolExecutionOutcome::FailedBeforeInvocation(error) => Err(error),
            ToolExecutionOutcome::Failed(error) => Err(error),
        }
    }

    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        match self.invoke_outcome_inner(request, ctx).await {
            Ok(result) => ToolExecutionOutcome::Completed(result),
            Err(ComposeOutcome::Interrupted(interruption)) => {
                ToolExecutionOutcome::Interrupted(interruption)
            }
            Err(ComposeOutcome::Failed(error)) => ToolExecutionOutcome::Failed(error),
        }
    }
}

impl ComposeTool {
    async fn invoke_outcome_inner(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ComposeOutcome> {
        let input: ComposeInput = serde_json::from_value(request.input.clone())
            .map_err(|error| ComposeOutcome::Failed(ToolError::InvalidInput(error.to_string())))?;
        if input.script.len() > self.config.max_script_bytes {
            return Err(ComposeOutcome::Failed(ToolError::InvalidInput(format!(
                "compose script exceeds {} bytes",
                self.config.max_script_bytes
            ))));
        }
        let Some(scope) = ctx.execution_scope.clone() else {
            return Err(ComposeOutcome::Failed(ToolError::Unavailable(
                "compose requires a tool execution scope".into(),
            )));
        };
        if ctx
            .cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.is_cancelled())
        {
            return Err(ComposeOutcome::Failed(ToolError::Cancelled));
        }

        {
            let mut states = self.states.lock().await;
            if ctx.approved_request.is_none() {
                states.remove(&request.call_id);
            }
            states.entry(request.call_id.clone()).or_default();
        }

        let cleanup_call_id = request.call_id.clone();
        let visible_specs = self.visible_specs(&scope);
        let cancellation = ctx.cancellation.clone();
        let dispatcher = ChildDispatcher {
            config: self.config.clone(),
            states: self.states.clone(),
            scope,
            parent_call_id: request.call_id.clone(),
            session_id: request.session_id.clone(),
            turn_id: request.turn_id.clone(),
            approved_request: ctx.approved_request.clone(),
            call_counter: Arc::new(AtomicUsize::new(0)),
            cancellation: cancellation.clone(),
        };
        let run = BackendRun {
            script: input.script,
            input: input.input,
            visible_specs,
            dispatcher,
            config: self.config.clone(),
            cancellation,
        };
        let outcome = self.run_backend(request.call_id, run).await;
        if !matches!(outcome, Err(ComposeOutcome::Interrupted(_))) {
            self.states.lock().await.remove(&cleanup_call_id);
        }
        outcome
    }

    async fn run_backend(
        &self,
        call_id: ToolCallId,
        run: BackendRun,
    ) -> Result<ToolResult, ComposeOutcome> {
        let json_result = self.backend.execute(run).await?;
        let result_bytes = serde_json::to_vec(&json_result)
            .map_err(|error| ComposeOutcome::Failed(ToolError::Internal(error.to_string())))?
            .len();
        if result_bytes > self.config.max_result_bytes {
            return Err(ComposeOutcome::Failed(ToolError::ExecutionFailed(format!(
                "compose result exceeds {} bytes",
                self.config.max_result_bytes
            ))));
        }
        let output = match self.config.result_encoding {
            ResultEncoding::Json => ToolOutput::structured(json_result),
            #[cfg(feature = "toon")]
            ResultEncoding::Toon => match serde_toon2::to_string(&json_result) {
                Ok(toon) => ToolOutput::text(toon),
                Err(_) => ToolOutput::structured(json_result),
            },
        };
        Ok(ToolResult::new(ToolResultPart::success(call_id, output)))
    }
}

fn tool_output_to_json(output: ToolOutput) -> Result<Value, ToolError> {
    match output {
        ToolOutput::Text(text) => Ok(Value::String(text)),
        ToolOutput::Structured(value) => Ok(value),
        other => {
            serde_json::to_value(other).map_err(|error| ToolError::Internal(error.to_string()))
        }
    }
}

#[cfg(test)]
mod tests;
