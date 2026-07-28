use crate::{
    BinaryOp, Diagnostic, Expr, ExprKind, Phase, Program, Schema, Severity, Span, ToolRegistry,
    UnaryOp, parse,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
/// A named value supplied by the host and available to a Runlet program.
pub struct ExternalInput {
    /// Name used to reference the input in source code.
    pub name: String,
    /// Schema the supplied value must satisfy.
    pub schema: Schema,
}

#[derive(Debug, Clone)]
/// A parsed and schema-checked program ready for execution.
pub struct CompiledProgram {
    pub(crate) program: Program,
    /// Original Runlet source.
    pub source: String,
    /// Hex-encoded digest of [`Self::source`].
    pub source_digest: String,
    /// Digest of the tool registry used during compilation.
    pub registry_digest: String,
    /// Non-fatal diagnostics, such as warnings about pruned bindings.
    pub diagnostics: Vec<Diagnostic>,
}

/// Parses and analyzes source against a tool registry and host inputs.
///
/// Returns all fatal parse or analysis diagnostics when compilation fails.
pub fn compile(
    source: &str,
    registry: &ToolRegistry,
    inputs: &[ExternalInput],
) -> Result<CompiledProgram, Vec<Diagnostic>> {
    let program = parse(source)?;
    let mut a = Analyzer {
        registry,
        diagnostics: vec![],
        scopes: vec![],
        registry_roots: registry.roots().into_iter().map(str::to_owned).collect(),
    };
    let mut root = BTreeMap::new();
    for i in inputs {
        if root.insert(i.name.clone(), i.schema.clone()).is_some() {
            a.diagnostics.push(Diagnostic::error(
                "RL8102",
                Phase::Analyze,
                Span::default(),
                "duplicate host input",
                format!("host input `{}` is defined more than once", i.name),
            ));
        }
    }
    a.scopes.push(root);
    a.analyze_program(&program);
    if a.diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return Err(a.diagnostics);
    }
    Ok(CompiledProgram {
        program,
        source: source.into(),
        source_digest: hex::encode(Sha256::digest(source.as_bytes())),
        registry_digest: registry.digest(),
        diagnostics: a.diagnostics,
    })
}

