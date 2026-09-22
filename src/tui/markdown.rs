//! A small Markdown renderer for agent messages.
//!
//! Models answer in Markdown, so the transcript reads much better with
//! headings, lists, and code told apart. This covers the constructs that
//! actually show up in agent output and deliberately stops there: block
//! quotes, rules, fenced and inline code, bullets, tables, and emphasis.

use std::ops::Range;

use super::{
    theme,
    wrap::{LinkedLine, LinkedSpan},
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

pub(super) fn render_copyable_at_width(
    source: &str,
    max_width: Option<usize>,
) -> Vec<(LinkedLine, Option<Range<usize>>)> {
    render_copyable_with_sources(source, max_width, false)
        .into_iter()
        .map(|(line, code, _)| (line, code))
        .collect()
}

/// Render the complete document while retaining source ranges for viewport placement.
/// A table is one layout unit: its source range belongs to its final rendered line.
pub(super) fn render_copyable_with_sources(
    source: &str,
    max_width: Option<usize>,
    split_images: bool,
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
        let source_range = offset..offset + raw.len();
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
                    source_range,
                ));
                fence = None;
            } else {
                lines.push((code_line(raw), Some(content.clone()), source_range));
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
                source_range,
            ));
            fence = Some((language, marker, length, content));
            continue;
        }
        if let Some((end, table)) = table_at(&raw_lines, index, max_width) {
            let last = table.len() - 1;
            let source_end = raw_lines[end - 1].0 + raw_lines[end - 1].1.len();
            lines.extend(table.into_iter().enumerate().map(|(index, line)| {
                let range = if index == last {
                    offset..source_end
                } else {
                    offset..offset
                };
                (line, None, range)
            }));
            table_end = end;
            continue;
        }
        let mut line = block_line(raw, trimmed);
        if split_images {
            // Parse the whole line first, so emphasis spanning a preview retains
            // its style. Only prose is split: tables and fences remain intact.
            let mut references = image_references(raw).into_iter();
            let mut start = offset;
            let mut pending = Vec::new();
            for span in line.spans.clone() {
                let image_end = span.image_end;
                pending.push(span);
                if image_end && let Some((range, _)) = references.next() {
                    let end = offset + range.end;
                    let segment_spans = std::mem::take(&mut pending);
                    let segment = if start == offset {
                        let mut segment = line.clone();
                        segment.spans = segment_spans;
                        segment
                    } else {
                        line.continuation(segment_spans)
                    };
                    lines.push((segment, None, start..end));
                    start = end;
                }
            }
            if !pending.is_empty() {
                if start == offset {
                    line.spans = pending;
                } else {
                    line = line.continuation(pending);
                }
                lines.push((line, None, start..source_range.end));
            }
        } else {
            lines.push((line, None, source_range));
        }
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

fn table_at(
    lines: &[(usize, &str)],
    start: usize,
    max_width: Option<usize>,
) -> Option<(usize, Vec<LinkedLine>)> {
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

    let table_width = 1 + widths.iter().map(|width| width + 3).sum::<usize>();
    if max_width.is_some_and(|max_width| table_width > max_width) {
        return Some((end, stacked_table(&rows)));
    }

    let mut rendered = vec![table_rule(&widths, '┌', '┬', '┐')];
    rendered.push(table_row(&rows[0], &widths, &alignments));
    rendered.push(table_rule(&widths, '├', '┼', '┤'));
    rendered.extend(
        rows[1..]
            .iter()
            .map(|row| table_row(row, &widths, &alignments)),
    );
    rendered.push(table_rule(&widths, '└', '┴', '┘'));
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
        image_end: false,
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
        image_end: false,
    }
}

struct Link<'a> {
    start: usize,
    end: usize,
    label: Option<&'a str>,
    url: &'a str,
    url_range: Range<usize>,
}

// Balanced-delimiter metadata is shared by every suffix searched during one
// inline parse. Incomplete streaming syntax is as important to cache as a
// successful match: a missing entry must not trigger another suffix scan.
struct DelimiterMatches {
    source_len: usize,
    image_labels: std::collections::HashMap<usize, usize>,
    raw_label_ends: std::collections::HashMap<usize, usize>,
    image_parentheses: std::collections::HashMap<usize, usize>,
    link_destinations: std::collections::HashMap<usize, usize>,
}

