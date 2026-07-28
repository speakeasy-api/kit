use crate::diagnostic::{Diagnostic, Phase, Span};
use crate::lexer::{Token, TokenKind as T, lex};
use crate::syntax::*;

/// Parses Runlet source into its syntax tree.
///
/// Returns structured diagnostics when the source is not valid Runlet.
pub fn parse(source: &str) -> Result<Program, Vec<Diagnostic>> {
    let tokens = lex(source)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        diagnostics: vec![],
        depth: 0,
        skip_allowed: false,
    };
    let program = p.program();
    if p.diagnostics.is_empty() {
        program.ok_or_else(|| vec![p.expected("program")])
    } else {
        Err(p.diagnostics)
    }
}

/// Maximum expression nesting the recursive-descent parser accepts. Tracked
/// explicitly so a pathologically nested program (e.g. thousands of opening
/// parentheses) produces a diagnostic instead of overflowing the native call
/// stack. Sized so the guard trips while debug-build parser frames still fit
/// a 2 MiB thread stack; far above anything a legitimate program nests.
const MAX_PARSE_DEPTH: usize = 64;

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    diagnostics: Vec<Diagnostic>,
    depth: usize,
    /// Whether `skip` is valid here: true inside `for`/`fold` bodies, false
    /// at program level and inside `boundary` blocks (a skip never crosses a
    /// boundary).
    skip_allowed: bool,
}

