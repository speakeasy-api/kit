use crate::analyzer::{CompiledProgram, ExternalInput, compile};
use crate::{
    BinaryOp, Block, CanonicalValue as V, Diagnostic, Expr, ExprKind, Graph, GraphChange,
    GraphEvent, NodeKind, NodeState, Schema, Span, ToolRegistry, UnaryOp,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use thiserror::Error;

#[derive(Debug, Clone)]
/// Metadata supplied to a host tool handler for one dispatch attempt.
pub struct ToolContext {
    /// Execution graph node associated with this dispatch.
    pub node_id: String,
    /// Stable identity shared by retries of the same logical operation.
    pub operation_id: String,
    /// Identity unique to this individual dispatch attempt.
    pub dispatch_id: String,
    /// Zero-based retry attempt.
    pub attempt: u32,
    /// Version declared by the tool descriptor.
    pub schema_version: String,
}

#[derive(Debug, Clone, Error)]
#[error("{code}: {message}")]
/// Structured failure returned by a tool handler or the runtime.
pub struct ToolError {
    /// Stable machine-readable error code.
    pub code: String,
    /// Human-readable failure description.
    pub message: String,
    /// Whether a boundary may retry the operation.
    pub retryable: bool,
    /// Whether the host cannot determine if an effect occurred.
    pub uncertain: bool,
    /// Additional structured error data exposed to a catch block.
    pub details: BTreeMap<String, V>,
    /// Byte span of the source expression whose evaluation failed, stamped by
    /// the innermost evaluator frame. `None` for errors raised outside
    /// expression evaluation (e.g. registry digest mismatch).
    pub span: Option<Span>,
    /// Host-provided hint for how long to wait before retrying this failure
    /// (e.g. from an HTTP `Retry-After` header). When present on a retryable
    /// failure, boundary retries honor it instead of the computed backoff,
    /// still capped by the configured backoff cap.
    pub retry_after: Option<std::time::Duration>,
}
impl ToolError {
    /// Creates a non-retryable error with no details or source span.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            uncertain: false,
            details: BTreeMap::new(),
            span: None,
            retry_after: None,
        }
    }
    /// Sets whether the error may be retried by a boundary.
    pub fn retryable(mut self, yes: bool) -> Self {
        self.retryable = yes;
        self
    }
    /// Attaches a host-suggested delay before retrying.
    pub fn with_retry_after(mut self, delay: std::time::Duration) -> Self {
        self.retry_after = Some(delay);
        self
    }
}

type Handler = Arc<dyn Fn(&[V], &ToolContext) -> Result<V, ToolError> + Send + Sync>;

#[derive(Clone)]
/// Immutable compiler and in-process executor configured by [`RuntimeBuilder`].
pub struct Runtime {
    registry: ToolRegistry,
    handlers: BTreeMap<String, Handler>,
    inputs: BTreeMap<String, (Schema, V)>,
    loop_concurrency: u32,
    max_graph_nodes: usize,
    max_eval_depth: usize,
    max_active_dispatches: usize,
    max_worker_threads: usize,
    retry_backoff: Option<RetryBackoff>,
}

/// Executor policy for delaying boundary retry attempts. Purely a scheduling
/// concern: delays never enter operation identity, canonical values, or the
/// execution graph.
#[derive(Debug, Clone, Copy)]
struct RetryBackoff {
    base: std::time::Duration,
    factor: f64,
    cap: std::time::Duration,
}

/// Counting semaphore that bounds concurrently-active tool dispatches.
///
/// Only leaf operations (tool and intrinsic executions) hold permits, so
/// acquisition can never deadlock through nesting: a loop iteration waiting
/// on a permit holds none itself. Blocking here is deliberate backpressure —
/// work queues instead of growing resident memory.
struct DispatchSemaphore {
    permits: Mutex<usize>,
    released: Condvar,
}

impl DispatchSemaphore {
    fn new(permits: usize) -> Self {
        Self {
            permits: Mutex::new(permits.max(1)),
            released: Condvar::new(),
        }
    }

    fn acquire(&self) -> DispatchPermit<'_> {
        let mut permits = self.permits.lock().unwrap();
        while *permits == 0 {
            permits = self.released.wait(permits).unwrap();
        }
        *permits -= 1;
        DispatchPermit(self)
    }
}

struct DispatchPermit<'a>(&'a DispatchSemaphore);

impl Drop for DispatchPermit<'_> {
    fn drop(&mut self) {
        *self.0.permits.lock().unwrap() += 1;
        self.0.released.notify_one();
    }
}
/// Builder for a [`Runtime`] and its complete host capability surface.
pub struct RuntimeBuilder {
    runtime: Runtime,
}

