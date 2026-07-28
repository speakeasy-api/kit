//! Runlet execution backend (experimental, feature `runlet`).
//!
//! Executes compose scripts as [Runlet](https://github.com/danielkov/runlet)
//! programs. Tool calls become graph nodes that runlet dispatches concurrently
//! through its threaded executor; each dispatch bridges back onto the tokio
//! runtime via [`tokio::runtime::Handle::block_on`], so the whole program runs
//! inside `spawn_blocking`.
//!
//! Replay after an approval interrupt keys on runlet's content-addressed
//! `operation_id` rather than call order, so completed children replay
//! correctly regardless of how the concurrent executor scheduled them.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use agentkit_tools_core::{ToolError, ToolInterruption, ToolName, ToolSpec};
use async_trait::async_trait;
use runlet::{
    CallSchema, CanonicalValue, Diagnostic, ExecutionPolicy, Property, Runtime, Schema,
    ToolDescriptor, ToolError as RunletToolError, ToolRegistry as RunletRegistry,
};
use serde_json::{Map, Value};

use crate::{
    BackendRun, CallKey, ComposeBackend, ComposeOutcome, DispatchError, render_catalog_shapes,
};

/// Loop concurrency defaults for compose runs. Each active iteration pins one
/// OS thread while its tool call blocks on the async executor, so these are
/// deliberately far below runlet's own defaults.
const LOOP_CONCURRENCY: u32 = 8;
/// Concurrently-active child dispatches per compose run. Additional
/// dispatches block until a permit frees (backpressure, not failure).
const DISPATCH_LIMIT: usize = 16;
/// Backstop on execution graph size; legitimate compose programs sit orders
/// of magnitude below this.
const GRAPH_NODE_LIMIT: usize = 50_000;

const SCHEMA_VERSION: &str = "1";

