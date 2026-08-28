//! A small Markdown renderer for agent messages.
//!
//! Models answer in Markdown, so the transcript reads much better with
//! headings, lists, and code told apart. This covers the constructs that
//! actually show up in agent output and deliberately stops there: block
//! quotes, rules, fenced and inline code, bullets, and emphasis.

use std::ops::Range;

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
#[cfg(test)]
pub fn render_linked(source: &str) -> Vec<LinkedLine> {
    render_copyable(source)
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

/// Renders Markdown and tags every row of a fenced code block with its exact
/// source content, excluding the fence and language label.
pub fn render_copyable(source: &str) -> Vec<(LinkedLine, Option<Range<usize>>)> {
    let mut next_offset = 0;
    let raw_lines: Vec<(usize, &str)> = source
        .split('\n')
        .map(|raw| {
            let offset = next_offset;
            next_offset += raw.len() + 1;
            (offset, raw)
        })
        .collect();
    let mut lines = Vec::new();
    let mut fence: Option<(String, char, usize, Range<usize>)> = None;
    for (index, (offset, raw)) in raw_lines.iter().copied().enumerate() {
        let trimmed = raw.trim_start();
        let candidate = fence_line(raw);
        if let Some((language, marker, length, content)) = fence.as_ref() {
            if candidate.is_some_and(|line| closing_fence(line, *marker, *length)) {
                lines.push((
                    code_frame(&format!(
                        "└─ {}",
                        if language.is_empty() {
                            "code"
                        } else {
                            language
                        }
                    )),
                    Some(content.clone()),
                ));
                fence = None;
            } else {
                lines.push((code_line(raw), Some(content.clone())));
            }
            continue;
        }
        if let Some((marker, length, language)) = candidate.and_then(opening_fence) {
            let content_start = (offset + raw.len() + 1).min(source.len());
            let closing_offset = raw_lines[index + 1..]
                .iter()
                .find(|(_, line)| {
                    fence_line(line).is_some_and(|line| closing_fence(line, marker, length))
                })
                .map(|(offset, _)| *offset);
            let content_end = closing_offset
                .map(|end| {
                    let before = &source[..end];
                    let line_ending = if before.ends_with("\r\n") { 2 } else { 1 };
                    end.saturating_sub(line_ending).max(content_start)
                })
                .unwrap_or(source.len());
            let content = content_start..content_end;
            let language = language.trim().to_string();
            lines.push((
                code_frame(&format!(
                    "┌─ {}",
                    if language.is_empty() {
                        "code"
                    } else {
                        &language
                    }
                )),
                Some(content.clone()),
            ));
            fence = Some((language, marker, length, content));
            continue;
        }
        lines.push((block_line(raw, trimmed), None));
    }
    if let Some((_, _, _, content)) = fence {
        lines.push((code_frame("└─ code"), Some(content)));
    }
    lines
}

fn fence_line(line: &str) -> Option<&str> {
    let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
    (spaces <= 3).then(|| &line[spaces..])
}

fn opening_fence(line: &str) -> Option<(char, usize, &str)> {
    let marker = line.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let length = line
        .chars()
        .take_while(|character| *character == marker)
        .count();
    (length >= 3).then(|| (marker, length, &line[length..]))
}

fn closing_fence(line: &str, marker: char, minimum: usize) -> bool {
    let length = line
        .chars()
        .take_while(|character| *character == marker)
        .count();
    length >= minimum && line[length..].trim().is_empty()
}

fn block_line(raw: &str, trimmed: &str) -> LinkedLine {
    let indent = " ".repeat(raw.len() - trimmed.len());
    if trimmed.starts_with("---") && trimmed.chars().all(|character| character == '-') {
        return plain_line(Line::from(Span::styled("─".repeat(40), theme::faint())));
    }
    if let Some(heading) = trimmed.strip_prefix("### ") {
        return heading_line(indent, heading, theme::bold(theme::text_color()));
    }
    if let Some(heading) = trimmed.strip_prefix("## ") {
        return heading_line(indent, heading, theme::bold(theme::accent_color()));
    }
    if let Some(heading) = trimmed.strip_prefix("# ") {
        return heading_line(indent, heading, theme::bold(theme::accent_color()));
    }
    if let Some(quoted) = trimmed.strip_prefix("> ") {
        let mut spans = vec![plain_span(format!("{indent}▏ "), theme::faint())];
        spans.extend(inline(quoted, theme::dim().add_modifier(Modifier::ITALIC)));
        return LinkedLine::new(spans).with_leading_gutter();
    }
    if let Some(item) = bullet(trimmed) {
        let mut spans = vec![plain_span(format!("{indent}• "), theme::accent())];
        spans.extend(inline(item, theme::text()));
        return LinkedLine::new(spans).with_leading_gutter();
    }
    LinkedLine::new(inline(raw, theme::text()))
}

fn heading_line(indent: String, heading: &str, style: Style) -> LinkedLine {
    let mut spans = vec![plain_span(indent, Style::default())];
    spans.extend(inline(heading, style));
    LinkedLine::new(spans)
}

fn bullet(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
}

fn code_line(raw: &str) -> LinkedLine {
    plain_line(Line::from(vec![
        Span::styled("│ ", theme::faint()),
        Span::styled(raw.replace('\t', "    "), theme::code()),
    ]))
    .with_leading_gutter()
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
        if super::safe_media_uri(url) {
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

pub(super) fn line_with_link(source: &str, url: &str) -> Option<usize> {
    source.split('\n').position(|line| {
        inline(line, Style::default())
            .iter()
            .any(|span| span.url.as_deref() == Some(url))
    })
}

pub(super) fn inline_spans(source: &str, base: Style) -> Vec<LinkedSpan> {
    inline_with_link_destinations(source, base, false)
}

/// Splits inline links, emphasis, and code spans out of one line of Markdown.
fn inline(source: &str, base: Style) -> Vec<LinkedSpan> {
    inline_with_link_destinations(source, base, true)
}

fn inline_with_link_destinations(
    source: &str,
    base: Style,
    show_link_destinations: bool,
) -> Vec<LinkedSpan> {
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
            let link_style = base
                .patch(theme::accent())
                .add_modifier(Modifier::UNDERLINED);
            if let Some(label) = link.label {
                spans.push(link_span(label.to_string(), link_style, link.url));
                if show_link_destinations {
                    spans.push(plain_span(" (", base));
                    spans.push(link_span(link.url.to_string(), link_style, link.url));
                    spans.push(plain_span(")", base));
                }
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
            ("`", theme::inline_code())
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
        if delimiter == "`" {
            spans.push(plain_span(format!("`{}`", &body[..close]), style));
        } else {
            spans.extend(inline_with_link_destinations(
                &body[..close],
                style,
                show_link_destinations,
            ));
        }
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

    use super::{render, render_copyable, render_linked};

    #[test]
    fn styles_code_fences_apart_from_prose() {
        let lines = render("text\n```rust\nlet x = 1;\n```\nmore");
        assert_eq!(lines.len(), 5);
        assert!(lines[1].spans[0].content.contains("rust"));
        assert_eq!(lines[2].spans[0].content, "│ ");
        assert!(lines[2].spans[1].content.contains("let x = 1;"));
        assert_eq!(lines[2].spans[1].style.bg, None);
    }

    #[test]
    fn tags_fenced_rows_with_exact_code_content() {
        let source = "before\n```rust\n\tlet x = 1;  \n\n```\nafter";
        let rendered = render_copyable(source);
        let code: Vec<_> = rendered.into_iter().filter_map(|(_, code)| code).collect();
        assert_eq!(code.len(), 4);
        assert!(
            code.iter()
                .all(|range| &source[range.clone()] == "\tlet x = 1;  \n")
        );
    }

    #[test]
    fn matches_fence_character_and_length() {
        let source = "~~~~rust\n```\nvalue\n```\n~~~~";
        let ranges: Vec<_> = render_copyable(source)
            .into_iter()
            .filter_map(|(_, range)| range)
            .collect();
        assert!(
            ranges
                .iter()
                .all(|range| &source[range.clone()] == "```\nvalue\n```")
        );
    }

    #[test]
    fn preserves_internal_crlf_but_excludes_the_closing_line_ending() {
        let source = "```text\r\none\r\ntwo\r\n```\r\n";
        let range = render_copyable(source)[0].1.clone().unwrap();
        assert_eq!(&source[range], "one\r\ntwo");
    }

    #[test]
    fn does_not_treat_four_space_indented_markers_as_fences() {
        let rendered = render_copyable("    ```\ncode\n    ```");
        assert!(rendered.into_iter().all(|(_, range)| range.is_none()));
    }

    #[test]
    fn splits_inline_code_and_emphasis() {
        let line = &render("run `cargo test` and **stop**")[0];
        let contents: Vec<_> = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(contents, ["run ", "`cargo test`", " and ", "stop"]);
        assert!(line.spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert!(
            !line.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
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
    fn parses_links_inside_bold_and_italic() {
        let url = "https://example.com/docs";
        for (source, emphasis) in [
            (format!("**[label]({url})**"), Modifier::BOLD),
            (format!("*[label]({url})*"), Modifier::ITALIC),
            (format!("_[label]({url})_"), Modifier::ITALIC),
        ] {
            let rendered = render_linked(&source);
            let line = &rendered[0];
            let joined: String = line
                .spans
                .iter()
                .map(|span| span.span.content.as_ref())
                .collect();
            assert_eq!(joined, format!("label ({url})"));

            let linked: Vec<_> = line
                .spans
                .iter()
                .filter(|span| span.url.is_some())
                .collect();
            assert_eq!(linked.len(), 2);
            assert!(linked.iter().all(|span| span.url.as_deref() == Some(url)));
            assert!(linked.iter().all(|span| {
                span.span.style.add_modifier.contains(emphasis)
                    && span.span.style.add_modifier.contains(Modifier::UNDERLINED)
            }));
        }
    }

    #[test]
    fn leaves_markdown_links_inside_code_spans_literal_and_unlinked() {
        let source = "`[label](https://example.com/docs)`";
        let rendered = render_linked(source);
        let line = &rendered[0];
        let joined: String = line
            .spans
            .iter()
            .map(|span| span.span.content.as_ref())
            .collect();

        assert_eq!(joined, "`[label](https://example.com/docs)`");
        assert!(linked_urls(source).is_empty());
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