struct Analyzer<'a> {
    registry: &'a ToolRegistry,
    diagnostics: Vec<Diagnostic>,
    scopes: Vec<BTreeMap<String, Schema>>,
    registry_roots: BTreeSet<String>,
}
impl Analyzer<'_> {
    fn analyze_program(&mut self, p: &Program) {
        self.block_bindings(&p.statements, &p.result, false);
    }
    fn block_bindings(&mut self, stmts: &[crate::Stmt], result: &Expr, push: bool) -> Schema {
        if push {
            self.scopes.push(BTreeMap::new())
        }
        let scope = self.scopes.len() - 1;
        let mut defs = BTreeMap::new();
        for s in stmts {
            let (name, value) = match &s.kind {
                crate::StmtKind::Binding { name, value } => (name, value),
                crate::StmtKind::Skip { condition } => {
                    if let Some(condition) = condition {
                        let c = self.expr(condition);
                        if !boolean_context(&c) {
                            self.err("RL2305", condition.span, "skip condition must be Boolean");
                        }
                    }
                    continue;
                }
                crate::StmtKind::Assert { condition, message } => {
                    let condition_type = self.expr(condition);
                    if !boolean_context(&condition_type) {
                        self.err("RL2305", condition.span, "assert condition must be Boolean");
                    }
                    if let Some(message) = message {
                        let message_type = self.expr(message);
                        if !matches!(
                            message_type,
                            Schema::String { .. }
                                | Schema::Any
                                | Schema::Union { .. }
                                | Schema::Never
                        ) {
                            self.err("RL2315", message.span, "assert message must be a string");
                        }
                    }
                    continue;
                }
            };
            if self.registry_roots.contains(name) {
                self.diagnostics.push(
                    Diagnostic::error(
                        "RL2107",
                        Phase::Analyze,
                        s.span,
                        "binding collides with registry root",
                        format!("`{name}` is a registered namespace root"),
                    )
                    .with_fix(
                        Span::new(s.span.start, s.span.start + name.len()),
                        format!("{name}_value"),
                        "rename the binding",
                    ),
                );
            }
            if defs.insert(name.clone(), value).is_some() || self.scopes[scope].contains_key(name) {
                self.diagnostics.push(
                    Diagnostic::error(
                        "RL2106",
                        Phase::Analyze,
                        s.span,
                        "duplicate binding",
                        format!(
                            "`{name}` is already declared in this scope; bindings are \
                             immutable — merge both cases into one expression \
                             (`{name} = if cond {{ ... }} else {{ ... }}`), bind a new \
                             name, or accumulate with a fold"
                        ),
                    )
                    .with_fix(s.span, "", "remove the duplicate binding"),
                );
                continue;
            }
            let ty = self.expr(value);
            self.scopes[scope].insert(name.clone(), ty);
        }
        let ty = self.expr(result);
        let mut used = BTreeSet::new();
        collect_names(result, &defs, &mut used);
        // Effect-rooted statements and skip guards are execution roots too:
        // names they reference are reachable, so a pure helper used only by
        // a fire-and-forget write or a guard condition is not dead.
        for s in stmts {
            match &s.kind {
                crate::StmtKind::Binding { name, value } => {
                    if contains_effectful_call(value, self.registry) {
                        used.insert(name.clone());
                        collect_names(value, &defs, &mut used);
                    }
                }
                crate::StmtKind::Skip {
                    condition: Some(condition),
                } => collect_names(condition, &defs, &mut used),
                crate::StmtKind::Skip { condition: None } => {}
                crate::StmtKind::Assert { condition, message } => {
                    collect_names(condition, &defs, &mut used);
                    if let Some(message) = message {
                        collect_names(message, &defs, &mut used);
                    }
                }
            }
        }
        for s in stmts {
            let Some((name, value)) = s.binding() else {
                continue;
            };
            if !used.contains(name) {
                // Statements containing effectful calls are implicit roots:
                // the runtime evaluates them when the block runs, so they are
                // not dead and get no diagnostic. Pure unused bindings never
                // evaluate; the warning keeps the pruned work visible in the
                // compiled diagnostics.
                if contains_effectful_call(value, self.registry) {
                    continue;
                }
                let (title, msg) = if contains_call(value) {
                    (
                        "unused tool call",
                        "this pure call is not reachable from the block return and is pruned",
                    )
                } else {
                    ("unused binding", "this local computation is pruned")
                };
                let d = Diagnostic::warning("RL1205", Phase::Analyze, s.span, title, msg).with_fix(
                    s.span,
                    "",
                    "remove the unused binding",
                );
                self.diagnostics.push(d)
            }
        }
        if push {
            self.scopes.pop();
        }
        ty
    }
    fn expr(&mut self, e: &Expr) -> Schema {
        match &e.kind {
            ExprKind::Null => Schema::Null,
            ExprKind::Boolean(_) => Schema::Boolean,
            ExprKind::Integer(s) => {
                if s.parse::<i64>().is_err() {
                    self.err(
                        "RL2301",
                        e.span,
                        "integer literal is outside the signed 64-bit range",
                    );
                }
                Schema::INTEGER
            }
            ExprKind::Number(s) => {
                if s.parse::<f64>().is_err() || !s.parse::<f64>().is_ok_and(f64::is_finite) {
                    self.err(
                        "RL2302",
                        e.span,
                        "number literal is not a finite binary64 value",
                    );
                }
                Schema::NUMBER
            }
            ExprKind::String(_) => Schema::string(),
            ExprKind::Name(n) => self.lookup(n).unwrap_or_else(|| {
                let c = closest(n, self.visible_names());
                self.diagnostics.push(
                    Diagnostic::error(
                        "RL2101",
                        Phase::Analyze,
                        e.span,
                        "unknown name",
                        format!("`{n}` is not a local, host input, or registered callable"),
                    )
                    .with_candidates(c),
                );
                Schema::Any
            }),
            ExprKind::List(xs) => {
                let ts = xs.iter().map(|x| self.expr(x)).collect::<Vec<_>>();
                Schema::list(union(ts))
            }
            ExprKind::Object(xs) => {
                let mut p = BTreeMap::new();
                let mut r = BTreeSet::new();
                let mut computed = Vec::new();
                for (k, v) in xs {
                    let value = self.expr(v);
                    match k {
                        crate::ObjectKey::Static(k) => {
                            p.insert(k.clone(), crate::Property::new(value));
                            r.insert(k.clone());
                        }
                        crate::ObjectKey::Computed(key) => {
                            let ks = self.expr(key);
                            if !computed_key_schema(&ks) {
                                self.err(
                                    "RL2315",
                                    key.span,
                                    &format!(
                                        "computed property keys must be strings or scalar \
                                         values, got {}",
                                        ks.kind_name()
                                    ),
                                );
                            }
                            computed.push(value);
                        }
                    }
                }
                if computed.is_empty() {
                    Schema::Object {
                        properties: p,
                        required: r,
                        additional: false,
                    }
                } else {
                    // Keys are unknowable statically, so the literal is a map
                    // over the union of every entry's value schema.
                    computed.extend(p.into_values().map(|p| p.schema));
                    Schema::Map {
                        values: Box::new(union(computed)),
                    }
                }
            }
            ExprKind::Member { target, field } => {
                if let Some(path) = path_of(e) {
                    let prefix = path + ".";
                    if self.registry.names().any(|n| n.starts_with(&prefix)) {
                        return Schema::Any;
                    }
                }
                let t = self.expr(target);
                project_schema(&t, field).unwrap_or_else(|| {
                    let mut message = format!(
                        "property `{field}` is not available on this value of type {}",
                        t.type_notation()
                    );
                    if matches!(t, Schema::List { .. }) {
                        message.push_str(" — iterate or index the list itself");
                    }
                    self.diagnostics.push(
                        Diagnostic::error(
                            "RL2103",
                            Phase::Analyze,
                            e.span,
                            "unknown property",
                            message,
                        )
                        .with_candidates(schema_fields(&t)),
                    );
                    Schema::Any
                })
            }
            ExprKind::Index { target, index } => {
                let t = self.expr(target);
                let i = self.expr(index);
                match t {
                    Schema::List { items, .. } if matches!(i, Schema::Integer { .. }) => *items,
                    Schema::String { .. } if matches!(i, Schema::Integer { .. }) => {
                        Schema::string()
                    }
                    Schema::Bytes if matches!(i, Schema::Integer { .. }) => Schema::INTEGER,
                    Schema::Map { values } if matches!(i, Schema::String { .. }) => *values,
                    Schema::Object { ref properties, .. } if matches!(i, Schema::String { .. }) => {
                        union(properties.values().map(|p| p.schema.clone()).collect())
                    }
                    Schema::Any => Schema::Any,
                    // An indexable target with a statically-unknown index
                    // defers to the runtime's projection checks.
                    Schema::List { .. }
                    | Schema::String { .. }
                    | Schema::Bytes
                    | Schema::Map { .. }
                    | Schema::Object { .. }
                        if matches!(i, Schema::Any | Schema::Union { .. }) =>
                    {
                        Schema::Any
                    }
                    _ => {
                        self.err(
                            "RL2308",
                            e.span,
                            "target and index schemas are incompatible",
                        );
                        Schema::Any
                    }
                }
            }
            ExprKind::Call { callee, arguments } => {
                let Some(name) = path_of(callee) else {
                    self.err(
                        "RL2104",
                        callee.span,
                        "values are not callable; use a registered tool name",
                    );
                    return Schema::Any;
                };
                let Some(tool) = self.registry.get(&name) else {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "RL2102",
                            Phase::Analyze,
                            callee.span,
                            "unknown callable",
                            match stdlib_hint(&name) {
                                Some(hint) => format!("`{name}` is not registered; {hint}"),
                                None => format!("`{name}` is not registered"),
                            },
                        )
                        .with_candidates(closest(
                            &name,
                            self.registry.names().map(str::to_owned).collect(),
                        )),
                    );
                    return Schema::Any;
                };
                let (min, max) = (tool.input.required_count(), tool.input.parameters.len());
                if arguments.len() < min || arguments.len() > max {
                    let expected = if min == max {
                        format!("{max}")
                    } else {
                        format!("{min} to {max}")
                    };
                    self.err(
                        "RL2208",
                        e.span,
                        &format!(
                            "`{name}` expects {expected} arguments but received {}",
                            arguments.len()
                        ),
                    );
                }
                for (a, expected) in arguments.iter().zip(&tool.input.parameters) {
                    let actual = self.expr(a);
                    if conversion_rank(&actual, expected).is_none() {
                        self.err(
                            "RL2311",
                            a.span,
                            "argument cannot be safely converted to the tool parameter schema",
                        )
                    }
                }
                // Literal regex patterns compile at compile time, so an
                // invalid pattern is a span-annotated diagnostic before
                // anything runs. Dynamic patterns stay a runtime concern.
                let invalid_pattern = name
                    .starts_with("regex.")
                    .then_some(arguments.get(1))
                    .flatten()
                    .and_then(|pattern| match &pattern.kind {
                        ExprKind::String(literal) => crate::prelude::validate_pattern(literal)
                            .err()
                            .map(|error| (pattern, error)),
                        _ => None,
                    });
                if let Some((pattern, error)) = invalid_pattern {
                    self.err("RL2316", pattern.span, &error.to_string());
                }
                tool.output.clone()
            }
            ExprKind::Unary { op, value } => {
                let t = self.expr(value);
                match (op, &t) {
                    (UnaryOp::Not, Schema::Boolean) => Schema::Boolean,
                    (UnaryOp::Not, Schema::Any | Schema::Union { .. }) => Schema::Boolean,
                    (UnaryOp::Negate, Schema::Integer { .. }) => Schema::INTEGER,
                    (UnaryOp::Negate, Schema::Number { .. }) => Schema::NUMBER,
                    (UnaryOp::Negate, Schema::Any | Schema::Union { .. }) => Schema::Any,
                    _ => {
                        self.err("RL2304", e.span, "operator does not accept this schema");
                        Schema::Any
                    }
                }
            }
            ExprKind::Binary { op, left, right } => {
                let l = self.expr(left);
                if matches!(op, BinaryOp::And | BinaryOp::Or)
                    && !matches!(l, Schema::Boolean | Schema::Any | Schema::Union { .. })
                {
                    self.err(
                        "RL2305",
                        left.span,
                        "Boolean operator requires Boolean operands",
                    )
                }
                let r = self.expr(right);
                binary_schema(*op, &l, &r).unwrap_or_else(|| {
                    self.err(
                        "RL2304",
                        e.span,
                        "operator does not accept these operand schemas",
                    );
                    Schema::Any
                })
            }
            ExprKind::Conditional {
                then_expr,
                condition,
                else_expr,
            } => {
                let c = self.expr(condition);
                if !boolean_context(&c) {
                    self.err("RL2305", condition.span, "condition must be Boolean")
                }
                let a = self.expr(then_expr);
                let b = self.expr(else_expr);
                union(vec![a, b])
            }
            ExprKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let c = self.expr(condition);
                if !boolean_context(&c) {
                    self.err("RL2305", condition.span, "condition must be Boolean")
                }
                self.scopes.push(BTreeMap::new());
                let a = self.block_bindings(&then_block.statements, &then_block.result, false);
                self.scopes.pop();
                let b = match else_block {
                    Some(block) => {
                        self.scopes.push(BTreeMap::new());
                        let b = self.block_bindings(&block.statements, &block.result, false);
                        self.scopes.pop();
                        b
                    }
                    None => Schema::Null,
                };
                union(vec![a, b])
            }
            ExprKind::For {
                binding,
                collection,
                body,
            } => {
                let c = self.expr(collection);
                let item = iterable_item_schema(&c).unwrap_or_else(|| {
                    self.err(
                        "RL2309",
                        collection.span,
                        "for collection must be a list, object, or map",
                    );
                    Schema::Any
                });
                self.scopes.push(BTreeMap::from([(binding.clone(), item)]));
                let out = self.block_bindings(&body.statements, &body.result, false);
                self.scopes.pop();
                Schema::list(out)
            }
            ExprKind::Fold {
                accumulator,
                init,
                binding,
                collection,
                body,
            } => {
                let acc = self.expr(init);
                let c = self.expr(collection);
                let item = iterable_item_schema(&c).unwrap_or_else(|| {
                    self.err(
                        "RL2309",
                        collection.span,
                        "fold collection must be a list, object, or map",
                    );
                    Schema::Any
                });
                let mark = self.diagnostics.len();
                self.scopes.push(BTreeMap::from([
                    (accumulator.clone(), acc.clone()),
                    (binding.clone(), item.clone()),
                ]));
                let out = self.block_bindings(&body.statements, &body.result, false);
                self.scopes.pop();
                // The accumulator keeps one schema across iterations; see
                // fold_unify for the widenings a seed is allowed. When the
                // seed widens, the body is re-typed under the widened schema
                // — a `null`-seeded accumulator (or property) otherwise
                // poisons every projection inside the body.
                let result = match fold_unify(&acc, &out) {
                    Ok(unified) => {
                        if unified == acc {
                            unified
                        } else {
                            self.diagnostics.truncate(mark);
                            self.scopes.push(BTreeMap::from([
                                (accumulator.clone(), unified.clone()),
                                (binding.clone(), item),
                            ]));
                            let out = self.block_bindings(&body.statements, &body.result, false);
                            self.scopes.pop();
                            match fold_unify(&unified, &out) {
                                Ok(stable) => stable,
                                Err(detail) => {
                                    self.err(
                                        "RL2313",
                                        body.result.span,
                                        &format!("fold accumulator must keep one schema: {detail}"),
                                    );
                                    unified
                                }
                            }
                        }
                    }
                    Err(detail) => {
                        self.err(
                            "RL2313",
                            body.result.span,
                            &format!("fold accumulator must keep one schema: {detail}"),
                        );
                        acc
                    }
                };
                self.warn_independent_effects(accumulator, body);
                result
            }
            ExprKind::Fail { arguments } => {
                if arguments.len() < 2 || arguments.len() > 3 {
                    self.err(
                        "RL2314",
                        e.span,
                        "fail takes a code, a message, and an optional details object: \
                         `fail(\"NO_MATCH\", \"no company for this domain\")`",
                    );
                }
                for (i, a) in arguments.iter().take(2).enumerate() {
                    let t = self.expr(a);
                    if conversion_rank(&t, &Schema::string()).is_none() {
                        self.err(
                            "RL2314",
                            a.span,
                            &format!(
                                "fail {} must be a string, got {}",
                                if i == 0 { "code" } else { "message" },
                                t.kind_name()
                            ),
                        );
                    }
                }
                if let Some(details) = arguments.get(2) {
                    let t = self.expr(details);
                    if !matches!(t, Schema::Object { .. } | Schema::Map { .. } | Schema::Any) {
                        self.err("RL2314", details.span, "fail details must be an object");
                    }
                }
                Schema::Never
            }
            ExprKind::Boundary {
                body,
                error_binding,
                catch,
                ..
            } => {
                self.scopes.push(BTreeMap::new());
                let a = self.block_bindings(&body.statements, &body.result, false);
                self.scopes.pop();
                self.scopes
                    .push(BTreeMap::from([(error_binding.clone(), error_schema())]));
                let b = self.block_bindings(&catch.statements, &catch.result, false);
                self.scopes.pop();
                union(vec![a, b])
            }
        }
    }
    /// Warns when an effectful statement in a `fold` body never references
    /// the accumulator: the fold serializes it for no reason, and a `for`
    /// loop would run the iterations concurrently.
    fn warn_independent_effects(&mut self, accumulator: &str, body: &crate::Block) {
        let mut defs = BTreeMap::new();
        for s in &body.statements {
            if let crate::StmtKind::Binding { name, value } = &s.kind {
                defs.insert(name.clone(), value);
            }
        }
        for s in &body.statements {
            let Some((_, value)) = s.binding() else {
                continue;
            };
            if !contains_effectful_call(value, self.registry) {
                continue;
            }
            let mut used = BTreeSet::new();
            collect_names(value, &defs, &mut used);
            if !used.contains(accumulator) {
                self.diagnostics.push(Diagnostic::warning(
                    "RL1206",
                    Phase::Analyze,
                    s.span,
                    "sequential effect does not use the accumulator",
                    format!(
                        "this tool call does not depend on `{accumulator}`; a `fold` runs \
                         iterations one after another — if each item is independent, use \
                         `for ...` to run them concurrently"
                    ),
                ));
            }
        }
    }
    fn lookup(&self, n: &str) -> Option<Schema> {
        self.scopes.iter().rev().find_map(|s| s.get(n).cloned())
    }
    fn visible_names(&self) -> Vec<String> {
        self.scopes
            .iter()
            .flat_map(|s| s.keys().cloned())
            .chain(self.registry.names().map(str::to_owned))
            .collect()
    }
    fn err(&mut self, code: &str, span: Span, msg: &str) {
        self.diagnostics.push(
            Diagnostic::error(code, Phase::Analyze, span, "schema error", msg).with_fix(
                span,
                "",
                "change this expression to an accepted shape",
            ),
        )
    }
}
/// Schemas acceptable where a Boolean is required: Boolean itself, plus
/// Any/Union (checked at runtime) and Never (the check is unreachable).
fn boolean_context(s: &Schema) -> bool {
    matches!(
        s,
        Schema::Boolean | Schema::Any | Schema::Union { .. } | Schema::Never
    )
}