/// Rules-first language primer (`COMPOSE_RUNLET_PRIMER=rules`): compact rule
/// list with inline micro-examples plus one worked example. Ties the exemplar
/// primer at sonnet tier but loses badly at haiku tier (statement-form and
/// missing-return storms), so it is no longer the default.
const RULES_PRIMER: &str = "Run a Runlet program that composes available tools. Prefer this tool whenever a \
             task takes more than two tool calls: iterating over list results, fetching details \
             per item, filtering or aggregating tool output, or chaining reads into writes. The \
             whole program executes in a single round-trip and only its returned value enters \
             the conversation. Independent tool calls run CONCURRENTLY: any two calls without a \
             data dependency between them execute in parallel automatically.\n\n\
             Runlet is not Lua/Python/JavaScript. Complete rules:\n\
             - `name = expression` creates an immutable binding; the program and every block \
             end with exactly one `return expression`.\n\
             - Call tools like functions with one object argument: `r = get_item({ id: 4 })`. \
             Results are plain values; access fields with `r.field` or `r.items[0]`. There is \
             no await: using a result creates the dependency.\n\
             - Objects `{ key: value }`, lists `[a, b]`, strings, integers, floats, booleans, \
             null. String concat with `+`; `a + b` on two objects merges them shallowly (right \
             side wins). `{ [expr]: value }` computes a property key (scalars stringify: \
             `{ [u.id]: u }` keys by \"42\"). Comparisons, `and`/`or`/`not`, and `x in xs` \
             (list membership, substring, object key).\n\
             - Conditionals are expressions: `value if condition else alternative`; `else` \
             is optional and defaults to null. For larger branches use the block form — \
             `x = if cond { ... return a } else { ... return b }` — every branch block ends \
             with `return`, and only the selected branch runs. Conditions must be booleans — \
             there is NO truthiness, so write `x != null`, `s != \"\"`, `xs != []`. To \
             include an object property conditionally, use `key: value if cond` — a null \
             value omits the key from optional properties.\n\
             - Concurrent loops: `out = for item in items { ... return shaped }` returns an \
             ordered list. Every item is processed; the host bounds active iterations. \
             Iterating an object yields `{ key, value }` pairs. Filter with \
             `skip`: `skip if item.status != \"open\"` drops the element and moves on.\n\
             - Aggregation: `fold acc = init for x in xs { ... return next_acc }` is a \
             sequential reduce — the body's return becomes the next accumulator: \
             `total = fold acc = 0 for o in orders { return acc + o.amount }`. `skip if cond` \
             keeps the accumulator unchanged. Count: `fold n = 0 for x in xs { return n + 1 }`. \
             Group or index by key with computed keys: \
             `by_id = fold acc = {} for u in users { return acc + { [u.id]: u } }`; for \
             grouping, guard the first hit: `acc + { [c.team]: (acc[c.team] if c.team in acc \
             else []) + [c] }`. \
             Use fold ONLY when iterations depend on each other (sums, cursors, chained \
             writes); independent per-item work belongs in `for`, which runs concurrently.\n\
             - Error handling: `boundary retry 2 { ... return v } catch err { return fallback }` \
             retries retryable failures and turns lasting failure into the fallback value \
             (`err.code`, `err.message`, `err.attempt`). Raise your own with \
             `fail(\"NO_MATCH\", \"explanation\")` — e.g. `x = items[0] if items != [] else \
             fail(\"EMPTY\", \"expected results\")` or a guard `g = fail(\"BAD\", \"...\") if \
             invalid`.\n\
             - Validate an invariant eagerly with `assert(condition, \"message\")`; a false \
             assertion fails the program. Assert inside Runlet instead of returning raw \
             intermediate values for model-side checking.\n\
             - No functions, imports, mutation, recursion, while loops, or method calls. \
             Bindings are immutable — `total = total + x` cannot work; use fold.\n\
             - Reads are lazy, writes always run: pure work executes only if the returned \
             value needs it, but any statement that calls a tool with side effects (writes, \
             updates, sends) runs when its block runs, even if you never use its result. \
             Fire-and-forget is fine: `r = update_contact({ id: c.id, phone: fixed }) if \
             needs_fix` inside a loop performs the update without returning `r`.\n\
             - Pure intrinsics (call like tools; [x] marks optional args): \
             text.length/lower/upper/trim(s), text.starts_with/ends_with(s, x), \
             text.slice(s, start[, end]), text.split(s, sep), text.join(strings, sep), \
             text.replace(s, from, to). \
             regex.test(s, pattern), regex.find_all(s, pattern), regex.captures(s, pattern) \
             -> { full, groups, names } or null, regex.replace(s, pattern, repl) with \
             $1/$name references, regex.split(s, pattern) — Rust regex syntax, NO lookahead \
             (?=...). \
             list.length(xs), list.sort(xs), list.sort_by(xs, \"a.b\"[, \"desc\"]), \
             list.slice(xs, start[, end]), list.range(start, end[, step]) (end exclusive; \
             step defaults to 1, negative counts down). \
             json.parse(s), json.encode(v). number.round/floor/ceil(x), number.parse(s). \
             time.parse(\"2026-07-12T09:30:00Z\") -> epoch ms, time.format(ms) -> RFC 3339; \
             time math is plain integer ms (86400000 per day). \
             Substring check is the `in` operator (`\"x\" in s`) — there is no text.contains.\n\
             - The global `input` holds the JSON value passed alongside the program.\n\n\
             Example — fetch and filter concurrently, aggregate with fold:\n\
             listing = list_items({ page: 1 })\n\
             open_items = for item in listing.items {\n\
                 detail = get_item({ id: item.id })\n\
                 skip if detail.status != \"open\"\n\
                 return { id: item.id, amount: detail.amount }\n\
             }\n\
             total = fold acc = 0 for o in open_items { return acc + o.amount }\n\
             return { open: open_items, total: total }";

