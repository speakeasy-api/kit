//! Turns a `compose` script into the shape of its execution graph.
//!
//! Runlet builds a dataflow graph from the program: calls are nodes, loops fan
//! out, boundaries own a retryable subgraph. The script travels to the client
//! as the `compose` tool call's raw input, so the client parses it with the
//! same front end the runtime uses and lays the structure out as a tree. Live
//! dispatch state is layered on top of this by
//! [`super::app::ToolCall::attach`].

use runlet::{Block, Expr, ExprKind, ObjectKey, Stmt, StmtKind};

/// One row of the rendered program tree.
#[derive(Debug, Clone)]
pub struct PlanNode {
    pub depth: usize,
    pub kind: PlanKind,
    pub label: String,
    /// Zero-based source line containing this node.
    #[allow(dead_code)]
    pub source_line: usize,
    /// Variable introduced by this node, when it represents a binding.
    #[allow(dead_code)]
    pub binding: Option<String>,
    /// Host tool this node dispatches, when it calls one.
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    Binding,
    Call,
    Loop,
    Fold,
    After,
    Branch,
    Boundary,
    Return,
}

impl PlanKind {
    /// Glyph that marks the node's role in the program.
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Binding => "=",
            Self::Call => "◆",
            Self::Loop => "⇉",
            Self::Fold => "∑",
            Self::After => "→",
            Self::Branch => "◇",
            Self::Boundary => "⟳",
            Self::Return => "▸",
        }
    }
}

/// Parses a compose script into its plan tree, empty when it does not parse.
#[must_use]
pub fn parse(script: &str) -> Vec<PlanNode> {
    let Ok(program) = runlet::parse(script) else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for statement in &program.statements {
        statement_nodes(statement, 0, script, &mut nodes);
    }
    if !expression(&program.result, 0, "return ", None, script, &mut nodes) {
        nodes.push(PlanNode {
            depth: 0,
            kind: PlanKind::Return,
            label: format!("return {}", summary(&program.result)),
            source_line: source_line(script, program.result.span.start),
            binding: None,
            tool: None,
        });
    }
    nodes
}

fn statement_nodes(statement: &Stmt, depth: usize, script: &str, nodes: &mut Vec<PlanNode>) {
    match &statement.kind {
        StmtKind::Binding { name, value } => {
            let binding = (name != "_").then_some(name.as_str());
            if !expression(value, depth, &format!("{name} = "), binding, script, nodes) {
                nodes.push(PlanNode {
                    depth,
                    kind: PlanKind::Binding,
                    label: format!("{name} = {}", summary(value)),
                    source_line: source_line(script, statement.span.start),
                    binding: binding.map(str::to_owned),
                    tool: None,
                });
            }
        }
        StmtKind::Skip { condition } => {
            if let Some(condition) = condition {
                expression(condition, depth, "", None, script, nodes);
            }
        }
        StmtKind::Break { value, condition } => {
            if let Some(condition) = condition {
                expression(condition, depth, "", None, script, nodes);
            }
            expression(value, depth, "", None, script, nodes);
        }
        StmtKind::Assert { condition, message } => {
            expression(condition, depth, "", None, script, nodes);
            if let Some(message) = message {
                expression(message, depth, "", None, script, nodes);
            }
        }
    }
}

