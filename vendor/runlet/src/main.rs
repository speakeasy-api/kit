use runlet::{
    CallSchema, CanonicalValue, Diagnostic, EdgeKind, ExecutionPolicy, GraphChange, GraphEvent,
    Node, NodeKind, NodeState, Runtime, Schema, Severity, ToolDescriptor, ToolError, ToolRegistry,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, IsTerminal};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, fs, process::ExitCode, thread};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => ExitCode::from(code),
    }
}

fn run() -> Result<(), u8> {
    let mut args = env::args_os();
    let executable = args.next().unwrap_or_default();
    let Some(first) = args.next() else {
        usage(&executable.to_string_lossy());
        return Err(2);
    };
    let (live_graph, path) = if first == "graph" {
        let Some(path) = args.next() else {
            usage(&executable.to_string_lossy());
            return Err(2);
        };
        (true, path)
    } else {
        (false, first)
    };
    if args.next().is_some() {
        eprintln!("error: expected exactly one .rnlt file");
        usage(&executable.to_string_lossy());
        return Err(2);
    }

    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("error: cannot read `{}`: {error}", path.to_string_lossy());
            return Err(2);
        }
    };

    let runtime = demo_runtime();
    let compiled = match runtime.compile(&source) {
        Ok(compiled) => compiled,
        Err(diagnostics) => {
            for diagnostic in diagnostics {
                render_diagnostic(&path.to_string_lossy(), &source, &diagnostic);
            }
            return Err(1);
        }
    };

    for diagnostic in &compiled.diagnostics {
        render_diagnostic(&path.to_string_lossy(), &source, diagnostic);
    }

    let execution = match if live_graph {
        let view = Arc::new(Mutex::new(LiveView::new()));
        runtime.run_observed(&compiled, move |event| {
            view.lock().unwrap().apply(event);
        })
    } else {
        runtime.run(&compiled)
    } {
        Ok(execution) => execution,
        Err(error) => {
            eprintln!("{}: runtime error: {error}", path.to_string_lossy());
            return Err(1);
        }
    };
    match execution.value.presentation_json() {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("{}: cannot display result: {error}", path.to_string_lossy());
            return Err(1);
        }
    }
    Ok(())
}

fn usage(executable: &str) {
    eprintln!("usage: {executable} [graph] <program.rnlt>");
}

fn demo_runtime() -> Runtime {
    let mut registry = ToolRegistry::new();
    registry
        .register(ToolDescriptor {
            name: "demo.task".into(),
            summary: "Sleep to simulate work, then return a stage envelope".into(),
            input: CallSchema::positional(vec![Schema::string(), Schema::INTEGER, Schema::Any]),
            output: Schema::Any,
            execution: ExecutionPolicy::Pure,
            schema_version: "1".into(),
        })
        .unwrap();
    registry
        .register(ToolDescriptor {
            name: "demo.unstable".into(),
            summary: "Simulate retryable work that fails for a configured number of attempts"
                .into(),
            input: CallSchema::positional(vec![
                Schema::string(),
                Schema::INTEGER,
                Schema::INTEGER,
                Schema::Any,
            ]),
            output: Schema::Any,
            execution: ExecutionPolicy::Pure,
            schema_version: "1".into(),
        })
        .unwrap();
    Runtime::builder()
        .registry(registry)
        .with_prelude()
        .tool("demo.task", |args, _| {
            let CanonicalValue::String(label) = &args[0] else {
                unreachable!()
            };
            let CanonicalValue::Integer(milliseconds) = args[1] else {
                unreachable!()
            };
            let milliseconds = milliseconds.clamp(0, 10_000) as u64;
            thread::sleep(Duration::from_millis(milliseconds));
            Ok(CanonicalValue::Object(BTreeMap::from([
                ("stage".into(), CanonicalValue::String(label.clone())),
                ("input".into(), args[2].clone()),
                (
                    "duration_ms".into(),
                    CanonicalValue::Integer(milliseconds as i64),
                ),
            ])))
        })
        .tool("demo.unstable", |args, context| {
            let CanonicalValue::String(label) = &args[0] else {
                unreachable!()
            };
            let CanonicalValue::Integer(milliseconds) = args[1] else {
                unreachable!()
            };
            let CanonicalValue::Integer(failures_before_success) = args[2] else {
                unreachable!()
            };
            let milliseconds = milliseconds.clamp(0, 10_000) as u64;
            thread::sleep(Duration::from_millis(milliseconds));
            if i64::from(context.attempt) < failures_before_success {
                return Err(ToolError::new(
                    "DEMO_TRANSIENT",
                    format!("{label} failed on attempt {}", context.attempt + 1),
                )
                .retryable(true));
            }
            Ok(CanonicalValue::Object(BTreeMap::from([
                ("stage".into(), CanonicalValue::String(label.clone())),
                ("input".into(), args[3].clone()),
                (
                    "attempts_used".into(),
                    CanonicalValue::Integer(i64::from(context.attempt) + 1),
                ),
            ])))
        })
        .build()
        .expect("demo runtime is valid")
}