fn path_of(e: &Expr) -> Option<String> {
    match &e.kind {
        ExprKind::Name(n) => Some(n.clone()),
        ExprKind::Member { target, field } => Some(format!("{}.{}", path_of(target)?, field)),
        _ => None,
    }
}
/// Reports whether an expression contains a call to a tool with observable
/// effects (any policy other than [`ExecutionPolicy::Pure`]). Statements whose
/// expressions do are implicit roots: the runtime evaluates them when their
/// block runs even if nothing references them, so a fire-and-forget write is
/// never silently dropped. Calls to unknown tools count as effectful so the
/// conservative path is "it runs".
pub(crate) fn contains_effectful_call(e: &Expr, registry: &ToolRegistry) -> bool {
    match &e.kind {
        ExprKind::Call { callee, arguments } => {
            let effectful_callee = match path_of(callee) {
                Some(name) => registry
                    .get(&name)
                    .map(|d| d.execution != crate::ExecutionPolicy::Pure)
                    .unwrap_or(true),
                None => true,
            };
            effectful_callee
                || arguments
                    .iter()
                    .any(|a| contains_effectful_call(a, registry))
        }
        ExprKind::List(x) => x.iter().any(|e| contains_effectful_call(e, registry)),
        ExprKind::Object(x) => x.iter().any(|(k, e)| {
            k.expr()
                .is_some_and(|k| contains_effectful_call(k, registry))
                || contains_effectful_call(e, registry)
        }),
        ExprKind::Member { target, .. } | ExprKind::Unary { value: target, .. } => {
            contains_effectful_call(target, registry)
        }
        ExprKind::Index { target, index }
        | ExprKind::Binary {
            left: target,
            right: index,
            ..
        } => contains_effectful_call(target, registry) || contains_effectful_call(index, registry),
        ExprKind::Conditional {
            then_expr,
            condition,
            else_expr,
        } => [then_expr, condition, else_expr]
            .into_iter()
            .any(|x| contains_effectful_call(x, registry)),
        ExprKind::If {
            condition,
            then_block,
            else_block,
        } => {
            contains_effectful_call(condition, registry)
                || block_contains(then_block, |e| contains_effectful_call(e, registry))
                || else_block
                    .as_ref()
                    .is_some_and(|b| block_contains(b, |e| contains_effectful_call(e, registry)))
        }
        ExprKind::For {
            collection, body, ..
        }
        | ExprKind::Fold {
            collection, body, ..
        } => {
            contains_effectful_call(collection, registry)
                || block_contains(body, |e| contains_effectful_call(e, registry))
        }
        ExprKind::Boundary { body, catch, .. } => [body, catch]
            .into_iter()
            .any(|b| block_contains(b, |e| contains_effectful_call(e, registry))),
        // A fail guard must always evaluate when its block runs, exactly
        // like an effectful call.
        ExprKind::Fail { arguments } => {
            let _ = arguments;
            true
        }
        _ => false,
    }
}

