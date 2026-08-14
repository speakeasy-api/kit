//! Word wrapping that keeps styles and hangs continuations under their gutter.
//!
//! The transcript scrolls by line, so the client needs to know exactly how
//! many display lines it has. Wrapping here rather than inside `Paragraph`
//! makes the count exact and lets a wrapped bullet or tool row line up under
//! its own indent instead of resetting to column zero.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// Wraps rendered lines to `width` columns.
pub fn wrap(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut wrapped = Vec::with_capacity(lines.len());
    for line in lines {
        if line.width() <= width {
            wrapped.push(line.clone());
        } else {
            wrapped.append(&mut wrap_line(line, width));
        }
    }
    wrapped
}

fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let indent = hanging_indent(line).min(width / 2);
    let mut lines = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;

    for (chunk, style) in chunks(line) {
        let chunk_width = chunk.width();
        let blank = chunk.trim().is_empty();
        if blank && spans.is_empty() {
            continue;
        }
        if used + chunk_width > width && !spans.is_empty() {
            lines.push(flush(&mut spans));
            used = 0;
            if blank {
                continue;
            }
            if indent > 0 {
                spans.push(Span::raw(" ".repeat(indent)));
                used = indent;
            }
        }
        if used + chunk_width <= width {
            used += chunk_width;
            spans.push(Span::styled(chunk, style));
            continue;
        }
        // A single token longer than the line: break it at the margin.
        for character in chunk.chars() {
            let character_width = character.to_string().width();
            if used + character_width > width {
                lines.push(flush(&mut spans));
                used = indent;
                if indent > 0 {
                    spans.push(Span::raw(" ".repeat(indent)));
                }
            }
            used += character_width;
            spans.push(Span::styled(character.to_string(), style));
        }
    }
    if !spans.is_empty() {
        lines.push(flush(&mut spans));
    }
    lines
}

/// Takes the pending spans as a line, without the trailing space that the
/// wrap point left behind.
fn flush(spans: &mut Vec<Span<'static>>) -> Line<'static> {
    while spans
        .last()
        .is_some_and(|span| span.content.trim().is_empty())
    {
        spans.pop();
    }
    Line::from(std::mem::take(spans))
}

/// Splits a line into styled words and whitespace runs.
fn chunks(line: &Line<'static>) -> Vec<(String, Style)> {
    let mut chunks = Vec::new();
    for span in &line.spans {
        let mut current = String::new();
        let mut blank = None;
        for character in span.content.chars() {
            let is_blank = character.is_whitespace();
            if blank.is_some_and(|previous| previous != is_blank) && !current.is_empty() {
                chunks.push((std::mem::take(&mut current), span.style));
            }
            blank = Some(is_blank);
            current.push(character);
        }
        if !current.is_empty() {
            chunks.push((current, span.style));
        }
    }
    chunks
}

/// Continuation indent: the line's own leading whitespace, plus the width of a
/// leading gutter span such as `› ` or `✓ ` so wrapped text stays aligned.
fn hanging_indent(line: &Line<'static>) -> usize {
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let leading = text.len() - text.trim_start().len();
    let gutter = line
        .spans
        .first()
        .filter(|span| span.content.ends_with(' ') && span.content.width() <= 8)
        .map_or(0, |span| span.content.width());
    leading.max(gutter)
}

#[cfg(test)]
mod tests {
    use ratatui::text::{Line, Span};

    use super::wrap;

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn breaks_on_word_boundaries() {
        let lines = wrap(&[Line::from("one two three four")], 9);
        assert_eq!(text(&lines), ["one two", "three", "four"]);
    }

    #[test]
    fn hangs_continuations_under_the_gutter() {
        let line = Line::from(vec![Span::raw("› "), Span::raw("alpha beta gamma")]);
        assert_eq!(text(&wrap(&[line], 10)), ["› alpha", "  beta", "  gamma"]);
    }

    #[test]
    fn splits_tokens_wider_than_the_line() {
        assert_eq!(
            text(&wrap(&[Line::from("abcdefghij")], 4)),
            ["abcd", "efgh", "ij"]
        );
    }

    #[test]
    fn leaves_short_lines_alone() {
        let lines = wrap(&[Line::from("short")], 40);
        assert_eq!(lines.len(), 1);
    }
}