struct LiveView {
    started: Instant,
    nodes: HashMap<String, Node>,
    parents: HashMap<String, String>,
    dependencies: Vec<(String, String)>,
    node_started: HashMap<String, Instant>,
    running_since: HashMap<String, Instant>,
    recent: VecDeque<String>,
    last_render: Instant,
    terminal: bool,
}

impl LiveView {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            nodes: HashMap::new(),
            parents: HashMap::new(),
            dependencies: Vec::new(),
            node_started: HashMap::new(),
            running_since: HashMap::new(),
            recent: VecDeque::new(),
            last_render: now,
            terminal: io::stderr().is_terminal(),
        }
    }

    fn apply(&mut self, event: &GraphEvent) {
        let mut important = false;
        match &event.change {
            GraphChange::NodeAdded(node) => {
                self.node_started.insert(node.id.clone(), Instant::now());
                self.nodes.insert(node.id.clone(), node.clone());
            }
            GraphChange::NodeUpdated(node) => {
                let previous = self.nodes.get(&node.id).map(|previous| previous.state);
                if node.state == NodeState::Running && previous != Some(NodeState::Running) {
                    self.running_since.insert(node.id.clone(), Instant::now());
                }
                if node.kind == NodeKind::Call {
                    match node.state {
                        NodeState::Dispatching
                            if node.attempt > 0
                                && previous == Some(NodeState::Blocked)
                                && !node.label.contains("FALLBACK") =>
                        {
                            self.note(format!(
                                "↻ RETRY attempt {}  {}  [{}]",
                                node.attempt + 1,
                                node.label,
                                self.scope(&node.id)
                            ));
                            important = true;
                        }
                        NodeState::Failed => {
                            self.note(format!(
                                "! FAILED attempt {}  {}  [{}]",
                                node.attempt + 1,
                                node.label,
                                self.scope(&node.id)
                            ));
                            important = true;
                        }
                        NodeState::Succeeded if node.label.contains("FALLBACK") => {
                            self.note(format!(
                                "⇢ CATCH used fallback  {}  [{}]",
                                node.label,
                                self.scope(&node.id)
                            ));
                            important = true;
                        }
                        NodeState::Succeeded if node.attempt > 0 => {
                            self.note(format!(
                                "✓ RECOVERED attempt {}  {}  [{}]",
                                node.attempt + 1,
                                node.label,
                                self.scope(&node.id)
                            ));
                            important = true;
                        }
                        NodeState::Running => important = true,
                        _ => {}
                    }
                }
                self.nodes.insert(node.id.clone(), node.clone());
            }
            GraphChange::EdgeAdded(edge) => {
                if edge.kind == EdgeKind::Contains {
                    self.parents.insert(edge.to.clone(), edge.from.clone());
                } else if matches!(edge.kind, EdgeKind::Data { .. }) {
                    self.dependencies.push((edge.from.clone(), edge.to.clone()));
                }
            }
        }
        if self.terminal && (important || self.last_render.elapsed() >= Duration::from_millis(120))
        {
            self.render(event.sequence);
            self.last_render = Instant::now();
        } else if let GraphChange::NodeUpdated(node) = &event.change {
            eprintln!(
                "[{:>6}ms] {:<10} {:<12} {}",
                self.started.elapsed().as_millis(),
                state_name(node.state),
                format!("{:?}", node.kind).to_lowercase(),
                node.label
            );
        }
    }

    fn render(&self, sequence: u64) {
        let running_calls = self
            .nodes
            .values()
            .filter(|node| {
                node.kind == NodeKind::Call
                    && matches!(node.state, NodeState::Running | NodeState::Dispatching)
            })
            .count();
        let completed_calls = self
            .nodes
            .values()
            .filter(|node| node.kind == NodeKind::Call && node.state == NodeState::Succeeded)
            .count();
        let failed_calls = self
            .nodes
            .values()
            .filter(|node| node.kind == NodeKind::Call && node.state == NodeState::Failed)
            .count();
        let blocked_calls = self
            .nodes
            .values()
            .filter(|node| node.kind == NodeKind::Call && node.state == NodeState::Blocked)
            .count();
        eprint!(
            "\x1b[2J\x1b[HRunlet execution graph  {:>5.1}s  event #{sequence}\n\
             tools: {running_calls} running  {blocked_calls} waiting  {completed_calls} succeeded  {failed_calls} failed\n",
            self.started.elapsed().as_secs_f32(),
        );

        eprintln!("\nACTIVE TOOL CALLS");
        let mut active = self
            .nodes
            .values()
            .filter(|node| node.kind == NodeKind::Call && node.state == NodeState::Running)
            .collect::<Vec<_>>();
        active.sort_by_key(|node| &node.id);
        if active.is_empty() {
            eprintln!("  (resolving dependencies)");
        }
        for node in active.iter().take(12) {
            let elapsed = self
                .running_since
                .get(&node.id)
                .map_or(0.0, |at| at.elapsed().as_secs_f32());
            eprintln!(
                "  ● {:>4.1}s  {:<44}  attempt {}  {}",
                elapsed,
                node.label,
                node.attempt + 1,
                self.scope(&node.id)
            );
        }

        eprintln!("\nBOUNDARIES / RETRIES");
        let boundaries = self
            .nodes
            .values()
            .filter(|node| node.kind == NodeKind::Boundary)
            .collect::<Vec<_>>();
        if boundaries.is_empty() {
            eprintln!("  (no boundaries materialized yet)");
        }
        for boundary in boundaries.iter().rev().take(8).rev() {
            let attempt = self.max_descendant_attempt(&boundary.id) + 1;
            eprintln!(
                "  {} {:<10} attempt {attempt}/3  {}",
                state_icon(boundary.state),
                state_name(boundary.state),
                self.scope(&boundary.id)
            );
        }

        eprintln!("\nRECENT RECOVERY EVENTS");
        if self.recent.is_empty() {
            eprintln!("  (waiting for the first retry or failure)");
        }
        for message in self.recent.iter().rev().take(6).rev() {
            eprintln!("  {message}");
        }

        if !self.dependencies.is_empty() {
            eprintln!("\nLATEST DATA FLOW");
            for (from, to) in self.dependencies.iter().rev().take(4).rev() {
                let from = self.nodes.get(from).map_or("?", |node| node.label.as_str());
                let to = self.nodes.get(to).map_or("?", |node| node.label.as_str());
                eprintln!("  {from}  →  {to}");
            }
        }
    }

    fn scope(&self, id: &str) -> String {
        let mut labels = Vec::new();
        let mut current = id;
        while let Some(parent) = self.parents.get(current) {
            if let Some(node) = self.nodes.get(parent) {
                if node.kind == NodeKind::Iteration {
                    labels.push(node.label.clone());
                }
            }
            current = parent;
        }
        labels.reverse();
        if labels.is_empty() {
            "global".into()
        } else {
            labels.join(" › ")
        }
    }

    fn max_descendant_attempt(&self, boundary_id: &str) -> u32 {
        self.nodes
            .values()
            .filter(|node| self.has_ancestor(&node.id, boundary_id))
            .map(|node| node.attempt)
            .max()
            .unwrap_or(0)
    }

    fn has_ancestor(&self, id: &str, ancestor: &str) -> bool {
        let mut current = id;
        while let Some(parent) = self.parents.get(current) {
            if parent == ancestor {
                return true;
            }
            current = parent;
        }
        false
    }

    fn note(&mut self, message: String) {
        self.recent.push_back(message);
        while self.recent.len() > 20 {
            self.recent.pop_front();
        }
    }
}