/// Applies a predicate to every statement expression (binding values and
/// skip conditions) and the result of a block.
fn block_contains(b: &crate::Block, pred: impl Fn(&Expr) -> bool + Copy) -> bool {
    b.statements.iter().any(|statement| match &statement.kind {
        crate::StmtKind::Binding { value, .. } => pred(value),
        crate::StmtKind::Skip { condition } => condition.as_ref().is_some_and(pred),
        crate::StmtKind::Assert { condition, message } => {
            pred(condition) || message.as_ref().is_some_and(pred)
        }
    }) || pred(&b.result)
}
fn contains_call(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Call { .. } => true,
        ExprKind::List(x) => x.iter().any(contains_call),
        ExprKind::Object(x) => x
            .iter()
            .any(|(k, e)| k.expr().is_some_and(contains_call) || contains_call(e)),
        ExprKind::Member { target, .. } | ExprKind::Unary { value: target, .. } => {
            contains_call(target)
        }
        ExprKind::Index { target, index }
        | ExprKind::Binary {
            left: target,
            right: index,
            ..
        } => contains_call(target) || contains_call(index),
        ExprKind::Conditional {
            then_expr,
            condition,
            else_expr,
        } => [then_expr, condition, else_expr]
            .into_iter()
            .any(|x| contains_call(x)),
        ExprKind::If {
            condition,
            then_block,
            else_block,
        } => {
            contains_call(condition)
                || block_contains(then_block, contains_call)
                || else_block
                    .as_ref()
                    .is_some_and(|b| block_contains(b, contains_call))
        }
        ExprKind::For {
            collection, body, ..
        }
        | ExprKind::Fold {
            collection, body, ..
        } => contains_call(collection) || block_contains(body, contains_call),
        ExprKind::Boundary { body, catch, .. } => [body, catch]
            .into_iter()
            .any(|b| block_contains(b, contains_call)),
        _ => false,
    }
}
fn collect_names(e: &Expr, defs: &BTreeMap<String, &Expr>, used: &mut BTreeSet<String>) {
    match &e.kind {
        ExprKind::Name(n) => {
            let first_reference = used.insert(n.clone());
            if let (true, Some(x)) = (first_reference, defs.get(n)) {
                collect_names(x, defs, used)
            }
        }
        ExprKind::List(x) => x.iter().for_each(|e| collect_names(e, defs, used)),
        ExprKind::Object(x) => x.iter().for_each(|(k, e)| {
            if let Some(k) = k.expr() {
                collect_names(k, defs, used);
            }
            collect_names(e, defs, used);
        }),
        ExprKind::Member { target, .. } | ExprKind::Unary { value: target, .. } => {
            collect_names(target, defs, used)
        }
        ExprKind::Index { target, index }
        | ExprKind::Binary {
            left: target,
            right: index,
            ..
        } => {
            collect_names(target, defs, used);
            collect_names(index, defs, used)
        }
        ExprKind::Call { arguments, .. } => {
            arguments.iter().for_each(|e| collect_names(e, defs, used))
        }
        ExprKind::Conditional {
            then_expr,
            condition,
            else_expr,
        } => [then_expr, condition, else_expr]
            .into_iter()
            .for_each(|e| collect_names(e, defs, used)),
        ExprKind::For {
            collection, body, ..
        } => {
            collect_names(collection, defs, used);
            collect_block_names(body, defs, used)
        }
        ExprKind::Fold {
            init,
            collection,
            body,
            ..
        } => {
            collect_names(init, defs, used);
            collect_names(collection, defs, used);
            collect_block_names(body, defs, used)
        }
        ExprKind::Fail { arguments } => arguments.iter().for_each(|e| collect_names(e, defs, used)),
        ExprKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_names(condition, defs, used);
            collect_block_names(then_block, defs, used);
            if let Some(block) = else_block {
                collect_block_names(block, defs, used);
            }
        }
        ExprKind::Boundary { body, catch, .. } => {
            collect_block_names(body, defs, used);
            collect_block_names(catch, defs, used)
        }
        _ => {}
    }
}

