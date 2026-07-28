use crate::Span;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Parsed Runlet compilation unit.
pub struct Program {
    /// Statements evaluated before the final result.
    pub statements: Vec<Stmt>,
    /// Required final `return` expression.
    pub result: Expr,
    /// Source range covering the complete program.
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Lexically scoped sequence of statements ending in a result expression.
pub struct Block {
    /// Statements evaluated within the block.
    pub statements: Vec<Stmt>,
    /// Expression returned by the block.
    pub result: Box<Expr>,
    /// Source range covering the complete block.
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Source statement and its location.
pub struct Stmt {
    /// Statement payload.
    pub kind: StmtKind,
    /// Source range covering the statement.
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Kind-specific statement data.
pub enum StmtKind {
    /// Immutable local binding.
    Binding {
        /// Name introduced in the current scope.
        name: String,
        /// Lazily evaluated bound expression.
        value: Expr,
    },
    /// `skip [if condition]` — abandons the current `for` iteration (the
    /// element is dropped from the loop result) or `fold` iteration (the
    /// accumulator passes through unchanged). Only valid inside loop bodies
    /// and never across a `boundary`.
    Skip {
        /// Optional condition; absent means always skip.
        condition: Option<Expr>,
    },
    /// `assert(condition[, message])` — eagerly checks a runtime invariant.
    Assert {
        /// Boolean condition that must hold.
        condition: Expr,
        /// Optional failure message, evaluated only when the assertion fails.
        message: Option<Expr>,
    },
}

impl Stmt {
    /// The name/value pair when this statement is a binding.
    pub fn binding(&self) -> Option<(&String, &Expr)> {
        match &self.kind {
            StmtKind::Binding { name, value } => Some((name, value)),
            StmtKind::Skip { .. } | StmtKind::Assert { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Source expression and its location.
pub struct Expr {
    /// Expression payload.
    pub kind: ExprKind,
    /// Source range covering the expression.
    pub span: Span,
}

/// An object-literal property key: written literally (`name:` / `"name":`)
/// or computed at runtime (`[expr]:`). Computed keys evaluate to a string;
/// scalar values (integers, numbers, booleans) convert to their canonical
/// text form. When keys collide, the last entry wins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ObjectKey {
    /// Key written directly in source.
    Static(String),
    /// Expression evaluated and converted to a key at runtime.
    Computed(Expr),
}

impl ObjectKey {
    /// The key expression when this key is computed.
    pub fn expr(&self) -> Option<&Expr> {
        match self {
            ObjectKey::Static(_) => None,
            ObjectKey::Computed(e) => Some(e),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Kind-specific expression data.
pub enum ExprKind {
    /// `null` literal.
    Null,
    /// Boolean literal.
    Boolean(bool),
    /// Integer literal preserved as source text until analysis.
    Integer(String),
    /// Number literal preserved as source text until analysis.
    Number(String),
    /// String literal after escape decoding.
    String(String),
    /// Local, input, or namespace name.
    Name(String),
    /// List literal.
    List(Vec<Expr>),
    /// Object literal in source order.
    Object(Vec<(ObjectKey, Expr)>),
    /// Named member projection.
    Member {
        /// Value being projected.
        target: Box<Expr>,
        /// Property name.
        field: String,
    },
    /// Dynamic index projection.
    Index {
        /// Value being indexed.
        target: Box<Expr>,
        /// Index or property-key expression.
        index: Box<Expr>,
    },
    /// Registered tool or intrinsic call.
    Call {
        /// Qualified callable name expression.
        callee: Box<Expr>,
        /// Positional arguments.
        arguments: Vec<Expr>,
    },
    /// Prefix unary operation.
    Unary {
        /// Operator to apply.
        op: UnaryOp,
        /// Operand expression.
        value: Box<Expr>,
    },
    /// Infix binary operation.
    Binary {
        /// Operator to apply.
        op: BinaryOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// Postfix conditional expression.
    Conditional {
        /// Value produced when the condition is true.
        then_expr: Box<Expr>,
        /// Boolean condition.
        condition: Box<Expr>,
        /// Value produced when the condition is false.
        else_expr: Box<Expr>,
    },
    /// `if condition { ... } else { ... }` — the block-bodied conditional
    /// expression, for branches too large for a postfix conditional. Each
    /// branch is a full block ending in `return`; only the selected branch
    /// evaluates (its effect roots included). A missing `else` yields
    /// `null`. `else if` chains parse as an else block whose result is a
    /// nested `If`.
    If {
        /// Boolean condition selecting a branch.
        condition: Box<Expr>,
        /// Block evaluated when the condition is true.
        then_block: Block,
        /// Optional block evaluated when the condition is false.
        else_block: Option<Block>,
    },
    /// Ordered, host-bounded concurrent map over a collection.
    For {
        /// Per-iteration element binding.
        binding: String,
        /// Collection expression.
        collection: Box<Expr>,
        /// Per-element body.
        body: Block,
    },
    /// `fold acc = init for item in items { ... return next_acc }` — a
    /// sequential left fold. Each iteration binds a fresh accumulator; the
    /// body's return becomes the next accumulator; an empty collection yields
    /// the initial value. The sequential counterpart to `for`.
    Fold {
        /// Accumulator binding visible in the body.
        accumulator: String,
        /// Initial accumulator expression.
        init: Box<Expr>,
        /// Per-iteration element binding.
        binding: String,
        /// Collection expression.
        collection: Box<Expr>,
        /// Sequential reduction body.
        body: Block,
    },
    /// `fail(code, message[, details])` — raises a catchable, non-retryable
    /// error, exactly like a failing tool call. Type `Never`.
    Fail {
        /// Code, message, and optional details expressions.
        arguments: Vec<Expr>,
    },
    /// Retryable subgraph with a catch fallback.
    Boundary {
        /// Maximum number of retries after the initial attempt.
        retries: u32,
        /// Protected block.
        body: Block,
        /// Name bound to structured failure data in the catch block.
        error_binding: String,
        /// Fallback block evaluated after an unrecovered failure.
        catch: Block,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Prefix unary operator.
pub enum UnaryOp {
    /// Numeric negation (`-value`).
    Negate,
    /// Boolean negation (`not value`).
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Infix binary operator.
pub enum BinaryOp {
    /// Addition, string/list concatenation, or object merge.
    Add,
    /// Numeric subtraction.
    Subtract,
    /// Numeric multiplication.
    Multiply,
    /// Numeric division.
    Divide,
    /// Integer remainder.
    Remainder,
    /// Equality comparison.
    Equal,
    /// Inequality comparison.
    NotEqual,
    /// Strict less-than comparison.
    Less,
    /// Less-than-or-equal comparison.
    LessEqual,
    /// Strict greater-than comparison.
    Greater,
    /// Greater-than-or-equal comparison.
    GreaterEqual,
    /// Membership test.
    In,
    /// Short-circuiting Boolean conjunction.
    And,
    /// Short-circuiting Boolean disjunction.
    Or,
}
