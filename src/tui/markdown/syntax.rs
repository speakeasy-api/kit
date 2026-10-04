//! Scope-based code colors: terminal palette only, never a bundled RGB theme.

use std::sync::OnceLock;

use ratatui::{style::Style, text::Span};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use super::super::theme;

// Bound input to the regex parser, not display/copy content. Once a line cannot
// be parsed, abandon that fence: resuming would use an invalid multiline state.
pub(super) const DOCUMENT_BUDGET: usize = 128 * 1024;
const LINE_LIMIT: usize = 4096;

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

pub(super) struct CodeSyntax {
    parser: Option<ParseState>,
    scopes: ScopeStack,
}

impl CodeSyntax {
    pub(super) fn plain() -> Self {
        Self {
            parser: None,
            scopes: ScopeStack::new(),
        }
    }

    pub(super) fn new(info: &str) -> Self {
        let token = info
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(token.as_str(), "" | "text" | "txt" | "plain" | "plaintext") {
            return Self::plain();
        }
        let set = syntaxes();
        let syntax = match token.as_str() {
            "ts" | "typescript" => set.find_syntax_by_name("TypeScript"),
            "tsx" => set.find_syntax_by_extension("tsx"),
            _ => set.find_syntax_by_token(&token),
        };
        Self {
            parser: syntax.map(ParseState::new),
            scopes: ScopeStack::new(),
        }
    }

    pub(super) fn line(&mut self, raw: &str, budget: &mut usize) -> Vec<Span<'static>> {
        let plain = || vec![Span::styled(raw.replace('\t', "    "), theme::code())];
        if raw.len() > LINE_LIMIT || raw.len() + 1 > *budget {
            self.parser = None;
        }
        let Some(parser) = self.parser.as_mut() else {
            return plain();
        };
        *budget -= raw.len() + 1;
        // The newline-aware grammar must see line endings, including the current
        // unterminated streaming line. Its synthetic newline is never displayed.
        let line = format!("{raw}\n");
        let Ok(operations) = parser.parse_line(&line, syntaxes()) else {
            self.parser = None;
            return plain();
        };
        let mut spans = Vec::new();
        let mut start = 0;
        for (offset, operation) in operations {
            let end = offset.min(raw.len());
            if end > start {
                push_span(&mut spans, &raw[start..end], scope_style(&self.scopes));
            }
            if self.scopes.apply(&operation).is_err() {
                self.parser = None;
                return plain();
            }
            start = end;
        }
        if start < raw.len() {
            push_span(&mut spans, &raw[start..], scope_style(&self.scopes));
        }
        if spans.is_empty() { plain() } else { spans }
    }
}

fn push_span(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    let text = text.replace('\t', "    ");
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(&text);
    } else {
        spans.push(Span::styled(text, style));
    }
}

fn scope_style(stack: &ScopeStack) -> Style {
    static STYLES: OnceLock<Vec<(Scope, Style)>> = OnceLock::new();
    let styles = STYLES.get_or_init(|| {
        [
            ("comment", theme::dim()),
            ("string", theme::code().fg(theme::success_color())),
            ("constant", theme::code().fg(theme::warn_color())),
            ("keyword", theme::bold(theme::accent_color())),
            ("storage", theme::bold(theme::accent_color())),
            ("entity.name.function", theme::accent()),
            ("entity.name.type", theme::code().fg(theme::user_color())),
            ("support", theme::code().fg(theme::user_color())),
            ("variable.language", theme::accent()),
            ("invalid", theme::code().fg(theme::error_color())),
        ]
        .into_iter()
        .filter_map(|(name, style)| Scope::new(name).ok().map(|scope| (scope, style)))
        .collect()
    });
    stack
        .as_slice()
        .iter()
        .rev()
        .find_map(|scope| {
            styles
                .iter()
                .find(|(prefix, _)| prefix.is_prefix_of(*scope))
                .map(|(_, style)| *style)
        })
        .unwrap_or_else(theme::code)
}

#[cfg(test)]
#[allow(clippy::disallowed_macros)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_budget_preserves_text_and_disables_current_fence() {
        let mut syntax = CodeSyntax::new("ts");
        let mut budget = "/*\n".len();
        let comment = syntax.line("/*", &mut budget);
        assert!(comment.iter().all(|span| span.style == theme::dim()));
        let fallback = syntax.line("*/ const 界 = 1;", &mut budget);
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].content, "*/ const 界 = 1;");
        assert_eq!(fallback[0].style, theme::code());
        // Even replenishing a caller's budget cannot resume a skipped state.
        let mut replenished = DOCUMENT_BUDGET;
        let later = syntax.line("const plain = 2;", &mut replenished);
        assert!(later.iter().all(|span| span.style == theme::code()));
    }
}