/// Annotated-exemplar primer (default): one full program exercising the
/// whole grammar, constraints stated as comments at the site where each
/// temptation arises. Weakly dominant over RULES_PRIMER in benchmarks —
/// equal at sonnet tier, roughly 4x fewer syntax rejections and half the
/// cost at haiku tier. The program is validated against a mock registry;
/// keep it compiling if you edit it.
const EXEMPLAR_PRIMER: &str = r#"Run a Runlet program that composes available tools. Prefer this tool whenever a task takes more than two tool calls: iterating over list results, fetching details per item, filtering or aggregating tool output, or chaining reads into writes. The whole program executes in a single round-trip and only its returned value enters the conversation. Independent tool calls run CONCURRENTLY: any two calls without a data dependency between them execute in parallel automatically.

Runlet is not Lua/Python/JavaScript. The annotated program below exercises the ENTIRE language; every construct and every rule you may rely on appears here, with its constraint in the comments:

# A program is immutable bindings ending in one `return`. No functions, imports,
# methods, while loops, mutation, or early returns exist. Tools take ONE object argument.
first = boundary retry 2 {                            # retry transient failures where they occur
    return list_records({ page: 1 })
} catch err {
    return fail("LIST_FAILED", err.code + ": " + err.message)
}
remaining = for page in list.range(2, first.total_pages + 1) {
    result = boundary retry 2 { return list_records({ page }) } catch err {
        return fail("LIST_FAILED", err.code + ": " + err.message)
    }
    return result.items
}
listing = fold acc = first.items for page in remaining { return acc + page }
config = json.parse(input.settings)                   # `input` is the JSON value submitted with the program

# These two bindings share no data, so their calls run IN PARALLEL — there is
# no await; referencing a result creates the dependency.

shaped = for record in listing {                      # concurrent loop; the host bounds active iterations;
                                                      # every item runs and results preserve input order
    detail = boundary retry 2 {                       # ANY remote call can fail transiently (503s, rate limits);
        return get_record({ id: record.id })          # wrap reads in `boundary retry` so one flaky call cannot
    } catch err {                                     # kill the whole program — iterations still run concurrently
        return fail("GET_FAILED", err.code + ": " + err.message)
    }
    skip if detail.status == "archived"               # `skip` drops this element. Conditions must be BOOLEAN —
                                                      # no truthiness: write x != null, s != "", xs != []
    r = fix_record({ id: record.id }) if detail.broken  # postfix conditional; `else` optional (defaults to null).
                                                      # Writes ALWAYS run when their statement runs, even if `r`
                                                      # is never used; pure reads run only if the result needs them.
    return {                                          # every block ends with exactly one `return`
        id: record.id,
        amount: detail.amount,
        flag: detail.note if "urgent" in detail.note  # `in`: substring, list membership, object key.
    }                                                 # null values omit the key from optional tool inputs
}

total = fold acc = 0 for row in shaped {              # fold is THE way to aggregate: sequential reduce,
    return acc + row.amount                           # the body's return becomes the next accumulator;
}                                                     # `total = total + x` is an error — bindings are immutable.
assert(total >= 0, "total cannot be negative")        # eager invariant check; no need to return checked rows
by_id = fold acc = {} for row in shaped {
    return acc + { [row.id]: row }                    # computed key { [expr]: v } — scalars stringify;
}                                                     # object + object merges shallowly, right side wins.
# grouping idiom: acc + { [row.kind]: (acc[row.kind] if row.kind in acc else []) + [row] }

label = if total >= config.threshold {                # block-bodied if is an expression; every branch
    audit = log_event({ kind: "high", total })        # is a block ending in `return`; only the selected
    return "high"                                     # branch runs (including its writes).
} else if total > 0 {
    return "normal"
} else {
    return "empty"
}

first_row = shaped[0] if shaped != [] and total > 0 else fail("EMPTY", "no rows")
                                                      # comparisons, `and`/`or`/`not` — operands must be boolean