fn state_name(state: NodeState) -> &'static str {
    match state {
        NodeState::Planned => "planned",
        NodeState::Blocked => "blocked",
        NodeState::Ready => "ready",
        NodeState::Dispatching => "dispatch",
        NodeState::Running => "running",
        NodeState::Succeeded => "succeeded",
        NodeState::Failed => "failed",
        NodeState::Cancelling => "cancelling",
        NodeState::Cancelled => "cancelled",
        NodeState::Pruned => "pruned",
    }
}

fn state_icon(state: NodeState) -> &'static str {
    match state {
        NodeState::Succeeded => "✓",
        NodeState::Failed | NodeState::Cancelled => "×",
        NodeState::Running | NodeState::Dispatching => "●",
        _ => "○",
    }
}

fn render_diagnostic(path: &str, source: &str, diagnostic: &Diagnostic) {
    let (line, column, line_text) = source_location(source, diagnostic.primary_span.start);
    let level = match diagnostic.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    eprintln!(
        "{path}:{line}:{column}: {level}[{}]: {}",
        diagnostic.code, diagnostic.title
    );
    eprintln!("  {line_text}");
    let width = diagnostic
        .primary_span
        .end
        .saturating_sub(diagnostic.primary_span.start)
        .max(1);
    eprintln!("  {}{}", " ".repeat(column - 1), "^".repeat(width));
    eprintln!("  {}", diagnostic.message);
    if let Some(fix) = diagnostic.fixes.first() {
        eprintln!("  help: {}", fix.message);
    } else if !diagnostic.candidates.is_empty() {
        eprintln!("  candidates: {}", diagnostic.candidates.join(", "));
    }
}

fn source_location(source: &str, offset: usize) -> (usize, usize, &str) {
    let safe_offset = offset.min(source.len());
    let line_start = source[..safe_offset].rfind('\n').map_or(0, |at| at + 1);
    let line_end = source[safe_offset..]
        .find('\n')
        .map_or(source.len(), |relative| safe_offset + relative);
    let line = source[..line_start]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    let column = source[line_start..safe_offset].chars().count() + 1;
    (line, column, &source[line_start..line_end])
}

#[cfg(test)]
mod tests {
    use super::source_location;

    #[test]
    fn reports_unicode_columns() {
        assert_eq!(source_location("éx\nnext", 2), (1, 2, "éx"));
        assert_eq!(source_location("éx\nnext", 4), (2, 1, "next"));
    }
}