/// Emits nodes for the runtime-visible parts of `expr`.
///
/// Returns whether anything was emitted so a pure binding can add its own node.
fn expression(
    expr: &Expr,
    depth: usize,
    prefix: &str,
    binding: Option<&str>,
    script: &str,
    nodes: &mut Vec<PlanNode>,
) -> bool {
    match &expr.kind {
        ExprKind::Call { callee, arguments } => {
            let tool = callee_name(callee);
            if is_intrinsic(&tool) {
                return arguments.iter().fold(false, |emitted, argument| {
                    expression(argument, depth, prefix, binding, script, nodes) || emitted
                });
            }
            nodes.push(PlanNode {
                depth,
                kind: PlanKind::Call,
                label: format!("{prefix}{tool}({})", arguments_summary(arguments)),
                source_line: source_line(script, expr.span.start),
                binding: binding.map(str::to_owned),
                tool: Some(tool),
            });
            for argument in arguments {
                expression(argument, depth + 1, "", None, script, nodes);
            }
            true
        }
        ExprKind::For {
            binding: item_binding,
            collection,
            body,
        } => {
            nodes.push(PlanNode {
                depth,
                kind: PlanKind::Loop,
                label: format!("{prefix}for {item_binding} in {}", summary(collection)),
                source_line: source_line(script, expr.span.start),
                binding: binding.map(str::to_owned),
                tool: None,
            });
            expression(collection, depth + 1, "", None, script, nodes);
            block(body, depth + 1, script, nodes);
            true
        }
        ExprKind::Fold {
            accumulator,
            init,
            binding: item_binding,
            collection,
            body,
        } => {
            nodes.push(PlanNode {
                depth,
                kind: PlanKind::Fold,
                label: format!(
                    "{prefix}fold {accumulator} = {} for {item_binding} in {}",
                    summary(init),
                    summary(collection)
                ),
                source_line: source_line(script, expr.span.start),
                binding: binding.map(str::to_owned),
                tool: None,
            });
            expression(init, depth + 1, "", None, script, nodes);
            expression(collection, depth + 1, "", None, script, nodes);
            block(body, depth + 1, script, nodes);
            true
        }
        ExprKind::After { prerequisite, body } => {
            nodes.push(PlanNode {
                depth,
                kind: PlanKind::After,
                label: format!("{prefix}after {}", summary(prerequisite)),
                source_line: source_line(script, expr.span.start),
                binding: binding.map(str::to_owned),
                tool: None,
            });
            expression(prerequisite, depth + 1, "", None, script, nodes);
            block(body, depth + 1, script, nodes);
            true
        }
        ExprKind::Boundary {
            retries,
            body,
            error_binding,
            catch,
        } => {
            nodes.push(PlanNode {
                depth,
                kind: PlanKind::Boundary,
                label: format!("{prefix}boundary retry {retries}"),
                source_line: source_line(script, expr.span.start),
                binding: binding.map(str::to_owned),
                tool: None,
            });
            block(body, depth + 1, script, nodes);
            let mut fallback = Vec::new();
            block(catch, depth + 2, script, &mut fallback);
            if !fallback.is_empty() {
                nodes.push(PlanNode {
                    depth: depth + 1,
                    kind: PlanKind::Branch,
                    label: format!("catch {error_binding}"),
                    source_line: source_line(script, catch.span.start),
                    binding: None,
                    tool: None,
                });
                nodes.append(&mut fallback);
            }
            true
        }
        ExprKind::If {
            condition,
            then_block,
            else_block,
        } => {
            nodes.push(PlanNode {
                depth,
                kind: PlanKind::Branch,
                label: format!("{prefix}if {}", summary(condition)),
                source_line: source_line(script, expr.span.start),
                binding: binding.map(str::to_owned),
                tool: None,
            });
            expression(condition, depth + 1, "", None, script, nodes);
            block(then_block, depth + 1, script, nodes);
            if let Some(else_block) = else_block {
                let mut alternative = Vec::new();
                block(else_block, depth + 2, script, &mut alternative);
                if !alternative.is_empty() {
                    nodes.push(PlanNode {
                        depth: depth + 1,
                        kind: PlanKind::Branch,
                        label: "else".into(),
                        source_line: source_line(script, else_block.span.start),
                        binding: None,
                        tool: None,
                    });
                    nodes.append(&mut alternative);
                }
            }
            true
        }
        ExprKind::Conditional {
            then_expr,
            condition,
            else_expr,
        } => [then_expr, condition, else_expr]
            .into_iter()
            .fold(false, |emitted, part| {
                expression(part, depth, prefix, binding, script, nodes) || emitted
            }),
        ExprKind::List(items) => items.iter().fold(false, |emitted, item| {
            expression(item, depth, prefix, binding, script, nodes) || emitted
        }),
        ExprKind::Object(entries) => entries.iter().fold(false, |emitted, (key, value)| {
            let key_emitted = match key {
                ObjectKey::Computed(expr) => {
                    expression(expr, depth, prefix, binding, script, nodes)
                }
                ObjectKey::Static(_) => false,
            };
            expression(value, depth, prefix, binding, script, nodes) || key_emitted || emitted
        }),
        ExprKind::Member { target, .. } => {
            expression(target, depth, prefix, binding, script, nodes)
        }
        ExprKind::Index { target, index } => {
            let target_emitted = expression(target, depth, prefix, binding, script, nodes);
            expression(index, depth, prefix, binding, script, nodes) || target_emitted
        }
        ExprKind::Unary { value, .. } => expression(value, depth, prefix, binding, script, nodes),
        ExprKind::Binary { left, right, .. } => {
            let left_emitted = expression(left, depth, prefix, binding, script, nodes);
            expression(right, depth, prefix, binding, script, nodes) || left_emitted
        }
        ExprKind::Fail { arguments } => arguments.iter().fold(false, |emitted, argument| {
            expression(argument, depth, prefix, binding, script, nodes) || emitted
        }),
        _ => false,
    }
}