/// Collects names reachable from a block's result, resolving through the
/// block's own lazy statements: a local binding referenced by the result
/// pulls in whatever names its expression references, including outer
/// bindings. Without this, an outer binding used only inside a nested block
/// was invisible to reachability and falsely rejected as unreachable.
fn collect_block_names(
    block: &crate::Block,
    defs: &BTreeMap<String, &Expr>,
    used: &mut BTreeSet<String>,
) {
    let mut extended = defs.clone();
    for stmt in &block.statements {
        if let crate::StmtKind::Binding { name, value } = &stmt.kind {
            extended.insert(name.clone(), value);
        }
    }
    collect_names(&block.result, &extended, used);
    // Guards always evaluate when the block runs; their expressions are
    // reachable regardless of the block result.
    for stmt in &block.statements {
        match &stmt.kind {
            crate::StmtKind::Skip {
                condition: Some(condition),
            } => collect_names(condition, &extended, used),
            crate::StmtKind::Assert { condition, message } => {
                collect_names(condition, &extended, used);
                if let Some(message) = message {
                    collect_names(message, &extended, used);
                }
            }
            _ => {}
        }
    }
}
/// Maximum number of variants a synthesized union keeps before collapsing to
/// `Any`. Nested loops and conditionals union schemas at every level; without
/// flattening, full dedup, and a cap, pathological programs can grow schema
/// trees combinatorially during analysis.
const UNION_VARIANT_CAP: usize = 8;