impl Runtime {
    /// Starts building a runtime with conservative default resource limits.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder {
            runtime: Runtime {
                registry: ToolRegistry::new(),
                handlers: BTreeMap::new(),
                inputs: BTreeMap::new(),
                loop_concurrency: 16,
                max_graph_nodes: 250_000,
                max_eval_depth: 512,
                max_active_dispatches: 64,
                max_worker_threads: 64,
                retry_backoff: None,
            },
        }
    }
    /// Parses and analyzes source against this runtime's tools and inputs.
    pub fn compile(&self, source: &str) -> Result<CompiledProgram, Vec<Diagnostic>> {
        let inputs = self
            .inputs
            .iter()
            .map(|(name, (schema, _))| ExternalInput {
                name: name.clone(),
                schema: schema.clone(),
            })
            .collect::<Vec<_>>();
        compile(source, &self.registry, &inputs)
    }
    /// Executes a previously compiled program to completion.
    ///
    /// The program must have been compiled against an equivalent registry.
    pub fn run(&self, program: &CompiledProgram) -> Result<Execution, ToolError> {
        self.run_observed(program, |_| {})
    }

    /// Runs a program and reports graph changes from executor threads as they happen.
    ///
    /// Calls to `observer` are serialized and sequence numbers are strictly increasing.
    pub fn run_observed<F>(
        &self,
        program: &CompiledProgram,
        observer: F,
    ) -> Result<Execution, ToolError>
    where
        F: Fn(&GraphEvent) + Send + Sync + 'static,
    {
        if program.registry_digest != self.registry.digest() {
            return Err(ToolError::new(
                "RL7102",
                "compiled program registry digest does not match this runtime",
            ));
        }
        let graph = Arc::new(LiveGraph::new(Arc::new(observer)));
        let mut ev = Evaluator {
            runtime: self,
            graph: graph.clone(),
            scopes: vec![],
            attempt: 0,
            owners: vec![],
            workflow: program.source_digest.clone(),
            dynamic: vec![],
            successful_operations: Arc::new(Mutex::new(HashMap::new())),
            dispatch_generations: Arc::new(Mutex::new(HashMap::new())),
            operation_nodes: Arc::new(Mutex::new(HashMap::new())),
            last_output: None,
            binding_caches: vec![],
            depth: 0,
            dispatches: Arc::new(DispatchSemaphore::new(self.max_active_dispatches)),
            worker_budget: Arc::new(AtomicUsize::new(self.max_worker_threads)),
        };
        let mut root = HashMap::new();
        for (n, (_, v)) in &self.inputs {
            root.insert(n.clone(), Binding::Value(v.clone(), None));
        }
        for s in &program.program.statements {
            if let crate::StmtKind::Binding { name, value } = &s.kind {
                root.insert(name.clone(), Binding::Expr(value.clone()));
            }
        }
        ev.scopes.push(root);
        let node = ev
            .graph
            .begin(NodeKind::Root, "return", program.program.result.span, 0);
        ev.graph.running(node);
        let outcome = ev
            .guards_and_effects(&program.program.statements)
            .and_then(|_| ev.eval(&program.program.result));
        match outcome {
            Ok(v) => {
                if let Some(producer) = ev.last_output {
                    ev.graph.data(producer, node, "result", "return");
                }
                ev.graph.success(node, v.clone());
                Ok(Execution {
                    value: v,
                    graph: graph.snapshot(),
                })
            }
            Err(e) => {
                ev.graph.fail(node, e.to_string());
                Err(e)
            }
        }
    }

    /// Delay to apply before boundary re-attempt `reattempt` (1-based) for a
    /// retryable failure, combining the configured backoff policy with the
    /// error's own `retry_after` hint.
    fn retry_delay(
        &self,
        reattempt: u32,
        retry_after: Option<std::time::Duration>,
    ) -> std::time::Duration {
        match (self.retry_backoff, retry_after) {
            (Some(backoff), Some(after)) => after.min(backoff.cap),
            (Some(backoff), None) => {
                let exponent = reattempt.saturating_sub(1).min(i32::MAX as u32) as i32;
                let scaled = backoff.base.as_secs_f64() * backoff.factor.powi(exponent);
                std::time::Duration::try_from_secs_f64(scaled)
                    .unwrap_or(backoff.cap)
                    .min(backoff.cap)
            }
            (None, Some(after)) => after,
            (None, None) => std::time::Duration::ZERO,
        }
    }
}
impl RuntimeBuilder {
    /// Replaces the registry describing tools visible to programs.
    pub fn registry(mut self, registry: ToolRegistry) -> Self {
        self.runtime.registry = registry;
        self
    }
    /// Adds or replaces a named, schema-checked host input.
    pub fn input(mut self, name: impl Into<String>, schema: Schema, value: V) -> Self {
        self.runtime.inputs.insert(name.into(), (schema, value));
        self
    }
    /// Adds or replaces the implementation for a registered tool name.
    pub fn tool<F>(mut self, name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(&[V], &ToolContext) -> Result<V, ToolError> + Send + Sync + 'static,
    {
        self.runtime.handlers.insert(name.into(), Arc::new(handler));
        self
    }
    /// Sets the host-owned concurrency used by every `for` loop.
    pub fn loop_concurrency(mut self, concurrency: u32) -> Self {
        self.runtime.loop_concurrency = concurrency.max(1);
        self
    }
    /// Caps the total number of execution graph nodes a run may create.
    /// A hard backstop against runaway evaluation: exceeding it fails the run
    /// with `RL4105` instead of exhausting memory.
    pub fn graph_node_limit(mut self, limit: usize) -> Self {
        self.runtime.max_graph_nodes = limit;
        self
    }
    /// Caps expression nesting depth during evaluation.
    ///
    /// Depth is tracked explicitly on the heap, so a deeply nested program
    /// fails with an `RL4106` diagnostic instead of overflowing the native
    /// call stack. The language has no recursion, so nesting is bounded by
    /// program text and legitimate programs sit far below the default.
    pub fn eval_depth_limit(mut self, limit: usize) -> Self {
        self.runtime.max_eval_depth = limit.max(1);
        self
    }
    /// Bounds concurrently-active tool and intrinsic dispatches per run.
    ///
    /// When the limit is reached, further dispatches block until a permit
    /// frees — backpressure instead of unbounded resident growth.
    pub fn dispatch_limit(mut self, limit: usize) -> Self {
        self.runtime.max_active_dispatches = limit.max(1);
        self
    }
    /// Bounds the total number of live loop worker threads per run.
    ///
    /// Nested loops multiply concurrency (each outer iteration runs the inner
    /// loop with its own workers — a triple-nested loop over 24 items would
    /// otherwise create ~14k threads). Every loop takes at most the threads
    /// still available and the evaluating thread always works its own loop,
    /// so exhaustion degrades to sequential execution instead of blocking.
    pub fn worker_thread_limit(mut self, limit: usize) -> Self {
        self.runtime.max_worker_threads = limit;
        self
    }
    /// Configures exponential backoff between boundary retry attempts.
    ///
    /// Re-attempt `k` (starting at 1 for the first retry) is delayed by
    /// `min(cap, base * factor^(k-1))`. A retryable [`ToolError`] carrying
    /// [`retry_after`](ToolError::retry_after) is honored instead of the
    /// computed delay, still capped by `cap`. Unconfigured runtimes retry
    /// immediately.
    pub fn retry_backoff(
        mut self,
        base: std::time::Duration,
        factor: f64,
        cap: std::time::Duration,
    ) -> Self {
        self.runtime.retry_backoff = Some(RetryBackoff { base, factor, cap });
        self
    }
    /// Installs the deterministic standard prelude (`text.*`, `regex.*`,
    /// `list.*`, `json.*`, `number.*`, and `time.*` intrinsics) into the
    /// registry and handler table.
    ///
    /// Call after [`registry`](Self::registry). A name the host has already
    /// registered is left untouched — host registrations win, including their
    /// implementation.
    pub fn with_prelude(mut self) -> Self {
        for (descriptor, handler) in crate::prelude::entries() {
            if self.runtime.registry.get(&descriptor.name).is_some() {
                continue;
            }
            let name = descriptor.name.clone();
            self.runtime
                .registry
                .register(descriptor)
                .expect("prelude descriptors are valid and deduplicated");
            self.runtime.handlers.insert(name, Arc::new(handler));
        }
        self
    }
    /// Validates the registry, handlers, and inputs, then builds the runtime.
    pub fn build(self) -> Result<Runtime, String> {
        for n in self.runtime.registry.names() {
            if !self.runtime.handlers.contains_key(n) {
                return Err(format!("registered tool `{n}` has no implementation"));
            }
        }
        for n in self.runtime.handlers.keys() {
            if self.runtime.registry.get(n).is_none() {
                return Err(format!("implementation `{n}` has no descriptor"));
            }
        }
        for (n, (s, v)) in &self.runtime.inputs {
            if !s.accepts(v) {
                return Err(format!("input `{n}` does not match its schema"));
            }
        }
        Ok(self.runtime)
    }
}

#[derive(Debug, Clone)]
/// Final value and execution graph from a successful run.
pub struct Execution {
    /// Value returned by the program.
    pub value: V,
    /// Final snapshot of the execution graph.
    pub graph: Graph,
}
#[derive(Clone)]
enum Binding {
    Expr(Expr),
    Value(V, Option<usize>),
    Evaluating,
}
/// Values of outer bindings forced inside loop iterations, shared across the
/// iterations of one loop. Keyed by (scope index, name); every worker of a
/// loop clones an identical scope prefix below the loop's base depth, so the
/// key identifies the same binding in all of them. Without this, a binding
/// referenced only inside a loop body is re-evaluated by every iteration —
/// and chains of such loops re-evaluate exponentially.
type BindingCache = Arc<Mutex<HashMap<(usize, String), (V, Option<usize>)>>>;

struct Evaluator<'a> {
    runtime: &'a Runtime,
    graph: Arc<LiveGraph>,
    scopes: Vec<HashMap<String, Binding>>,
    attempt: u32,
    owners: Vec<usize>,
    workflow: String,
    dynamic: Vec<String>,
    successful_operations: Arc<Mutex<HashMap<String, V>>>,
    dispatch_generations: Arc<Mutex<HashMap<String, u32>>>,
    operation_nodes: Arc<Mutex<HashMap<String, usize>>>,
    last_output: Option<usize>,
    binding_caches: Vec<(usize, BindingCache)>,
    depth: usize,
    dispatches: Arc<DispatchSemaphore>,
    worker_budget: Arc<AtomicUsize>,
}

