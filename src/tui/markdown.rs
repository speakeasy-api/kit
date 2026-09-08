//! A small Markdown renderer for agent messages.
//!
//! Models answer in Markdown, so the transcript reads much better with
//! headings, lists, and code told apart. This covers the constructs that
//! actually show up in agent output and deliberately stops there: block
//! quotes, rules, fenced and inline code, bullets, tables, and emphasis.

use std::ops::Range;

/// A completed CommonMark image occurrence. Ordinary links and code never load media.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ImageNode {
    pub range: Range<usize>,
    pub line: usize,
    pub destination: String,
    pub alt: String,
}

pub(super) fn image_nodes(source: &str) -> Vec<ImageNode> {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    // Bound parser work and occurrence retention independently of image bytes.
    if source.len() > 1024 * 1024 {
        return Vec::new();
    }
    let mut nodes = Vec::new();
    let mut current: Option<ImageNode> = None;
    let mut depth = 0usize;
    for (event, range) in Parser::new(source).into_offset_iter() {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                if depth == 0 {
                    current = Some(ImageNode {
                        line: source[..range.start]
                            .bytes()
                            .filter(|byte| *byte == b'\n')
                            .count(),
                        range,
                        destination: dest_url.into_string(),
                        alt: String::new(),
                    });
                }
                depth += 1;
            }
            Event::End(TagEnd::Image) => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(mut node) = current.take()
                {
                    node.range.end = range.end;
                    nodes.push(node);
                    if nodes.len() == 64 {
                        break;
                    }
                }
            }
            Event::Text(text) | Event::Code(text) if depth > 0 => {
                if let Some(node) = &mut current {
                    node.alt.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak if depth > 0 => {
                if let Some(node) = &mut current {
                    node.alt.push(' ');
                }
            }
            _ => {}
        }
    }
    nodes
}

use super::{
    theme,
    wrap::{LinkedLine, LinkedSpan},
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
pub(super) fn render_copyable_at_width(
    source: &str,
    max_width: Option<usize>,
) -> Vec<(LinkedLine, Option<Range<usize>>)> {
    render_copyable_with_source_at_width(source, max_width)
        .into_iter()
        .map(|(line, code, _)| (line, code))
        .collect()
}

/// Render the complete document, retaining code-copy ranges and byte coverage.
/// Coverage includes raw line endings; synthetic frames have empty ranges.
/// A source row expanded into several table lines is covered by its last line,
/// so media insertion cannot interrupt that row before its contents are shown.
pub(super) fn render_copyable_with_source_at_width(
    source: &str,
    max_width: Option<usize>,
) -> Vec<(LinkedLine, Option<Range<usize>>, Range<usize>)> {
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
    let mut table_end = 0;
    for (index, (offset, raw)) in raw_lines.iter().copied().enumerate() {
        if index < table_end {
            continue;
        }
        let coverage = offset..(offset + raw.len() + 1).min(source.len());
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
                    coverage,
                ));
                fence = None;
            } else {
                lines.push((code_line(raw), Some(content.clone()), coverage));
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
                coverage,
            ));
            fence = Some((language, marker, length, content));
            continue;
        }
        if let Some((end, table)) = table_at(&raw_lines, index, max_width, source.len()) {
            lines.extend(
                table
                    .into_iter()
                    .map(|(line, coverage)| (line, None, coverage)),
            );
            table_end = end;
            continue;
        }
        lines.push((block_line(raw, trimmed), None, coverage));
    }
    if let Some((_, _, _, content)) = fence {
        lines.push((
            code_frame("└─ code"),
            Some(content),
            source.len()..source.len(),
        ));
    }
    lines
}

#[derive(Clone, Copy)]
enum TableAlignment {
    Left,
    Center,
    Right,
}

struct TableCell {
    spans: Vec<LinkedSpan>,
    width: usize,
}

type CoveredLine = (LinkedLine, Range<usize>);