summary = text.upper(by_id[first_row.id].id)          # fail(code, message) raises a catchable error
return { total, label, summary }                      # return only requested summaries, not source rows;
                                                      # shorthand: { total } means { total: total }

Iterating an object yields { key, value } pairs. Use fold ONLY when iterations depend on each other (sums, cursors, chained writes); independent per-item work belongs in `for`, which runs concurrently.

Catalog return shapes use type notation: `string[]` is a bare list — iterate or index it directly, it has NO `.items` property; `field?` may be absent.

Keep reads, transforms, writes, checks, and final submission in one program. Do not return source records for model-side planning or re-read successful writes. A nested final submission call satisfies the requirement and avoids another model round-trip.

Pure intrinsics (call like tools; [x] marks optional args): text.length/lower/upper/trim(s), text.starts_with/ends_with(s, x), text.slice(s, start[, end]), text.split(s, sep), text.join(strings, sep), text.replace(s, from, to). regex.test(s, pattern), regex.find_all(s, pattern), regex.captures(s, pattern) -> { full, groups, names } or null, regex.replace(s, pattern, repl) with $1/$name references, regex.split(s, pattern) - Rust regex syntax, NO lookahead (?=...). list.length(xs), list.sort(xs), list.sort_by(xs, "a.b"[, "desc"]), list.slice(xs, start[, end]), list.range(start, end[, step]) (end exclusive; step defaults to 1, negative counts down). json.parse(s), json.encode(v). number.round/floor/ceil(x), number.parse(s). time.parse("2026-07-12T09:30:00Z") -> epoch ms, time.format(ms) -> RFC 3339; time math is plain integer ms (86400000 per day). Substring check is the `in` operator ("x" in s) - there is no text.contains."#;

/// Executes compose scripts as Runlet programs.
#[derive(Clone, Copy, Debug, Default)]
pub struct RunletBackend;