struct LiveGraph {
    state: Mutex<LiveGraphState>,
    observer: Arc<dyn Fn(&GraphEvent) + Send + Sync>,
}

#[derive(Default)]
struct LiveGraphState {
    graph: Graph,
    sequence: u64,
}

impl LiveGraph {
    fn new(observer: Arc<dyn Fn(&GraphEvent) + Send + Sync>) -> Self {
        Self {
            state: Mutex::new(LiveGraphState::default()),
            observer,
        }
    }

    fn publish(&self, state: &mut LiveGraphState, change: GraphChange) {
        state.sequence += 1;
        (self.observer)(&GraphEvent {
            sequence: state.sequence,
            change,
        });
    }

    fn begin(&self, kind: NodeKind, label: impl Into<String>, span: Span, attempt: u32) -> usize {
        let mut state = self.state.lock().unwrap();
        let index = state.graph.begin(kind, label, span, attempt);
        let node = state.graph.nodes[index].clone();
        self.publish(&mut state, GraphChange::NodeAdded(node));
        index
    }

    fn update(&self, index: usize, apply: impl FnOnce(&mut Graph, usize)) {
        let mut state = self.state.lock().unwrap();
        apply(&mut state.graph, index);
        let node = state.graph.nodes[index].clone();
        self.publish(&mut state, GraphChange::NodeUpdated(node));
    }

    fn running(&self, index: usize) {
        self.update(index, |graph, index| graph.running(index));
    }

    fn dispatching(&self, index: usize) {
        self.update(index, |graph, index| {
            graph.nodes[index].state = NodeState::Dispatching
        });
    }

    fn label(&self, index: usize, label: String) {
        self.update(index, |graph, index| graph.nodes[index].label = label);
    }

    fn blocked(&self, index: usize) {
        self.update(index, |graph, index| {
            graph.nodes[index].state = NodeState::Blocked
        });
    }

    fn success(&self, index: usize, value: V) {
        self.update(index, |graph, index| graph.success(index, value));
    }

    fn fail(&self, index: usize, error: impl Into<String>) {
        let error = error.into();
        self.update(index, |graph, index| graph.fail(index, error));
    }

    fn contains(&self, parent: usize, child: usize) {
        let mut state = self.state.lock().unwrap();
        state.graph.contains(parent, child);
        let edge = state.graph.edges.last().unwrap().clone();
        self.publish(&mut state, GraphChange::EdgeAdded(edge));
    }

    fn data(&self, producer: usize, consumer: usize, producer_path: &str, consumer_path: &str) {
        let mut state = self.state.lock().unwrap();
        let edge = crate::Edge {
            from: state.graph.nodes[producer].id.clone(),
            to: state.graph.nodes[consumer].id.clone(),
            kind: crate::EdgeKind::Data {
                producer_path: producer_path.into(),
                consumer_path: consumer_path.into(),
            },
        };
        state.graph.edges.push(edge.clone());
        self.publish(&mut state, GraphChange::EdgeAdded(edge));
    }

    fn retry_of(&self, retry: usize, previous_attempt: usize) {
        let mut state = self.state.lock().unwrap();
        let edge = crate::Edge {
            from: state.graph.nodes[retry].id.clone(),
            to: state.graph.nodes[previous_attempt].id.clone(),
            kind: crate::EdgeKind::RetryOf,
        };
        state.graph.edges.push(edge.clone());
        self.publish(&mut state, GraphChange::EdgeAdded(edge));
    }

    fn node_id(&self, index: usize) -> String {
        self.state.lock().unwrap().graph.nodes[index].id.clone()
    }

    fn node_count(&self) -> usize {
        self.state.lock().unwrap().graph.nodes.len()
    }

    fn snapshot(&self) -> Graph {
        self.state.lock().unwrap().graph.clone()
    }
}

