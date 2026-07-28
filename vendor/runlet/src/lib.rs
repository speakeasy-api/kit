//! Runlet's executable semantic model.
//!
//! The runtime keeps deterministic values and ordered results while allowing
//! bounded loop subgraphs to execute concurrently. Durable journals and remote
//! production executors remain later phases described in `DESIGN.md`.
//!
//! # Quick start
//!
//! ```
//! use runlet::{CanonicalValue, Runtime};
//!
//! let runtime = Runtime::builder().with_prelude().build()?;
//! let program = runtime.compile(r#"return text.upper("runlet")"#)
//!     .expect("valid Runlet source");
//! let execution = runtime.run(&program)?;
//!
//! assert_eq!(execution.value, CanonicalValue::from("RUNLET"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![warn(missing_docs)]

mod analyzer;
mod diagnostic;
mod graph;
mod heal;
mod lexer;
mod parser;
mod prelude;
mod runtime;
mod schema;
mod syntax;
mod value;

pub use analyzer::{CompiledProgram, ExternalInput, compile};
pub use diagnostic::{Diagnostic, Fix, Phase, Severity, Span};
pub use graph::{Edge, EdgeKind, Graph, GraphChange, GraphEvent, Node, NodeKind, NodeState};
pub use heal::{Healed, heal};
pub use parser::parse;
pub use runtime::{Execution, Runtime, RuntimeBuilder, ToolContext, ToolError};
pub use schema::{CallSchema, ExecutionPolicy, Property, Schema, ToolDescriptor, ToolRegistry};
pub use syntax::{BinaryOp, Block, Expr, ExprKind, ObjectKey, Program, Stmt, StmtKind, UnaryOp};
pub use value::{CanonicalValue, ValueError};

/// Language semantic version implemented by this crate.
pub const LANGUAGE_VERSION: &str = "1";
/// Canonical value encoding version implemented by this crate.
pub const VALUE_ENCODING_VERSION: &str = "rcve-v1";