impl Parser {
    fn program(&mut self) -> Option<Program> {
        self.separators();
        let start = self.current().span.start;
        let mut statements = vec![];
        while !self.at(&T::Return) && !self.at(&T::Eof) {
            let before = self.pos;
            let mark = self.diagnostics.len();
            if let Some(s) = self.let_stmt() {
                statements.push(s);
            } else if let Some(value) = self.bare_expr_recovery(before, mark) {
                self.expression_statement_diagnostic(&value);
            } else {
                self.recover();
            }
            if self.pos == before {
                // `recover()` stops at tokens like an unmatched `}` that this
                // loop cannot consume either; error recovery must always make
                // progress or it loops forever accumulating diagnostics.
                self.bump();
            }
            self.require_separator();
            self.separators();
        }
        if !self.take(&T::Return) {
            self.diagnostics
                .push(self.expected("a final `return` statement"));
            return None;
        }
        let result = self.expr(0)?;
        self.separators();
        if !self.at(&T::Eof) {
            self.diagnostics.push(
                Diagnostic::error(
                    "RL1012",
                    Phase::Parse,
                    self.current().span,
                    "early or duplicate return",
                    "a block has exactly one final return",
                )
                .with_fix(
                    self.current().span,
                    "",
                    "remove statements after return",
                ),
            );
        }
        Some(Program {
            span: Span::new(start, result.span.end),
            statements,
            result,
        })
    }
    fn let_stmt(&mut self) -> Option<Stmt> {
        if self.at(&T::Skip) {
            return self.skip_stmt();
        }
        if self.at(&T::Assert) {
            return self.assert_stmt();
        }
        let (name, start) = match self.current().kind.clone() {
            T::Ident(s) => {
                let sp = self.bump().span;
                (s, sp)
            }
            T::If | T::For | T::Fold | T::Boundary => {
                let keyword = self.current().kind.clone();
                let guidance = match keyword {
                    T::If => {
                        "`if` is not a statement; bind the conditional's \
                         result: `x = if condition { ... } else { ... }` — \
                         inside loops, `skip if condition` filters"
                    }
                    _ => {
                        "control structures are expressions; bind the result \
                         to a name: `results = for item in items { ... }`"
                    }
                };
                self.diagnostics.push(Diagnostic::error(
                    "RL1014",
                    Phase::Parse,
                    self.current().span,
                    "control structures are expressions",
                    guidance,
                ));
                // One construct, one diagnostic: consume the whole
                // statement-form construct so its body does not get
                // re-parsed as loose statements.
                self.bump();
                self.skip_construct();
                return None;
            }
            _ => {
                self.diagnostics
                    .push(self.expected("a binding such as `name = expression`"));
                return None;
            }
        };
        if !self.take(&T::Equal) {
            self.diagnostics
                .push(self.expected("`=` after the binding name"));
            return None;
        }
        let value = self.expr(0)?;
        Some(Stmt {
            span: start.join(value.span),
            kind: StmtKind::Binding { name, value },
        })
    }
    fn skip_stmt(&mut self) -> Option<Stmt> {
        let start = self.bump().span;
        if !self.skip_allowed {
            self.diagnostics.push(Diagnostic::error(
                "RL1018",
                Phase::Parse,
                start,
                "skip outside a loop body",
                "`skip` is only valid directly inside a `for` or `fold` body \
                 and cannot cross a `boundary`",
            ));
            return None;
        }
        let condition = if self.take(&T::If) {
            Some(self.expr(1)?)
        } else {
            None
        };
        let end = condition.as_ref().map(|c| c.span).unwrap_or(start);
        Some(Stmt {
            span: start.join(end),
            kind: StmtKind::Skip { condition },
        })
    }
    fn assert_stmt(&mut self) -> Option<Stmt> {
        let start = self.bump().span;
        self.expect_take(&T::LParen, "`(` after `assert`")?;
        let condition = self.expr(0)?;
        let message = if self.take(&T::Comma) {
            Some(self.expr(0)?)
        } else {
            None
        };
        let end = self.expect_take(&T::RParen, "`)` after assertion")?.span;
        Some(Stmt {
            span: start.join(end),
            kind: StmtKind::Assert { condition, message },
        })
    }
    fn block(&mut self) -> Option<Block> {
        let start = self.expect_take(&T::LBrace, "`{`")?.span;
        self.separators();
        let mut statements = vec![];
        while !self.at(&T::Return) && !self.at(&T::RBrace) && !self.at(&T::Eof) {
            let before = self.pos;
            let mark = self.diagnostics.len();
            if let Some(s) = self.let_stmt() {
                statements.push(s);
            } else if let Some(value) = self.bare_expr_recovery(before, mark) {
                let after_expr = self.pos;
                self.separators();
                if !self.at(&T::RBrace) {
                    self.pos = after_expr;
                }
                if self.at(&T::RBrace) {
                    // A trailing bare expression is an intended result with
                    // the `return` keyword missing (Rust/JS habit): one
                    // diagnostic with a machine fix, and the expression
                    // becomes the block result so analysis continues.
                    self.diagnostics.push(
                        Diagnostic::error(
                            "RL1017",
                            Phase::Parse,
                            value.span,
                            "a block must return a value",
                            "write `return expression` as the final statement",
                        )
                        .with_fix(
                            Span::new(value.span.start, value.span.start),
                            "return ",
                            "insert `return`",
                        ),
                    );
                    let end = self.expect_take(&T::RBrace, "`}` after block return")?.span;
                    return Some(Block {
                        statements,
                        result: Box::new(value),
                        span: start.join(end),
                    });
                }
                self.expression_statement_diagnostic(&value);
            } else {
                self.recover();
            }
            if self.pos == before {
                // See program(): recovery must always make progress.
                self.bump();
            }
            self.require_separator();
            self.separators();
        }
        if !self.take(&T::Return) {
            // The loop stopped at `}`: the block has statements but no
            // result at all, so the mechanical fix is an explicit null.
            let d = Diagnostic::error(
                "RL1017",
                Phase::Parse,
                self.current().span,
                "a block must return a value",
                "write `return expression` as the final statement",
            )
            .with_fix(
                Span::new(self.current().span.start, self.current().span.start),
                "return null\n",
                "insert `return null`",
            );
            self.diagnostics.push(d);
            return None;
        }
        let result = Box::new(self.expr(0)?);
        self.separators();
        let end = self.expect_take(&T::RBrace, "`}` after block return")?.span;
        Some(Block {
            statements,
            result,
            span: start.join(end),
        })
    }
    fn expr(&mut self, min_bp: u8) -> Option<Expr> {
        if self.depth >= MAX_PARSE_DEPTH {
            let span = self.current().span;
            self.diagnostics.push(Diagnostic::error(
                "RL1015",
                Phase::Parse,
                span,
                "expression too deeply nested",
                format!("expression nesting exceeds the limit of {MAX_PARSE_DEPTH}"),
            ));
            return None;
        }
        self.depth += 1;
        let result = self.expr_inner(min_bp);
        self.depth -= 1;
        result
    }
    fn expr_inner(&mut self, min_bp: u8) -> Option<Expr> {
        let mut lhs = self.prefix()?;
        loop {
            if min_bp == 0 && self.take(&T::If) {
                let condition = self.expr(1)?;
                // `else` is optional: `value if condition` defaults the
                // alternative to null, so conditional effects and object
                // properties need no explicit null arm.
                let else_expr = if self.take(&T::Else) {
                    self.expr(0)?
                } else {
                    Expr {
                        kind: ExprKind::Null,
                        span: condition.span,
                    }
                };
                let span = lhs.span.join(else_expr.span);
                lhs = Expr {
                    span,
                    kind: ExprKind::Conditional {
                        then_expr: Box::new(lhs),
                        condition: Box::new(condition),
                        else_expr: Box::new(else_expr),
                    },
                };
                continue;
            }
            let Some((l, r, op)) = self.infix() else {
                break;
            };
            if l < min_bp {
                break;
            }
            let op_span = self.bump().span;
            let rhs = self.expr(r)?;
            if is_chain_op(op)
                && matches!(lhs.kind, ExprKind::Binary { op: old, .. } if is_chain_op(old))
            {
                self.diagnostics.push(Diagnostic::error(
                    "RL2306",
                    Phase::Analyze,
                    op_span,
                    "chained comparisons are not supported",
                    "join explicit comparisons with `and`, for example `a < b and b < c`",
                ));
            }
            let span = lhs.span.join(rhs.span);
            lhs = Expr {
                span,
                kind: ExprKind::Binary {
                    op,
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                },
            };
        }
        Some(lhs)
    }
    fn prefix(&mut self) -> Option<Expr> {
        let tok = self.bump().clone();
        let mut e = match tok.kind {
            T::Null => Expr {
                span: tok.span,
                kind: ExprKind::Null,
            },
            T::True => Expr {
                span: tok.span,
                kind: ExprKind::Boolean(true),
            },
            T::False => Expr {
                span: tok.span,
                kind: ExprKind::Boolean(false),
            },
            T::Integer(v) => Expr {
                span: tok.span,
                kind: ExprKind::Integer(v),
            },
            T::Number(v) => Expr {
                span: tok.span,
                kind: ExprKind::Number(v),
            },
            T::String(v) => Expr {
                span: tok.span,
                kind: ExprKind::String(v),
            },
            T::Ident(v) => Expr {
                span: tok.span,
                kind: ExprKind::Name(v),
            },
            T::Minus | T::Not => {
                let op = if matches!(tok.kind, T::Minus) {
                    UnaryOp::Negate
                } else {
                    UnaryOp::Not
                };
                let value = self.expr(13)?;
                Expr {
                    span: tok.span.join(value.span),
                    kind: ExprKind::Unary {
                        op,
                        value: Box::new(value),
                    },
                }
            }
            T::LParen => {
                let e = self.expr(0)?;
                self.expect_take(&T::RParen, "`)`")?;
                e
            }
            T::LBracket => self.list(tok.span)?,
            T::LBrace => self.object(tok.span)?,
            T::For => self.for_expr(tok.span)?,
            T::Fold => self.fold_expr(tok.span)?,
            T::Fail => self.fail_expr(tok.span)?,
            T::Boundary => self.boundary(tok.span)?,
            T::If => self.if_expr(tok.span)?,
            _ => {
                self.pos -= 1;
                self.diagnostics.push(self.expected("an expression"));
                return None;
            }
        };
        loop {
            if self.take(&T::Dot) {
                let (field, sp) = self.field_name()?;
                let span = e.span.join(sp);
                e = Expr {
                    span,
                    kind: ExprKind::Member {
                        target: Box::new(e),
                        field,
                    },
                };
            } else if self.take(&T::LBracket) {
                let index = self.expr(0)?;
                let end = self.expect_take(&T::RBracket, "`]`")?.span;
                let span = e.span.join(end);
                e = Expr {
                    span,
                    kind: ExprKind::Index {
                        target: Box::new(e),
                        index: Box::new(index),
                    },
                };
            } else if self.take(&T::LParen) {
                let mut arguments = vec![];
                if !self.at(&T::RParen) {
                    loop {
                        arguments.push(self.expr(0)?);
                        if !self.take(&T::Comma) {
                            break;
                        }
                        if self.at(&T::RParen) {
                            break;
                        }
                    }
                }
                let end = self.expect_take(&T::RParen, "`)` after arguments")?.span;
                let span = e.span.join(end);
                e = Expr {
                    span,
                    kind: ExprKind::Call {
                        callee: Box::new(e),
                        arguments,
                    },
                };
            } else {
                break;
            }
        }
        Some(e)
    }
    fn list(&mut self, start: Span) -> Option<Expr> {
        let mut values = vec![];
        self.separators();
        if !self.at(&T::RBracket) {
            loop {
                values.push(self.expr(0)?);
                self.separators();
                if !self.take(&T::Comma) {
                    break;
                }
                self.separators();
                if self.at(&T::RBracket) {
                    break;
                }
            }
        }
        let end = self.expect_take(&T::RBracket, "`]`")?.span;
        Some(Expr {
            span: start.join(end),
            kind: ExprKind::List(values),
        })
    }
    fn object(&mut self, start: Span) -> Option<Expr> {
        let mut values = vec![];
        self.separators();
        if !self.at(&T::RBrace) {
            loop {
                if self.take(&T::LBracket) {
                    // Computed key: `[expr]: value`. Collisions resolve at
                    // runtime (last entry wins), so no duplicate check here.
                    let key = self.expr(0)?;
                    self.expect_take(&T::RBracket, "`]` after computed property key")?;
                    self.expect_take(&T::Colon, "`:` after property name")?;
                    let value = self.expr(0)?;
                    values.push((ObjectKey::Computed(key), value));
                    self.separators();
                    if !self.take(&T::Comma) {
                        break;
                    }
                    self.separators();
                    if self.at(&T::RBrace) {
                        break;
                    }
                    continue;
                }
                let (key, key_span, shorthand) = match self.current().kind.clone() {
                    T::String(s) => {
                        let sp = self.bump().span;
                        (s, sp, false)
                    }
                    T::Ident(s) => {
                        let sp = self.bump().span;
                        let shorthand = !self.at(&T::Colon);
                        (s, sp, shorthand)
                    }
                    _ if is_field_token(&self.current().kind) => {
                        let (s, sp) = self.field_name()?;
                        (s, sp, false)
                    }
                    _ => {
                        self.diagnostics
                            .push(self.expected("an object property name"));
                        return None;
                    }
                };
                let value = if shorthand {
                    Expr {
                        span: key_span,
                        kind: ExprKind::Name(key.clone()),
                    }
                } else {
                    self.expect_take(&T::Colon, "`:` after property name")?;
                    self.expr(0)?
                };
                if values
                    .iter()
                    .any(|(k, _)| matches!(k, ObjectKey::Static(s) if s == &key))
                {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "RL2203",
                            Phase::Analyze,
                            key_span,
                            "duplicate object property",
                            format!("property `{key}` is already defined"),
                        )
                        .with_fix(
                            key_span.join(value.span),
                            "",
                            "remove the duplicate property",
                        ),
                    );
                }
                values.push((ObjectKey::Static(key), value));
                self.separators();
                if !self.take(&T::Comma) {
                    break;
                }
                self.separators();
                if self.at(&T::RBrace) {
                    break;
                }
            }
        }
        let end = self.expect_take(&T::RBrace, "`}`")?.span;
        Some(Expr {
            span: start.join(end),
            kind: ExprKind::Object(values),
        })
    }
    /// `if condition { ... } [else if ...]* [else { ... }]` as an
    /// expression. Branch blocks end with `return` like every block; a
    /// missing `else` yields `null`; `else if` desugars to an else block
    /// whose result is the nested conditional.
    fn if_expr(&mut self, start: Span) -> Option<Expr> {
        let condition = Box::new(self.expr(0)?);
        let then_block = self.block()?;
        let mut end = then_block.span;
        // `else` may sit on the next line; consume separators only when it
        // actually follows, so the caller still sees its statement terminator.
        let checkpoint = self.pos;
        self.separators();
        if !self.at(&T::Else) {
            self.pos = checkpoint;
        }
        let else_block = if self.take(&T::Else) {
            if self.at(&T::If) {
                let keyword = self.bump().span;
                let nested = self.if_expr(keyword)?;
                let span = nested.span;
                end = span;
                Some(Block {
                    statements: vec![],
                    result: Box::new(nested),
                    span,
                })
            } else {
                let block = self.block()?;
                end = block.span;
                Some(block)
            }
        } else {
            None
        };
        Some(Expr {
            span: start.join(end),
            kind: ExprKind::If {
                condition,
                then_block,
                else_block,
            },
        })
    }
    fn for_expr(&mut self, start: Span) -> Option<Expr> {
        let binding = match self.bump().kind.clone() {
            T::Ident(s) => s,
            _ => {
                self.pos -= 1;
                self.diagnostics.push(self.expected("loop binding name"));
                return None;
            }
        };
        self.expect_take(&T::In, "`in`")?;
        let collection = Box::new(self.expr(0)?);
        let body = self.loop_body()?;
        Some(Expr {
            span: start.join(body.span),
            kind: ExprKind::For {
                binding,
                collection,
                body,
            },
        })
    }
    fn fold_expr(&mut self, start: Span) -> Option<Expr> {
        let accumulator = match self.bump().kind.clone() {
            T::Ident(s) => s,
            _ => {
                self.pos -= 1;
                self.diagnostics
                    .push(self.expected("accumulator name, as in `fold acc = 0 for x in xs`"));
                return None;
            }
        };
        self.expect_take(&T::Equal, "`=` after the accumulator name")?;
        let init = Box::new(self.expr(0)?);
        self.expect_take(&T::For, "`for` after the accumulator initial value")?;
        let binding = match self.bump().kind.clone() {
            T::Ident(s) => s,
            _ => {
                self.pos -= 1;
                self.diagnostics.push(self.expected("loop binding name"));
                return None;
            }
        };
        self.expect_take(&T::In, "`in`")?;
        let collection = Box::new(self.expr(0)?);
        let body = self.loop_body()?;
        Some(Expr {
            span: start.join(body.span),
            kind: ExprKind::Fold {
                accumulator,
                init,
                binding,
                collection,
                body,
            },
        })
    }
    fn fail_expr(&mut self, start: Span) -> Option<Expr> {
        self.expect_take(
            &T::LParen,
            "`(` — fail is called like a tool: `fail(code, message)`",
        )?;
        let mut arguments = vec![];
        if !self.at(&T::RParen) {
            loop {
                arguments.push(self.expr(0)?);
                if !self.take(&T::Comma) {
                    break;
                }
                if self.at(&T::RParen) {
                    break;
                }
            }
        }
        let end = self
            .expect_take(&T::RParen, "`)` after fail arguments")?
            .span;
        Some(Expr {
            span: start.join(end),
            kind: ExprKind::Fail { arguments },
        })
    }
    /// Parses a `for`/`fold` body with `skip` enabled.
    fn loop_body(&mut self) -> Option<Block> {
        let saved = self.skip_allowed;
        self.skip_allowed = true;
        let body = self.block();
        self.skip_allowed = saved;
        body
    }
    fn boundary(&mut self, start: Span) -> Option<Expr> {
        let retries = if self.take(&T::Retry) {
            match self.bump().kind.clone() {
                T::Integer(s) => s.parse().unwrap_or(u32::MAX),
                _ => {
                    self.diagnostics.push(self.expected("integer retry count"));
                    0
                }
            }
        } else {
            0
        };
        // A skip inside a boundary would abandon retry accounting mid-flight;
        // boundary blocks re-disable it.
        let saved = self.skip_allowed;
        self.skip_allowed = false;
        let body = self.block();
        self.skip_allowed = saved;
        let body = body?;
        if self.at(&T::Newline) || self.at(&T::Semicolon) {
            self.diagnostics.push(Diagnostic::error(
                "RL1019",
                Phase::Parse,
                self.current().span,
                "`catch` must follow the boundary on the same line",
                "write `} catch err {` without a statement break",
            ));
            self.separators();
        }
        self.expect_take(&T::Catch, "`catch` after boundary body")?;
        let error_binding = match self.bump().kind.clone() {
            T::Ident(s) => s,
            _ => {
                self.diagnostics.push(self.expected("catch error binding"));
                return None;
            }
        };
        let saved = self.skip_allowed;
        self.skip_allowed = false;
        let catch = self.block();
        self.skip_allowed = saved;
        let catch = catch?;
        Some(Expr {
            span: start.join(catch.span),
            kind: ExprKind::Boundary {
                retries,
                body,
                error_binding,
                catch,
            },
        })
    }
    fn infix(&self) -> Option<(u8, u8, BinaryOp)> {
        Some(match self.current().kind {
            T::Or => (1, 2, BinaryOp::Or),
            T::And => (3, 4, BinaryOp::And),
            T::EqualEqual => (5, 6, BinaryOp::Equal),
            T::BangEqual => (5, 6, BinaryOp::NotEqual),
            T::Less => (7, 8, BinaryOp::Less),
            T::LessEqual => (7, 8, BinaryOp::LessEqual),
            T::Greater => (7, 8, BinaryOp::Greater),
            T::GreaterEqual => (7, 8, BinaryOp::GreaterEqual),
            T::In => (7, 8, BinaryOp::In),
            T::Plus => (9, 10, BinaryOp::Add),
            T::Minus => (9, 10, BinaryOp::Subtract),
            T::Star => (11, 12, BinaryOp::Multiply),
            T::Slash => (11, 12, BinaryOp::Divide),
            T::Percent => (11, 12, BinaryOp::Remainder),
            _ => return None,
        })
    }
    fn field_name(&mut self) -> Option<(String, Span)> {
        let t = self.bump().clone();
        let s = match t.kind {
            T::Ident(s) => s,
            T::Return => "return".into(),
            T::For => "for".into(),
            T::In => "in".into(),
            T::Boundary => "boundary".into(),
            T::Fold => "fold".into(),
            T::Skip => "skip".into(),
            T::Assert => "assert".into(),
            T::Fail => "fail".into(),
            T::Retry => "retry".into(),
            T::Catch => "catch".into(),
            T::If => "if".into(),
            T::Else => "else".into(),
            T::And => "and".into(),
            T::Or => "or".into(),
            T::Not => "not".into(),
            T::Null => "null".into(),
            T::True => "true".into(),
            T::False => "false".into(),
            _ => {
                self.pos -= 1;
                self.diagnostics.push(self.expected("field name"));
                return None;
            }
        };
        Some((s, t.span))
    }
    fn current(&self) -> &Token {
        &self.tokens[self.pos]
    }
    fn bump(&mut self) -> &Token {
        let i = self.pos;
        if !self.at(&T::Eof) {
            self.pos += 1;
        }
        &self.tokens[i]
    }
    fn at(&self, kind: &T) -> bool {
        std::mem::discriminant(&self.current().kind) == std::mem::discriminant(kind)
    }
    fn take(&mut self, kind: &T) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect_take(&mut self, kind: &T, what: &str) -> Option<Token> {
        if self.at(kind) {
            Some(self.bump().clone())
        } else {
            self.diagnostics.push(self.expected(what));
            None
        }
    }
    fn expected(&self, what: &str) -> Diagnostic {
        Diagnostic::error(
            "RL1008",
            Phase::Parse,
            self.current().span,
            "unexpected token",
            format!("expected {what}"),
        )
        .with_fix(
            Span::new(self.current().span.start, self.current().span.start),
            "",
            format!("insert {what}"),
        )
    }
    fn separators(&mut self) {
        while self.at(&T::Newline) || self.at(&T::Semicolon) {
            self.bump();
        }
    }
    fn require_separator(&mut self) {
        if !(self.at(&T::Newline)
            || self.at(&T::Semicolon)
            || self.at(&T::RBrace)
            || self.at(&T::Eof))
        {
            self.diagnostics.push(self.expected("a newline or `;`"));
        }
    }
    /// Fallback when a statement failed to parse as a binding: recognize a
    /// bare expression so the caller can issue one construct-level
    /// diagnostic instead of token soup. Restores the original diagnostics
    /// when no expression parses either.
    fn bare_expr_recovery(&mut self, checkpoint: usize, mark: usize) -> Option<Expr> {
        // Statement-form control keywords already got construct-level
        // recovery in let_stmt (the construct was consumed whole); rewinding
        // here would undo that and shred the construct's body.
        if matches!(
            self.tokens[checkpoint].kind,
            T::If | T::For | T::Fold | T::Boundary
        ) {
            return None;
        }
        self.pos = checkpoint;
        let held = self.diagnostics.split_off(mark);
        match self.expr(0) {
            // The bare-expression reading only wins when it explains the
            // whole statement — i.e. it ends at a statement boundary.
            // Otherwise the original failure (e.g. a missing return deep
            // inside a binding's block) is the truthful diagnosis.
            Some(value)
                if matches!(
                    self.current().kind,
                    T::Newline | T::Semicolon | T::RBrace | T::Eof
                ) =>
            {
                Some(value)
            }
            _ => {
                self.diagnostics.truncate(mark);
                self.diagnostics.extend(held);
                self.pos = checkpoint;
                None
            }
        }
    }
    fn expression_statement_diagnostic(&mut self, value: &Expr) {
        self.diagnostics.push(
            Diagnostic::error(
                "RL1019",
                Phase::Parse,
                value.span,
                "statements are bindings",
                "an expression cannot stand alone; bind its result to a name: \
                 `result = expression`",
            )
            .with_fix(
                Span::new(value.span.start, value.span.start),
                "",
                "bind the result to a name",
            ),
        );
    }
    fn recover(&mut self) {
        while !matches!(
            self.current().kind,
            T::Newline | T::Semicolon | T::RBrace | T::Eof
        ) {
            self.bump();
        }
    }
    /// Consumes the remainder of a statement-form construct after its
    /// keyword: header tokens up to the opening `{`, the balanced brace
    /// group, and any `else`/`catch` continuations (including `else if`
    /// chains). Recovery for familiar-but-invalid statement syntax emits
    /// one construct-level diagnostic instead of re-parsing the body as
    /// loose statements.
    fn skip_construct(&mut self) {
        loop {
            while !matches!(
                self.current().kind,
                T::LBrace | T::Newline | T::Semicolon | T::Eof
            ) {
                self.bump();
            }
            if !self.at(&T::LBrace) {
                return;
            }
            let mut depth = 0usize;
            loop {
                match self.current().kind {
                    T::LBrace => depth += 1,
                    T::RBrace => {
                        depth -= 1;
                        if depth == 0 {
                            self.bump();
                            break;
                        }
                    }
                    T::Eof => return,
                    _ => {}
                }
                self.bump();
            }
            // Continuations may sit on the next line; only consume the
            // separators when one actually follows.
            let checkpoint = self.pos;
            self.separators();
            match self.current().kind {
                T::Else | T::Catch => {
                    self.bump();
                    // `else if cond { ... }` re-enters the header scan;
                    // `catch err { ... }` skips the error binding first.
                    if let T::Ident(_) = self.current().kind {
                        self.bump();
                    }
                }
                _ => {
                    self.pos = checkpoint;
                    return;
                }
            }
        }
    }
}

fn is_chain_op(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterEqual
            | BinaryOp::In
    )
}
fn is_field_token(t: &T) -> bool {
    matches!(
        t,
        T::Return
            | T::For
            | T::In
            | T::Boundary
            | T::Fold
            | T::Skip
            | T::Assert
            | T::Fail
            | T::Retry
            | T::Catch
            | T::If
            | T::Else
            | T::And
            | T::Or
            | T::Not
            | T::Null
            | T::True
            | T::False
    )
}