impl Evaluator<'_> {
    fn eval(&mut self, e: &Expr) -> Result<V, ToolError> {
        if self.depth >= self.runtime.max_eval_depth {
            return Err(lang(
                "RL4106",
                &format!(
                    "expression nesting exceeded the host depth limit of {}",
                    self.runtime.max_eval_depth
                ),
            ));
        }
        self.depth += 1;
        let result = self.eval_inner(e);
        self.depth -= 1;
        // The innermost failing frame stamps its span; outer frames leave it,
        // so hosts can point at the exact source expression that failed.
        result.map_err(|mut error| {
            if error.span.is_none() {
                error.span = Some(e.span);
            }
            error
        })
    }
    fn eval_inner(&mut self, e: &Expr) -> Result<V, ToolError> {
        self.last_output = None;
        match &e.kind {
            ExprKind::Null => Ok(V::Null),
            ExprKind::Boolean(v) => Ok(V::Boolean(*v)),
            ExprKind::Integer(s) => s
                .parse()
                .map(V::Integer)
                .map_err(|_| lang("RL5101", "NUMERIC_OVERFLOW")),
            ExprKind::Number(s) => {
                V::number(s.parse().map_err(|_| lang("RL5103", "NON_FINITE_NUMBER"))?)
                    .map_err(|x| lang("RL5103", &x.to_string()))
            }
            ExprKind::String(s) => Ok(V::String(s.clone())),
            ExprKind::Name(n) => self.name(n),
            ExprKind::List(xs) => {
                let node = self.node(NodeKind::Composite, "list", e.span)?;
                let r = xs
                    .iter()
                    .map(|x| self.eval(x))
                    .collect::<Result<Vec<_>, _>>()
                    .map(V::List);
                self.finish(node, r)
            }
            ExprKind::Object(xs) => {
                let node = self.node(NodeKind::Composite, "object", e.span)?;
                let mut o = BTreeMap::new();
                for (k, x) in xs {
                    // Last entry wins on key collisions (BTreeMap insert
                    // overwrites), matching `+` merge's right bias.
                    let key = match k {
                        crate::ObjectKey::Static(k) => k.clone(),
                        crate::ObjectKey::Computed(key_expr) => {
                            match self.eval(key_expr).and_then(|v| {
                                computed_key(v).map_err(|mut err| {
                                    err.span = err.span.or(Some(key_expr.span));
                                    err
                                })
                            }) {
                                Ok(key) => key,
                                Err(err) => {
                                    self.graph.fail(node, err.to_string());
                                    return Err(err);
                                }
                            }
                        }
                    };
                    o.insert(
                        key,
                        match self.eval(x) {
                            Ok(v) => v,
                            Err(err) => {
                                self.graph.fail(node, err.to_string());
                                return Err(err);
                            }
                        },
                    );
                }
                self.finish(node, Ok(V::Object(o)))
            }
            ExprKind::Member { target, field } => {
                let node = self.node(NodeKind::Project, format!(".{field}"), e.span)?;
                let r = self
                    .eval(target)
                    .and_then(|v| project(v, &V::String(field.clone())));
                self.finish(node, r)
            }
            ExprKind::Index { target, index } => {
                let node = self.node(NodeKind::Project, "index", e.span)?;
                let r = (|| {
                    let v = self.eval(target)?;
                    let i = self.eval(index)?;
                    project(v, &i)
                })();
                self.finish(node, r)
            }
            ExprKind::Call { callee, arguments } => self.call(e.span, callee, arguments),
            ExprKind::Unary { op, value } => {
                let node = self.node(NodeKind::Compute, format!("{op:?}"), e.span)?;
                let r = self.eval(value).and_then(|v| unary(*op, v));
                self.finish(node, r)
            }
            ExprKind::Binary { op, left, right } => self.binary(e.span, *op, left, right),
            ExprKind::Conditional {
                then_expr,
                condition,
                else_expr,
            } => {
                let node = self.node(NodeKind::Branch, "if", e.span)?;
                let c = self.eval(condition)?;
                let chosen = match c {
                    V::Boolean(true) => then_expr,
                    V::Boolean(false) => else_expr,
                    other => {
                        let mut er = lang(
                            "RL5202",
                            &format!(
                                "condition evaluated to {}; conditions must be true or false — \
                                 Runlet has no truthiness, compare explicitly (e.g. \
                                 `value != null`, `list.count(xs) > 0`, `text != \"\"`)",
                                value_kind(&other)
                            ),
                        );
                        er.span = Some(condition.span);
                        self.graph.fail(node, er.to_string());
                        return Err(er);
                    }
                };
                let r = self.eval(chosen);
                self.finish(node, r)
            }
            ExprKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let node = self.node(NodeKind::Branch, "if", e.span)?;
                let c = match self.eval(condition) {
                    Ok(v) => v,
                    Err(err) => {
                        self.graph.fail(node, err.to_string());
                        return Err(err);
                    }
                };
                let r = match c {
                    V::Boolean(true) => self.block(then_block),
                    V::Boolean(false) => match else_block {
                        Some(block) => self.block(block),
                        None => Ok(V::Null),
                    },
                    other => {
                        let mut er = lang(
                            "RL5202",
                            &format!(
                                "condition evaluated to {}; conditions must be true or false — \
                                 Runlet has no truthiness, compare explicitly (e.g. \
                                 `value != null`, `list.length(xs) > 0`, `text != \"\"`)",
                                value_kind(&other)
                            ),
                        );
                        er.span = Some(condition.span);
                        Err(er)
                    }
                };
                self.finish(node, r)
            }
            ExprKind::For {
                binding,
                collection,
                body,
            } => self.loop_expr(e.span, binding, collection, body),
            ExprKind::Fold {
                accumulator,
                init,
                binding,
                collection,
                body,
            } => self.fold_expr(e.span, accumulator, init, binding, collection, body),
            ExprKind::Fail { arguments } => {
                let node = self.node(NodeKind::Compute, "fail", e.span)?;
                let r = self.fail_error(arguments).and_then(Err);
                self.finish(node, r)
            }
            ExprKind::Boundary {
                retries,
                body,
                error_binding,
                catch,
            } => self.boundary(e.span, *retries, body, error_binding, catch),
        }
    }
    /// Builds the error a `fail(code, message[, details])` expression raises.
    fn fail_error(&mut self, arguments: &[Expr]) -> Result<ToolError, ToolError> {
        let mut strings = Vec::new();
        for (i, a) in arguments.iter().take(2).enumerate() {
            match self.eval(a)? {
                V::String(s) => strings.push(s),
                other => {
                    return Err(lang(
                        "RL5201",
                        &format!(
                            "fail {} must be a string, got {}",
                            if i == 0 { "code" } else { "message" },
                            value_kind(&other)
                        ),
                    ));
                }
            }
        }
        let mut error = ToolError::new(
            strings.first().cloned().unwrap_or_else(|| "FAILED".into()),
            strings.get(1).cloned().unwrap_or_default(),
        );
        if let Some(details) = arguments.get(2) {
            match self.eval(details)? {
                V::Object(o) => error.details = o,
                other => {
                    return Err(lang(
                        "RL5201",
                        &format!("fail details must be an object, got {}", value_kind(&other)),
                    ));
                }
            }
        }
        Ok(error)
    }
    /// `fold acc = init for x in xs { ... }` — a sequential left fold. Each
    /// iteration binds a fresh accumulator and item; the body result becomes
    /// the next accumulator; a taken `skip` leaves the accumulator unchanged;
    /// an empty collection yields the initial value.
    fn fold_expr(
        &mut self,
        span: Span,
        accumulator: &str,
        init: &Expr,
        binding: &str,
        collection: &Expr,
        body: &Block,
    ) -> Result<V, ToolError> {
        let node = self.node(NodeKind::Loop, "fold", span)?;
        let prelude: Result<(V, V), ToolError> = (|| {
            let acc = self.eval(init)?;
            let c = self.eval(collection)?;
            Ok((acc, c))
        })();
        let (mut acc, c) = match prelude {
            Ok(x) => x,
            Err(e) => {
                self.graph.fail(node, e.to_string());
                return Err(e);
            }
        };
        let values = match c {
            V::List(x) => x,
            V::Object(o) => o
                .into_iter()
                .map(|(k, v)| {
                    V::Object(BTreeMap::from([
                        ("key".into(), V::String(k)),
                        ("value".into(), v),
                    ]))
                })
                .collect(),
            other => {
                let e = lang(
                    "RL5204",
                    &format!(
                        "cannot iterate a {} value; `fold` needs a list or an object",
                        value_kind(&other)
                    ),
                );
                self.graph.fail(node, e.to_string());
                return Err(e);
            }
        };
        for (i, value) in values.into_iter().enumerate() {
            let it = self.node(
                NodeKind::Iteration,
                iteration_label(binding, i, &value),
                body.span,
            )?;
            self.graph.contains(node, it);
            self.owners.push(it);
            self.dynamic
                .push(format!("{i}:{}", value.digest_hex().unwrap_or_default()));
            self.scopes.push(HashMap::from([
                (accumulator.to_string(), Binding::Value(acc.clone(), None)),
                (binding.to_string(), Binding::Value(value, None)),
            ]));
            let result = self.block(body);
            self.scopes.pop();
            self.dynamic.pop();
            self.owners.pop();
            match result {
                Ok(v) => {
                    self.graph.success(it, v.clone());
                    acc = v;
                }
                // A taken `skip`: the accumulator passes through unchanged.
                Err(e) if e.code == SKIP_SIGNAL => {
                    self.graph.success(it, acc.clone());
                }
                Err(e) => {
                    self.graph.fail(it, e.to_string());
                    self.graph.fail(node, e.to_string());
                    return Err(e);
                }
            }
        }
        self.finish(node, Ok(acc))
    }
    fn name(&mut self, n: &str) -> Result<V, ToolError> {
        let found = (0..self.scopes.len())
            .rev()
            .find(|&i| self.scopes[i].contains_key(n))
            .ok_or_else(|| lang("RL8101", &format!("unknown runtime name `{n}`")))?;
        let binding = self.scopes[found].remove(n).unwrap();
        match binding {
            Binding::Value(v, producer) => {
                self.last_output = producer;
                self.scopes[found].insert(n.into(), Binding::Value(v.clone(), producer));
                Ok(v)
            }
            Binding::Evaluating => Err(lang(
                "RL4104",
                &format!(
                    "binding `{n}` depends on itself; bindings are immutable, so accumulator \
                     patterns like `x = x + 1` cannot work — reduce with a fold instead: \
                     `total = fold acc = 0 for item in items {{ return acc + item }}`"
                ),
            )),
            Binding::Expr(e) => {
                // The outermost cache whose base depth covers this binding is
                // shared by the widest set of loop iterations.
                let cache = self
                    .binding_caches
                    .iter()
                    .find(|(base, _)| found < *base)
                    .map(|(_, cache)| cache.clone());
                if let Some(cache) = &cache {
                    let cached = cache.lock().unwrap().get(&(found, n.to_string())).cloned();
                    if let Some((v, producer)) = cached {
                        self.last_output = producer;
                        self.scopes[found].insert(n.into(), Binding::Value(v.clone(), producer));
                        return Ok(v);
                    }
                }
                self.scopes[found].insert(n.into(), Binding::Evaluating);
                let r = self.eval(&e);
                match &r {
                    Ok(v) => {
                        self.scopes[found]
                            .insert(n.into(), Binding::Value(v.clone(), self.last_output));
                        if let Some(cache) = &cache {
                            cache
                                .lock()
                                .unwrap()
                                .insert((found, n.to_string()), (v.clone(), self.last_output));
                        }
                    }
                    Err(_) => {
                        self.scopes[found].insert(n.into(), Binding::Expr(e));
                    }
                }
                r
            }
        }
    }
    fn call(&mut self, span: Span, callee: &Expr, args: &[Expr]) -> Result<V, ToolError> {
        let name = path(callee).ok_or_else(|| lang("RL2104", "value is not callable"))?;
        let desc = self
            .runtime
            .registry
            .get(&name)
            .ok_or_else(|| lang("RL2102", &format!("unknown tool `{name}`")))?;
        let node = self.blocked_node(NodeKind::Call, &name, span)?;
        let mut values = vec![];
        for (index, (a, expected)) in args.iter().zip(&desc.input.parameters).enumerate() {
            match self.eval(a).and_then(|v| self.convert(a.span, v, expected)) {
                Ok(v) => {
                    if let Some(producer) = self.last_output {
                        self.graph
                            .data(producer, node, "output", &format!("argument[{index}]"));
                    }
                    values.push(v)
                }
                Err(e) => {
                    self.graph.fail(node, e.to_string());
                    return Err(e);
                }
            }
        }
        if values.len() < desc.input.required_count() || values.len() > desc.input.parameters.len()
        {
            let e = lang("RL6102", "TOOL_INPUT_SCHEMA_MISMATCH");
            self.graph.fail(node, e.to_string());
            return Err(e);
        }
        if let Some(V::String(display)) = values.first() {
            let display = display.chars().take(80).collect::<String>();
            self.graph.label(node, format!("{name} · {display}"));
        }
        self.graph.dispatching(node);
        let canonical = V::List(values.clone())
            .rcve()
            .map_err(|x| lang("RL5207", &x.to_string()))?;
        let identity = format!(
            "{}\0{}\0{}\0{}\0{}",
            self.workflow,
            name,
            desc.schema_version,
            span.start,
            self.dynamic.join("/")
        );
        let op = hex::encode(Sha256::digest([identity.as_bytes(), &canonical].concat()));
        if let Some(previous_attempt) = self
            .operation_nodes
            .lock()
            .unwrap()
            .insert(op.clone(), node)
        {
            self.graph.retry_of(node, previous_attempt);
        }
        let previous = self.successful_operations.lock().unwrap().get(&op).cloned();
        if let Some(value) = previous {
            return self.finish(node, Ok(value));
        }
        let dispatch_id = {
            let mut generations = self.dispatch_generations.lock().unwrap();
            let generation = generations.entry(op.clone()).or_default();
            let dispatch_id = format!("{op}:{}", *generation);
            *generation += 1;
            dispatch_id
        };
        let ctx = ToolContext {
            node_id: self.graph.node_id(node),
            operation_id: op.clone(),
            dispatch_id,
            attempt: self.attempt,
            schema_version: desc.schema_version.clone(),
        };
        self.graph.running(node);
        let handler = self
            .runtime
            .handlers
            .get(&name)
            .ok_or_else(|| lang("RL8103", "missing tool implementation"))?;
        // Leaf-only permit: bounds active dispatches without nesting deadlock.
        let dispatches = self.dispatches.clone();
        let permit = dispatches.acquire();
        let mut r = handler(&values, &ctx).and_then(|v| {
            if desc.output.accepts(&v) {
                Ok(v)
            } else {
                Err(lang("RL6103", "TOOL_OUTPUT_SCHEMA_MISMATCH"))
            }
        });
        drop(permit);
        if let Err(error) = &mut r {
            error.retryable &= matches!(
                desc.execution,
                crate::ExecutionPolicy::Pure
                    | crate::ExecutionPolicy::Idempotent
                    | crate::ExecutionPolicy::Recoverable
            );
        }
        if let Ok(value) = &r {
            self.successful_operations
                .lock()
                .unwrap()
                .insert(op, value.clone());
        }
        self.finish(node, r)
    }
    fn binary(
        &mut self,
        span: Span,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<V, ToolError> {
        let node = self.node(
            if matches!(op, BinaryOp::And | BinaryOp::Or) {
                NodeKind::Branch
            } else {
                NodeKind::Compute
            },
            format!("{op:?}"),
            span,
        )?;
        self.binary_inner(node, op, left, right)
    }
    fn binary_inner(
        &mut self,
        node: usize,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<V, ToolError> {
        let l = self.eval(left)?;
        if let V::Boolean(b) = l {
            if op == BinaryOp::And && !b {
                return self.finish(node, Ok(V::Boolean(false)));
            }
            if op == BinaryOp::Or && b {
                return self.finish(node, Ok(V::Boolean(true)));
            }
        }
        let r = self.eval(right)?;
        let out = binary(op, l, r);
        self.finish(node, out)
    }
    fn convert(&mut self, span: Span, value: V, expected: &Schema) -> Result<V, ToolError> {
        if expected.accepts(&value) {
            return Ok(value);
        }
        let node = self.node(NodeKind::Convert, "convert", span)?;
        let result = convert_value(value, expected);
        self.finish(node, result)
    }
    fn loop_expr(
        &mut self,
        span: Span,
        binding: &str,
        collection: &Expr,
        body: &Block,
    ) -> Result<V, ToolError> {
        let concurrency = self.runtime.loop_concurrency;
        let node = self.node(
            NodeKind::Loop,
            format!("for concurrency {concurrency}"),
            span,
        )?;
        let c = self.eval(collection)?;
        let values = match c {
            V::List(x) => x,
            V::Object(o) => o
                .into_iter()
                .map(|(k, v)| {
                    V::Object(BTreeMap::from([
                        ("key".into(), V::String(k)),
                        ("value".into(), v),
                    ]))
                })
                .collect(),
            other => {
                let e = lang(
                    "RL5204",
                    &format!(
                        "cannot iterate a {} value; `for` needs a list or an object",
                        value_kind(&other)
                    ),
                );
                self.graph.fail(node, e.to_string());
                return Err(e);
            }
        };
        let values = Arc::new(values);
        // Iteration 0 runs sequentially first, remaining iterations
        // concurrently.
        let next = Arc::new(AtomicUsize::new(1));
        let results = Arc::new(Mutex::new(vec![None; values.len()]));
        let worker_count = (concurrency as usize).min(values.len().saturating_sub(1));
        // Iterations share one cache of outer bindings they force, so a
        // binding referenced only inside the body evaluates once, not once
        // per iteration (see [`BindingCache`]).
        self.binding_caches
            .push((self.scopes.len(), Arc::new(Mutex::new(HashMap::new()))));
        let run_iteration = |mut child: Evaluator<'_>, i: usize, value: V| {
            let it = match child.node(
                NodeKind::Iteration,
                iteration_label(binding, i, &value),
                body.span,
            ) {
                Ok(it) => it,
                Err(error) => return Err(error),
            };
            child.graph.contains(node, it);
            child.owners.push(it);
            child
                .dynamic
                .push(format!("{i}:{}", value.digest_hex().unwrap_or_default()));
            child.scopes.push(HashMap::from([(
                binding.into(),
                Binding::Value(value, None),
            )]));
            let result = child.block(body);
            match result {
                Ok(value) => {
                    child.graph.success(it, value.clone());
                    Ok(Some(value))
                }
                // A taken `skip`: the iteration contributes no element.
                Err(error) if error.code == SKIP_SIGNAL => {
                    child.graph.success(it, V::Null);
                    Ok(None)
                }
                Err(error) => {
                    child.graph.fail(it, error.to_string());
                    Err(error)
                }
            }
        };
        // Running the first iteration before spawning workers warms the
        // binding cache: every outer binding the body forces is published
        // before any concurrent iteration can race to re-evaluate it.
        if let Some(value) = values.first().cloned() {
            let first = run_iteration(self.clone_for_branch(), 0, value);
            results.lock().unwrap()[0] = Some(first);
        }
        // The evaluating thread is one of the loop's concurrent lanes; extra
        // worker threads come out of the run-wide budget so nested loops
        // cannot multiply threads without bound. A loop that gets no extra
        // threads simply runs sequentially on this thread.
        let desired_extra = worker_count.saturating_sub(1);
        let mut reserved = 0;
        let _ =
            self.worker_budget
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |available| {
                    reserved = available.min(desired_extra);
                    Some(available - reserved)
                });
        let templates = (0..reserved)
            .map(|_| self.clone_for_branch())
            .collect::<Vec<_>>();
        std::thread::scope(|scope| {
            for template in templates {
                let values = values.clone();
                let next = next.clone();
                let results = results.clone();
                let run_iteration = &run_iteration;
                scope.spawn(move || {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(value) = values.get(i).cloned() else {
                            break;
                        };
                        let result = run_iteration(template.clone_for_branch(), i, value);
                        results.lock().unwrap()[i] = Some(result);
                    }
                });
            }
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(value) = values.get(i).cloned() else {
                    break;
                };
                let result = run_iteration(self.clone_for_branch(), i, value);
                results.lock().unwrap()[i] = Some(result);
            }
        });
        self.worker_budget.fetch_add(reserved, Ordering::Relaxed);
        self.binding_caches.pop();
        let mut out = Vec::with_capacity(values.len());
        for result in results.lock().unwrap().iter_mut() {
            match result.take().expect("every iteration produced a result") {
                Ok(Some(value)) => out.push(value),
                Ok(None) => {}
                Err(error) => {
                    self.graph.fail(node, error.to_string());
                    return Err(error);
                }
            }
        }
        self.finish(node, Ok(V::List(out)))
    }
    fn boundary(
        &mut self,
        span: Span,
        retries: u32,
        body: &Block,
        error_binding: &str,
        catch: &Block,
    ) -> Result<V, ToolError> {
        let node = self.node(
            NodeKind::Boundary,
            format!("boundary retry {retries}"),
            span,
        )?;
        let mut last = None;
        for attempt in 0..=retries {
            self.attempt = attempt;
            self.owners.push(node);
            let r = self.block(body);
            self.owners.pop();
            match r {
                Ok(v) => {
                    self.attempt = 0;
                    return self.finish(node, Ok(v));
                }
                Err(e) => {
                    let retry = e.retryable && attempt < retries;
                    let retry_after = e.retry_after;
                    last = Some(e);
                    if !retry {
                        break;
                    }
                    // Executor-policy backoff: a pure scheduling delay before
                    // the next attempt, invisible to operation identity and
                    // the execution graph.
                    let delay = self.runtime.retry_delay(attempt + 1, retry_after);
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                }
            }
        }
        let e = last.unwrap();
        let error = error_value(&e, self.attempt);
        self.scopes.push(HashMap::from([(
            error_binding.into(),
            Binding::Value(error, None),
        )]));
        self.owners.push(node);
        let r = self.block(catch);
        self.owners.pop();
        self.scopes.pop();
        self.attempt = 0;
        self.finish(node, r)
    }
    fn block(&mut self, b: &Block) -> Result<V, ToolError> {
        let mut scope = HashMap::new();
        for s in &b.statements {
            if let crate::StmtKind::Binding { name, value } = &s.kind {
                scope.insert(name.clone(), Binding::Expr(value.clone()));
            }
        }
        self.scopes.push(scope);
        let r = self
            .guards_and_effects(&b.statements)
            .and_then(|_| self.eval(&b.result));
        self.scopes.pop();
        r
    }
    /// Runs a block's guards and implicit effect roots in statement order.
    ///
    /// `skip` statements evaluate immediately; a taken skip abandons the
    /// current loop iteration via the internal `RL4107` signal. Statements
    /// whose expressions contain effectful calls (or `fail`) are implicit
    /// roots: they evaluate whether or not the block result references them,
    /// so fire-and-forget writes are never silently dropped. Pure dead code
    /// stays lazily pruned. A failing effect fails the block like any other
    /// error.
    fn guards_and_effects(&mut self, stmts: &[crate::Stmt]) -> Result<(), ToolError> {
        for s in stmts {
            match &s.kind {
                crate::StmtKind::Skip { condition } => {
                    let taken = match condition {
                        None => true,
                        Some(c) => match self.eval(c)? {
                            V::Boolean(b) => b,
                            other => {
                                let mut e = lang(
                                    "RL5202",
                                    &format!(
                                        "skip condition evaluated to {}; conditions must be \
                                         true or false — Runlet has no truthiness, compare \
                                         explicitly (e.g. `value != null`)",
                                        value_kind(&other)
                                    ),
                                );
                                e.span = Some(c.span);
                                return Err(e);
                            }
                        },
                    };
                    if taken {
                        let mut e = lang(SKIP_SIGNAL, "skip");
                        e.span = Some(s.span);
                        return Err(e);
                    }
                }
                crate::StmtKind::Assert { condition, message } => {
                    let node = self.node(NodeKind::Branch, "assert", s.span)?;
                    let result = match self.eval(condition) {
                        Ok(V::Boolean(true)) => Ok(V::Null),
                        Ok(V::Boolean(false)) => {
                            let message = match message {
                                Some(message) => match self.eval(message)? {
                                    V::String(message) => message,
                                    other => {
                                        let mut error = lang(
                                            "RL5201",
                                            &format!(
                                                "assert message must be a string, got {}",
                                                value_kind(&other)
                                            ),
                                        );
                                        error.span = Some(message.span);
                                        return Err(error);
                                    }
                                },
                                None => "assertion failed".into(),
                            };
                            let mut error = ToolError::new("ASSERTION_FAILED", message);
                            error.span = Some(s.span);
                            Err(error)
                        }
                        Ok(other) => {
                            let mut error = lang(
                                "RL5202",
                                &format!(
                                    "assert condition evaluated to {}; conditions must be true or false",
                                    value_kind(&other)
                                ),
                            );
                            error.span = Some(condition.span);
                            Err(error)
                        }
                        Err(error) => Err(error),
                    };
                    self.finish(node, result)?;
                }
                crate::StmtKind::Binding { name, value } => {
                    if crate::analyzer::contains_effectful_call(value, &self.runtime.registry) {
                        self.name(name)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn budget(&self) -> Result<(), ToolError> {
        if self.graph.node_count() >= self.runtime.max_graph_nodes {
            return Err(lang(
                "RL4105",
                &format!(
                    "execution graph exceeded the host limit of {} nodes",
                    self.runtime.max_graph_nodes
                ),
            ));
        }
        Ok(())
    }
    fn node(&mut self, k: NodeKind, l: impl Into<String>, s: Span) -> Result<usize, ToolError> {
        self.budget()?;
        let i = self.graph.begin(k, l, s, self.attempt);
        self.graph.running(i);
        if let Some(&p) = self.owners.last() {
            self.graph.contains(p, i)
        }
        Ok(i)
    }
    fn blocked_node(
        &mut self,
        k: NodeKind,
        l: impl Into<String>,
        s: Span,
    ) -> Result<usize, ToolError> {
        self.budget()?;
        let i = self.graph.begin(k, l, s, self.attempt);
        self.graph.blocked(i);
        if let Some(&parent) = self.owners.last() {
            self.graph.contains(parent, i)
        }
        Ok(i)
    }
    fn clone_for_branch(&self) -> Evaluator<'_> {
        Evaluator {
            runtime: self.runtime,
            graph: self.graph.clone(),
            scopes: self.scopes.clone(),
            attempt: self.attempt,
            owners: self.owners.clone(),
            workflow: self.workflow.clone(),
            dynamic: self.dynamic.clone(),
            successful_operations: self.successful_operations.clone(),
            dispatch_generations: self.dispatch_generations.clone(),
            operation_nodes: self.operation_nodes.clone(),
            last_output: self.last_output,
            binding_caches: self.binding_caches.clone(),
            depth: self.depth,
            dispatches: self.dispatches.clone(),
            worker_budget: self.worker_budget.clone(),
        }
    }
    fn finish(&mut self, node: usize, r: Result<V, ToolError>) -> Result<V, ToolError> {
        match &r {
            Ok(v) => {
                self.graph.success(node, v.clone());
                self.last_output = Some(node);
            }
            Err(e) => self.graph.fail(node, e.to_string()),
        }
        r
    }
}

fn iteration_label(binding: &str, index: usize, value: &V) -> String {
    let key = match value {
        V::String(value) => Some(value.as_str()),
        V::Object(fields) => fields.get("name").and_then(|value| match value {
            V::String(value) => Some(value.as_str()),
            _ => None,
        }),
        _ => None,
    };
    key.map_or_else(
        || format!("{binding}[{index}]"),
        |key| format!("{binding}[{index}] {key}"),
    )
}

fn path(e: &Expr) -> Option<String> {
    match &e.kind {
        ExprKind::Name(n) => Some(n.clone()),
        ExprKind::Member { target, field } => Some(format!("{}.{}", path(target)?, field)),
        _ => None,
    }
}
fn lang(code: &str, msg: &str) -> ToolError {
    ToolError::new(code, msg)
}
/// Internal control signal for a taken `skip`; intercepted by the enclosing
/// `for`/`fold` iteration and never observable by programs (the parser
/// rejects `skip` outside loop bodies and inside boundaries).
pub(crate) const SKIP_SIGNAL: &str = "RL4107";
fn project(v: V, i: &V) -> Result<V, ToolError> {
    match (v, i) {
        (V::List(x), V::Integer(i)) => idx(x.len(), *i)
            .and_then(|n| x.get(n).cloned())
            .ok_or_else(|| lang("RL5205", "INDEX_OUT_OF_BOUNDS")),
        (V::String(s), V::Integer(i)) => {
            let x = s.chars().collect::<Vec<_>>();
            idx(x.len(), *i)
                .and_then(|n| x.get(n))
                .map(|c| V::String(c.to_string()))
                .ok_or_else(|| lang("RL5205", "INDEX_OUT_OF_BOUNDS"))
        }
        (V::Bytes(x), V::Integer(i)) => idx(x.len(), *i)
            .and_then(|n| x.get(n))
            .map(|x| V::Integer(*x as i64))
            .ok_or_else(|| lang("RL5205", "INDEX_OUT_OF_BOUNDS")),
        (V::Object(mut o), V::String(k)) => o
            .remove(k)
            .ok_or_else(|| lang("RL5206", &format!("KEY_NOT_FOUND: `{k}`"))),
        _ => Err(lang("RL5203", "NOT_INDEXABLE")),
    }
}
fn idx(len: usize, i: i64) -> Option<usize> {
    let n = if i < 0 { len as i64 + i } else { i };
    (n >= 0 && n < len as i64).then_some(n as usize)
}
fn unary(op: UnaryOp, v: V) -> Result<V, ToolError> {
    match (op, v) {
        (UnaryOp::Not, V::Boolean(x)) => Ok(V::Boolean(!x)),
        (UnaryOp::Negate, V::Integer(x)) => x
            .checked_neg()
            .map(V::Integer)
            .ok_or_else(|| lang("RL5101", "NUMERIC_OVERFLOW")),
        (UnaryOp::Negate, V::Number(x)) => {
            V::number(-x).map_err(|_| lang("RL5103", "NON_FINITE_NUMBER"))
        }
        (UnaryOp::Not, v) => Err(lang(
            "RL5202",
            &format!(
                "cannot apply `not` to a {} value; Runlet has no truthiness — compare \
                 explicitly first (e.g. `not (value != null)`)",
                value_kind(&v)
            ),
        )),
        (UnaryOp::Negate, v) => Err(lang(
            "RL5201",
            &format!("cannot negate a {} value", value_kind(&v)),
        )),
    }
}
fn binary(op: BinaryOp, l: V, r: V) -> Result<V, ToolError> {
    use BinaryOp::*;
    match (op, l, r) {
        (Add, V::Integer(a), V::Integer(b)) => a
            .checked_add(b)
            .map(V::Integer)
            .ok_or_else(|| lang("RL5101", "NUMERIC_OVERFLOW")),
        (Subtract, V::Integer(a), V::Integer(b)) => a
            .checked_sub(b)
            .map(V::Integer)
            .ok_or_else(|| lang("RL5101", "NUMERIC_OVERFLOW")),
        (Multiply, V::Integer(a), V::Integer(b)) => a
            .checked_mul(b)
            .map(V::Integer)
            .ok_or_else(|| lang("RL5101", "NUMERIC_OVERFLOW")),
        (Divide, _, V::Integer(0))
        | (Divide, _, V::Number(0.0))
        | (Remainder, _, V::Integer(0)) => Err(lang("RL5102", "DIVISION_BY_ZERO")),
        (Remainder, V::Integer(a), V::Integer(b)) => a
            .checked_rem(b)
            .map(V::Integer)
            .ok_or_else(|| lang("RL5101", "NUMERIC_OVERFLOW")),
        (Add, V::String(a), V::String(b)) => Ok(V::String(a + &b)),
        (Add, V::String(a), b) => Ok(V::String(a + &format_value(&b)?)),
        (Add, a, V::String(b)) => Ok(V::String(format_value(&a)? + &b)),
        (Add, V::List(mut a), V::List(b)) => {
            a.extend(b);
            Ok(V::List(a))
        }
        // Shallow, right-biased merge: keys from the right side overwrite.
        (Add, V::Object(mut a), V::Object(b)) => {
            a.extend(b);
            Ok(V::Object(a))
        }
        (Equal, V::Integer(a), V::Number(b)) | (Equal, V::Number(b), V::Integer(a)) => {
            Ok(V::Boolean(exact(a)? == b))
        }
        (NotEqual, V::Integer(a), V::Number(b)) | (NotEqual, V::Number(b), V::Integer(a)) => {
            Ok(V::Boolean(exact(a)? != b))
        }
        (Equal, a, b) => Ok(V::Boolean(a == b)),
        (NotEqual, a, b) => Ok(V::Boolean(a != b)),
        (And, V::Boolean(a), V::Boolean(b)) => Ok(V::Boolean(a && b)),
        (Or, V::Boolean(a), V::Boolean(b)) => Ok(V::Boolean(a || b)),
        (And | Or, a, b) => Err(lang(
            "RL5202",
            &format!(
                "`and`/`or` need boolean operands, got {} and {}; Runlet has no truthiness — \
                 compare explicitly (e.g. `value != null`, `text != \"\"`)",
                value_kind(&a),
                value_kind(&b)
            ),
        )),
        (In, a, V::List(b)) => Ok(V::Boolean(b.contains(&a))),
        (In, V::String(a), V::String(b)) => Ok(V::Boolean(b.contains(&a))),
        (In, V::String(a), V::Object(b)) => Ok(V::Boolean(b.contains_key(&a))),
        (op, a, b)
            if matches!(
                op,
                Add | Subtract | Multiply | Divide | Less | LessEqual | Greater | GreaterEqual
            ) =>
        {
            numeric_binary(op, a, b)
        }
        (op, a, b) => Err(lang(
            "RL5201",
            &format!(
                "operator `{op:?}` cannot combine {} and {} values",
                value_kind(&a),
                value_kind(&b)
            ),
        )),
    }
}
fn numeric_binary(op: BinaryOp, a: V, b: V) -> Result<V, ToolError> {
    let (a, b) = match (a, b) {
        (V::Integer(a), V::Integer(b)) => (a as f64, b as f64),
        (V::Integer(a), V::Number(b)) => (exact(a)?, b),
        (V::Number(a), V::Integer(b)) => (a, exact(b)?),
        (V::Number(a), V::Number(b)) => (a, b),
        (V::String(a), V::String(b)) => {
            return Ok(V::Boolean(match op {
                BinaryOp::Less => a < b,
                BinaryOp::LessEqual => a <= b,
                BinaryOp::Greater => a > b,
                BinaryOp::GreaterEqual => a >= b,
                _ => false,
            }));
        }
        _ => return Err(lang("RL5201", "INVALID_NUMERIC_OPERANDS")),
    };
    use BinaryOp::*;
    match op {
        Add => V::number(a + b),
        Subtract => V::number(a - b),
        Multiply => V::number(a * b),
        Divide => V::number(a / b),
        Less => return Ok(V::Boolean(a < b)),
        LessEqual => return Ok(V::Boolean(a <= b)),
        Greater => return Ok(V::Boolean(a > b)),
        GreaterEqual => return Ok(V::Boolean(a >= b)),
        _ => unreachable!(),
    }
    .map_err(|_| lang("RL5103", "NON_FINITE_NUMBER"))
}
fn exact(x: i64) -> Result<f64, ToolError> {
    let f = x as f64;
    if f as i64 == x {
        Ok(f)
    } else {
        Err(lang("RL5208", "LOSSY_NUMERIC_WIDENING"))
    }
}
fn format_value(v: &V) -> Result<String, ToolError> {
    match v {
        V::Null | V::Bytes(_) => Err(lang("RL5207", "NOT_JSON_REPRESENTABLE")),
        V::String(s) => Ok(s.clone()),
        V::Integer(x) => Ok(x.to_string()),
        V::Boolean(x) => Ok(x.to_string()),
        V::Number(_) | V::List(_) | V::Object(_) => v
            .presentation_json()
            .map_err(|e| lang("RL5207", &e.to_string())),
    }
}
/// A computed object key as a key string: strings pass through, scalar
/// values convert to their canonical text form.
fn computed_key(v: V) -> Result<String, ToolError> {
    match v {
        V::String(s) => Ok(s),
        V::Integer(_) | V::Number(_) | V::Boolean(_) => format_value(&v),
        other => Err(lang(
            "RL5209",
            &format!(
                "computed property keys must be strings or scalar values, got {}",
                value_kind(&other)
            ),
        )),
    }
}
fn convert_value(value: V, expected: &Schema) -> Result<V, ToolError> {
    if expected.accepts(&value) {
        return Ok(value);
    }
    match (value, expected) {
        (V::Integer(x), Schema::Number { .. }) => {
            V::number(exact(x)?).map_err(|_| lang("RL5103", "NON_FINITE_NUMBER"))
        }
        (
            v @ (V::Integer(_) | V::Number(_) | V::Boolean(_) | V::List(_) | V::Object(_)),
            Schema::String { .. },
        ) => Ok(V::String(format_value(&v)?)),
        (V::List(values), Schema::List { items, .. }) => values
            .into_iter()
            .map(|v| convert_value(v, items))
            .collect::<Result<Vec<_>, _>>()
            .map(V::List),
        (
            V::Object(values),
            Schema::Object {
                properties,
                required,
                additional,
            },
        ) => {
            let missing = required
                .iter()
                .filter(|key| !values.contains_key(*key))
                .cloned()
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                return Err(lang(
                    "RL5208",
                    &format!(
                        "missing required object propert{}: {}",
                        if missing.len() == 1 { "y" } else { "ies" },
                        missing.join(", ")
                    ),
                ));
            }
            let mut out = BTreeMap::new();
            for (key, value) in values {
                match properties.get(&key) {
                    Some(property) => {
                        // `key: value if cond else null` is the only way to
                        // express a conditional property, so null for an
                        // optional property that is not itself nullable means
                        // "omit the key", matching JSON null-as-absent.
                        if matches!(value, V::Null)
                            && !required.contains(&key)
                            && !property.schema.accepts(&V::Null)
                        {
                            continue;
                        }
                        out.insert(key, convert_value(value, &property.schema)?);
                    }
                    None if *additional => {
                        out.insert(key, value);
                    }
                    None => {
                        return Err(lang(
                            "RL5208",
                            &format!("unexpected object property `{key}`"),
                        ));
                    }
                }
            }
            Ok(V::Object(out))
        }
        (value, Schema::Union { variants, .. }) => {
            let converted = variants
                .iter()
                .filter_map(|schema| convert_value(value.clone(), schema).ok())
                .collect::<Vec<_>>();
            if converted.len() == 1 {
                Ok(converted.into_iter().next().unwrap())
            } else {
                Err(lang("RL5208", "ambiguous or invalid union conversion"))
            }
        }
        (value, expected) => Err(lang(
            "RL5208",
            &format!(
                "{} value cannot be safely converted to the expected {} schema",
                value_kind(&value),
                schema_kind(expected)
            ),
        )),
    }
}
fn value_kind(v: &V) -> &'static str {
    match v {
        V::Null => "null",
        V::Boolean(_) => "boolean",
        V::Integer(_) => "integer",
        V::Number(_) => "number",
        V::String(_) => "string",
        V::Bytes(_) => "bytes",
        V::List(_) => "list",
        V::Object(_) => "object",
    }
}
fn schema_kind(s: &Schema) -> &'static str {
    match s {
        Schema::Any => "any",
        Schema::Null => "null",
        Schema::Boolean => "boolean",
        Schema::Integer { .. } => "integer",
        Schema::Number { .. } => "number",
        Schema::String { .. } => "string",
        Schema::Bytes => "bytes",
        Schema::List { .. } => "list",
        Schema::Map { .. } => "map",
        Schema::Object { .. } => "object",
        Schema::Union { .. } => "union",
        Schema::Never => "never",
    }
}
fn error_value(e: &ToolError, attempt: u32) -> V {
    let mut o = e.details.clone();
    for (k, v) in [
        ("code", V::String(e.code.clone())),
        ("message", V::String(e.message.clone())),
        ("retryable", V::Boolean(e.retryable)),
        ("node_id", V::String(String::new())),
        ("attempt", V::Integer(attempt as i64)),
        ("uncertain", V::Boolean(e.uncertain)),
        (
            // Where the failure happened, as source byte offsets — so a
            // caught-and-reported error still points at its expression.
            "span",
            match e.span {
                Some(span) => V::Object(BTreeMap::from([
                    ("start".to_string(), V::Integer(span.start as i64)),
                    ("end".to_string(), V::Integer(span.end as i64)),
                ])),
                None => V::Null,
            },
        ),
    ] {
        o.insert(k.into(), v);
    }
    V::Object(o)
}