fn union(x: Vec<Schema>) -> Schema {
    let mut flat: Vec<Schema> = Vec::new();
    let push = |s: Schema, flat: &mut Vec<Schema>| {
        if !flat.contains(&s) {
            flat.push(s);
        }
    };
    for s in x {
        match s {
            Schema::Union { variants, .. } => {
                for v in variants {
                    if !matches!(v, Schema::Never) {
                        push(v, &mut flat);
                    }
                }
            }
            // Never contributes nothing: a failing branch has no value.
            Schema::Never => {}
            s => push(s, &mut flat),
        }
    }
    if flat.is_empty()
        || flat.iter().any(|s| matches!(s, Schema::Any))
        || flat.len() > UNION_VARIANT_CAP
    {
        return Schema::Any;
    }
    if flat.len() == 1 {
        flat.remove(0)
    } else {
        Schema::Union {
            variants: flat,
            discriminator: None,
        }
    }
}
fn project_schema(s: &Schema, f: &str) -> Option<Schema> {
    match s {
        Schema::Object {
            properties,
            additional,
            ..
        } => properties.get(f).map(|p| p.schema.clone()).or_else(|| {
            // An open object may carry keys the schema does not declare
            // (e.g. `fail` details on the catch error); their schema is
            // unknown, and absence stays a runtime failure.
            additional.then_some(Schema::Any)
        }),
        Schema::Map { values } => Some(*values.clone()),
        Schema::Any => Some(Schema::Any),
        Schema::Union { variants, .. } => {
            // Projectable when any variant is: the runtime value is a single
            // variant, and projection on the wrong one fails at runtime.
            let x: Vec<Schema> = variants
                .iter()
                .filter_map(|s| project_schema(s, f))
                .collect();
            if x.is_empty() { None } else { Some(union(x)) }
        }
        _ => None,
    }
}
/// The element schema a `for` loop binds when iterating `s`, or `None` if no
/// possible runtime value of `s` is iterable. Union collections iterate when
/// at least one variant does; the runtime rejects non-iterable values.
fn iterable_item_schema(s: &Schema) -> Option<Schema> {
    match s {
        Schema::List { items, .. } => Some((**items).clone()),
        Schema::Map { values } => Some(entry_schema((**values).clone())),
        Schema::Object { properties, .. } => Some(entry_schema(union(
            properties.values().map(|p| p.schema.clone()).collect(),
        ))),
        Schema::Any => Some(Schema::Any),
        Schema::Union { variants, .. } => {
            let items: Vec<Schema> = variants.iter().filter_map(iterable_item_schema).collect();
            if items.is_empty() {
                None
            } else {
                Some(union(items))
            }
        }
        _ => None,
    }
}

/// Rewrite hints for callables the standard library deliberately omits
/// (STDLIB.md sections 3 and 6): the capability exists, spelled with the
/// language's single way of expressing it.
fn stdlib_hint(name: &str) -> Option<&'static str> {
    Some(match name {
        "text.contains" => "use the `in` operator instead: `needle in s`",
        "list.sum" | "list.count" | "list.min" | "list.max" | "list.unique" | "list.flatten"
        | "list.reverse" | "list.compact" => {
            "reduce with a fold instead: `total = fold acc = 0 for x in xs { return acc + x }`"
        }
        "list.filter" | "list.map" => "use a `for` loop; `skip if condition` filters elements",
        "list.group_by" | "list.index_by" | "object.from_entries" => {
            "accumulate with computed keys: `fold acc = {} for x in xs { return acc + { [x.key]: x } }`"
        }
        "object.get" => "guard with membership: `o[key] if key in o else default`",
        "object.merge" => "merge with the `+` operator: `a + b` (the right side wins)",
        "object.keys" | "object.values" | "object.entries" => {
            "iterate the object instead: `for pair in obj { return pair.key }`"
        }
        "regex.find" | "regex.match" => {
            "use `regex.captures(s, pattern)` and read `.full` after a null check"
        }
        "number.abs" => "write it directly: `x if x >= 0 else 0 - x`",
        "number.clamp" => "write it directly: `lo if x < lo else (hi if x > hi else x)`",
        "number.min" | "number.max" | "math.min" | "math.max" => {
            "write it directly: `a if a < b else b`"
        }
        "time.now" => "wall time is host state; pass it in as a program input instead",
        "time.add" | "time.diff" => {
            "epoch milliseconds are plain integers: `t + 3 * 86400000` adds three days"
        }
        "number.format" | "text.format" | "text.pad" | "text.pad_start" => {
            "build strings with `+`; zero-pad with a slice: `text.slice(\"0\" + minutes, -2)`"
        }
        _ if name.ends_with(".get") => {
            "values have no methods; index instead: `value[\"key\"]`, guarded by `\"key\" in value`"
        }
        _ => return None,
    })
}