#[async_trait]
impl ComposeBackend for RunletBackend {
    fn name(&self) -> &'static str {
        "runlet"
    }

    fn script_description(&self) -> &'static str {
        "Runlet program to execute. The program must end with a single `return`; \
         its value becomes the compose result."
    }

    fn description(&self, catalog: Option<&[ToolSpec]>) -> String {
        let mut description =
            String::from(match std::env::var("COMPOSE_RUNLET_PRIMER").as_deref() {
                Ok("rules") => RULES_PRIMER,
                _ => EXEMPLAR_PRIMER,
            });
        if let Some(catalog) = catalog {
            if !catalog.is_empty() {
                description.push_str(&render_catalog_shapes(catalog));
                let renames: Vec<String> = catalog
                    .iter()
                    .filter(|spec| sanitize_name(spec.name.0.as_str()) != spec.name.0.as_str())
                    .map(|spec| {
                        format!(
                            "{} → call as `{}`",
                            spec.name.0,
                            sanitize_name(spec.name.0.as_str())
                        )
                    })
                    .collect();
                if !renames.is_empty() {
                    description.push_str("\n\nRenamed for Runlet (names must be identifiers):\n");
                    for rename in renames {
                        description.push_str("\n- ");
                        description.push_str(&rename);
                    }
                }
            }
        }
        description
    }

    async fn execute(&self, run: BackendRun) -> Result<Value, ComposeOutcome> {
        if std::env::var_os("COMPOSE_RUNLET_DEBUG").is_some() {
            eprintln!("[compose-runlet] executing program:\n{}\n---", run.script);
        }
        let handle = tokio::runtime::Handle::current();
        let bridge = Arc::new(HostBridge {
            interrupt: StdMutex::new(None),
            failures: StdMutex::new(HashMap::new()),
            failure_counter: AtomicU64::new(0),
        });

        let mut registry = RunletRegistry::new();
        let mut names: Vec<(String, ToolName)> = Vec::new();
        for spec in &run.visible_specs {
            let runlet_name = unique_name(&names, spec.name.0.as_str());
            let descriptor = ToolDescriptor {
                name: runlet_name.clone(),
                summary: spec.description.chars().take(200).collect(),
                input: CallSchema::one(schema_from_json(&spec.input_schema)),
                output: spec
                    .output_schema
                    .as_ref()
                    .map(schema_from_json)
                    .unwrap_or(Schema::Any),
                execution: execution_policy(spec),
                schema_version: SCHEMA_VERSION.into(),
            };
            registry.register(descriptor).map_err(|error| {
                ComposeOutcome::Failed(ToolError::Internal(format!(
                    "compose could not register tool for runlet: {error}"
                )))
            })?;
            names.push((runlet_name, spec.name.clone()));
        }

        let mut builder = Runtime::builder()
            .registry(registry)
            .with_prelude()
            .loop_concurrency(LOOP_CONCURRENCY)
            .dispatch_limit(DISPATCH_LIMIT)
            .graph_node_limit(GRAPH_NODE_LIMIT)
            // Transient upstream failures (rate limits, 503s) back off
            // exponentially between `boundary retry` attempts; a
            // `retry_after` hint on the failure overrides the computed delay.
            .retry_backoff(
                std::time::Duration::from_millis(100),
                2.0,
                std::time::Duration::from_secs(2),
            )
            .input("input", Schema::Any, canonical_from_json(&run.input));
        for (runlet_name, tool_name) in &names {
            let dispatcher = run.dispatcher.clone();
            let bridge = bridge.clone();
            let handle = handle.clone();
            let tool_name = tool_name.clone();
            builder = builder.tool(runlet_name.clone(), move |args, ctx| {
                if bridge.interrupted() {
                    return Err(RunletToolError::new(
                        "HOST_INTERRUPTED",
                        "compose run is suspended awaiting approval",
                    ));
                }
                let input = json_from_canonical(args.first().unwrap_or(&CanonicalValue::Null))
                    .map_err(|error| bridge.record_failure(error))?;
                let outcome = handle.block_on(dispatcher.call(
                    CallKey::Operation(ctx.operation_id.clone()),
                    tool_name.clone(),
                    input,
                ));
                match outcome {
                    Ok(output) => Ok(canonical_from_json(&output)),
                    Err(DispatchError::Interrupted(interruption)) => {
                        bridge.record_interrupt(interruption);
                        Err(RunletToolError::new(
                            "HOST_INTERRUPTED",
                            "compose run is suspended awaiting approval",
                        ))
                    }
                    Err(DispatchError::Failed(error)) => Err(bridge.record_failure(error)),
                }
            });
        }
        let runtime = builder.build().map_err(|error| {
            ComposeOutcome::Failed(ToolError::Internal(format!(
                "compose could not build runlet runtime: {error}"
            )))
        })?;

        let script = run.script.clone();
        let result = tokio::task::spawn_blocking(move || {
            let (program, heal_notes) = match runtime.compile(&script) {
                Ok(program) => (program, Vec::new()),
                Err(diagnostics) => {
                    // Auto-healing pre-pass: mechanical, insertion-only
                    // repairs (missing returns, unbound statements). The
                    // healed program runs and the repairs come back as
                    // warnings, saving the model a retry round-trip. When
                    // repairs fix the syntax but semantic errors remain,
                    // report THOSE errors — they are the real work left, and
                    // re-listing the already-repaired syntax would send the
                    // model fixing the wrong thing.
                    match runlet::heal(&script) {
                        Some(healed) => match runtime.compile(&healed.source) {
                            Ok(program) => (program, healed.notes),
                            Err(remaining) => {
                                return Err(RunletRunError::HealedCompile {
                                    notes: healed.notes,
                                    diagnostics: remaining,
                                });
                            }
                        },
                        None => return Err(RunletRunError::Compile(diagnostics)),
                    }
                }
            };
            runtime
                .run(&program)
                .map(|execution| (execution, heal_notes))
                .map_err(RunletRunError::Run)
        })
        .await
        .map_err(|error| {
            ComposeOutcome::Failed(ToolError::Internal(format!(
                "compose runlet execution panicked: {error}"
            )))
        })?;

        // An approval interrupt wins over whatever the program did afterwards
        // (including a `boundary ... catch` that swallowed the interrupt
        // error): the run resumes from replay once the approval is resolved.
        if let Some(interruption) = bridge.take_interrupt() {
            return Err(ComposeOutcome::Interrupted(interruption));
        }

        match result {
            Ok((execution, heal_notes)) => {
                let value =
                    json_from_canonical(&execution.value).map_err(ComposeOutcome::Failed)?;
                if heal_notes.is_empty() {
                    Ok(value)
                } else {
                    // The value arrives wrapped so the repairs are impossible
                    // to miss; future submissions should not need healing.
                    Ok(serde_json::json!({
                        "compose_warnings": {
                            "auto_repaired": heal_notes,
                            "note": "the program was repaired before execution; \
                                     apply these corrections to future programs",
                        },
                        "value": value,
                    }))
                }
            }
            Err(RunletRunError::Compile(diagnostics)) => Err(ComposeOutcome::Failed(
                ToolError::InvalidInput(render_diagnostics(&diagnostics)),
            )),
            Err(RunletRunError::HealedCompile { notes, diagnostics }) => {
                Err(ComposeOutcome::Failed(ToolError::InvalidInput(format!(
                    "syntax was repaired automatically ({}), but these errors remain — fix \
                     them and resubmit with the syntax corrections applied:\n\n{}",
                    notes.join("; "),
                    render_diagnostics(&diagnostics)
                ))))
            }
            Err(RunletRunError::Run(error)) => Err(ComposeOutcome::Failed(
                bridge.recall_failure(&error).unwrap_or_else(|| {
                    ToolError::ExecutionFailed(render_runtime_error(&error, &run.script))
                }),
            )),
        }
    }
}

