//! A small Markdown renderer for agent messages.
//!
//! Models answer in Markdown, so the transcript reads much better with
//! headings, lists, and code told apart. This covers the constructs that
//! actually show up in agent output and deliberately stops there: block
//! quotes, rules, fenced and inline code, bullets, and emphasis.

use super::{
    theme,
    wrap::{LinkedLine, LinkedSpan},
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

/// Renders Markdown source into styled transcript lines.
#[cfg(test)]
pub fn render(source: &str) -> Vec<Line<'static>> {
    render_linked(source)
        .into_iter()
        .map(|line| line.line())
        .collect()
}

/// Renders Markdown while attaching each URL directly to the spans it owns.
pub fn render_linked(source: &str) -> Vec<LinkedLine> {
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

fn block_line(raw: &str, trimmed: &str) -> LinkedLine {
    let indent = " ".repeat(raw.len() - trimmed.len());
    if trimmed.starts_with("---") && trimmed.chars().all(|character| character == '-') {
        return plain_line(Line::from(Span::styled("─".repeat(40), theme::faint())));
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
        let mut spans = vec![plain_span(format!("{indent}▏ "), theme::faint())];
        spans.extend(inline(quoted, theme::dim().add_modifier(Modifier::ITALIC)));
        return LinkedLine { spans };
    }
    if let Some(item) = bullet(trimmed) {
        let mut spans = vec![plain_span(format!("{indent}• "), theme::accent())];
        spans.extend(inline(item, theme::text()));
        return LinkedLine { spans };
    }
    LinkedLine {
        spans: inline(raw, theme::text()),
    }
}

fn heading_line(indent: String, heading: &str, style: Style) -> LinkedLine {
    let mut spans = vec![plain_span(indent, Style::default())];
    spans.extend(inline(heading, style));
    LinkedLine { spans }
}

fn bullet(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
}

fn code_line(raw: &str) -> LinkedLine {
    plain_line(Line::from(vec![
        Span::styled("  ", theme::code()),
        Span::styled(raw.replace('\t', "    "), theme::code()),
    ]))
}

fn code_frame(label: &str) -> LinkedLine {
    plain_line(Line::from(Span::styled(label.to_string(), theme::faint())))
}

fn plain_line(line: Line<'static>) -> LinkedLine {
    LinkedLine::plain(line)
}

fn plain_span(content: impl Into<std::borrow::Cow<'static, str>>, style: Style) -> LinkedSpan {
    LinkedSpan {
        span: Span::styled(content, style),
        url: None,
    }
}

fn link_span(
    content: impl Into<std::borrow::Cow<'static, str>>,
    style: Style,
    url: &str,
) -> LinkedSpan {
    LinkedSpan {
        span: Span::styled(content, style),
        url: Some(url.to_string()),
    }
}

struct Link<'a> {
    start: usize,
    end: usize,
    label: Option<&'a str>,
    url: &'a str,
}

fn next_markdown_link(source: &str) -> Option<Link<'_>> {
    let mut offset = 0;
    while let Some(relative_start) = source[offset..].find('[') {
        let start = offset + relative_start;
        let label_end = source[start + 1..].find(']').map(|end| start + 1 + end)?;
        if !source[label_end..].starts_with("](") {
            offset = label_end + 1;
            continue;
        }
        let url_start = label_end + 2;
        let mut depth = 0;
        let mut url_end = None;
        for (relative, character) in source[url_start..].char_indices() {
            match character {
                '(' => depth += 1,
                ')' if depth == 0 => {
                    url_end = Some(url_start + relative);
                    break;
                }
                ')' => depth -= 1,
                _ => {}
            }
        }
        let url_end = url_end?;
        let url = &source[url_start..url_end];
        if url.starts_with("https://") || url.starts_with("http://") {
            return Some(Link {
                start,
                end: url_end + 1,
                label: Some(&source[start + 1..label_end]),
                url,
            });
        }
        offset = url_end + 1;
    }
    None
}

fn next_link(source: &str) -> Option<Link<'_>> {
    let bare = ["https://", "http://"]
        .into_iter()
        .filter_map(|scheme| source.find(scheme))
        .min()
        .map(|start| {
            let mut end = source[start..]
                .find(char::is_whitespace)
                .map_or(source.len(), |length| start + length);
            loop {
                let Some(character) = source[..end].chars().next_back() else {
                    break;
                };
                let unmatched_close = character == ')'
                    && source[start..end].chars().filter(|&c| c == ')').count()
                        > source[start..end].chars().filter(|&c| c == '(').count();
                if matches!(
                    character,
                    '.' | ',' | ';' | ':' | '!' | '?' | ']' | '}' | '\'' | '"'
                ) || unmatched_close
                {
                    end -= character.len_utf8();
                } else {
                    break;
                }
            }
            Link {
                start,
                end,
                label: None,
                url: &source[start..end],
            }
        });
    let markdown = next_markdown_link(source);
    match (bare, markdown) {
        (Some(bare), Some(markdown)) if markdown.start < bare.start => Some(markdown),
        (Some(bare), _) => Some(bare),
        (None, markdown) => markdown,
    }
}

