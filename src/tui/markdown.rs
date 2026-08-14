//! A small Markdown renderer for agent messages.
//!
//! Models answer in Markdown, so the transcript reads much better with
//! headings, lists, and code told apart. This covers the constructs that
//! actually show up in agent output and deliberately stops there: block
//! quotes, rules, fenced and inline code, bullets, and emphasis.

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::theme;

/// Renders Markdown source into styled transcript lines.
pub fn render(source: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut fence: Option<String> = None;
    for raw in source.split('\n') {
        let trimmed = raw.trim_start();
        if let Some(language) = fence.as_ref() {
            if trimmed.starts_with("```") {
                lines.push(code_frame(&format!(
                    "└─ {}",
                    if language.is_empty() {
                        "code"
                    } else {
                        language
                    }
                )));
                fence = None;
            } else {
                lines.push(code_line(raw));
            }
            continue;
        }
        if let Some(language) = trimmed.strip_prefix("```") {
            let language = language.trim().to_string();
            lines.push(code_frame(&format!(
                "┌─ {}",
                if language.is_empty() {
                    "code"
                } else {
                    &language
                }
            )));
            fence = Some(language);
            continue;
        }
        lines.push(block_line(raw, trimmed));
    }
    if fence.is_some() {
        lines.push(code_frame("└─ code"));
    }
    lines
}

fn block_line(raw: &str, trimmed: &str) -> Line<'static> {
    let indent = " ".repeat(raw.len() - trimmed.len());
    if trimmed.starts_with("---") && trimmed.chars().all(|character| character == '-') {
        return Line::from(Span::styled("─".repeat(40), theme::faint()));
    }
    if let Some(heading) = trimmed.strip_prefix("### ") {
        return heading_line(indent, heading, theme::bold(theme::TEXT));
    }
    if let Some(heading) = trimmed.strip_prefix("## ") {
        return heading_line(indent, heading, theme::bold(theme::ACCENT));
    }
    if let Some(heading) = trimmed.strip_prefix("# ") {
        return heading_line(indent, heading, theme::bold(theme::ACCENT));
    }
    if let Some(quoted) = trimmed.strip_prefix("> ") {
        let mut spans = vec![Span::styled(format!("{indent}▏ "), theme::faint())];
        spans.extend(inline(quoted, theme::dim().add_modifier(Modifier::ITALIC)));
        return Line::from(spans);
    }
    if let Some(item) = bullet(trimmed) {
        let mut spans = vec![Span::styled(format!("{indent}• "), theme::accent())];
        spans.extend(inline(item, theme::text()));
        return Line::from(spans);
    }
    Line::from(inline(raw, theme::text()))
}

fn heading_line(indent: String, heading: &str, style: Style) -> Line<'static> {
    let mut spans = vec![Span::raw(indent)];
    spans.extend(inline(heading, style));
    Line::from(spans)
}

fn bullet(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
}

fn code_line(raw: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ", theme::code()),
        Span::styled(raw.replace('\t', "    "), theme::code()),
    ])
}

fn code_frame(label: &str) -> Line<'static> {
    Line::from(Span::styled(label.to_string(), theme::faint()))
}

/// Splits inline emphasis and code spans out of one line of Markdown.
fn inline(source: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut rest = source;
    while !rest.is_empty() {
        let marker = rest
            .find(['`', '*', '_'])
            .map(|index| (index, &rest[index..]));
        let Some((index, tail)) = marker else {
            plain.push_str(rest);
            break;
        };
        let (delimiter, style) = if tail.starts_with("**") {
            ("**", base.add_modifier(Modifier::BOLD))
        } else if tail.starts_with('`') {
            ("`", theme::code())
        } else if tail.starts_with('*') {
            ("*", base.add_modifier(Modifier::ITALIC))
        } else {
            ("_", base.add_modifier(Modifier::ITALIC))
        };
        let body = &tail[delimiter.len()..];
        // Emphasis needs the markers hugging the text, so arithmetic like
        // `2 * 3 * 4` stays arithmetic instead of turning italic.
        let paired = body.find(delimiter).filter(|end| {
            *end > 0
                && (delimiter == "`"
                    || (!body.starts_with(char::is_whitespace)
                        && !body[..*end].ends_with(char::is_whitespace)))
        });
        let Some(close) = paired else {
            plain.push_str(&rest[..index + delimiter.len()]);
            rest = &rest[index + delimiter.len()..];
            continue;
        };
        plain.push_str(&rest[..index]);
        if !plain.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut plain), base));
        }
        spans.push(Span::styled(body[..close].to_string(), style));
        rest = &body[close + delimiter.len()..];
    }
    if !plain.is_empty() {
        spans.push(Span::styled(plain, base));
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn styles_code_fences_apart_from_prose() {
        let lines = render("text\n```rust\nlet x = 1;\n```\nmore");
        assert_eq!(lines.len(), 5);
        assert!(lines[1].spans[0].content.contains("rust"));
        assert!(lines[2].spans[1].content.contains("let x = 1;"));
    }

    #[test]
    fn splits_inline_code_and_emphasis() {
        let line = &render("run `cargo test` and **stop**")[0];
        let contents: Vec<_> = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(contents, ["run ", "cargo test", " and ", "stop"]);
    }

    #[test]
    fn leaves_unpaired_markers_as_text() {
        let line = &render("2 * 3 * 4 = 24")[0];
        let joined: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(joined, "2 * 3 * 4 = 24");
    }
}