/// Renders a runtime failure with the source expression it points at, so the
/// model can repair the exact spot instead of guessing from the code alone.
fn render_runtime_error(error: &RunletToolError, source: &str) -> String {
    let mut text = error.to_string();
    if let Some(span) = error.span {
        let snippet: String = source
            .get(span.start..span.end)
            .unwrap_or_default()
            .chars()
            .take(160)
            .collect();
        if !snippet.is_empty() {
            text.push_str(&format!(
                "\n  at {}..{}: `{}`",
                span.start,
                span.end,
                snippet.trim()
            ));
        }
    }
    text
}

enum RunletRunError {
    Compile(Vec<Diagnostic>),
    /// Syntax was auto-repaired but semantic errors remain; spans refer to
    /// the repaired program.
    HealedCompile {
        notes: Vec<String>,
        diagnostics: Vec<Diagnostic>,
    },
    Run(RunletToolError),
}

/// Carries typed host state across the sync/async boundary: the pending
/// approval interruption and the original [`ToolError`] behind each dispatch
/// failure (runlet only transports its own string-based error type).
struct HostBridge {
    interrupt: StdMutex<Option<ToolInterruption>>,
    failures: StdMutex<HashMap<u64, ToolError>>,
    failure_counter: AtomicU64,
}

const FAILURE_CODE_PREFIX: &str = "HOST_FAILURE_";

impl HostBridge {
    fn interrupted(&self) -> bool {
        self.interrupt.lock().expect("interrupt lock").is_some()
    }

    fn record_interrupt(&self, interruption: ToolInterruption) {
        let mut slot = self.interrupt.lock().expect("interrupt lock");
        if slot.is_none() {
            *slot = Some(interruption);
        }
    }

    fn take_interrupt(&self) -> Option<ToolInterruption> {
        self.interrupt.lock().expect("interrupt lock").take()
    }

