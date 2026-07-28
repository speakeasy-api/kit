use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
/// Half-open byte range in the original UTF-8 source.
pub struct Span {
    /// Inclusive starting byte offset.
    pub start: usize,
    /// Exclusive ending byte offset.
    pub end: usize,
}

impl Span {
    /// Creates a span covering `start..end`.
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
    /// Returns the smallest span containing both inputs.
    pub fn join(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Diagnostic importance.
pub enum Severity {
    /// A problem that prevents compilation or execution.
    Error,
    /// A non-fatal issue worth surfacing to the caller.
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Pipeline phase that produced a diagnostic.
pub enum Phase {
    /// Source parsing.
    Parse,
    /// Static schema and name analysis.
    Analyze,
    /// Execution planning.
    Plan,
    /// Runtime evaluation.
    Execute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// A source edit that may resolve a diagnostic.
pub struct Fix {
    /// Source range to replace.
    pub span: Span,
    /// Replacement text.
    pub replacement: String,
    /// Human-readable description of the edit.
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Structured parse, analysis, planning, or execution feedback.
pub struct Diagnostic {
    /// Stable Runlet diagnostic code.
    pub code: String,
    /// Short summary suitable for a heading.
    pub title: String,
    /// Detailed explanation.
    pub message: String,
    /// Pipeline phase that emitted the diagnostic.
    pub phase: Phase,
    /// Whether the diagnostic is fatal.
    pub severity: Severity,
    /// Most relevant source range.
    pub primary_span: Span,
    #[serde(default)]
    /// Suggested source edits.
    pub fixes: Vec<Fix>,
    #[serde(default)]
    /// Candidate names for unknown-name diagnostics.
    pub candidates: Vec<String>,
}

impl Diagnostic {
    /// Constructs an error diagnostic without fixes or candidates.
    pub fn error(
        code: &str,
        phase: Phase,
        span: Span,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            title: title.into(),
            message: message.into(),
            phase,
            severity: Severity::Error,
            primary_span: span,
            fixes: vec![],
            candidates: vec![],
        }
    }
    /// Constructs a warning diagnostic without fixes or candidates.
    pub fn warning(
        code: &str,
        phase: Phase,
        span: Span,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let mut d = Self::error(code, phase, span, title, message);
        d.severity = Severity::Warning;
        d
    }
    /// Appends a suggested source edit.
    pub fn with_fix(
        mut self,
        span: Span,
        replacement: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        self.fixes.push(Fix {
            span,
            replacement: replacement.into(),
            message: message.into(),
        });
        self
    }
    /// Replaces the diagnostic's suggested-name candidates.
    pub fn with_candidates(mut self, candidates: Vec<String>) -> Self {
        self.candidates = candidates;
        self
    }
}