impl DelimiterMatches {
    fn new(source: &str) -> Self {
        let mut matches = Self {
            source_len: source.len(),
            image_labels: std::collections::HashMap::new(),
            raw_label_ends: std::collections::HashMap::new(),
            image_parentheses: std::collections::HashMap::new(),
            link_destinations: std::collections::HashMap::new(),
        };
        let mut raw_brackets = Vec::new();
        let mut image_parentheses = Vec::new();
        let mut brackets = Vec::new();
        let mut parentheses = Vec::new();
        let mut escaped = false;
        for (index, byte) in source.bytes().enumerate() {
            match byte {
                b'[' => raw_brackets.push(index),
                b']' => {
                    for open in raw_brackets.drain(..) {
                        matches.raw_label_ends.insert(open, index);
                    }
                }
                _ => {}
            }
            // Markdown link destinations historically count even escaped
            // parentheses, whereas image alt text honors punctuation escapes.
            match byte {
                b'(' => parentheses.push(index),
                b')' => {
                    if let Some(open) = parentheses.pop() {
                        matches.link_destinations.insert(open, index);
                    }
                }
                _ => {}
            }
            if escaped {
                escaped = false;
                if byte.is_ascii_punctuation() {
                    continue;
                }
            }
            match byte {
                b'\\' => escaped = true,
                b'\r' | b'\n' => image_parentheses.clear(),
                b'(' => image_parentheses.push(index),
                b')' => {
                    if let Some(open) = image_parentheses.pop() {
                        matches.image_parentheses.insert(open, index);
                    }
                }
                b'[' => brackets.push(index),
                b']' => {
                    if let Some(open) = brackets.pop() {
                        matches.image_labels.insert(open, index);
                    }
                }
                _ => {}
            }
        }
        matches
    }

    // Callers pass suffixes ending at the same source boundary. Document image
    // scanning creates fresh metadata for each fence-delimited prose segment.
    fn closing(&self, source: &str, opening: usize, image: bool) -> Option<usize> {
        let base = self.source_len - source.len();
        let matches = if image {
            &self.image_labels
        } else {
            &self.link_destinations
        };
        matches.get(&(base + opening)).map(|end| *end - base)
    }
}

/// Returns byte ranges for complete inline images and their destinations, in source order.
/// Reference-style images are left literal. Destinations may contain local-path spaces;
/// Markdown punctuation escapes are decoded, but URI validation belongs to the caller.
pub(super) fn image_references(source: &str) -> Vec<(Range<usize>, String)> {
    image_references_before(source, source.len(), false, 0, None)
}

// Only search starts before the next competing token. Keep the complete source
// available because an image destination may itself contain Markdown markers.
fn image_references_before(
    source: &str,
    before: usize,
    first_only: bool,
    first_newline: usize,
    matches: Option<&DelimiterMatches>,
) -> Vec<(Range<usize>, String)> {
    // Fence segmentation takes priority even over image syntax that starts on
    // an earlier line. Retain the document scanner for multiline helper input.
    if first_only && first_newline < source.len() {
        return image_references(source)
            .into_iter()
            .take(1)
            .filter(|(range, _)| range.start < before)
            .collect();
    }
    let mut images = Vec::new();
    let mut fence = None;
    let mut prose_start = 0;
    let mut offset = 0;
    // A line without a fence needs no line-end scan before finding its first
    // image. This common path also avoids repeatedly splitting a long line.
    if first_only && before <= first_newline && fence_line(source).and_then(opening_fence).is_none()
    {
        collect_inline_images(source, 0, before, true, &mut images, matches);
        return images;
    }
    for line in source[..before].split_inclusive('\n') {
        let raw = line.trim_end_matches(['\r', '\n']);
        let marker_line = fence_line(raw);
        if let Some((marker, minimum)) = fence {
            if marker_line.is_some_and(|line| closing_fence(line, marker, minimum)) {
                fence = None;
                prose_start = offset + line.len();
            }
        } else if let Some((marker, length, _)) = marker_line.and_then(opening_fence) {
            collect_inline_images(
                &source[prose_start..offset],
                prose_start,
                offset - prose_start,
                first_only,
                &mut images,
                None,
            );
            if first_only && !images.is_empty() {
                return images;
            }
            fence = Some((marker, length));
        }
        offset += line.len();
    }
    if fence.is_none() {
        collect_inline_images(
            &source[prose_start..],
            prose_start,
            before.saturating_sub(prose_start),
            first_only,
            &mut images,
            None,
        );
    }
    images
}

