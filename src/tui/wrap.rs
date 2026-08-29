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
    leading_gutter: Option<bool>,
}

impl LinkedLine {
    pub fn new(spans: Vec<LinkedSpan>) -> Self {
        Self {
            spans,
            leading_gutter: Some(false),
        }
    }

    pub fn plain(line: Line<'static>) -> Self {
        Self {
            spans: line
                .spans
                .into_iter()
                .map(|span| LinkedSpan { span, url: None })
                .collect(),
            leading_gutter: None,
        }
    }

    pub fn with_leading_gutter(mut self) -> Self {
        self.leading_gutter = Some(true);
        self
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

/// Wraps lines whose spans carry exact link identity, retaining that identity
/// in the hit ranges for every display row produced. The final tuple field is
/// source whitespace removed before that row, so copy can restore word wraps
/// without inserting spaces into hard-wrapped tokens.
pub fn wrap_linked_tagged<T: Clone>(
    lines: &[(LinkedLine, T)],
    width: usize,
) -> Vec<(Line<'static>, T, Vec<LinkHit>, String)> {
    if width == 0 {
        return Vec::new();
    }
    let mut wrapped = Vec::with_capacity(lines.len());
    for (line, tag) in lines {
        for (row, separator) in wrap_linked_line(line, width) {
            let (line, hits) = linked_flush(row);
            wrapped.push((line, tag.clone(), hits, separator));
        }
    }
    wrapped
}

/// Takes the pending spans as a line, without the trailing space that the
/// wrap point left behind.
fn wrap_linked_line(line: &LinkedLine, width: usize) -> Vec<(Vec<LinkedSpan>, String)> {
    if line.width() <= width {
        return vec![(line.spans.clone(), String::new())];
    }
    let indent = hanging_indent(&line.line(), line.leading_gutter).min(width / 2);
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut separator = String::new();
    let mut used = 0;

    for (chunk, style, url) in linked_chunks(line) {
        let chunk_width = chunk.width();
        let blank = chunk.trim().is_empty();
        if blank && spans.is_empty() && !lines.is_empty() {
            separator.push_str(&chunk);
            continue;
        }
        if used + chunk_width > width && used > indent {
            let before = std::mem::take(&mut separator);
            let trailing = trim_linked_and_push(&mut lines, &mut spans, before);
            separator.push_str(&trailing);
            used = 0;
            if blank {
                separator.push_str(&chunk);
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
        if spans.is_empty() && !lines.is_empty() && !blank && indent > 0 {
            spans.push(LinkedSpan {
                span: Span::raw(" ".repeat(indent)),
                url: None,
            });
            used = indent;
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
                let before = std::mem::take(&mut separator);
                let trailing = trim_linked_and_push(&mut lines, &mut spans, before);
                separator.push_str(&trailing);
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
        trim_linked_and_push(&mut lines, &mut spans, separator);
    }
    lines
}

fn trim_linked_and_push(
    lines: &mut Vec<(Vec<LinkedSpan>, String)>,
    spans: &mut Vec<LinkedSpan>,
    separator: String,
) -> String {
    let mut trailing = Vec::new();
    while spans
        .last()
        .is_some_and(|span| span.span.content.trim().is_empty())
    {
        trailing.push(spans.pop().expect("the final span exists").span.content);
    }
    trailing.reverse();
    lines.push((std::mem::take(spans), separator));
    trailing.concat()
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
    for (index, linked) in line.spans.iter().enumerate() {
        if index == 0 && line.leading_gutter == Some(true) {
            chunks.push((
                linked.span.content.to_string(),
                linked.span.style,
                linked.url.clone(),
            ));
            continue;
        }
        for (chunk, style) in span_chunks(&linked.span) {
            chunks.push((chunk, style, linked.url.clone()));
        }
    }
    chunks
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
fn hanging_indent(line: &Line<'static>, leading_gutter: Option<bool>) -> usize {
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let leading_width = |text: &str| {
        let end = text.len() - text.trim_start().len();
        UnicodeWidthStr::width(&text[..end])
    };
    let leading = leading_width(&text);
    let gutter_span = match leading_gutter {
        Some(true) => line.spans.first(),
        Some(false) => None,
        None => line
            .spans
            .first()
            .filter(|span| span.content.ends_with(' ') && span.content.width() <= 8),
    };
    let Some(gutter_span) = gutter_span else {
        return leading;
    };
    let gutter = gutter_span.content.width();
    let content_indent = if gutter_span.content.trim().is_empty() {
        0
    } else {
        leading_width(&text[gutter_span.content.len()..])
    };
    leading.max(gutter + content_indent)
}

#[cfg(test)]
mod tests {
    use ratatui::text::{Line, Span};

    use super::{LinkedLine, wrap_linked_tagged};

    fn wrap(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
        let linked = lines
            .iter()
            .cloned()
            .map(|line| {
                let has_gutter = line.spans.len() > 1
                    && line
                        .spans
                        .first()
                        .is_some_and(|span| !span.content.trim().is_empty());
                let line = LinkedLine::plain(line);
                (
                    if has_gutter {
                        line.with_leading_gutter()
                    } else {
                        line
                    },
                    (),
                )
            })
            .collect::<Vec<_>>();
        wrap_linked_tagged(&linked, width)
            .into_iter()
            .map(|(line, (), _, _)| line)
            .collect()
    }

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
    fn preserves_content_indent_after_a_visible_gutter() {
        let line = Line::from(vec![Span::raw("│ "), Span::raw("  alpha beta")]);
        assert_eq!(
            text(&wrap(std::slice::from_ref(&line), 10)),
            ["│   alpha", "    beta"]
        );

        let linked = wrap_linked_tagged(&[(LinkedLine::plain(line).with_leading_gutter(), ())], 10);
        let linked_lines = linked
            .into_iter()
            .map(|(line, (), _, _)| line)
            .collect::<Vec<_>>();
        assert_eq!(text(&linked_lines), ["│   alpha", "    beta"]);
    }

    #[test]
    fn fills_the_first_row_before_splitting_an_oversized_first_token() {
        let line = Line::from(vec![Span::raw("│ "), Span::raw("  abcdefghijk")]);
        assert_eq!(
            text(&wrap(std::slice::from_ref(&line), 10)),
            ["│   abcdef", "    ghijk"]
        );

        let linked = wrap_linked_tagged(&[(LinkedLine::plain(line).with_leading_gutter(), ())], 10);
        let linked_lines = linked
            .into_iter()
            .map(|(line, (), _, _)| line)
            .collect::<Vec<_>>();
        assert_eq!(text(&linked_lines), ["│   abcdef", "    ghijk"]);
    }

    #[test]
    fn preserves_a_leading_margin_when_the_line_wraps() {
        let line = Line::from(vec![Span::raw("  "), Span::raw("alpha beta gamma")]);
        assert_eq!(
            text(&wrap(std::slice::from_ref(&line), 10)),
            ["  alpha", "  beta", "  gamma"]
        );

        let linked = wrap_linked_tagged(&[(LinkedLine::plain(line), ())], 10);
        let linked_lines = linked
            .into_iter()
            .map(|(line, (), _, _)| line)
            .collect::<Vec<_>>();
        assert_eq!(text(&linked_lines), ["  alpha", "  beta", "  gamma"]);
    }

    #[test]
    fn restores_the_margin_when_whitespace_causes_the_wrap() {
        let line = Line::from(vec![Span::raw("› "), Span::raw("alpha beta")]);
        assert_eq!(text(&wrap(&[line], 7)), ["› alpha", "  beta"]);
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