    fn record_failure(&self, error: ToolError) -> RunletToolError {
        let id = self.failure_counter.fetch_add(1, Ordering::Relaxed);
        let retryable = matches!(error, ToolError::Unavailable(_));
        let message = error.to_string();
        self.failures
            .lock()
            .expect("failures lock")
            .insert(id, error);
        RunletToolError::new(format!("{FAILURE_CODE_PREFIX}{id}"), message).retryable(retryable)
    }

    fn recall_failure(&self, error: &RunletToolError) -> Option<ToolError> {
        let id: u64 = error.code.strip_prefix(FAILURE_CODE_PREFIX)?.parse().ok()?;
        self.failures.lock().expect("failures lock").remove(&id)
    }
}

fn render_diagnostics(diagnostics: &[Diagnostic]) -> String {
    let mut out = String::from(
        "runlet program rejected before execution; fix the errors and retry \
         (warnings are advisory and do not block execution):\n",
    );
    for diagnostic in diagnostics {
        out.push_str(&format!(
            "\n{} {} [{}] at {}..{}: {}",
            match diagnostic.severity {
                runlet::Severity::Error => "error",
                runlet::Severity::Warning => "warning",
            },
            diagnostic.code,
            diagnostic.title,
            diagnostic.primary_span.start,
            diagnostic.primary_span.end,
            diagnostic.message,
        ));
        for fix in &diagnostic.fixes {
            out.push_str(&format!("\n  fix: {} → `{}`", fix.message, fix.replacement));
        }
        if !diagnostic.candidates.is_empty() {
            out.push_str(&format!(
                "\n  did you mean: {}",
                diagnostic.candidates.join(", ")
            ));
        }
    }
    out
}

const KEYWORDS: &[&str] = &[
    "return", "for", "in", "limit", "boundary", "retry", "catch", "if", "else", "and", "or", "not",
    "null", "true", "false", "input",
];

/// Rewrites a tool name into a valid runlet identifier.
fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c == '_' || c.is_alphanumeric() {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    if KEYWORDS.contains(&out.as_str()) {
        out.insert(0, '_');
    }
    out
}