fn collect_inline_images(
    source: &str,
    base: usize,
    before: usize,
    first_only: bool,
    images: &mut Vec<(Range<usize>, String)>,
    matches: Option<&DelimiterMatches>,
) {
    let owned;
    let matches = match matches {
        Some(matches) => matches,
        None => {
            owned = DelimiterMatches::new(source);
            &owned
        }
    };
    let bytes = source.as_bytes();
    let mut offset = 0;
    while offset < before {
        match bytes[offset] {
            b'\\' if bytes.get(offset + 1).is_some_and(u8::is_ascii_punctuation) => {
                offset += 2;
            }
            b'`' => {
                let length = bytes[offset..].iter().take_while(|&&b| b == b'`').count();
                let mut end = offset + length;
                let mut closing = None;
                while end < bytes.len() {
                    if bytes[end] != b'`' {
                        end += 1;
                        continue;
                    }
                    let run = bytes[end..].iter().take_while(|&&b| b == b'`').count();
                    end += run;
                    if run == length {
                        closing = Some(end);
                        break;
                    }
                }
                // Unmatched backticks are literal, not an unfinished code span.
                offset = closing.unwrap_or(offset + length);
            }
            b'!' if bytes.get(offset + 1) == Some(&b'[') => {
                if let Some((end, destination)) = image_at(source, offset, matches) {
                    images.push((base + offset..base + end, destination));
                    if first_only {
                        return;
                    }
                    offset = end;
                } else {
                    offset += 2;
                }
            }
            _ => offset += 1,
        }
    }
}

fn image_at(source: &str, start: usize, matches: &DelimiterMatches) -> Option<(usize, String)> {
    let bytes = source.as_bytes();
    let mut cursor = matches.closing(source, start + 1, true)?;
    if bytes.get(cursor..cursor + 2)? != b"](" {
        return None;
    }
    cursor += 2;
    while matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
        cursor += 1;
    }
    let angle = bytes.get(cursor) == Some(&b'<');
    cursor += usize::from(angle);
    let destination_start = cursor;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\r' | b'\n' => return None,
            b'\\' if bytes.get(cursor + 1).is_some_and(u8::is_ascii_punctuation) => {
                cursor += 2;
                continue;
            }
            b'>' if angle => break,
            b'"' | b'\''
                if !angle
                    && cursor > destination_start
                    && bytes[cursor - 1].is_ascii_whitespace() =>
            {
                break;
            }
            b'<' if angle => return None,
            b'(' if !angle => {
                // Inside nested parentheses quotes are destination text, not
                // titles. Skip the balanced region, or fail immediately when
                // it cannot close before a newline/end. Do not use this map
                // for the outer delimiter: titles can contain unbalanced parens.
                let base = matches.source_len - source.len();
                cursor = *matches.image_parentheses.get(&(base + cursor))? - base;
            }
            b')' if !angle => break,
            _ => {}
        }
        cursor += 1;
    }
    bytes.get(cursor)?;
    let destination = source[destination_start..cursor].trim();
    if angle {
        cursor += 1;
    }
    while matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
        cursor += 1;
    }
    // An optional quoted title is part of the syntax, not the destination.
    if matches!(bytes.get(cursor), Some(b'"' | b'\'')) {
        let quote = bytes[cursor];
        cursor += 1;
        loop {
            match *bytes.get(cursor)? {
                b'\r' | b'\n' => return None,
                b'\\' => cursor += 2,
                byte if byte == quote => {
                    cursor += 1;
                    break;
                }
                _ => cursor += 1,
            }
        }
        while matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
            cursor += 1;
        }
    }
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    let mut decoded = String::new();
    let mut characters = destination.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' && characters.peek().is_some_and(char::is_ascii_punctuation) {
            decoded.push(characters.next()?);
        } else {
            decoded.push(character);
        }
    }
    Some((cursor + 1, decoded))
}

