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
    /// Host tool this node dispatches, when it calls one.
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    Call,
    Loop,
    Fold,
    Branch,
    Boundary,
    Return,
}

impl PlanKind {
    /// Glyph that marks the node's role in the program.
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Call => "◆",
            Self::Loop => "⇉",
            Self::Fold => "∑",
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
        statement_nodes(statement, 0, &mut nodes);
    }
    if !expression(&program.result, 0, "return ", &mut nodes) {
        nodes.push(PlanNode {
            depth: 0,
            kind: PlanKind::Return,
            label: format!("return {}", summary(&program.result)),
            tool: None,
        });
    }
    nodes
}

fn statement_nodes(statement: &Stmt, depth: usize, nodes: &mut Vec<PlanNode>) {
    match &statement.kind {
        StmtKind::Binding { name, value } => {
            expression(value, depth, &format!("{name} = "), nodes);
        }
        StmtKind::Skip { condition } => {
            if let Some(condition) = condition {
                expression(condition, depth, "", nodes);
            }
        }
        StmtKind::Assert { condition, message } => {
            expression(condition, depth, "", nodes);
            if let Some(message) = message {
                expression(message, depth, "", nodes);
            }
        }
    }
}

/// Emits nodes for the runtime-visible parts of `expr`.
///
/// Returns whether anything was emitted: a binding whose value is pure
/// arithmetic has no graph presence and is left out of the tree entirely.
fn expression(expr: &Expr, depth: usize, prefix: &str, nodes: &mut Vec<PlanNode>) -> bool {
    match &expr.kind {
        ExprKind::Call { callee, arguments } => {
            let tool = callee_name(callee);
            nodes.push(PlanNode {
                depth,
                kind: PlanKind::Call,
                label: format!("{prefix}{tool}({})", arguments_summary(arguments)),
                tool: Some(tool),
            });
            for argument in arguments {
                expression(argument, depth + 1, "", nodes);
            }
            true
        }
        ExprKind::For {
            binding,
            collection,
            body,
        } => {
            nodes.push(PlanNode {
                depth,
                kind: PlanKind::Loop,
                label: format!("{prefix}for {binding} in {}", summary(collection)),
                tool: None,
            });
            expression(collection, depth + 1, "", nodes);
            block(body, depth + 1, nodes);
            true
        }
        ExprKind::Fold {
            accumulator,
            init,
            binding,
            collection,
            body,
        } => {
            nodes.push(PlanNode {
                depth,
                kind: PlanKind::Fold,
                label: format!(
                    "{prefix}fold {accumulator} = {} for {binding} in {}",
                    summary(init),
                    summary(collection)
                ),
                tool: None,
            });
            expression(collection, depth + 1, "", nodes);
            block(body, depth + 1, nodes);
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
                tool: None,
            });
            block(body, depth + 1, nodes);
            let mut fallback = Vec::new();
            block(catch, depth + 2, &mut fallback);
            if !fallback.is_empty() {
                nodes.push(PlanNode {
                    depth: depth + 1,
                    kind: PlanKind::Branch,
                    label: format!("catch {error_binding}"),
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
                tool: None,
            });
            expression(condition, depth + 1, "", nodes);
            block(then_block, depth + 1, nodes);
            if let Some(else_block) = else_block {
                let mut alternative = Vec::new();
                block(else_block, depth + 2, &mut alternative);
                if !alternative.is_empty() {
                    nodes.push(PlanNode {
                        depth: depth + 1,
                        kind: PlanKind::Branch,
                        label: "else".into(),
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
                expression(part, depth, prefix, nodes) || emitted
            }),
        ExprKind::List(items) => items.iter().fold(false, |emitted, item| {
            expression(item, depth, prefix, nodes) || emitted
        }),
        ExprKind::Object(entries) => entries.iter().fold(false, |emitted, (key, value)| {
            let key_emitted = match key {
                ObjectKey::Computed(expr) => expression(expr, depth, prefix, nodes),
                ObjectKey::Static(_) => false,
            };
            expression(value, depth, prefix, nodes) || key_emitted || emitted
        }),
        ExprKind::Member { target, .. } => expression(target, depth, prefix, nodes),
        ExprKind::Index { target, index } => {
            let target_emitted = expression(target, depth, prefix, nodes);
            expression(index, depth, prefix, nodes) || target_emitted
        }
        ExprKind::Unary { value, .. } => expression(value, depth, prefix, nodes),
        ExprKind::Binary { left, right, .. } => {
            let left_emitted = expression(left, depth, prefix, nodes);
            expression(right, depth, prefix, nodes) || left_emitted
        }
        ExprKind::Fail { arguments } => arguments.iter().fold(false, |emitted, argument| {
            expression(argument, depth, prefix, nodes) || emitted
        }),
        _ => false,
    }
}

fn block(body: &Block, depth: usize, nodes: &mut Vec<PlanNode>) {
    for statement in &body.statements {
        statement_nodes(statement, depth, nodes);
    }
    expression(&body.result, depth, "", nodes);
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
    fn leaves_out_bindings_with_no_runtime_work() {
        let nodes = parse("total = 1 + 2\nreturn total");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].kind, PlanKind::Return);
    }

    #[test]
    fn tolerates_a_script_that_does_not_parse() {
        assert!(parse("this is ( not runlet").is_empty());
    }
}
