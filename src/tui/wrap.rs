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

#[derive(Clone, Debug)]
pub struct LinkedSpan {
    pub span: Span<'static>,
    pub url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LinkedLine {
    pub spans: Vec<LinkedSpan>,
}

impl LinkedLine {
    pub fn plain(line: Line<'static>) -> Self {
        Self {
            spans: line
                .spans
                .into_iter()
                .map(|span| LinkedSpan { span, url: None })
                .collect(),
        }
    }

    pub fn line(&self) -> Line<'static> {
        Line::from(
            self.spans
                .iter()
                .map(|span| span.span.clone())
                .collect::<Vec<_>>(),
        )
    }

    fn width(&self) -> usize {
        self.spans
            .iter()
            .map(|span| span.span.content.width())
            .sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkHit {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

/// Wraps rendered lines to `width` columns.
pub fn wrap(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let tagged: Vec<(Line<'static>, ())> = lines.iter().map(|line| (line.clone(), ())).collect();
    wrap_tagged(&tagged, width)
        .into_iter()
        .map(|(line, ())| line)
        .collect()
}

/// Wraps lines that carry a tag, copying each source line's tag onto every
/// display row it produces. The client uses this to know which tool call a
/// clicked row belongs to.
pub fn wrap_tagged<T: Clone>(
    lines: &[(Line<'static>, T)],
    width: usize,
) -> Vec<(Line<'static>, T)> {
    if width == 0 {
        return Vec::new();
    }
    let mut wrapped = Vec::with_capacity(lines.len());
    for (line, tag) in lines {
        if line.width() <= width {
            wrapped.push((line.clone(), tag.clone()));
        } else {
            wrapped.extend(
                wrap_line(line, width)
                    .into_iter()
                    .map(|line| (line, tag.clone())),
            );
        }
    }
    wrapped
}

/// Wraps lines whose spans carry exact link identity, retaining that identity
/// in the hit ranges for every display row produced.
pub fn wrap_linked_tagged<T: Clone>(
    lines: &[(LinkedLine, T)],
    width: usize,
) -> Vec<(Line<'static>, T, Vec<LinkHit>)> {
    if width == 0 {
        return Vec::new();
    }
    let mut wrapped = Vec::with_capacity(lines.len());
    for (line, tag) in lines {
        for row in wrap_linked_line(line, width) {
            let (line, hits) = linked_flush(row);
            wrapped.push((line, tag.clone(), hits));
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
fn wrap_linked_line(line: &LinkedLine, width: usize) -> Vec<Vec<LinkedSpan>> {
    if line.width() <= width {
        return vec![line.spans.clone()];
    }
    let indent = hanging_indent(&line.line()).min(width / 2);
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;

    for (chunk, style, url) in linked_chunks(line) {
        let chunk_width = chunk.width();
        let blank = chunk.trim().is_empty();
        if blank && spans.is_empty() {
            continue;
        }
        if used + chunk_width > width && !spans.is_empty() {
            trim_linked_and_push(&mut lines, &mut spans);
            used = 0;
            if blank {
                continue;
            }
            if indent > 0 {
                spans.push(LinkedSpan {
                    span: Span::raw(" ".repeat(indent)),
                    url: None,
                });
                used = indent;
            }
        }
        if used + chunk_width <= width {
            used += chunk_width;
            spans.push(LinkedSpan {
                span: Span::styled(chunk, style),
                url,
            });
            continue;
        }
        for character in chunk.chars() {
            let character = character.to_string();
            let character_width = character.width();
            if used + character_width > width {
                trim_linked_and_push(&mut lines, &mut spans);
                used = indent;
                if indent > 0 {
                    spans.push(LinkedSpan {
                        span: Span::raw(" ".repeat(indent)),
                        url: None,
                    });
                }
            }
            used += character_width;
            spans.push(LinkedSpan {
                span: Span::styled(character, style),
                url: url.clone(),
            });
        }
    }
    if !spans.is_empty() {
        trim_linked_and_push(&mut lines, &mut spans);
    }
    lines
}

fn trim_linked_and_push(lines: &mut Vec<Vec<LinkedSpan>>, spans: &mut Vec<LinkedSpan>) {
    while spans
        .last()
        .is_some_and(|span| span.span.content.trim().is_empty())
    {
        spans.pop();
    }
    lines.push(std::mem::take(spans));
}

fn linked_flush(spans: Vec<LinkedSpan>) -> (Line<'static>, Vec<LinkHit>) {
    let mut hits: Vec<LinkHit> = Vec::new();
    let mut column = 0;
    for linked in &spans {
        let span_width = linked.span.content.width();
        if let Some(url) = &linked.url {
            if let Some(hit) = hits.last_mut()
                && hit.end == column
                && hit.url == *url
            {
                hit.end += span_width;
            } else {
                hits.push(LinkHit {
                    start: column,
                    end: column + span_width,
                    url: url.clone(),
                });
            }
        }
        column += span_width;
    }
    let line = Line::from(spans.into_iter().map(|span| span.span).collect::<Vec<_>>());
    (line, hits)
}

fn linked_chunks(line: &LinkedLine) -> Vec<(String, Style, Option<String>)> {
    let mut chunks = Vec::new();
    for linked in &line.spans {
        for (chunk, style) in span_chunks(&linked.span) {
            chunks.push((chunk, style, linked.url.clone()));
        }
    }
    chunks
}

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
    line.spans.iter().flat_map(span_chunks).collect()
}

fn span_chunks(span: &Span<'static>) -> Vec<(String, Style)> {
    let mut chunks = Vec::new();
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