fn block(body: &Block, depth: usize, script: &str, nodes: &mut Vec<PlanNode>) {
    for statement in &body.statements {
        statement_nodes(statement, depth, script, nodes);
    }
    expression(&body.result, depth, "", None, script, nodes);
}

fn source_line(script: &str, byte_offset: usize) -> usize {
    script.as_bytes()[..byte_offset.min(script.len())]
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count()
}

fn is_intrinsic(name: &str) -> bool {
    matches!(
        name.split('.').next(),
        Some("text" | "regex" | "list" | "json" | "number" | "time")
    )
}

fn callee_name(callee: &Expr) -> String {
    match &callee.kind {
        ExprKind::Name(name) => name.clone(),
        ExprKind::Member { target, field } => format!("{}.{field}", callee_name(target)),
        _ => "call".into(),
    }
}

fn arguments_summary(arguments: &[Expr]) -> String {
    let rendered: Vec<String> = arguments.iter().map(summary).collect();
    clip(&rendered.join(", "), 56)
}

/// A compact one-line rendering of an expression for labels.
fn summary(expr: &Expr) -> String {
    let text = match &expr.kind {
        ExprKind::Null => "null".into(),
        ExprKind::Boolean(value) => value.to_string(),
        ExprKind::Integer(text) | ExprKind::Number(text) => text.clone(),
        ExprKind::String(text) => format!("\"{}\"", clip(&text.replace('\n', " "), 24)),
        ExprKind::Name(name) => name.clone(),
        ExprKind::List(items) if items.is_empty() => "[]".into(),
        ExprKind::List(_) => "[…]".into(),
        ExprKind::Object(entries) => object_summary(entries),
        ExprKind::Member { target, field } => format!("{}.{field}", summary(target)),
        ExprKind::Index { target, index } => format!("{}[{}]", summary(target), summary(index)),
        ExprKind::Call { callee, .. } => format!("{}(…)", callee_name(callee)),
        ExprKind::Unary { value, .. } => format!("…{}", summary(value)),
        ExprKind::Binary { left, right, .. } => format!("{} … {}", summary(left), summary(right)),
        _ => "…".into(),
    };
    clip(&text, 48)
}