fn next_markdown_link<'a>(
    source: &'a str,
    before: usize,
    matches: &DelimiterMatches,
) -> Option<Link<'a>> {
    let mut offset = 0;
    while offset < before {
        let Some(relative_start) = source[offset..before].find('[') else {
            break;
        };
        let start = offset + relative_start;
        // Images have their own rendering path; do not consume their alt text as a link.
        if start > 0 && source.as_bytes()[start - 1] == b'!' {
            offset = image_at(source, start - 1, matches).map_or(start + 1, |(end, _)| end);
            continue;
        }
        let base = matches.source_len - source.len();
        let label_end = *matches.raw_label_ends.get(&(base + start))? - base;
        if !source[label_end..].starts_with("](") {
            offset = label_end + 1;
            continue;
        }
        let url_start = label_end + 2;
        let url_end = matches.closing(source, url_start - 1, false)?;
        let url = &source[url_start..url_end];
        if super::safe_media_uri(url) {
            return Some(Link {
                start,
                end: url_end + 1,
                label: Some(&source[start + 1..label_end]),
                url,
                url_range: url_start..url_end,
            });
        }
        offset = url_end + 1;
    }
    None
}

/// Extract destinations through the rendering parser so replay rewrites exactly
/// the links displayed by the terminal, including within nested emphasis.
pub(super) fn image_label_link_destinations(source: &str) -> Vec<(Range<usize>, String)> {
    let mut destinations = Vec::new();
    inline_with_link_destinations_and_ranges(
        source,
        Style::default(),
        false,
        0,
        Some(&mut destinations),
        &[],
    );
    destinations
}

fn is_image_label(label: &str) -> bool {
    label == "Image"
        || label.strip_prefix("Image #").is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn next_link<'a>(
    source: &'a str,
    before: usize,
    bare_start: Option<usize>,
    matches: &DelimiterMatches,
) -> Option<Link<'a>> {
    let bare_start = bare_start.filter(|start| *start < before);
    if let Some(markdown) = next_markdown_link(source, bare_start.unwrap_or(before), matches) {
        return Some(markdown);
    }
    bare_start.map(|start| {
        let mut end = source[start..]
            .find(char::is_whitespace)
            .map_or(source.len(), |length| start + length);
        let mut balance = source[start..end].bytes().fold(0isize, |balance, byte| {
            balance + isize::from(byte == b')') - isize::from(byte == b'(')
        });
        loop {
            let Some(character) = source[..end].chars().next_back() else {
                break;
            };
            let unmatched_close = character == ')' && balance > 0;
            if matches!(
                character,
                '.' | ',' | ';' | ':' | '!' | '?' | ']' | '}' | '\'' | '"'
            ) || unmatched_close
            {
                end -= character.len_utf8();
                balance -= isize::from(character == ')');
            } else {
                break;
            }
        }
        Link {
            start,
            end,
            label: None,
            url: &source[start..end],
            url_range: start..end,
        }
    })
}

pub(super) fn image_label_links(source: &str) -> Vec<(usize, String)> {
    source
        .split('\n')
        .enumerate()
        .flat_map(|(line, source)| {
            image_label_link_destinations(source)
                .into_iter()
                .map(move |(_, uri)| (line, uri))
        })
        .collect()
}

pub(super) fn inline_spans(source: &str, base: Style) -> Vec<LinkedSpan> {
    inline_with_link_destinations(source, base, false)
}

/// Render internal image hits only at caller-associated source ranges. The
/// ordinary Markdown parser still rejects internal URI schemes, including when
/// an attacker copies a real image target into another link.
pub(super) fn inline_spans_with_image_labels(
    source: &str,
    base: Style,
    labels: &[(Range<usize>, String)],
) -> Vec<LinkedSpan> {
    inline_with_link_destinations_and_ranges(source, base, false, 0, None, labels)
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
    inline_with_link_destinations_and_ranges(source, base, show_link_destinations, 0, None, &[])
}