fn unique_name(taken: &[(String, ToolName)], name: &str) -> String {
    let base = sanitize_name(name);
    if !taken.iter().any(|(existing, _)| *existing == base) {
        return base;
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}_{suffix}");
        if !taken.iter().any(|(existing, _)| *existing == candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn execution_policy(spec: &ToolSpec) -> ExecutionPolicy {
    if spec.annotations.read_only_hint {
        ExecutionPolicy::Pure
    } else if spec.annotations.idempotent_hint {
        ExecutionPolicy::Idempotent
    } else {
        ExecutionPolicy::AtMostOnce
    }
}

/// Converts a JSON Schema into a runlet [`Schema`], falling back to
/// [`Schema::Any`] for constructs runlet cannot express. The conversion errs
/// permissive: a too-strict schema would make runlet's runtime reject values
/// the real tool accepts.
fn schema_from_json(value: &Value) -> Schema {
    let Some(object) = value.as_object() else {
        return Schema::Any;
    };
    if object.contains_key("$ref") {
        return Schema::Any;
    }
    if let Some(variants) = object
        .get("anyOf")
        .or_else(|| object.get("oneOf"))
        .and_then(Value::as_array)
    {
        return Schema::Union {
            variants: variants.iter().map(schema_from_json).collect(),
            discriminator: None,
        };
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        if values.iter().all(Value::is_string) {
            return Schema::String {
                format: None,
                enumeration: values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect(),
                min_len: None,
                max_len: None,
            };
        }
        return Schema::Any;
    }
    match object.get("type") {
        Some(Value::String(kind)) => match kind.as_str() {
            "object" => {
                let props = object.get("properties").and_then(Value::as_object);
                if props.is_none_or(|props| props.is_empty()) {
                    if let Some(additional) =
                        object.get("additionalProperties").filter(|v| v.is_object())
                    {
                        return Schema::Map {
                            values: Box::new(schema_from_json(additional)),
                        };
                    }
                }
                let mut properties = BTreeMap::new();
                if let Some(props) = props {
                    for (key, prop_value) in props {
                        let mut property = Property::new(schema_from_json(prop_value));
                        if let Some(documentation) =
                            prop_value.get("description").and_then(Value::as_str)
                        {
                            property.documentation = documentation.into();
                        }
                        properties.insert(key.clone(), property);
                    }
                }
                let required = object
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|keys| {
                        keys.iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                let additional = object
                    .get("additionalProperties")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                Schema::Object {
                    properties,
                    required,
                    additional,
                }
            }
            "array" => Schema::List {
                items: Box::new(
                    object
                        .get("items")
                        .map(schema_from_json)
                        .unwrap_or(Schema::Any),
                ),
                min_len: None,
                max_len: None,
            },
            "string" => Schema::String {
                format: object
                    .get("format")
                    .and_then(Value::as_str)
                    .map(String::from),
                enumeration: vec![],
                min_len: None,
                max_len: None,
            },
            "integer" => Schema::Integer {
                min: object.get("minimum").and_then(Value::as_i64),
                max: object.get("maximum").and_then(Value::as_i64),
            },
            "number" => Schema::Number {
                min: object.get("minimum").and_then(Value::as_f64),
                max: object.get("maximum").and_then(Value::as_f64),
            },
            "boolean" => Schema::Boolean,
            "null" => Schema::Null,
            _ => Schema::Any,
        },
        Some(Value::Array(kinds)) => Schema::Union {
            variants: kinds
                .iter()
                .filter_map(Value::as_str)
                .map(|kind| {
                    schema_from_json(&serde_json::json!({
                        "type": kind
                    }))
                })
                .collect(),
            discriminator: None,
        },
        _ => Schema::Any,
    }
}

fn canonical_from_json(value: &Value) -> CanonicalValue {
    match value {
        Value::Null => CanonicalValue::Null,
        Value::Bool(v) => CanonicalValue::Boolean(*v),
        Value::Number(number) => number
            .as_i64()
            .map(CanonicalValue::Integer)
            .or_else(|| number.as_f64().and_then(|v| CanonicalValue::number(v).ok()))
            .unwrap_or(CanonicalValue::Null),
        Value::String(v) => CanonicalValue::String(v.clone()),
        Value::Array(values) => {
            CanonicalValue::List(values.iter().map(canonical_from_json).collect())
        }
        Value::Object(fields) => CanonicalValue::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), canonical_from_json(value)))
                .collect(),
        ),
    }
}

fn json_from_canonical(value: &CanonicalValue) -> Result<Value, ToolError> {
    Ok(match value {
        CanonicalValue::Null => Value::Null,
        CanonicalValue::Boolean(v) => Value::Bool(*v),
        CanonicalValue::Integer(v) => Value::Number((*v).into()),
        CanonicalValue::Number(v) => serde_json::Number::from_f64(*v)
            .map(Value::Number)
            .ok_or_else(|| {
                ToolError::ExecutionFailed("compose result contains a non-finite number".into())
            })?,
        CanonicalValue::String(v) => Value::String(v.clone()),
        CanonicalValue::Bytes(_) => {
            return Err(ToolError::ExecutionFailed(
                "compose result contains bytes, which have no JSON representation".into(),
            ));
        }
        CanonicalValue::List(values) => Value::Array(
            values
                .iter()
                .map(json_from_canonical)
                .collect::<Result<_, _>>()?,
        ),
        CanonicalValue::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| Ok((key.clone(), json_from_canonical(value)?)))
                .collect::<Result<Map<_, _>, ToolError>>()?,
        ),
    })
}