fn schema_fields(s: &Schema) -> Vec<String> {
    match s {
        Schema::Object { properties, .. } => properties.keys().cloned().collect(),
        _ => vec![],
    }
}
fn conversion_rank(a: &Schema, e: &Schema) -> Option<u8> {
    if a == e || matches!(e, Schema::Any) || matches!(a, Schema::Any) || matches!(a, Schema::Never)
    {
        Some(1)
    } else if matches!(
        (a, e),
        (Schema::Integer { .. }, Schema::Number { .. })
            | (
                Schema::Integer { .. }
                    | Schema::Number { .. }
                    | Schema::Boolean
                    | Schema::List { .. }
                    | Schema::Object { .. },
                Schema::String { .. }
            )
    ) {
        Some(3)
    } else if matches!(e,Schema::Union{variants,..} if variants.iter().any(|v|conversion_rank(a,v).is_some()))
    {
        Some(2)
    } else {
        // Structural conversions the runtime's `convert_value` performs; the
        // analyzer must not reject a program the runtime would execute.
        match (a, e) {
            // A union argument converts when any variant can: the runtime
            // value is a single variant, checked at dispatch time.
            (Schema::Union { variants, .. }, e) => variants
                .iter()
                .any(|variant| conversion_rank(variant, e).is_some())
                .then_some(2),
            // Same-kind schemas that differ only in constraints (enumerations,
            // bounds, lengths, formats): the analyzer cannot prove the value
            // level, so constraint checks defer to the runtime.
            (Schema::String { .. }, Schema::String { .. })
            | (Schema::Integer { .. }, Schema::Integer { .. })
            | (Schema::Number { .. }, Schema::Number { .. }) => Some(2),
            (
                Schema::List { items: actual, .. },
                Schema::List {
                    items: expected, ..
                },
            )
            | (Schema::Map { values: actual }, Schema::Map { values: expected }) => {
                conversion_rank(actual, expected).map(|_| 2)
            }
            (
                Schema::Object {
                    properties: actual,
                    additional: open,
                    ..
                },
                Schema::Object {
                    properties: expected,
                    required,
                    additional,
                },
            ) => {
                for (key, property) in actual {
                    match expected.get(key) {
                        Some(target) => {
                            if required.contains(key) {
                                conversion_rank(&property.schema, &target.schema)?;
                            } else {
                                optional_property_rank(&property.schema, &target.schema)?;
                            }
                        }
                        None if *additional => {}
                        None => return None,
                    }
                }
                if !open && required.iter().any(|key| !actual.contains_key(key)) {
                    return None;
                }
                Some(2)
            }
            (Schema::Object { properties, .. }, Schema::Map { values }) => {
                for property in properties.values() {
                    conversion_rank(&property.schema, values)?;
                }
                Some(2)
            }
            _ => None,
        }
    }
}
/// For an optional object property, `null` means "omit the key" at runtime
/// (`key: value if cond else null` is the only way to express a conditional
/// property), so only the non-null part of the actual schema must convert.
fn optional_property_rank(actual: &Schema, expected: &Schema) -> Option<u8> {
    match actual {
        Schema::Null => Some(2),
        Schema::Union { variants, .. } => {
            let non_null = variants
                .iter()
                .filter(|v| !matches!(v, Schema::Null))
                .collect::<Vec<_>>();
            if non_null.is_empty() {
                return Some(2);
            }
            non_null
                .iter()
                .any(|v| conversion_rank(v, expected).is_some())
                .then_some(2)
        }
        _ => conversion_rank(actual, expected),
    }
}
fn binary_schema(op: BinaryOp, l: &Schema, r: &Schema) -> Option<Schema> {
    use BinaryOp::*;
    // `Any` and unions defer to the runtime's value checks: comparisons still
    // produce Booleans, everything else stays `Any`.
    if matches!(l, Schema::Any | Schema::Union { .. } | Schema::Never)
        || matches!(r, Schema::Any | Schema::Union { .. } | Schema::Never)
    {
        return Some(match op {
            And | Or | Equal | NotEqual | Less | LessEqual | Greater | GreaterEqual | In => {
                Schema::Boolean
            }
            _ => Schema::Any,
        });
    }
    match op {
        And | Or if matches!((l, r), (Schema::Boolean, Schema::Boolean)) => Some(Schema::Boolean),
        Equal | NotEqual => Some(Schema::Boolean),
        Less | LessEqual | Greater | GreaterEqual
            if numeric(l) && numeric(r)
                || matches!((l, r), (Schema::String { .. }, Schema::String { .. })) =>
        {
            Some(Schema::Boolean)
        }
        In => Some(Schema::Boolean),
        Add if matches!((l, r), (Schema::String { .. }, Schema::String { .. })) => {
            Some(Schema::string())
        }
        Add if matches!(l, Schema::String { .. }) && string_formattable(r)
            || matches!(r, Schema::String { .. }) && string_formattable(l) =>
        {
            Some(Schema::string())
        }
        Add if matches!((l, r), (Schema::List { .. }, Schema::List { .. })) => {
            Some(Schema::list(Schema::Any))
        }
        // `object + object` merges shallowly; the right side wins on key
        // collisions. Any map operand makes the result a map.
        Add if matches!(
            (l, r),
            (
                Schema::Object { .. } | Schema::Map { .. },
                Schema::Object { .. } | Schema::Map { .. }
            )
        ) =>
        {
            Some(merge_schema(l, r))
        }
        Add | Subtract | Multiply if integer(l) && integer(r) => Some(Schema::INTEGER),
        Add | Subtract | Multiply | Divide if numeric(l) && numeric(r) => Some(Schema::NUMBER),
        Remainder if integer(l) && integer(r) => Some(Schema::INTEGER),
        _ => None,
    }
}
/// Unifies a fold accumulator's seed schema with the body's return schema.
///
/// The seed keeps its schema when the body converts back to it. Otherwise
/// one widening is allowed, always structural (never by formatting, so an
/// integer seed with a string body is still an error):
/// - a seed that converts structurally into the body's schema adopts it
///   (`{}` widens to the map the body merges);
/// - a `null` seed against a non-null body widens to `T | null` — the
///   find-first and last-wins idioms;
/// - two objects with the same property names unify property-by-property
///   under the same rules, so a `{ best: null, count: 0 }` state seed
///   widens its null fields.
///
/// The widening check runs before the convert-back check would succeed
/// spuriously: a union body result also "converts" into a bare seed under
/// the lenient any-variant rule, which would collapse the fold to the
/// seed's schema and hide the found values.
fn fold_unify(acc: &Schema, out: &Schema) -> Result<Schema, String> {
    if acc == out {
        return Ok(acc.clone());
    }
    // An `Any` body (untyped tool data flowing through) makes the whole
    // accumulator `Any` — the always-converts rank would otherwise collapse
    // it back to the seed's schema.
    if matches!(out, Schema::Any) {
        return Ok(Schema::Any);
    }
    // Structural widenings run before any conversion-rank check: the
    // lenient union-as-actual rank would otherwise "convert" a union body
    // back into the bare seed (at any nesting depth) and hide the found
    // values behind the seed's schema.
    match (acc, out) {
        (Schema::Null, other) | (other, Schema::Null) if !matches!(other, Schema::Null) => {
            return Ok(union(vec![other.clone(), Schema::Null]));
        }
        (
            Schema::Object {
                properties: a,
                required: ra,
                additional: aa,
            },
            Schema::Object {
                properties: b,
                required: rb,
                additional: ab,
            },
        ) if a.len() == b.len() && a.keys().eq(b.keys()) => {
            let mut properties = BTreeMap::new();
            for (key, property) in a {
                let unified = fold_unify(&property.schema, &b[key].schema)
                    .map_err(|detail| format!("property `{key}`: {detail}"))?;
                properties.insert(key.clone(), crate::Property::new(unified));
            }
            return Ok(Schema::Object {
                required: ra.union(rb).cloned().collect(),
                properties,
                additional: *aa || *ab,
            });
        }
        _ => {}
    }
    // A union body absorbs a seed that is one of its variants; a non-union
    // body that converts back keeps the seed's schema — including a
    // previously widened union seed. Widening by structural conversion
    // (never formatting) covers the rest (`{}` seed → map).
    if matches!(out, Schema::Union { .. }) && conversion_rank(acc, out).is_some_and(|r| r <= 2) {
        return Ok(out.clone());
    }
    if conversion_rank(out, acc).is_some() {
        return Ok(acc.clone());
    }
    if conversion_rank(acc, out).is_some_and(|rank| rank <= 2) {
        return Ok(out.clone());
    }
    Err(format!(
        "the initial value is {} but the body returns {}",
        acc.kind_name(),
        out.kind_name()
    ))
}