fn table_at(
    lines: &[(usize, &str)],
    start: usize,
    max_width: Option<usize>,
    source_len: usize,
) -> Option<(usize, Vec<CoveredLine>)> {
    let header_line = lines.get(start)?.1;
    if !table_row_allowed(header_line) {
        return None;
    }
    let header = table_cells(header_line)?;
    let delimiter = table_cells(lines.get(start + 1)?.1)?;
    if header.len() != delimiter.len() || header.is_empty() {
        return None;
    }
    let alignments: Vec<_> = delimiter
        .iter()
        .map(|cell| table_alignment(cell))
        .collect::<Option<_>>()?;

    let columns = header.len();
    let mut rows = vec![header];
    let mut end = start + 2;
    while let Some((_, raw)) = lines.get(end) {
        if !table_row_allowed(raw) {
            break;
        }
        let Some(mut row) = table_cells(raw) else {
            break;
        };
        row.resize(columns, String::new());
        row.truncate(columns);
        rows.push(row);
        end += 1;
    }

    let rows: Vec<Vec<TableCell>> = rows
        .into_iter()
        .enumerate()
        .map(|(row, cells)| {
            cells
                .into_iter()
                .map(|cell| {
                    let base = if row == 0 {
                        theme::text().add_modifier(Modifier::BOLD)
                    } else {
                        theme::text()
                    };
                    let spans = inline(&cell, base);
                    let width = spans.iter().map(|span| span.span.content.width()).sum();
                    TableCell { spans, width }
                })
                .collect()
        })
        .collect();
    let widths = (0..columns)
        .map(|column| rows.iter().map(|row| row[column].width).max().unwrap_or(0))
        .collect::<Vec<_>>();

    let coverage = |index: usize| {
        let (offset, raw) = lines[index];
        offset..(offset + raw.len() + 1).min(source_len)
    };
    let table_width = 1 + widths.iter().map(|width| width + 3).sum::<usize>();
    if max_width.is_some_and(|max_width| table_width > max_width) {
        let rendered = stacked_table(&rows)
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                let row = index / columns;
                let last_column = index % columns == columns - 1;
                let raw_row = if rows.len() == 1 {
                    start + 1
                } else {
                    start + 2 + row
                };
                let range_start = if row == 0 {
                    lines[start].0
                } else {
                    lines[raw_row].0
                };
                let range_end = if last_column {
                    coverage(raw_row).end
                } else {
                    range_start
                };
                (line, range_start..range_end)
            })
            .collect();
        return Some((end, rendered));
    }

    let table_start = lines[start].0;
    let mut rendered = vec![(table_rule(&widths, '┌', '┬', '┐'), table_start..table_start)];
    rendered.push((table_row(&rows[0], &widths, &alignments), coverage(start)));
    rendered.push((table_rule(&widths, '├', '┼', '┤'), coverage(start + 1)));
    rendered.extend(rows[1..].iter().enumerate().map(|(row, cells)| {
        (
            table_row(cells, &widths, &alignments),
            coverage(start + 2 + row),
        )
    }));
    let table_end = coverage(end - 1).end;
    rendered.push((table_rule(&widths, '└', '┴', '┘'), table_end..table_end));
    Some((end, rendered))
}

fn table_row_allowed(line: &str) -> bool {
    let Some(trimmed) = fence_line(line) else {
        return false;
    };
    opening_fence(trimmed).is_none()
        && !trimmed.starts_with("# ")
        && !trimmed.starts_with("## ")
        && !trimmed.starts_with("### ")
        && !trimmed.starts_with("> ")
        && bullet(trimmed).is_none()
        && !(trimmed.starts_with("---") && trimmed.chars().all(|character| character == '-'))
}

fn table_cells(line: &str) -> Option<Vec<String>> {
    let indentation = line.bytes().take_while(|byte| *byte == b' ').count();
    if indentation > 3 || line[indentation..].starts_with('\t') {
        return None;
    }
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    let characters: Vec<char> = line.chars().collect();
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut saw_pipe = false;
    let mut ends_with_separator = false;
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '\\' {
            let start = index;
            while index < characters.len() && characters[index] == '\\' {
                index += 1;
            }
            let count = index - start;
            if index < characters.len() && characters[index] == '|' {
                cell.extend(std::iter::repeat_n('\\', count / 2));
                if count % 2 == 1 {
                    cell.push('|');
                    ends_with_separator = false;
                    index += 1;
                    continue;
                }
            } else {
                cell.extend(std::iter::repeat_n('\\', count));
                ends_with_separator = false;
                continue;
            }
        }
        if characters[index] == '|' {
            cells.push(cell.trim().to_string());
            cell.clear();
            saw_pipe = true;
            ends_with_separator = true;
        } else {
            cell.push(characters[index]);
            ends_with_separator = false;
        }
        index += 1;
    }
    cells.push(cell.trim().to_string());

    if !saw_pipe {
        return None;
    }
    if line.starts_with('|') {
        cells.remove(0);
    }
    if ends_with_separator {
        cells.pop();
    }
    Some(cells)
}