fn object_summary(entries: &[(ObjectKey, Expr)]) -> String {
    if entries.is_empty() {
        return "{}".into();
    }
    let keys: Vec<String> = entries
        .iter()
        .take(2)
        .map(|(key, value)| match key {
            ObjectKey::Static(name) => format!("{name}: {}", summary(value)),
            ObjectKey::Computed(_) => format!("[…]: {}", summary(value)),
        })
        .collect();
    let ellipsis = if entries.len() > 2 { ", …" } else { "" };
    format!("{{ {}{ellipsis} }}", keys.join(", "))
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    format!("{}…", text.chars().take(limit).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::{PlanKind, parse};

    #[test]
    fn lays_out_calls_loops_and_boundaries() {
        let nodes = parse(
            "files = shell({ command: \"ls\" })\n\
             checked = for file in files.items {\n\
                 result = boundary retry 2 {\n\
                     return shell({ command: \"test \" + file })\n\
                 } catch error {\n\
                     return { failed: error.code }\n\
                 }\n\
                 return result\n\
             }\n\
             return checked",
        );
        let shape: Vec<_> = nodes
            .iter()
            .map(|node| (node.depth, node.kind, node.tool.clone()))
            .collect();
        assert_eq!(
            shape,
            [
                (0, PlanKind::Call, Some("shell".into())),
                (0, PlanKind::Loop, None),
                (1, PlanKind::Boundary, None),
                (2, PlanKind::Call, Some("shell".into())),
                (0, PlanKind::Return, None),
            ]
        );
    }

    #[test]
    fn includes_pure_bindings_with_variable_metadata() {
        let nodes = parse("total = 1 + 2\nreturn total");

        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].kind, PlanKind::Binding);
        assert_eq!(nodes[0].label, "total = 1 … 2");
        assert_eq!(nodes[0].binding.as_deref(), Some("total"));
        assert_eq!(nodes[0].source_line, 0);
        assert_eq!(nodes[1].kind, PlanKind::Return);
        assert_eq!(nodes[1].binding, None);
        assert_eq!(nodes[1].source_line, 1);
    }

    #[test]
    fn records_source_lines_and_bindings_without_changing_nesting() {
        let nodes = parse(
            "first = shell({ command: \"líst\" })\n\
             items = for item in first.items {\n\
                 result = boundary retry 1 {\n\
                     return shell({ command: item })\n\
                 } catch error {\n\
                     return log_error({ code: error.code })\n\
                 }\n\
                 return result\n\
             }\n\
             return items",
        );

        let metadata: Vec<_> = nodes
            .iter()
            .map(|node| {
                (
                    node.depth,
                    node.kind,
                    node.source_line,
                    node.binding.as_deref(),
                    node.tool.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            metadata,
            [
                (0, PlanKind::Call, 0, Some("first"), Some("shell")),
                (0, PlanKind::Loop, 1, Some("items"), None),
                (1, PlanKind::Boundary, 2, Some("result"), None),
                (2, PlanKind::Call, 3, None, Some("shell")),
                (2, PlanKind::Branch, 4, None, None),
                (3, PlanKind::Call, 5, None, Some("log_error")),
                (0, PlanKind::Return, 9, None, None),
            ]
        );
    }

    #[test]
    fn includes_after_bodies_fold_initializers_and_pure_intrinsics() {
        let nodes = parse(
            "config = json.parse(\"{}\")\n\
             seed = shell({ command: \"prepare\" })\n\
             published = after seed {\n\
                 return edit({ op: \"delete\", path: \"old\" })\n\
             }\n\
             total = fold acc = shell({ command: \"init\" }) for item in [] {\n\
                 return acc\n\
             }\n\
             return { config, published, total }",
        );

        assert_eq!(nodes[0].kind, PlanKind::Binding);
        assert_eq!(nodes[0].binding.as_deref(), Some("config"));
        assert!(nodes.iter().any(|node| node.kind == PlanKind::After));
        assert_eq!(
            nodes
                .iter()
                .filter_map(|node| node.tool.as_deref())
                .collect::<Vec<_>>(),
            ["shell", "edit", "shell"]
        );
    }

    #[test]
    fn includes_break_conditions_and_values() {
        let nodes = parse(
            "total = fold acc = 0 for item in [] {\n\
                 break finish({ acc }) if should_stop({ item })\n\
                 return acc + item\n\
             }\n\
             return total",
        );

        assert_eq!(
            nodes
                .iter()
                .filter_map(|node| node.tool.as_deref())
                .collect::<Vec<_>>(),
            ["should_stop", "finish"]
        );
    }

    #[test]
    fn tolerates_a_script_that_does_not_parse() {
        assert!(parse("this is ( not runlet").is_empty());
    }
}