/// Splits inline links, emphasis, and code spans out of one line of Markdown.
fn inline(source: &str, base: Style) -> Vec<LinkedSpan> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut rest = source;
    while !rest.is_empty() {
        let marker = rest
            .find(['`', '*', '_'])
            .map(|index| (index, &rest[index..]));
        if let Some(link) = next_link(rest)
            && marker.as_ref().is_none_or(|(index, _)| link.start < *index)
        {
            plain.push_str(&rest[..link.start]);
            if !plain.is_empty() {
                spans.push(plain_span(std::mem::take(&mut plain), base));
            }
            let link_style = theme::accent().add_modifier(Modifier::UNDERLINED);
            if let Some(label) = link.label {
                spans.push(link_span(label.to_string(), link_style, link.url));
                spans.push(plain_span(" (", base));
                spans.push(link_span(link.url.to_string(), link_style, link.url));
                spans.push(plain_span(")", base));
            } else {
                spans.push(link_span(link.url.to_string(), link_style, link.url));
            }
            rest = &rest[link.end..];
            continue;
        }
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
        if delimiter == "_"
            && (rest[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
                || tail[1..].chars().next().is_none_or(char::is_whitespace))
        {
            plain.push_str(&rest[..index + 1]);
            rest = &rest[index + 1..];
            continue;
        }
        let body = &tail[delimiter.len()..];
        // Emphasis needs the markers hugging the text, so arithmetic like
        // `2 * 3 * 4` stays arithmetic instead of turning italic.
        let paired = if delimiter == "_" {
            body.match_indices('_').map(|(index, _)| index).find(|end| {
                *end > 0
                    && !body[..*end].ends_with(char::is_whitespace)
                    && body[*end + 1..]
                        .chars()
                        .next()
                        .is_none_or(|character| !character.is_alphanumeric())
            })
        } else {
            body.find(delimiter).filter(|end| {
                *end > 0
                    && (delimiter == "`"
                        || (!body.starts_with(char::is_whitespace)
                            && !body[..*end].ends_with(char::is_whitespace)))
            })
        };
        let Some(close) = paired else {
            plain.push_str(&rest[..index + delimiter.len()]);
            rest = &rest[index + delimiter.len()..];
            continue;
        };
        plain.push_str(&rest[..index]);
        if !plain.is_empty() {
            spans.push(plain_span(std::mem::take(&mut plain), base));
        }
        spans.push(plain_span(body[..close].to_string(), style));
        rest = &body[close + delimiter.len()..];
    }
    if !plain.is_empty() {
        spans.push(plain_span(plain, base));
    }
    if spans.is_empty() {
        spans.push(plain_span(String::new(), base));
    }
    spans
}

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;

    use super::{render, render_linked};

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

    #[test]
    fn underscores_only_emphasize_at_word_boundaries() {
        for source in ["not_like this_", "snake_case"] {
            let joined: String = render(source)[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(joined, source);
        }
        let internal = &render("_like_this_")[0];
        assert_eq!(internal.spans[0].content, "like_this");
        assert!(
            internal.spans[0]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        let emphasized = &render("Use _this phrase_ now")[0];
        assert_eq!(emphasized.spans[1].content, "this phrase");
        assert!(
            emphasized.spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn preserves_underscores_inside_urls() {
        let url = "https://example.com/authorize?response_type=code&client_id=kit&code_challenge_method=S256&redirect_uri=http%3A%2F%2F127.0.0.1";
        let line = &render(url)[0];
        let joined: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(joined, url);
        assert!(
            line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
    }

    fn linked_urls(source: &str) -> Vec<String> {
        render_linked(source)
            .into_iter()
            .flat_map(|line| line.spans)
            .filter_map(|span| span.url)
            .collect()
    }

    #[test]
    fn attaches_the_exact_url_to_markdown_link_spans() {
        let source = "Open [Linear](https://linear.app/docs/mcp).";
        let line = &render(source)[0];
        let joined: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(joined, "Open Linear (https://linear.app/docs/mcp).");
        assert_eq!(
            linked_urls(source),
            ["https://linear.app/docs/mcp", "https://linear.app/docs/mcp"]
        );
    }

    #[test]
    fn parses_balanced_parentheses_in_link_destinations() {
        assert_eq!(
            linked_urls("[docs](https://example.com/a_(b))"),
            ["https://example.com/a_(b)", "https://example.com/a_(b)"]
        );
    }

    #[test]
    fn bare_links_keep_balanced_parentheses_and_trim_sentence_punctuation() {
        assert_eq!(
            linked_urls("See (https://example.com/a_(b))."),
            ["https://example.com/a_(b)"]
        );
        assert_eq!(
            linked_urls("Try https://example.com/path?!"),
            ["https://example.com/path"]
        );
        assert_eq!(
            linked_urls("Read \"https://example.com/docs\""),
            ["https://example.com/docs"]
        );
    }
}