/// The schema of `left + right` when both operands are objects or maps:
/// shallow, right-biased. Two literal objects merge to the exact combined
/// object; a map on either side generalizes the result to a map over the
/// union of every value schema.
fn merge_schema(l: &Schema, r: &Schema) -> Schema {
    match (l, r) {
        (
            Schema::Object {
                properties: lp,
                required: lr,
                additional: la,
            },
            Schema::Object {
                properties: rp,
                required: rr,
                additional: ra,
            },
        ) => {
            let mut properties = lp.clone();
            let mut required = lr.clone();
            for (key, property) in rp {
                properties.insert(key.clone(), property.clone());
            }
            required.extend(rr.iter().cloned());
            Schema::Object {
                properties,
                required,
                additional: *la || *ra,
            }
        }
        _ => {
            let mut values = Vec::new();
            for side in [l, r] {
                match side {
                    Schema::Object { properties, .. } => {
                        values.extend(properties.values().map(|p| p.schema.clone()))
                    }
                    Schema::Map { values: v } => values.push((**v).clone()),
                    _ => {}
                }
            }
            Schema::Map {
                values: Box::new(union(values)),
            }
        }
    }
}
/// Whether a computed property key's schema can become a key string: strings
/// pass through, scalars convert to their canonical text form.
fn computed_key_schema(s: &Schema) -> bool {
    match s {
        Schema::String { .. }
        | Schema::Integer { .. }
        | Schema::Number { .. }
        | Schema::Boolean
        | Schema::Any
        | Schema::Never => true,
        Schema::Union { variants, .. } => variants.iter().all(computed_key_schema),
        _ => false,
    }
}
fn numeric(s: &Schema) -> bool {
    integer(s) || matches!(s, Schema::Number { .. })
}
fn integer(s: &Schema) -> bool {
    matches!(s, Schema::Integer { .. })
}
fn string_formattable(s: &Schema) -> bool {
    matches!(
        s,
        Schema::Integer { .. }
            | Schema::Number { .. }
            | Schema::Boolean
            | Schema::String { .. }
            | Schema::List { .. }
            | Schema::Object { .. }
            | Schema::Map { .. }
    )
}
fn entry_schema(v: Schema) -> Schema {
    let mut p = BTreeMap::new();
    p.insert("key".into(), crate::Property::new(Schema::string()));
    p.insert("value".into(), crate::Property::new(v));
    Schema::Object {
        properties: p,
        required: BTreeSet::from(["key".into(), "value".into()]),
        additional: false,
    }
}
fn error_schema() -> Schema {
    let mut p = BTreeMap::new();
    for (k, s) in [
        ("code", Schema::string()),
        ("message", Schema::string()),
        ("retryable", Schema::Boolean),
        ("node_id", Schema::string()),
        ("attempt", Schema::INTEGER),
        ("uncertain", Schema::Boolean),
        ("span", Schema::Any),
    ] {
        p.insert(k.into(), crate::Property::new(s));
    }
    Schema::Object {
        required: p.keys().cloned().collect(),
        properties: p,
        additional: true,
    }
}
fn closest(s: &str, mut names: Vec<String>) -> Vec<String> {
    names.sort_by_key(|n| lev(s, n));
    names.truncate(5);
    names
}
fn lev(a: &str, b: &str) -> usize {
    let mut d = (0..=b.chars().count()).collect::<Vec<_>>();
    for (i, x) in a.chars().enumerate() {
        let mut last = i;
        d[0] = i + 1;
        for (j, y) in b.chars().enumerate() {
            let old = d[j + 1];
            d[j + 1] = (d[j + 1] + 1).min(d[j] + 1).min(last + usize::from(x != y));
            last = old
        }
    }
    *d.last().unwrap()
}