fn table_alignment(cell: &str) -> Option<TableAlignment> {
    let cell = cell.trim();
    let left = cell.starts_with(':');
    let right = cell.ends_with(':');
    let rule = cell.strip_prefix(':').unwrap_or(cell);
    let rule = rule.strip_suffix(':').unwrap_or(rule);
    if rule.is_empty() || !rule.chars().all(|character| character == '-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => TableAlignment::Center,
        (false, true) => TableAlignment::Right,
        _ => TableAlignment::Left,
    })
}

fn stacked_table(rows: &[Vec<TableCell>]) -> Vec<LinkedLine> {
    if rows.len() == 1 {
        return rows[0]
            .iter()
            .map(|header| {
                let mut spans = vec![plain_span("• ", theme::accent())];
                spans.extend(header.spans.iter().cloned());
                LinkedLine::new(spans).with_leading_gutter()
            })
            .collect();
    }

    rows[1..]
        .iter()
        .flat_map(|row| {
            row.iter()
                .enumerate()
                .map(|(column, cell)| {
                    let mut spans = vec![plain_span(
                        if column == 0 { "• " } else { "  " },
                        theme::accent(),
                    )];
                    spans.extend(rows[0][column].spans.iter().cloned());
                    spans.push(plain_span(": ", theme::faint()));
                    spans.extend(cell.spans.iter().cloned());
                    LinkedLine::new(spans).with_leading_gutter()
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn table_rule(widths: &[usize], left: char, middle: char, right: char) -> LinkedLine {
    let mut rule = String::from(left);
    for (index, width) in widths.iter().enumerate() {
        rule.push_str(&"─".repeat(width + 2));
        rule.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    plain_line(Line::from(Span::styled(rule, theme::faint())))
}

fn table_row(cells: &[TableCell], widths: &[usize], alignments: &[TableAlignment]) -> LinkedLine {
    let mut spans = vec![plain_span("│", theme::faint())];
    for (index, cell) in cells.iter().enumerate() {
        let remaining = widths[index] - cell.width;
        let (left, right) = match alignments[index] {
            TableAlignment::Left => (0, remaining),
            TableAlignment::Center => (remaining / 2, remaining - remaining / 2),
            TableAlignment::Right => (remaining, 0),
        };
        spans.push(plain_span(" ".repeat(left + 1), theme::text()));
        spans.extend(cell.spans.iter().cloned());
        spans.push(plain_span(" ".repeat(right + 1), theme::text()));
        spans.push(plain_span("│", theme::faint()));
    }
    LinkedLine::new(spans)
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use ratatui::style::Modifier;

    use super::*;

    /// Renders Markdown source into styled transcript lines.
    pub fn render(source: &str) -> Vec<Line<'static>> {
        render_linked(source)
            .into_iter()
            .map(|line| line.line())
            .collect()
    }

    /// Renders Markdown while attaching each URL directly to the spans it owns.
    pub fn render_linked(source: &str) -> Vec<LinkedLine> {
        render_copyable(source)
            .into_iter()
            .map(|(line, _)| line)
            .collect()
    }

    /// Renders Markdown and tags every row of a fenced code block with its exact
    /// source content, excluding the fence and language label.
    pub fn render_copyable(source: &str) -> Vec<(LinkedLine, Option<Range<usize>>)> {
        render_copyable_at_width(source, None)
    }

    fn line_text(line: &ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn source_coverage_uses_raw_unicode_and_crlf_byte_offsets() {
        let source = "# café\r\n雪\r\n";
        let rendered = render_copyable_with_source_at_width(source, None);
        let covered: Vec<_> = rendered
            .iter()
            .map(|(_, _, range)| &source[range.clone()])
            .collect();
        assert_eq!(covered, ["# café\r\n", "雪\r\n", ""]);
        assert_eq!(rendered.last().unwrap().2, source.len()..source.len());
        assert!(rendered.iter().all(|(_, code, _)| code.is_none()));
    }

    #[test]
    fn fenced_source_coverage_is_separate_from_complete_code_copy_range() {
        let source = "前\r\n```rust\r\n雪\r\n```\r\n後";
        let rendered = render_copyable_with_source_at_width(source, None);
        let covered: Vec<_> = rendered
            .iter()
            .map(|(_, _, range)| &source[range.clone()])
            .collect();
        assert_eq!(
            covered,
            ["前\r\n", "```rust\r\n", "雪\r\n", "```\r\n", "後"]
        );
        for (_, code, _) in &rendered[1..4] {
            assert_eq!(&source[code.clone().unwrap()], "雪");
        }
        let unclosed = "~~~\r\n雪";
        let rendered = render_copyable_with_source_at_width(unclosed, None);
        assert_eq!(rendered[0].2, 0..5);
        assert_eq!(rendered[1].2, 5..unclosed.len());
        assert_eq!(rendered[2].2, unclosed.len()..unclosed.len());
        for (_, code, _) in rendered {
            assert_eq!(&unclosed[code.unwrap()], "雪");
        }
    }

    #[test]
    fn table_source_coverage_places_images_after_their_rendered_rows() {
        let source = "| 頭 | B |\r\n| --- | --- |\r\n| ![雪](image.png) | x |\r\n| last | y |";
        let rendered = render_copyable_with_source_at_width(source, None);
        let covered: Vec<_> = rendered
            .iter()
            .map(|(_, _, range)| &source[range.clone()])
            .collect();
        assert_eq!(
            covered,
            [
                "",
                "| 頭 | B |\r\n",
                "| --- | --- |\r\n",
                "| ![雪](image.png) | x |\r\n",
                "| last | y |",
                ""
            ]
        );
        let node = image_nodes(source).remove(0);
        assert_eq!(
            rendered
                .iter()
                .position(|(_, _, range)| range.end >= node.range.end),
            Some(3)
        );
        let stacked = render_copyable_with_source_at_width(source, Some(1));
        assert_eq!(stacked.len(), 4);
        assert_eq!(stacked[0].2, 0..0);
        assert_eq!(stacked[1].2.end, rendered[3].2.end);
        assert_eq!(
            stacked
                .iter()
                .position(|(_, _, range)| range.end >= node.range.end),
            Some(1)
        );
        assert_eq!(stacked[2].2.start, stacked[2].2.end);
        assert_eq!(stacked[3].2.end, source.len());

        let header_only = "| ![頭](header.png) | B |\n| --- | --- |";
        let node = image_nodes(header_only).remove(0);
        for width in [None, Some(1)] {
            let rows = render_copyable_with_source_at_width(header_only, width);
            assert_eq!(
                rows.iter()
                    .position(|(_, _, range)| range.end >= node.range.end),
                Some(1)
            );
        }
    }

    #[test]
    fn compatibility_wrapper_preserves_styles_links_gutters_and_code_ranges() {
        for source in [
            "# café\n- **bold** [link](https://example.com)\n> quote",
            "before\r\n```rust\r\nlet 雪 = 1;\r\n```\r\nafter",
            "~~~\n雪",
            "| **A** | B |\n| --- | --- |\n| [link](https://example.com) | 雪 |",
            "| A | B |\n| --- | --- |",
            "",
        ] {
            for width in [None, Some(1), Some(80)] {
                let original = render_copyable_at_width(source, width);
                let covered = render_copyable_with_source_at_width(source, width);
                assert_eq!(original.len(), covered.len());
                for ((line, code), (with_source, source_code, range)) in
                    original.iter().zip(&covered)
                {
                    assert_eq!(format!("{line:?}"), format!("{with_source:?}"));
                    assert_eq!(code, source_code);
                    assert!(source.get(range.clone()).is_some());
                }
                assert!(
                    covered
                        .windows(2)
                        .all(|rows| rows[0].2.end <= rows[1].2.end)
                );
            }
        }
    }

    #[test]
    fn renders_aligned_pipe_tables() {
        let rendered =
            render("| Name | Status |\n| :--- | ---: |\n| alpha | Ready |\n| beta | In progress |");
        let text: Vec<_> = rendered.iter().map(line_text).collect();
        assert_eq!(
            text,
            [
                "┌───────┬─────────────┐",
                "│ Name  │      Status │",
                "├───────┼─────────────┤",
                "│ alpha │       Ready │",
                "│ beta  │ In progress │",
                "└───────┴─────────────┘",
            ]
        );
        assert!(
            rendered[1]
                .spans
                .iter()
                .any(|span| span.content == "Name"
                    && span.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn preserves_inline_table_formatting_and_links() {
        let source =
            "| Resource | Value |\n| --- | :---: |\n| [Docs](https://example.com) | **a \\| b** |";
        let rendered = render_linked(source);
        let text: Vec<_> = rendered
            .iter()
            .map(|line| line_text(&line.line()))
            .collect();
        assert_eq!(text[3], "│ Docs (https://example.com) │ a | b │");
        assert_eq!(
            rendered
                .iter()
                .flat_map(|line| &line.spans)
                .filter_map(|span| span.url.as_deref())
                .collect::<Vec<_>>(),
            ["https://example.com", "https://example.com"]
        );
        assert!(rendered[3].spans.iter().any(|span| {
            span.span.content == "a | b" && span.span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn stacks_tables_that_are_wider_than_the_transcript() {
        let rendered =
            render_copyable_at_width("| A | B |\n| --- | --- |\n| 123456 | x |", Some(12));
        let text: Vec<_> = rendered
            .iter()
            .map(|(line, _)| line_text(&line.line()))
            .collect();
        assert_eq!(text, ["• A: 123456", "  B: x"]);
    }

    #[test]
    fn preserves_block_boundaries_around_tables() {
        let source =
            "| Name | Value |\n| --- | --- |\n| alpha | beta |\n~~~text|meta\ncode\n~~~\nafter";
        let rendered = render(source);
        assert_eq!(line_text(&rendered[4]), "└───────┴───────┘");
        assert!(line_text(&rendered[5]).contains("text|meta"));
        assert_eq!(line_text(&rendered[6]), "│ code");
        assert_eq!(line_text(rendered.last().unwrap()), "after");

        let heading = render("# Name | Value\n--- | ---");
        assert_eq!(line_text(&heading[0]), "Name | Value");
    }

    #[test]
    fn splits_only_unescaped_table_pipes() {
        assert_eq!(super::table_cells(r"| a\\| b |").unwrap(), [r"a\", "b"]);
        assert_eq!(super::table_cells(r"| a\\\| b |").unwrap(), [r"a\| b"]);
        assert_eq!(super::table_cells(r"| `a|b` | c |").unwrap().len(), 3);
    }

    #[test]
    fn requires_a_valid_delimiter_row_before_rendering_a_table() {
        for source in [
            "| Name | Value |\n| alpha | beta |",
            "| Name | Value |\n| ::--- | --- |",
            "    | Name | Value |\n    | --- | --- |",
        ] {
            let rendered = render(source);
            assert_eq!(rendered.len(), 2);
            assert_eq!(line_text(&rendered[0]), source.lines().next().unwrap());
        }
    }

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
    fn image_nodes_are_completed_commonmark_not_links_or_code() {
        let source = "![a *bold* image](kit-file://example)\n![again][pic]\n\n[pic]: /tmp/image.png\n\n`![code](bad.png)`\n```md\n![fence](bad.png)\n```\n\\![escaped](bad.png)\n[ordinary](bad.png)\n![partial](";
        let nodes = super::image_nodes(source);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].alt, "a bold image");
        assert_eq!(nodes[0].destination, "kit-file://example");
        assert_eq!(
            &source[nodes[0].range.clone()],
            "![a *bold* image](kit-file://example)"
        );
        assert_eq!(nodes[1].destination, "/tmp/image.png");
        assert_eq!(nodes[1].line, 1);
    }

    #[test]
    fn image_nodes_preserve_repeated_occurrences_and_stream_completion() {
        assert!(super::image_nodes("![alt](https://example.com/a").is_empty());
        let nodes =
            super::image_nodes("![alt](https://example.com/a) ![alt](https://example.com/a)");
        assert_eq!(nodes.len(), 2);
        assert_ne!(nodes[0].range, nodes[1].range);
        assert_eq!(nodes[0].destination, nodes[1].destination);
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