fn inline_with_link_destinations_and_ranges(
    source: &str,
    base: Style,
    show_link_destinations: bool,
    offset: usize,
    mut destinations: Option<&mut Vec<(Range<usize>, String)>>,
    labels: &[(Range<usize>, String)],
) -> Vec<LinkedSpan> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut rest = source;
    let matches = DelimiterMatches::new(source);
    let mut markers = source.match_indices(['`', '*', '_']).peekable();
    let mut schemes = source
        .match_indices("http")
        .filter_map(|(index, _)| {
            (source[index..].starts_with("https://") || source[index..].starts_with("http://"))
                .then_some(index)
        })
        .peekable();
    let mut image_starts = source
        .match_indices("![")
        .map(|(index, _)| index)
        .peekable();
    let mut newlines = source
        .match_indices('\n')
        .map(|(index, _)| index)
        .peekable();
    // A failed underscore closer search must not rescan the remaining line for
    // every opener. These are precisely the old parser's eligible closers.
    let underscore_closers: Vec<_> = source
        .match_indices('_')
        .filter_map(|(index, _)| {
            (!source[..index].ends_with(char::is_whitespace)
                && source[index + 1..]
                    .chars()
                    .next()
                    .is_none_or(|character| !character.is_alphanumeric()))
            .then_some(index)
        })
        .collect();
    while !rest.is_empty() {
        let relative = source.len() - rest.len();
        while markers.peek().is_some_and(|(index, _)| *index < relative) {
            markers.next();
        }
        let marker = markers.peek().map(|(index, _)| {
            let index = *index - relative;
            (index, &rest[index..])
        });
        let before = marker.as_ref().map_or(rest.len(), |(index, _)| *index);
        while schemes.peek().is_some_and(|index| *index < relative) {
            schemes.next();
        }
        while newlines.peek().is_some_and(|index| *index < relative) {
            newlines.next();
        }
        while image_starts.peek().is_some_and(|index| *index < relative) {
            image_starts.next();
        }
        let image_start = image_starts
            .peek()
            .map_or(before, |index| (*index - relative).min(before));
        let bare_start = schemes.peek().map(|index| *index - relative);
        let mut link = next_link(rest, image_start, bare_start, &matches);
        let image = if link.is_none() && image_start < before {
            image_references_before(
                rest,
                before,
                true,
                newlines
                    .peek()
                    .map_or(rest.len(), |index| *index - relative),
                Some(&matches),
            )
            .into_iter()
            .next()
        } else {
            None
        };
        if link.is_none() && image_start < before {
            link = next_link(
                rest,
                image.as_ref().map_or(before, |(range, _)| range.start),
                bare_start,
                &matches,
            );
        }
        let consumed = offset + source.len() - rest.len();
        let label = labels
            .iter()
            .find(|(range, _)| range.start >= consumed && range.end <= consumed + rest.len());
        if let Some((range, target)) = label {
            let start = range.start - consumed;
            let end = range.end - consumed;
            if marker.as_ref().is_none_or(|(index, _)| start < *index)
                && image.as_ref().is_none_or(|(range, _)| start <= range.start)
                && link.as_ref().is_none_or(|link| start <= link.start)
            {
                plain.push_str(&rest[..start]);
                if !plain.is_empty() {
                    spans.push(plain_span(std::mem::take(&mut plain), base));
                }
                spans.push(link_span(
                    rest[start..end].to_owned(),
                    base.add_modifier(Modifier::UNDERLINED),
                    target,
                ));
                rest = &rest[end..];
                continue;
            }
        }
        if let Some((range, _)) = image
            && marker
                .as_ref()
                .is_none_or(|(index, _)| range.start <= *index)
            && link.as_ref().is_none_or(|link| range.start <= link.start)
        {
            plain.push_str(&rest[..range.start]);
            if !plain.is_empty() {
                spans.push(plain_span(std::mem::take(&mut plain), base));
            }
            // Keep complete image syntax atomic, including punctuation in its
            // destination, and retain the surrounding inline style.
            let mut span = plain_span(rest[range.clone()].to_string(), base);
            span.image_end = true;
            spans.push(span);
            rest = &rest[range.end..];
            continue;
        }
        if let Some(link) = link
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
                if is_image_label(label)
                    && let Some(destinations) = destinations.as_deref_mut()
                {
                    let start = offset + source.len() - rest.len();
                    destinations.push((
                        start + link.url_range.start..start + link.url_range.end,
                        link.url.to_string(),
                    ));
                }
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
            let body_start = source.len() - body.len();
            underscore_closers
                .get(underscore_closers.partition_point(|index| *index <= body_start))
                .map(|index| *index - body_start)
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
            spans.extend(inline_with_link_destinations_and_ranges(
                &body[..close],
                style,
                show_link_destinations,
                offset + source.len() - body.len(),
                destinations.as_deref_mut(),
                labels,
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
    fn incomplete_streaming_delimiters_preserve_later_complete_tokens() {
        for count in [32, 1000] {
            let prefix = "![".repeat(count);
            let source = format!("{prefix}![inner](local.png)");
            let images = image_references(&source);
            assert_eq!(images, [(prefix.len()..source.len(), "local.png".into())]);
            let spans = inline_spans(&source, Style::default());
            assert_eq!(
                spans
                    .iter()
                    .map(|span| span.span.content.as_ref())
                    .collect::<String>(),
                source
            );
            assert_eq!(
                spans
                    .iter()
                    .filter(|span| span.image_end)
                    .map(|span| span.span.content.as_ref())
                    .collect::<Vec<_>>(),
                ["![inner](local.png)"]
            );

            let prefix = "[label](https://example.com/( ".repeat(count);
            let source = format!("{prefix}[Image #7](file:///good)");
            let spans = inline_spans(&source, Style::default());
            assert_eq!(
                spans
                    .iter()
                    .map(|span| span.span.content.as_ref())
                    .collect::<String>(),
                format!("{prefix}Image #7")
            );
            let urls = spans
                .iter()
                .filter_map(|span| span.url.as_deref())
                .collect::<Vec<_>>();
            assert_eq!(urls.len(), count + 1);
            assert!(
                urls[..count]
                    .iter()
                    .all(|url| *url == "https://example.com/(")
            );
            assert_eq!(urls[count], "file:///good");
            let destinations = image_label_link_destinations(&source);
            let start = source.rfind("file:///good").unwrap();
            assert_eq!(
                destinations,
                [(start..start + "file:///good".len(), "file:///good".into())]
            );
        }
    }

    #[test]
    fn repeated_rich_inline_tokens_preserve_styles_links_images_and_ranges() {
        let unit =
            "**bold _inner_** [Image #1](file:///tmp/a_(b).png) ![alt](a_b.png) `literal *code*` ";
        let source = unit.repeat(1000);
        let spans = inline_spans(&source, Style::default());
        let visible = spans
            .iter()
            .map(|span| span.span.content.as_ref())
            .collect::<String>();
        assert_eq!(
            visible,
            "bold inner Image #1 ![alt](a_b.png) `literal *code*` ".repeat(1000)
        );
        assert_eq!(spans.iter().filter(|span| span.image_end).count(), 1000);
        let inner = spans
            .iter()
            .filter(|span| span.span.content == "inner")
            .collect::<Vec<_>>();
        assert_eq!(inner.len(), 1000);
        assert!(inner.iter().all(|span| {
            span.span
                .style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        }));
        let destinations = image_label_link_destinations(&source);
        assert_eq!(destinations.len(), 1000);
        for (index, (range, url)) in destinations.iter().enumerate() {
            assert_eq!(&source[range.clone()], "file:///tmp/a_(b).png");
            assert_eq!(url, "file:///tmp/a_(b).png");
            assert_eq!(
                range.start,
                index * unit.len() + unit.find("file:///").unwrap()
            );
        }
    }

    #[test]
    fn bounded_searches_preserve_malformed_syntax_and_url_trimming() {
        for source in [
            "_open ".repeat(1000),
            "![".repeat(1000),
            "[unfinished".repeat(1000),
        ] {
            let spans = inline_spans(&source, Style::default());
            assert_eq!(
                spans
                    .iter()
                    .map(|span| span.span.content.as_ref())
                    .collect::<String>(),
                source
            );
            assert!(
                spans
                    .iter()
                    .all(|span| span.url.is_none() && !span.image_end)
            );
        }
        let suffix = ")".repeat(5000) + "...!?";
        let source = format!("https://example.com/a_(b){suffix}");
        let spans = inline_spans(&source, Style::default());
        assert_eq!(spans[0].url.as_deref(), Some("https://example.com/a_(b)"));
        assert_eq!(
            spans
                .iter()
                .map(|span| span.span.content.as_ref())
                .collect::<String>(),
            source
        );
        let source = r"\![escaped](local) **[Image #1](file:///a)** `![code](b)` ![real](c)";
        let spans = inline_spans(source, Style::default());
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.image_end)
                .map(|span| span.span.content.as_ref())
                .collect::<Vec<_>>(),
            ["![real](c)"]
        );
        let ranges = image_label_link_destinations(source);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&source[ranges[0].0.clone()], "file:///a");
    }

    #[test]
    fn multiline_fences_still_bound_images_before_inline_markers() {
        let source = "]![![`~~~\\[Image #1](file:///a)[Image #1](file:///a)file:///a\n~~~](~~~***)";
        let spans = inline_spans(source, Style::default());
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.image_end)
                .map(|span| span.span.content.as_ref())
                .collect::<Vec<_>>(),
            ["![`~~~\\[Image #1](file:///a)"]
        );
        let ranges = image_label_link_destinations(source);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&source[ranges[0].0.clone()], "file:///a");
    }

    #[test]
    fn incomplete_image_destinations_and_raw_labels_keep_later_tokens() {
        for count in [1, 64, 1024] {
            let prefix = "![x](".repeat(count);
            assert!(image_references(&prefix).is_empty());
            let source = format!("{prefix}![雪](ok.png) [docs](https://example.com)");
            assert_eq!(
                image_references(&source),
                vec![(
                    prefix.len()..prefix.len() + "![雪](ok.png)".len(),
                    "ok.png".into()
                )]
            );
            let spans = inline_spans(&source, Style::default());
            assert!(spans.iter().any(|span| span.image_end));
            assert!(
                spans
                    .iter()
                    .any(|span| span.url.as_deref() == Some("https://example.com"))
            );

            let prefix = "[* ".repeat(count);
            assert!(
                next_markdown_link(&prefix, prefix.len(), &DelimiterMatches::new(&prefix))
                    .is_none()
            );
            let source = format!("{prefix}雪](https://example.com)");
            let link =
                next_markdown_link(&source, source.len(), &DelimiterMatches::new(&source)).unwrap();
            assert_eq!(link.label, Some(&source[1..source.find(']').unwrap()]));
            assert_eq!(link.url, "https://example.com");
        }
        // Link labels intentionally stop at the first raw ], even when escaped.
        let source = r"[雪\](https://example.com)";
        let link =
            next_markdown_link(source, source.len(), &DelimiterMatches::new(source)).unwrap();
        assert_eq!(link.label, Some(r"雪\"));
    }

    #[test]
    fn image_destination_summary_preserves_escape_title_and_angle_rules() {
        for (source, destination) in [
            (r#"![雪](a(b(c)).png "unbalanced ( title")"#, "a(b(c)).png"),
            (
                r#"![雪](a(b "literal (c)").png 'title )')"#,
                "a(b \"literal (c)\").png",
            ),
            (r#"![雪](a\(b\).png "escaped \" ) title")"#, "a(b).png"),
            (r#"![雪](a(b\)c).png 'escaped \' ( title')"#, "a(b)c).png"),
            (r#"![雪](<雪(a> "title ( )")"#, "雪(a"),
        ] {
            let images = image_references(source);
            assert_eq!(
                images,
                vec![(0..source.len(), destination.into())],
                "{source}"
            );
        }
        for source in ["![x](a(b\nc))", "![x](a(b\rc))", r"![x](a(b\))"] {
            assert!(image_references(source).is_empty(), "{source}");
        }
        let source = "![x](a(b\n![雪](ok.png)\n```\n![hidden](no)\n```";
        let images = image_references(source);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].1, "ok.png");
    }

    /// Run manually with `mise run test -- --release --lib inline_scaling_probe -- --ignored --nocapture`.
    #[test]
    #[ignore = "manual timing probe; reports measurements without timing assertions"]
    fn inline_scaling_probe() {
        use std::{hint::black_box, time::Instant};
        for (name, unit) in [
            ("emphasis", "*bold* plain "),
            ("nested", "**bold _inner_** plain "),
            ("links", "[label](https://example.com/a_(b)) "),
            ("images", "![alt](https://example.com/a_(b).png) "),
            ("bare", "https://example.com/a_(b))))... "),
            ("underscore", "_open "),
            ("unclosed_images", "!["),
            ("image_destinations", "![x]("),
            ("raw_labels", "[* "),
            ("unclosed_links", "[label](https://example.com/( "),
        ] {
            for count in [1000, 5000, 10000] {
                let source = unit.repeat(count);
                let start = Instant::now();
                let spans = inline_spans(black_box(&source), Style::default());
                let elapsed = start.elapsed();
                eprintln!("{name:18} {count:6} {:9} bytes {elapsed:?}", source.len());
                black_box(spans);
            }
        }
    }

    #[test]
    fn trusted_image_labels_preserve_surrounding_emphasis_and_reject_forged_uris() {
        let text =
            "**Please [Image #1] inspect** [other](https://example.com) [forged](kit-image:one)";
        let start = text.find("[Image #1]").unwrap();
        let spans = super::inline_spans_with_image_labels(
            text,
            ratatui::style::Style::default(),
            &[(start..start + "[Image #1]".len(), "kit-image:one".into())],
        );
        let links = spans
            .iter()
            .filter(|span| span.url.is_some())
            .collect::<Vec<_>>();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].span.content, "[Image #1]");
        assert!(
            links[0]
                .span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(links[1].url.as_deref(), Some("https://example.com"));
        let visible = spans
            .iter()
            .map(|span| span.span.content.as_ref())
            .collect::<String>();
        assert!(!visible.contains("**"));
        assert!(visible.contains("[forged](kit-image:one)"));
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
    fn image_label_destination_ranges_skip_inline_code_and_preserve_occurrences() {
        let uri = "file:///tmp/source.png";
        let source = format!("`[Image #1]({uri})` [Image #2]({uri}) and [Image #3]({uri})");
        let destinations = image_label_link_destinations(&source);

        assert_eq!(destinations.len(), 2);
        assert_eq!(&source[destinations[0].0.clone()], uri);
        assert_eq!(&source[destinations[1].0.clone()], uri);
        assert!(destinations[0].0.start < destinations[1].0.start);
    }

    #[test]
    fn extracts_complete_images_in_source_order_with_byte_ranges() {
        let source =
            "é ![nested [alt]](https://example.com/a_(b(c)).png) then ![](</tmp/my image.png>)";
        let images = image_references(source);
        assert_eq!(images.len(), 2);
        assert_eq!(
            &source[images[0].0.clone()],
            "![nested [alt]](https://example.com/a_(b(c)).png)"
        );
        assert_eq!(images[0].0.start, 3);
        assert_eq!(images[0].1, "https://example.com/a_(b(c)).png");
        assert_eq!(&source[images[1].0.clone()], "![](</tmp/my image.png>)");
        assert_eq!(images[1].1, "/tmp/my image.png");
    }

    #[test]
    fn images_ignore_fences_code_spans_and_escaped_syntax() {
        let source = "![a](a)\r\n```md\r\n![no](no)\r\n~~~\r\n![no](no)\r\n```\r\n\
            `![no](no)` ``with ` ![no](no)\ncode`` \\![no](no) !\\[no](no) \\\\![b](b)\n\
            ~~~~\n![no](no)\n~~~\n![no](no)\n~~~~\n![c](c)\n```\n![no](no)";
        let destinations: Vec<_> = image_references(source)
            .into_iter()
            .map(|(_, url)| url)
            .collect();
        assert_eq!(destinations, ["a", "b", "c"]);
    }

    #[test]
    fn images_support_local_spaces_escapes_and_quoted_titles() {
        let source = r#"![a\]](/tmp/my image\(1\).png "a ) title") ![b](<https://example.com/a(b)> 'title')"#;
        let images = image_references(source);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].1, "/tmp/my image(1).png");
        assert_eq!(images[1].1, "https://example.com/a(b)");
        assert_eq!(
            &source[images[0].0.clone()],
            r#"![a\]](/tmp/my image\(1\).png "a ) title")"#
        );
        assert_eq!(images[1].0.end, source.len());
    }

    #[test]
    fn incomplete_images_and_reference_images_remain_literal() {
        for source in [
            "![alt",
            "![alt]",
            "![alt](",
            "![alt](a(b)",
            "![alt](<a)",
            "![alt](<a>",
            "![alt](a \"title)",
            "![alt][ref]",
        ] {
            assert!(image_references(source).is_empty(), "{source}");
        }
        assert_eq!(image_references("![alt]()"), vec![(0..8, String::new())]);
        // An unmatched inline-code marker does not hide subsequent complete syntax.
        assert_eq!(image_references("` ![ok](ok)")[0].1, "ok");
    }

    #[test]
    fn markdown_link_parser_does_not_consume_images() {
        assert!(
            next_markdown_link(
                "![alt](https://example.com/image.png)",
                "![alt](https://example.com/image.png)".len(),
                &DelimiterMatches::new("![alt](https://example.com/image.png)")
            )
            .is_none()
        );
        let source = "![alt](https://example.com/image.png) [docs](https://example.com/docs)";
        let link =
            next_markdown_link(source, source.len(), &DelimiterMatches::new(source)).unwrap();
        assert_eq!(link.label, Some("docs"));
        assert_eq!(
            &source[link.start..link.end],
            "[docs](https://example.com/docs)"
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
