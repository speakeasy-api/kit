//! Frame drawing: header, transcript, runtime graph, prompt, status.

use std::ops::Range;

use agent_client_protocol::schema::v2::{ToolCallStatus, ToolKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block as Panel, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
};
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
thread_local! {
    static MATERIALIZED_TRANSCRIPT_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REFRESHED_TRANSCRIPT_BLOCKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static VISITED_TRANSCRIPT_BLOCKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

use super::{
    app::{
        App, Block, CachedTranscriptBlock, CachedTranscriptImage, CachedTranscriptRow, Child,
        CodeHit, Phase, ToolCall, UserMessage,
    },
    command,
    image::{ImageRuntime, RESERVED_ROWS},
    markdown,
    plan::PlanKind,
    theme,
    wrap::{LinkedLine, LinkedSpan, wrap, wrap_linked_tagged},
};

/// Width at which the graph moves beside the transcript instead of below it.
const SIDE_BY_SIDE_WIDTH: u16 = 108;
const GRAPH_WIDTH: u16 = 46;
const MAX_PROMPT_ROWS: usize = 10;
const MAX_PENDING_STEER_ROWS: usize = 3;
/// Rows of raw tool output rendered when a card is opened.
const MAX_OUTPUT_ROWS: usize = 400;

type TranscriptTag = (Option<String>, Option<CodeHit>, Option<usize>);
type TaggedTranscriptLine = (LinkedLine, TranscriptTag);

pub fn draw(frame: &mut Frame<'_>, app: &mut App, images: &mut ImageRuntime) {
    // Two border columns plus the `›` gutter; the prompt grows as the wrapped
    // text needs more rows, up to the cap.
    let prompt_width = frame.area().width.saturating_sub(4).max(1) as usize;
    app.prompt_width = prompt_width;
    let prompt_rows = app
        .editor
        .display_rows(prompt_width)
        .clamp(1, MAX_PROMPT_ROWS) as u16
        + 2;
    let logs_rows = if app.show_logs { 9 } else { 0 };
    let pending_rows = app.pending_steers.len().min(MAX_PENDING_STEER_ROWS) as u16;
    let [header, body, logs, pending, prompt, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(logs_rows),
        Constraint::Length(pending_rows),
        Constraint::Length(prompt_rows),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    draw_body(frame, app, images, body);
    if app.show_logs {
        draw_logs(frame, app, logs);
    }
    draw_pending_steers(frame, app, pending);
    draw_prompt(frame, app, prompt);
    draw_status(frame, app, status);
    if app.model_dialog.is_some() {
        draw_model_dialog(frame, app);
    } else if app.effort_dialog.is_some() {
        draw_effort_dialog(frame, app);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ModelDialogRow<'a> {
    Provider(&'a str),
    Choice { index: usize, label: String },
}

fn model_dialog_rows<'a>(
    choices: &[&'a crate::tui::app::ModelChoice],
    grouped: bool,
) -> Vec<ModelDialogRow<'a>> {
    let mut rows = Vec::new();
    let mut last_provider = None;
    for (index, choice) in choices.iter().enumerate() {
        if grouped && last_provider != Some(choice.provider.as_str()) {
            rows.push(ModelDialogRow::Provider(&choice.provider));
            last_provider = Some(&choice.provider);
        }
        let label = if grouped {
            choice.model.clone()
        } else {
            format!("{} · {}", choice.provider, choice.model)
        };
        rows.push(ModelDialogRow::Choice { index, label });
    }
    rows
}

fn model_dialog_viewport(
    rows: &[ModelDialogRow<'_>],
    selected: usize,
    height: usize,
) -> std::ops::Range<usize> {
    if height == 0 || rows.is_empty() {
        return 0..0;
    }
    let selected_row = rows
        .iter()
        .position(|row| matches!(row, ModelDialogRow::Choice { index, .. } if *index == selected))
        .unwrap_or(0);
    let start = selected_row
        .saturating_sub(height / 2)
        .min(rows.len().saturating_sub(height));
    start..(start + height).min(rows.len())
}

fn visible_query_tail(query: &str, width: usize) -> &str {
    let mut tail = query;
    while UnicodeWidthStr::width(tail) > width {
        let Some((index, _)) = tail.char_indices().nth(1) else {
            return "";
        };
        tail = &tail[index..];
    }
    tail
}

fn draw_effort_dialog(frame: &mut Frame<'_>, app: &App) {
    let outer = frame.area();
    let width = outer.width.min(48);
    let height = outer.height.min(app.effort_choices.len() as u16 + 3);
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let dialog = app.effort_dialog.as_ref().expect("checked above");
    let footer = format!(
        "tab defaults [{}] · enter select · esc close",
        if dialog.save_defaults { "x" } else { " " }
    );
    let panel = Panel::bordered().title(" reasoning effort ");
    let inner = panel.inner(area);
    let footer_rows = u16::from(inner.height > 1);
    let [list, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(footer_rows)]).areas(inner);
    let lines = app
        .effort_choices
        .iter()
        .enumerate()
        .take(list.height as usize)
        .map(|(index, choice)| {
            let selected = index == dialog.selected;
            Line::from(Span::styled(
                format!("{}{}", if selected { "› " } else { "  " }, choice.name),
                if selected {
                    theme::accent()
                } else {
                    theme::text()
                },
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(panel, area);
    frame.render_widget(Paragraph::new(lines), list);
    frame.render_widget(
        Paragraph::new(Span::styled(footer, theme::dim())),
        footer_area,
    );
}

fn draw_model_dialog(frame: &mut Frame<'_>, app: &App) {
    let outer = frame.area();
    let width = if outer.width > 20 {
        outer.width.saturating_sub(4).min(72)
    } else {
        outer.width
    };
    let height = if outer.height > 8 {
        outer.height.saturating_sub(4).min(22)
    } else {
        outer.height
    };
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let dialog = app.model_dialog.as_ref().expect("checked above");
    let choices = app.selected_model_choices();
    let footer = format!(
        "tab defaults [{}]  ·  enter select  ·  esc close",
        if dialog.save_defaults { "x" } else { " " }
    );
    let panel = Panel::bordered().title(" model ");
    let inner = panel.inner(area);
    let footer_rows = u16::from(inner.height >= 3);
    let search_rows = u16::from(inner.height >= 2);
    let [search, list, footer_area] = Layout::vertical([
        Constraint::Length(search_rows),
        Constraint::Min(0),
        Constraint::Length(footer_rows),
    ])
    .areas(inner);
    let visible = list.height as usize;
    let rows = model_dialog_rows(&choices, dialog.query.trim().is_empty());
    let viewport = model_dialog_viewport(&rows, dialog.selected, visible);
    let lines = rows[viewport]
        .iter()
        .map(|row| match row {
            ModelDialogRow::Provider(provider) => {
                Line::from(Span::styled((*provider).to_owned(), theme::faint()))
            }
            ModelDialogRow::Choice { index, label } => {
                let selected = *index == dialog.selected;
                let marker = if selected { "› " } else { "  " };
                let style = if selected {
                    theme::accent()
                } else {
                    theme::text()
                };
                Line::from(Span::styled(format!("{marker}{label}"), style))
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(panel, area);
    let search_prefix = if search.width >= 8 {
        "search: "
    } else {
        "› "
    };
    let prefix_width = UnicodeWidthStr::width(search_prefix).min(search.width as usize);
    let query = visible_query_tail(
        &dialog.query,
        (search.width as usize)
            .saturating_sub(prefix_width)
            .saturating_sub(1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(search_prefix, theme::dim()),
            Span::styled(query.to_owned(), theme::text()),
        ])),
        search,
    );
    frame.render_widget(Paragraph::new(lines), list);
    frame.render_widget(
        Paragraph::new(Span::styled(footer, theme::dim())),
        footer_area,
    );
    if search.width > 0 && search.height > 0 {
        let column = prefix_width + UnicodeWidthStr::width(query);
        frame.set_cursor_position(Position::new(
            search.x
                + u16::try_from(column)
                    .unwrap_or(u16::MAX)
                    .min(search.width - 1),
            search.y,
        ));
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let root = app.root.file_name().map_or_else(
        || app.root.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut spans = vec![
        Span::styled(" kit ", theme::bold(theme::accent_color())),
        Span::styled("▏ ", theme::faint()),
        Span::styled(root, theme::text()),
        Span::styled("  ·  ", theme::faint()),
        Span::styled(format!("{} / {}", app.provider, app.model), theme::dim()),
        Span::styled("  ·  ", theme::faint()),
        Span::styled(format!("effort {}", app.reasoning_effort), theme::dim()),
        Span::styled("  ·  ", theme::faint()),
        Span::styled(
            format!(
                "session {}",
                app.session_id.as_deref().unwrap_or("starting")
            ),
            theme::dim(),
        ),
    ];
    if let Some(usage) = app.usage {
        spans.push(Span::styled("  ·  ", theme::faint()));
        spans.push(Span::styled(
            format!(
                "{} {}/{}",
                percent(usage.used, usage.size),
                compact(usage.used),
                compact(usage.size)
            ),
            theme::dim(),
        ));
    }
    spans.push(Span::styled("  ·  ", theme::faint()));
    spans.push(Span::styled(format!("a2a {}", app.a2a), theme::dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme::bar()), area);
}

fn draw_body(frame: &mut Frame<'_>, app: &mut App, images: &mut ImageRuntime, area: Rect) {
    if !app.show_graph() {
        draw_transcript(frame, app, images, area);
        return;
    }
    if area.width >= SIDE_BY_SIDE_WIDTH {
        let [transcript, graph] =
            Layout::horizontal([Constraint::Min(40), Constraint::Length(GRAPH_WIDTH)]).areas(area);
        draw_transcript(frame, app, images, transcript);
        draw_graph(frame, app, graph);
    } else {
        let [transcript, graph] =
            Layout::vertical([Constraint::Min(6), Constraint::Percentage(45)]).areas(area);
        draw_transcript(frame, app, images, transcript);
        draw_graph(frame, app, graph);
    }
}

fn draw_transcript(frame: &mut Frame<'_>, app: &mut App, images: &mut ImageRuntime, area: Rect) {
    let [text_area, bar_area] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let inner = Rect {
        x: text_area.x + 1,
        width: text_area.width.saturating_sub(2),
        ..text_area
    };
    if app.blocks.is_empty() {
        app.row_calls.clear();
        app.row_links.clear();
        app.row_code.clear();
        frame.render_widget(welcome(), inner);
        return;
    }

    let width = inner.width.max(1) as usize;
    refresh_transcript_cache_with_images(app, images, width);
    let working_rows = if app.working() {
        wrap_linked_tagged(
            &[(LinkedLine::plain(working_line(app)), (None, None, None))],
            width,
        )
    } else {
        Vec::new()
    };
    let transcript_rows = app.transcript_prefixes.last().copied().unwrap_or(0);
    let total = transcript_rows + usize::from(!working_rows.is_empty()) + working_rows.len();
    let height = inner.height as usize;
    app.total_lines = total;
    app.viewport = height;
    app.transcript_top = inner.y as usize;
    app.transcript_left = inner.x as usize;
    app.transcript_width = inner.width as usize;
    let bottom = total.saturating_sub(height);
    let offset = if app.follow {
        bottom
    } else {
        app.scroll.min(bottom)
    };
    app.scroll = offset;

    app.row_calls.clear();
    app.row_code.clear();
    app.row_links.clear();
    app.row_calls.reserve(height);
    app.row_code.reserve(height);
    app.row_links.reserve(height);
    let mut visible = Vec::with_capacity(height);
    let end = offset.saturating_add(height);
    let separator = (
        Line::default(),
        (None, None, None),
        Vec::new(),
        String::new(),
    );
    let mut visible_images: Vec<(usize, usize, i16)> = Vec::new();
    let mut materialize = |row: &crate::tui::app::CachedTranscriptRow| {
        #[cfg(test)]
        MATERIALIZED_TRANSCRIPT_ROWS.with(|count| count.set(count.get() + 1));
        visible.push(row.0.clone());
        app.row_calls.push(row.1.0.clone());
        app.row_code.push(row.1.1.clone());
        app.row_links.push(row.2.clone());
    };
    if offset < transcript_rows && !app.blocks.is_empty() {
        let mut block_index = app
            .transcript_prefixes
            .partition_point(|prefix| *prefix <= offset)
            .saturating_sub(1)
            .min(app.blocks.len() - 1);
        while block_index < app.blocks.len() {
            #[cfg(test)]
            VISITED_TRANSCRIPT_BLOCKS.with(|count| count.set(count.get() + 1));
            let span_start = app.transcript_prefixes[block_index];
            if span_start >= end {
                break;
            }
            let separator_rows = usize::from(span_start > 0);
            if separator_rows > 0 && offset <= span_start && span_start < end {
                materialize(&separator);
            }
            let content_start = span_start + separator_rows;
            if let Some(block) = &app.transcript_cache[block_index] {
                let first_row = offset.saturating_sub(content_start);
                for (row_index, row) in block.rows.iter().enumerate().skip(first_row) {
                    let absolute = content_start + row_index;
                    if absolute >= end {
                        break;
                    }
                    materialize(row);
                }
                for placement in &block.images {
                    let image_start = content_start + placement.row;
                    let image_end = image_start + usize::from(RESERVED_ROWS);
                    if image_start < end && image_end > offset {
                        let y = image_start as isize - offset as isize;
                        visible_images.push((
                            block_index,
                            placement.source,
                            y.clamp(i16::MIN as isize, i16::MAX as isize) as i16,
                        ));
                    }
                }
            }
            block_index += 1;
        }
    }
    if !working_rows.is_empty() && transcript_rows < end {
        let separator_row = transcript_rows;
        if offset <= separator_row && separator_row < end {
            materialize(&separator);
        }
        let content_start = separator_row + 1;
        let first_row = offset.saturating_sub(content_start);
        for (row_index, row) in working_rows.iter().enumerate().skip(first_row) {
            if content_start + row_index >= end {
                break;
            }
            materialize(row);
        }
    }
    let row_widths: Vec<usize> = visible.iter().map(ratatui::text::Line::width).collect();
    frame.render_widget(Paragraph::new(visible), inner);
    draw_selection(frame, app, inner, offset, &row_widths);
    for (block_index, source_index, y) in visible_images {
        let Some(Block::User(message)) = app.blocks.get(block_index) else {
            continue;
        };
        let Some(source) = message.images.get(source_index) else {
            continue;
        };
        if let Some(image) = images.prepare(source, inner.width.max(1)) {
            images.render(frame, image, inner, y);
        }
    }
    if total > height {
        let mut state = ScrollbarState::new(bottom).position(offset);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_style(theme::dim())
                .track_style(theme::faint()),
            bar_area,
            &mut state,
        );
    }
}

/// Restyles the cells a drag selected. The highlight hugs each row's text
/// instead of running to the margin, so it shows exactly what a copy takes.
fn draw_selection(
    frame: &mut Frame<'_>,
    app: &App,
    inner: Rect,
    offset: usize,
    row_widths: &[usize],
) {
    let Some(selection) = app.selection else {
        return;
    };
    let (start, end) = selection.ordered();
    for (row_index, row_width) in row_widths.iter().copied().enumerate() {
        let line = offset + row_index;
        if line < start.0 || line > end.0 {
            continue;
        }
        let from = if line == start.0 { start.1 } else { 0 };
        let to = if line == end.0 {
            (end.1 + 1).min(row_width)
        } else {
            row_width
        };
        let to = to.min(inner.width as usize);
        if from >= to {
            continue;
        }
        frame.buffer_mut().set_style(
            Rect {
                x: inner.x + from as u16,
                y: inner.y + row_index as u16,
                width: (to - from) as u16,
                height: 1,
            },
            theme::selection(),
        );
    }
}

fn welcome() -> Paragraph<'static> {
    let lines = vec![
        Line::default(),
        Line::from(Span::styled("kit", theme::bold(theme::accent_color()))),
        Line::from(Span::styled(
            "a directory-rooted coding agent",
            theme::dim(),
        )),
        Line::default(),
        Line::from(Span::styled(
            "ask for a change, a review, or a command to run",
            theme::text(),
        )),
        Line::default(),
        hint("⏎", "send", "⇧⏎", "newline"),
        hint("esc", "interrupt", "^c", "quit"),
        hint("^g", "runtime graph", "^l", "agent log"),
        hint("^t", "reasoning", "⇧↑ ⇧↓", "scroll"),
    ];
    Paragraph::new(lines)
}

fn hint(left_key: &str, left: &str, right_key: &str, right: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{left_key:>5} "), theme::accent()),
        Span::styled(format!("{left:<18}"), theme::dim()),
        Span::styled(format!("{right_key:>5} "), theme::accent()),
        Span::styled(right.to_string(), theme::dim()),
    ])
}

/// Renders the transcript, tagging each line with the tool call it belongs to
/// so a click on a card can be traced back to it.
fn refresh_transcript_cache_with_images(app: &mut App, images: &mut ImageRuntime, width: usize) {
    let structure_changed = app.transcript_revisions.len() != app.blocks.len()
        || app.transcript_cache.len() != app.blocks.len()
        || app.transcript_prefixes.len() != app.blocks.len() + 1;
    if structure_changed {
        app.sync_transcript_cache();
    }
    let width_changed = app.transcript_cache_width != width;
    let mut layout_changed = structure_changed || width_changed;
    if width_changed {
        app.transcript_cache_width = width;
        app.transcript_dirty.extend(0..app.blocks.len());
    }
    app.transcript_dirty
        .extend(app.transcript_dynamic.iter().copied());
    let dirty = std::mem::take(&mut app.transcript_dirty);
    let show_graph = !dirty.is_empty() && app.show_graph();
    let mut first_changed_count = app.blocks.len();
    for block_index in dirty {
        #[cfg(test)]
        REFRESHED_TRANSCRIPT_BLOCKS.with(|count| count.set(count.get() + 1));
        let dynamic = match &app.blocks[block_index] {
            Block::Thought { millis, .. } => millis.is_none(),
            Block::Tool(call) => call.running() || call.running_children() > 0,
            _ => false,
        };
        let revision = app.transcript_revisions[block_index];
        if !width_changed
            && !dynamic
            && app.transcript_cache[block_index]
                .as_ref()
                .is_some_and(|cached| cached.revision == revision)
        {
            continue;
        }
        let missing = app.transcript_cache[block_index].is_none();
        let old_count = app.transcript_cache[block_index]
            .as_ref()
            .map_or(0, |cached| cached.rows.len());
        let (rows, cached_images) = if let Block::User(message) = &app.blocks[block_index] {
            user_block_rows(message, width, images.enabled())
        } else {
            (
                wrap_linked_tagged(&transcript_block_lines(app, block_index, show_graph), width),
                Vec::new(),
            )
        };
        if missing || rows.len() != old_count {
            first_changed_count = first_changed_count.min(block_index);
            layout_changed |= !missing;
        }
        app.transcript_cache[block_index] = Some(CachedTranscriptBlock {
            revision,
            rows,
            images: cached_images,
        });
        if dynamic {
            app.transcript_dynamic.insert(block_index);
        } else {
            app.transcript_dynamic.remove(&block_index);
        }
    }
    for index in first_changed_count..app.blocks.len() {
        let rows = app.transcript_cache[index]
            .as_ref()
            .map_or(0, |cached| cached.rows.len());
        app.transcript_prefixes[index + 1] =
            app.transcript_prefixes[index] + rows + usize::from(app.transcript_prefixes[index] > 0);
    }
    if layout_changed {
        app.clear_transcript_interaction();
    }
}

#[cfg(test)]
fn refresh_transcript_cache(app: &mut App, width: usize) {
    let mut images = ImageRuntime::disabled();
    refresh_transcript_cache_with_images(app, &mut images, width);
}

fn user_block_rows(
    message: &UserMessage,
    width: usize,
    reserve_images: bool,
) -> (Vec<CachedTranscriptRow>, Vec<CachedTranscriptImage>) {
    let mut rows = Vec::new();
    let mut placements = Vec::new();
    for (line_index, text) in message.text.split('\n').enumerate() {
        rows.extend(wrap_linked_tagged(
            &[(
                user_line(text, line_index == 0),
                (None, None, Some(line_index)),
            )],
            width,
        ));
        if reserve_images {
            for (source, _) in message
                .images
                .iter()
                .enumerate()
                .filter(|(_, image)| image.line == line_index)
            {
                let row = rows.len();
                rows.extend((0..RESERVED_ROWS).map(|_| {
                    (
                        Line::default(),
                        (None, None, None),
                        Vec::new(),
                        String::new(),
                    )
                }));
                placements.push(CachedTranscriptImage { source, row });
            }
        }
    }
    (rows, placements)
}

fn transcript_block_lines(
    app: &App,
    block_index: usize,
    show_graph: bool,
) -> Vec<TaggedTranscriptLine> {
    let block = &app.blocks[block_index];
    let (block_lines, call) = match block {
        Block::User(_) => unreachable!("user blocks are laid out with image anchors"),
        Block::Agent(text) => (markdown::render_copyable(text), None),
        Block::Thought {
            text,
            started,
            millis,
        } => (
            uncopyable(plain_lines(thought_lines(
                app,
                text,
                started.elapsed().as_millis(),
                *millis,
            ))),
            None,
        ),
        Block::Tool(call) => (
            uncopyable(plain_lines(tool_lines(
                app,
                call,
                show_graph,
                app.transcript_call_is_focused(block_index),
            ))),
            Some(call.id.clone()),
        ),
        Block::TurnDuration(millis) => (
            uncopyable(plain_lines(vec![Line::from(Span::styled(
                format!("· took {}", theme::duration(*millis)),
                theme::faint(),
            ))])),
            None,
        ),
        Block::Notice(text) => (
            uncopyable(plain_lines(vec![Line::from(Span::styled(
                format!("· {text}"),
                theme::faint(),
            ))])),
            None,
        ),
        Block::Error(text) => (
            uncopyable(plain_lines(vec![Line::from(vec![
                Span::styled("✗ ", theme::bold(theme::error_color())),
                Span::styled(text.clone(), Style::default().fg(theme::error_color())),
            ])])),
            None,
        ),
    };
    block_lines
        .into_iter()
        .enumerate()
        .map(|(line_index, (line, code))| {
            let code = code.map(|range| CodeHit {
                block: block_index,
                range,
            });
            (line, (call.clone(), code, Some(line_index)))
        })
        .collect()
}

fn uncopyable(lines: Vec<LinkedLine>) -> Vec<(LinkedLine, Option<Range<usize>>)> {
    lines.into_iter().map(|line| (line, None)).collect()
}

fn plain_lines(lines: Vec<Line<'static>>) -> Vec<LinkedLine> {
    lines.into_iter().map(LinkedLine::plain).collect()
}

fn user_line(text: &str, first: bool) -> LinkedLine {
    let style = theme::bold(theme::user_color());
    let mut spans = vec![LinkedSpan {
        span: Span::styled(if first { "› " } else { "  " }, style),
        url: None,
    }];
    spans.extend(markdown::inline_spans(text, style));
    LinkedLine { spans }
}

fn thought_lines(
    app: &App,
    text: &str,
    running_millis: u128,
    millis: Option<u64>,
) -> Vec<Line<'static>> {
    let elapsed = millis.unwrap_or(u64::try_from(running_millis).unwrap_or(u64::MAX));
    if millis.is_some() && !app.show_thoughts {
        return vec![Line::from(Span::styled(
            format!("⋮ thought for {} · ^t to read", theme::duration(elapsed)),
            theme::faint(),
        ))];
    }
    let style = theme::dim().add_modifier(Modifier::ITALIC);
    let all: Vec<&str> = text.split('\n').collect();
    let shown = if app.show_thoughts {
        all.as_slice()
    } else {
        &all[all.len().saturating_sub(4)..]
    };
    shown
        .iter()
        .map(|line| {
            Line::from(vec![
                Span::styled("⋮ ", theme::faint()),
                Span::styled((*line).to_string(), style),
            ])
        })
        .collect()
}

fn tool_lines(app: &App, call: &ToolCall, show_graph: bool, active: bool) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(tool_header(app, call, active))];
    if call.running() {
        // The live detail belongs in the graph pane, which is open while a
        // call runs; repeating it here would just churn the transcript.
        if !show_graph && let Some(child) = call.children.iter().rev().find(|child| child.running())
        {
            lines.push(Line::from(vec![
                Span::styled("   ↳ ", theme::faint()),
                Span::styled(child.summary.clone(), theme::dim()),
            ]));
        }
        return lines;
    }
    for child in call.children.iter().take(6) {
        lines.push(Line::from(child_spans(app, child, "   ")));
    }
    if call.children.len() > 6 {
        lines.push(Line::from(Span::styled(
            format!("   … {} more calls", call.children.len() - 6),
            theme::faint(),
        )));
    }
    lines.extend(output_lines(call));
    lines
}

/// Raw tool output stays folded: it is machine-shaped, often thousands of
/// lines, and unreadable inline. The fold row says how much there is and opens
/// on a click or `^o`.
fn output_lines(call: &ToolCall) -> Vec<Line<'static>> {
    if call.output.is_empty() {
        return Vec::new();
    }
    let count = call.output.len();
    if !call.expanded {
        return vec![Line::from(vec![
            Span::styled("   ▸ ", theme::dim()),
            Span::styled(
                format!("{count} {} of output", plural("line", count)),
                theme::dim(),
            ),
            Span::styled("  click or ^o to open", theme::faint()),
        ])];
    }
    let mut lines = vec![Line::from(vec![
        Span::styled("   ▾ ", theme::dim()),
        Span::styled("output", theme::dim()),
    ])];
    lines.extend(call.output.iter().take(MAX_OUTPUT_ROWS).map(|line| {
        Line::from(vec![
            Span::styled("   │ ", theme::faint()),
            Span::styled(line.clone(), theme::dim()),
        ])
    }));
    if count > MAX_OUTPUT_ROWS {
        lines.push(Line::from(Span::styled(
            format!("   │ … {} more lines", count - MAX_OUTPUT_ROWS),
            theme::faint(),
        )));
    }
    lines
}

fn plural(word: &str, count: usize) -> String {
    if count == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

fn tool_header(app: &App, call: &ToolCall, active: bool) -> Vec<Span<'static>> {
    let (glyph, style) = match call.status {
        _ if call.running() => (
            theme::pulse(theme::Pulse::Tool, app.tick).to_string(),
            theme::bold(theme::running_color()),
        ),
        ToolCallStatus::Failed => ("✗".into(), theme::bold(theme::error_color())),
        _ => ("✓".into(), theme::bold(theme::success_color())),
    };
    let mut spans = vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(
            call.title.clone(),
            theme::bold(if active {
                theme::accent_color()
            } else {
                theme::text_color()
            }),
        ),
        Span::styled(kind_label(&call.kind).to_string(), theme::faint()),
        Span::styled(
            format!("  {}", theme::duration(call.elapsed())),
            theme::dim(),
        ),
    ];
    if call.backgrounded && call.running() {
        spans.push(Span::styled("  · background", theme::accent()));
        if active {
            spans.push(Span::styled(" · ^k kill", theme::accent()));
        }
    }
    let running = call.running_children();
    if running > 0 {
        spans.push(Span::styled(
            format!("  · {running} in flight"),
            Style::default().fg(theme::running_color()),
        ));
    } else if !call.children.is_empty() {
        spans.push(Span::styled(
            format!("  · {} calls", call.children.len()),
            theme::faint(),
        ));
    }
    spans
}

/// Tool kinds worth naming; `other` reads as noise next to the tool's title.
fn kind_label(kind: &ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "  read",
        ToolKind::Edit => "  edit",
        ToolKind::Delete => "  delete",
        ToolKind::Move => "  move",
        ToolKind::Search => "  search",
        ToolKind::Execute => "  execute",
        ToolKind::Think => "  think",
        ToolKind::Fetch => "  fetch",
        ToolKind::SwitchMode => "  mode",
        _ => "",
    }
}

fn child_spans(app: &App, child: &Child, indent: &str) -> Vec<Span<'static>> {
    let (glyph, style) = if child.running() {
        (
            theme::pulse(theme::Pulse::Child, app.tick).to_string(),
            Style::default().fg(theme::running_color()),
        )
    } else if child.ok {
        ("✓".into(), Style::default().fg(theme::success_color()))
    } else {
        ("✗".into(), Style::default().fg(theme::error_color()))
    };
    let detail = if child.running() || child.result.is_empty() {
        child.summary.clone()
    } else {
        child.result.clone()
    };
    vec![
        Span::styled(format!("{indent}{glyph} "), style),
        Span::styled(format!("{:<8}", child.tool), theme::dim()),
        Span::styled(
            format!("{:>7}  ", theme::duration(child.elapsed())),
            theme::faint(),
        ),
        Span::styled(detail, theme::dim()),
    ]
}

fn working_line(app: &App) -> Line<'static> {
    let label = match app.phase {
        Phase::Cancelling => "stopping",
        _ if app.compacting => "compacting context",
        _ if app.focus_call().is_some_and(ToolCall::running) => "running tools",
        _ => "thinking",
    };
    Line::from(vec![
        Span::styled(
            format!("{} ", theme::pulse(theme::Pulse::Turn, app.tick)),
            theme::bold(theme::accent_color()),
        ),
        Span::styled(label.to_string(), theme::accent()),
        Span::styled(
            format!("  {}", theme::duration(app.elapsed())),
            theme::dim(),
        ),
        Span::styled("   esc interrupts", theme::faint()),
    ])
}

fn draw_graph(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Panel::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::faint())
        .title(Span::styled(" runtime graph ", theme::accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(call) = app.focus_call() else {
        frame.render_widget(
            Paragraph::new(Span::styled("no tool calls yet", theme::faint())),
            inner,
        );
        return;
    };
    let lines = wrap(&graph_lines(app, call), inner.width.max(1) as usize);
    let offset = lines.len().saturating_sub(inner.height as usize);
    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(offset).collect::<Vec<_>>()),
        inner,
    );
}

fn graph_lines(app: &App, call: &ToolCall) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled(call.title.clone(), theme::bold(theme::text_color())),
        Span::styled(
            format!("  {}", theme::duration(call.elapsed())),
            theme::dim(),
        ),
    ])];
    let done = call.children.len() - call.running_children();
    let failed = call.children.iter().filter(|child| !child.ok).count();
    let program = if call.running() {
        "running"
    } else if call.status == ToolCallStatus::Failed {
        "failed"
    } else {
        "complete"
    };
    lines.push(Line::from(Span::styled(
        format!(
            "{} plan nodes · {done} calls done · {} calls running{} · program {program}",
            call.plan.len(),
            call.running_children(),
            if failed > 0 {
                format!(" · {failed} calls failed")
            } else {
                String::new()
            }
        ),
        theme::faint(),
    )));
    lines.push(Line::default());

    if call.plan.is_empty() {
        if call.children.is_empty() {
            lines.push(Line::from(Span::styled(
                "waiting for the program to dispatch…",
                theme::faint(),
            )));
        }
        for child in call.children.iter().rev().take(20) {
            lines.push(Line::from(child_spans(app, child, "")));
        }
        return lines;
    }

    for (index, node) in call.plan.iter().enumerate() {
        let attached: Vec<&Child> = call
            .children
            .iter()
            .filter(|child| child.node == Some(index))
            .collect();
        let indent = "  ".repeat(node.depth);
        let style = node_style(node.kind, &attached);
        let mut spans = vec![
            Span::styled(format!("{indent}{} ", node.kind.glyph()), style),
            Span::styled(node.label.clone(), style),
        ];
        if attached.len() > 1 {
            spans.push(Span::styled(
                format!("  ×{}", attached.len()),
                theme::faint(),
            ));
        }
        lines.push(Line::from(spans));
        for child in attached.iter().rev().take(4) {
            lines.push(Line::from(child_spans(app, child, &format!("{indent}  "))));
        }
        if attached.len() > 4 {
            lines.push(Line::from(Span::styled(
                format!("{indent}  … {} earlier", attached.len() - 4),
                theme::faint(),
            )));
        }
    }
    lines
}

fn node_style(kind: PlanKind, attached: &[&Child]) -> Style {
    if attached.iter().any(|child| child.running()) {
        return theme::bold(theme::running_color());
    }
    if attached.iter().any(|child| !child.ok) {
        return Style::default().fg(theme::error_color());
    }
    if !attached.is_empty() {
        return Style::default().fg(theme::success_color());
    }
    match kind {
        PlanKind::Call => theme::dim(),
        _ => theme::faint(),
    }
}

fn draw_logs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Panel::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::faint())
        .title(Span::styled(" agent log ", theme::dim()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = inner.height as usize;
    let tail: Vec<Line<'static>> = app
        .logs
        .iter()
        .rev()
        .take(height)
        .rev()
        .map(|line| Line::from(Span::styled(line.clone(), theme::faint())))
        .collect();
    // The newest line sits on the bottom row, so the pane reads like a tail.
    let mut lines = vec![Line::default(); height.saturating_sub(tail.len())];
    lines.extend(tail);
    if app.logs.is_empty() {
        lines.pop();
        lines.push(Line::from(Span::styled(
            "no diagnostics yet",
            theme::faint(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_pending_steers(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let visible = area.height as usize;
    if visible == 0 || app.pending_steers.is_empty() {
        return;
    }

    let mut lines = Vec::with_capacity(visible);
    let skip = if app.pending_steers.len() > visible && visible > 1 {
        let hidden = app.pending_steers.len() - (visible - 1);
        lines.push(Line::from(Span::styled(
            format!("  … {hidden} earlier pending"),
            theme::faint(),
        )));
        hidden
    } else {
        app.pending_steers.len().saturating_sub(visible)
    };
    lines.extend(app.pending_steers.iter().skip(skip).map(|pending| {
        let text = pending
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        Line::from(vec![
            Span::styled("  › ", theme::bold(theme::user_color())),
            Span::styled(text, theme::bold(theme::user_color())),
            Span::styled("  · pending", theme::faint()),
        ])
    }));
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_prompt(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let border = if app.phase == Phase::Working && app.can_steer || app.phase == Phase::Idle {
        Style::default().fg(theme::accent_color())
    } else {
        theme::faint()
    };
    let block = Panel::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [gutter, field] =
        Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Span::styled("›", theme::bold(theme::accent_color()))),
        gutter,
    );

    let height = field.height as usize;
    let (rows, (cursor_row, cursor_column)) = app.editor.wrapped(field.width as usize);
    // Keep the cursor's row on screen when the prompt is taller than the box.
    let first = cursor_row.saturating_sub(height.saturating_sub(1));
    let lines: Vec<Line<'static>> = if app.editor.text().is_empty() {
        vec![Line::from(Span::styled(
            if app.phase == Phase::Working && app.can_steer {
                "steer kit…"
            } else {
                "message kit…"
            },
            theme::faint(),
        ))]
    } else {
        prompt_lines(rows, app.editor.text(), &app.available_commands)
            .into_iter()
            .skip(first)
            .take(height)
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), field);
    frame.set_cursor_position(Position::new(
        field.x
            + u16::try_from(cursor_column)
                .unwrap_or(0)
                .min(field.width.saturating_sub(1)),
        field.y + u16::try_from(cursor_row - first).unwrap_or(0),
    ));
}

fn prompt_lines(
    rows: Vec<String>,
    input: &str,
    available_commands: &[String],
) -> Vec<Line<'static>> {
    let mut highlighted =
        command::known_token(input, available_commands).map_or(0, |range| range.len());
    rows.into_iter()
        .map(|row| {
            let prefix = highlighted.min(row.len());
            highlighted -= prefix;
            if prefix == 0 {
                Line::from(Span::styled(row, theme::text()))
            } else {
                Line::from(vec![
                    Span::styled(row[..prefix].to_string(), theme::accent()),
                    Span::styled(row[prefix..].to_string(), theme::text()),
                ])
            }
        })
        .collect()
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut left = match app.phase {
        Phase::Idle => vec![Span::styled(" ready", theme::dim())],
        Phase::Cancelling => vec![
            Span::styled(
                format!(" {} ", theme::pulse(theme::Pulse::Status, app.tick)),
                theme::bar(),
            ),
            Span::styled("stopping", Style::default().fg(theme::warn_color())),
        ],
        Phase::Blocked => vec![Span::styled(
            " waiting for input",
            Style::default().fg(theme::warn_color()),
        )],
        Phase::Working => vec![
            Span::styled(
                format!(" {} ", theme::pulse(theme::Pulse::Status, app.tick)),
                Style::default()
                    .fg(theme::accent_color())
                    .bg(theme::bar_bg()),
            ),
            Span::styled(
                format!("working {}", theme::duration(app.elapsed())),
                Style::default()
                    .fg(theme::accent_color())
                    .bg(theme::bar_bg()),
            ),
        ],
    };
    if let Some(toast) = app.toast_text() {
        left.push(Span::styled("  ", theme::bar()));
        left.push(Span::styled(
            toast.to_string(),
            Style::default().fg(theme::warn_color()).bg(theme::bar_bg()),
        ));
    }
    let hints = "⏎ send   ⇧⏎ newline   ^g graph   ^l log   ^c quit ";
    let used: usize = left.iter().map(|span| span.content.chars().count()).sum();
    let gap = (area.width as usize)
        .saturating_sub(used + hints.chars().count())
        .max(1);
    left.push(Span::styled(" ".repeat(gap), theme::bar()));
    left.push(Span::styled(hints, theme::bar()));
    frame.render_widget(Paragraph::new(Line::from(left)).style(theme::bar()), area);
}

fn percent(used: u64, size: u64) -> String {
    if size == 0 {
        return "?%".to_owned();
    }
    format!("{:.1}%", used as f64 * 100.0 / size as f64)
}

/// Token counts read better rounded: `128k` beats `128000`.
fn compact(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{}k", value / 1_000)
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agent_client_protocol::schema::v2::ToolKind;
    use base64::Engine as _;
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::picker::Picker;

    use super::{
        MAX_PROMPT_ROWS, ModelDialogRow, draw, graph_lines, model_dialog_rows,
        model_dialog_viewport, prompt_lines, refresh_transcript_cache,
        refresh_transcript_cache_with_images, user_block_rows, user_line,
    };
    use crate::{
        events::RuntimeEvent,
        tui::app::{
            Action, App, Block, EffortChoice, EffortDialog, ModelDialog, Update, UserImage,
            UserMessage,
        },
    };

    fn model_choice(provider: &str, model: &str) -> crate::tui::app::ModelChoice {
        crate::tui::app::ModelChoice {
            id: format!("{provider}:{model}"),
            provider: provider.into(),
            model: model.into(),
        }
    }

    #[test]
    fn model_viewport_keeps_a_selection_beyond_the_first_page_visible() {
        let choices = (0..8)
            .map(|index| model_choice("provider", &format!("model-{index}")))
            .collect::<Vec<_>>();
        let choices = choices.iter().collect::<Vec<_>>();
        let rows = model_dialog_rows(&choices, true);
        let viewport = model_dialog_viewport(&rows, 6, 4);

        assert!(
            rows[viewport]
                .iter()
                .any(|row| matches!(row, ModelDialogRow::Choice { index: 6, .. }))
        );
    }

    #[test]
    fn model_viewport_counts_provider_headers_at_group_boundaries() {
        let choices = [
            model_choice("alpha", "one"),
            model_choice("alpha", "two"),
            model_choice("beta", "three"),
            model_choice("beta", "four"),
        ];
        let choices = choices.iter().collect::<Vec<_>>();
        let rows = model_dialog_rows(&choices, true);
        let viewport = model_dialog_viewport(&rows, 2, 2);

        assert_eq!(
            &rows[viewport],
            &[
                ModelDialogRow::Provider("beta"),
                ModelDialogRow::Choice {
                    index: 2,
                    label: "three".into(),
                },
            ]
        );
    }

    /// `sample()` with every clock stopped.
    ///
    /// The contrast comparison renders the transcript twice, once per palette,
    /// and reads the two frames cell against cell. A duration still counting
    /// up between the two renders would widen a card's label in one of them
    /// and shift every cell after it.
    fn frozen() -> App {
        use std::time::Duration;

        let mut app = sample();
        app.turn_started = None;
        app.tick = 0;
        app.toast = None;
        for block in &mut app.blocks {
            let Block::Tool(call) = block else { continue };
            call.status = agent_client_protocol::schema::v2::ToolCallStatus::Completed;
            call.finished = Some(call.started + Duration::from_millis(120));
            for (index, child) in call.children.iter_mut().enumerate() {
                child.millis = Some(40 * (index as u64 + 1));
            }
        }
        app
    }

    /// Every colour the client picks on a light terminal must read at least
    /// as well as the colour it picks for the same cell on a dark one. A dark
    /// palette on a light terminal is what makes the transcript vanish: its
    /// colours all sit a few shades from white.
    #[test]
    fn a_light_terminal_reads_as_well_as_a_dark_one() {
        use ratatui::style::Color;

        use crate::tui::theme::{self, Appearance, contrast};

        // What the terminal itself paints where the client sets no colour.
        fn ink(appearance: Appearance, paper: Color) -> Vec<(String, f64)> {
            theme::set(appearance);
            let mut app = frozen();
            let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("terminal");
            let mut images = crate::tui::image::ImageRuntime::disabled();
            terminal
                .draw(|frame| draw(frame, &mut app, &mut images))
                .expect("draw succeeds");
            let buffer = terminal.backend().buffer().clone();
            theme::set(Appearance::Dark);
            (0..buffer.area.height)
                .flat_map(|row| (0..buffer.area.width).map(move |column| (column, row)))
                .map(|cell| {
                    let cell = &buffer[cell];
                    let background = match cell.bg {
                        Color::Reset => paper,
                        background => background,
                    };
                    let contrast = match cell.fg {
                        _ if cell.symbol().trim().is_empty() => f64::INFINITY,
                        Color::Reset => f64::INFINITY,
                        foreground => contrast(foreground, background),
                    };
                    (cell.symbol().to_string(), contrast)
                })
                .collect()
        }

        let light = ink(Appearance::Light, Color::Rgb(255, 255, 255));
        let dark = ink(Appearance::Dark, Color::Rgb(0, 0, 0));
        assert_eq!(light.len(), dark.len());
        assert!(
            light.iter().any(|(_, contrast)| contrast.is_finite()),
            "the frame draws no colour at all"
        );
        // Deliberately quiet chrome — faint text on the status bar — reads low
        // in either palette, so each cell is held to whichever is weaker: the
        // legibility floor, or what the dark palette already settled for.
        for ((symbol, light), (drawn, dark)) in light.into_iter().zip(dark) {
            // Both frames are drawn from the same stopped clock, so a cell
            // holding different text means the comparison has slipped.
            assert_eq!(symbol, drawn, "the two frames drew different text");
            let wanted = dark.min(4.5);
            assert!(
                light + 0.01 >= wanted,
                "{symbol:?} reads at {light:.2}:1 on a light terminal, short of {wanted:.2}:1"
            );
        }
    }

    #[test]
    fn effort_dialog_renders_selection_and_default_toggle() {
        let mut app = sample();
        app.effort_choices = vec![
            EffortChoice {
                id: "default".into(),
                name: "Default".into(),
            },
            EffortChoice {
                id: "high".into(),
                name: "High".into(),
            },
        ];
        app.effort_dialog = Some(EffortDialog {
            selected: 1,
            save_defaults: true,
        });

        let frame = render(&mut app, 60, 12);
        assert!(frame.contains("reasoning effort"), "{frame}");
        assert!(frame.contains("› High"), "{frame}");
        assert!(frame.contains("defaults [x]"), "{frame}");
    }

    #[test]
    fn model_dialog_renders_the_selected_item_on_a_small_terminal() {
        let mut app = sample();
        app.model_choices = (0..10)
            .map(|index| model_choice("provider", &format!("model-{index}")))
            .collect();
        app.model_dialog = Some(ModelDialog {
            query: String::new(),
            selected: 9,
            save_defaults: false,
        });

        let frame = render(&mut app, 24, 8);
        assert!(frame.contains("› model-9"), "{frame}");

        let tiny = render(&mut app, 16, 3);
        assert!(tiny.contains("› model-9"), "{tiny}");
    }

    const SCRIPT: &str = "files = shell({ command: \"ls src\" })\n\
        checked = for file in files.lines {\n\
            return shell({ command: \"cargo check\" })\n\
        }\n\
        return checked";

    fn sample() -> App {
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        app.push_user("check every source file".into());
        app.apply(Update::test_text(
            "Reading the tree first.\n\n- one\n- two\n\n```sh\ncargo check\n```".into(),
        ));
        app.apply(Update::ToolStarted {
            id: "call-1".into(),
            title: "compose".into(),
            kind: ToolKind::Other,
            script: Some(SCRIPT.into()),
            backgrounded: false,
        });
        app.apply(Update::Runtime(RuntimeEvent::ChildStarted {
            call: "call-1:compose:one".into(),
            tool: "shell".into(),
            summary: "ls src".into(),
            at: 0,
        }));
        app.apply(Update::Runtime(RuntimeEvent::ChildFinished {
            call: "call-1:compose:one".into(),
            tool: "shell".into(),
            ok: true,
            summary: "main.rs".into(),
            millis: 120,
        }));
        app.apply(Update::Runtime(RuntimeEvent::ChildStarted {
            call: "call-1:compose:two".into(),
            tool: "shell".into(),
            summary: "cargo check".into(),
            at: 0,
        }));
        app
    }

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        let mut images = crate::tui::image::ImageRuntime::disabled();
        terminal
            .draw(|frame| draw(frame, app, &mut images))
            .expect("draw succeeds");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn pending_steers_render_above_input_until_delivery() {
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        app.can_steer = true;
        app.apply(Update::State {
            active: true,
            steerable: true,
            cancelled: false,
        });
        app.apply(Update::SteerAccepted {
            id: "first".into(),
            text: "first pending".into(),
        });
        app.apply(Update::SteerAccepted {
            id: "second".into(),
            text: "second pending".into(),
        });

        let frame = render(&mut app, 80, 18);
        let first = frame.find("first pending").expect("first steer");
        let second = frame.find("second pending").expect("second steer");
        let input = frame.find("steer kit…").expect("steering input");
        assert!(first < second && second < input, "{frame}");
        assert_eq!(frame.matches("· pending").count(), 2, "{frame}");
        assert!(
            !app.blocks
                .iter()
                .any(|block| matches!(block, Block::User(_)))
        );

        app.apply(Update::UserMessage {
            id: "first".into(),
            text: "first pending".into(),
            images: Vec::new(),
            append: false,
        });
        let frame = render(&mut app, 80, 18);
        assert_eq!(frame.matches("· pending").count(), 1, "{frame}");
        assert_eq!(app.pending_steers.len(), 1);
        assert!(
            matches!(app.blocks.last(), Some(Block::User(message)) if message.text == "first pending")
        );
    }

    #[test]
    fn completed_turn_duration_is_rendered() {
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        app.blocks.push(Block::TurnDuration(788_645_000));

        let frame = render(&mut app, 80, 12);

        assert!(frame.contains("· took 1w2d 3h04m05s"), "{frame}");
    }

    #[test]
    fn labels_compaction_while_it_is_running() {
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        app.push_user("continue".into());
        app.apply(Update::Runtime(RuntimeEvent::CompactionStarted {
            reason: "TokenThreshold".into(),
            at: 0,
        }));
        let frame = render(&mut app, 80, 20);
        assert!(frame.contains("compacting context"));
    }

    #[test]
    fn draws_the_transcript_beside_a_live_runtime_graph() {
        let frame = render(&mut sample(), 120, 30);
        println!("{frame}");
        assert!(frame.contains("kit"));
        assert!(frame.contains("› check every source file"));
        assert!(frame.contains("runtime graph"));
        assert!(frame.contains("for file in files.lines"));
        assert!(frame.contains("working"));
    }

    #[test]
    fn stacks_the_graph_below_the_transcript_when_narrow() {
        let frame = render(&mut sample(), 70, 30);
        println!("{frame}");
        assert!(frame.contains("runtime graph"));
    }

    #[test]
    fn distinguishes_finished_child_calls_from_a_running_program() {
        let mut app = sample();
        app.apply(Update::Runtime(RuntimeEvent::ChildFinished {
            call: "call-1:compose:two".into(),
            tool: "shell".into(),
            ok: true,
            summary: "checked".into(),
            millis: 240,
        }));

        let summary = graph_lines(&app, app.focus_call().unwrap())[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(
            summary,
            "4 plan nodes · 2 calls done · 0 calls running · program running"
        );

        app.apply(Update::ToolUpdated {
            id: "call-1".into(),
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
            script: None,
            output: Vec::new(),
            backgrounded: false,
        });
        let summary = graph_lines(&app, app.focus_call().unwrap())[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(summary.ends_with("0 calls running · program complete"));
    }

    #[test]
    fn only_the_graphs_active_call_shows_the_kill_hint() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        for id in ["first", "second"] {
            app.apply(Update::ToolStarted {
                id: id.into(),
                title: format!("compose {id}"),
                kind: ToolKind::Other,
                script: Some("return 1".into()),
                backgrounded: true,
            });
        }
        fn headers(app: &App) -> Vec<String> {
            app.blocks
                .iter()
                .filter_map(|block| match block {
                    Block::Tool(call) => Some(
                        super::tool_header(
                            app,
                            call,
                            app.focus_call()
                                .is_some_and(|focused| focused.id == call.id),
                        )
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>(),
                    ),
                    _ => None,
                })
                .collect()
        }

        let initial = headers(&app);
        assert!(!initial[0].contains("^k kill"));
        assert!(initial[1].contains("^k kill"));

        app.focused_call_id = Some("first".into());
        app.graph_pinned = Some(false);
        let selected = headers(&app);
        assert!(selected[0].contains("^k kill"));
        assert!(!selected[1].contains("^k kill"));
    }

    #[test]
    fn scrolls_back_through_a_long_transcript_with_the_log_pane_open() {
        let mut app = sample();
        app.apply(Update::ToolUpdated {
            id: "call-1".into(),
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Failed),
            script: None,
            output: vec!["exit code 1".into()],
            backgrounded: false,
        });
        app.apply(Update::State {
            active: false,
            steerable: false,
            cancelled: false,
        });
        app.apply(Update::Log("warn: retrying provider request".into()));
        app.show_logs = true;
        for index in 0..12 {
            app.push_user(format!("follow-up number {index}"));
            app.apply(Update::test_text(format!("answer number {index}")));
        }
        app.apply(Update::State {
            active: false,
            steerable: false,
            cancelled: false,
        });
        let _ = render(&mut app, 100, 24);
        app.scroll_by(-6);
        let frame = render(&mut app, 100, 24);
        println!("{frame}");
        assert!(!app.follow);
        assert!(frame.contains("agent log"));
        assert!(frame.contains("retrying provider request"));
        assert!(!frame.contains("follow-up number 11"));
    }

    #[test]
    fn grows_and_wraps_the_prompt_instead_of_running_past_the_edge() {
        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        for word in "explain how the compose tool dispatches hidden children and \
                     everything internal to it. give me a plan"
            .split(' ')
        {
            for character in word.chars() {
                app.editor.insert_char(character);
            }
            app.editor.insert_char(' ');
        }
        let frame = render(&mut app, 60, 20);
        println!("{frame}");
        let rows: Vec<&str> = frame.lines().collect();
        let prompt: Vec<&&str> = rows.iter().filter(|row| row.starts_with('│')).collect();
        assert!(prompt.len() >= 2, "prompt should have grown: {prompt:?}");
        for row in prompt {
            assert!(row.ends_with('│'), "prompt row overflowed: {row:?}");
        }

        // A prompt taller than the cap scrolls inside the box instead of
        // pushing the transcript off the screen.
        for _ in 0..40 {
            app.editor.insert_str("more text to type ");
        }
        let frame = render(&mut app, 60, 20);
        let prompt = frame.lines().filter(|row| row.starts_with('│')).count();
        assert_eq!(prompt, MAX_PROMPT_ROWS);
        assert!(frame.contains("message kit") || frame.contains("more text"));
    }

    #[test]
    fn clicking_a_code_block_copies_exact_content() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mut app = sample();
        let frame = render(&mut app, 100, 24);
        let row = frame
            .lines()
            .position(|line| line.contains("  cargo check"))
            .expect("code row is on screen");

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: u16::try_from(row).unwrap(),
            modifiers: KeyModifiers::NONE,
        });
        let action = app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 5,
            row: u16::try_from(row).unwrap(),
            modifiers: KeyModifiers::NONE,
        });

        let Action::Copy(text) = action else {
            panic!("expected code copy action");
        };
        assert_eq!(text, "cargo check");
    }

    #[test]
    fn folds_raw_tool_output_until_the_card_is_clicked() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mut app = sample();
        app.apply(Update::ToolUpdated {
            id: "call-1".into(),
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
            script: None,
            output: (0..40)
                .map(|index| format!("output line {index}"))
                .collect(),
            backgrounded: false,
        });
        let frame = render(&mut app, 100, 24);
        println!("{frame}");
        assert!(frame.contains("40 lines of output"));
        assert!(!frame.contains("output line 39"));

        let row = frame
            .lines()
            .position(|line| line.contains("lines of output"))
            .expect("fold row is on screen");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.handle_mouse(MouseEvent {
                kind,
                column: 6,
                row: u16::try_from(row).unwrap(),
                modifiers: KeyModifiers::NONE,
            });
        }
        let frame = render(&mut app, 100, 24);
        println!("{frame}");
        assert!(frame.contains("output line 39"));
    }

    #[test]
    fn selection_copy_rejoins_wrapped_lines_and_keeps_paragraphs() {
        use crate::tui::app::Selection;

        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        let source = "alpha beta gamma delta epsilon zeta\n\nsecond paragraph";
        app.blocks.push(Block::Agent(source.into()));
        let frame = render(&mut app, 30, 24);
        assert!(
            frame.lines().any(|line| line.contains("alpha beta")),
            "text should be on screen: {frame}"
        );
        assert!(
            !frame.lines().any(|line| line.contains("delta epsilon")),
            "paragraph should have wrapped: {frame}"
        );

        app.selection = Some(Selection {
            anchor: (0, 0),
            head: (
                app.total_lines.saturating_sub(1),
                app.transcript_width.saturating_sub(1),
            ),
        });
        assert_eq!(app.selection_text().as_deref(), Some(source));
    }

    #[test]
    fn selection_copy_does_not_add_spaces_to_hard_wrapped_tokens() {
        use crate::tui::app::Selection;

        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        let source = "abcdefghijklmnopqrstuvwxyz0123456789";
        app.blocks.push(Block::Agent(source.into()));
        let _ = render(&mut app, 20, 24);
        assert!(app.total_lines > 1);

        app.selection = Some(Selection {
            anchor: (0, 0),
            head: (
                app.total_lines.saturating_sub(1),
                app.transcript_width.saturating_sub(1),
            ),
        });
        assert_eq!(app.selection_text().as_deref(), Some(source));
    }

    #[test]
    fn transcript_reflow_clears_display_coordinate_selection() {
        use crate::tui::app::Selection;

        let mut app = sample();
        let _ = render(&mut app, 40, 24);
        app.selection = Some(Selection {
            anchor: (0, 0),
            head: (0, 2),
        });

        let _ = render(&mut app, 100, 24);
        assert!(app.selection.is_none());
    }

    #[test]
    fn selection_copy_strips_the_code_display_indent() {
        use crate::tui::app::Selection;

        let mut app = sample();
        let frame = render(&mut app, 100, 24);
        let row = frame
            .lines()
            .position(|line| line.contains("  cargo check"))
            .expect("code row is on screen");
        let line = app.scroll + (row - app.transcript_top);
        app.selection = Some(Selection {
            anchor: (line, 0),
            head: (line, app.transcript_width.saturating_sub(1)),
        });
        assert_eq!(app.selection_text().as_deref(), Some("cargo check"));
    }

    #[test]
    fn dragging_selects_and_ctrl_y_copies_instead_of_clicking() {
        use crossterm::event::{
            KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };

        let mut app = sample();
        let frame = render(&mut app, 100, 24);
        let row = frame
            .lines()
            .position(|line| line.contains("• one"))
            .expect("bullet line is on screen");
        let row = u16::try_from(row).unwrap();
        let left = u16::try_from(app.transcript_left).unwrap();
        let mouse = |kind, column| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), left + 2));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), left + 4));
        let action = app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), left + 4));
        assert!(
            matches!(action, Action::None),
            "releasing a drag must not click"
        );
        assert!(app.selection.is_some());

        let action = app.handle_key(KeyEvent {
            code: KeyCode::Char('y'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        let Action::Copy(text) = action else {
            panic!("expected selection copy");
        };
        assert_eq!(text, "one");

        // The next press clears the selection.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), left));
        assert!(app.selection.is_none());
    }

    #[test]
    fn keeps_duplicate_and_wrapped_link_targets_exact() {
        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        let first = "https://first.example/a/very/long/path";
        let second = "https://second.example/a/very/long/path";
        app.blocks.push(Block::Agent(format!("[same]({first})")));
        app.blocks.push(Block::Agent(format!("[same]({second})")));

        let _ = render(&mut app, 28, 24);
        let all_urls: Vec<_> = app
            .row_links
            .iter()
            .flatten()
            .map(|hit| hit.url.as_str())
            .collect();
        assert!(all_urls.len() > 4, "URLs should have wrapped: {all_urls:?}");
        let mut targets = all_urls.clone();
        targets.dedup();
        assert_eq!(targets, [first, second]);
    }

    #[test]
    fn shows_reported_context_usage_in_the_header() {
        let mut app = sample();
        app.apply(Update::Usage {
            used: 1_360,
            size: 272_000,
        });

        let frame = render(&mut app, 120, 24);

        assert!(frame.contains("openai-subscription / gpt-5.4"));
        assert!(frame.contains("0.5% 1k/272k"));
        assert!(!frame.contains("ctx "));
    }

    #[test]
    fn highlights_only_the_known_new_token() {
        let known = prompt_lines(vec!["/new prompt".into()], "/new prompt", &[]);
        assert_eq!(known[0].spans[0].content, "/new");
        assert_eq!(known[0].spans[0].style, crate::tui::theme::accent());
        assert_eq!(known[0].spans[1].content, " prompt");

        let unknown = prompt_lines(vec!["/newer prompt".into()], "/newer prompt", &[]);
        assert_eq!(unknown[0].spans.len(), 1);
        assert_eq!(unknown[0].spans[0].style, crate::tui::theme::text());

        let advertised = vec!["compact".to_string()];
        let dynamic = prompt_lines(
            vec!["/compact prompt".into()],
            "/compact prompt",
            &advertised,
        );
        assert_eq!(dynamic[0].spans[0].content, "/compact");
        assert_eq!(dynamic[0].spans[0].style, crate::tui::theme::accent());
    }

    #[test]
    fn keeps_new_highlighted_when_the_real_editor_wraps_the_token() {
        let mut editor = crate::tui::editor::Editor::default();
        editor.insert_str("/new prompt");
        let (rows, _) = editor.wrapped(2);
        let lines = prompt_lines(rows, editor.text(), &[]);
        let highlighted = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.style == crate::tui::theme::accent())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(highlighted, "/new");
        assert_eq!(lines[0].spans[0].content, "/n");
        assert_eq!(lines[1].spans[0].content, "ew");
    }

    #[test]
    fn dynamic_cache_entries_refresh_until_the_block_finishes() {
        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        app.apply(Update::test_thought("still thinking".into()));

        refresh_transcript_cache(&mut app, 40);
        let first_rows = app.transcript_cache[0].as_ref().unwrap().rows.as_ptr();
        refresh_transcript_cache(&mut app, 40);
        assert_ne!(
            app.transcript_cache[0].as_ref().unwrap().rows.as_ptr(),
            first_rows
        );

        app.apply(Update::test_text("done".into()));
        refresh_transcript_cache(&mut app, 40);
        assert!(!app.transcript_dynamic.contains(&0));
        let stable_rows = app.transcript_cache[0].as_ref().unwrap().rows.as_ptr();
        refresh_transcript_cache(&mut app, 40);
        assert_eq!(
            app.transcript_cache[0].as_ref().unwrap().rows.as_ptr(),
            stable_rows
        );
    }

    #[test]
    fn materializes_click_metadata_only_for_visible_rows() {
        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        for index in 0..30 {
            app.blocks
                .push(Block::User(format!("old row {index}").into()));
        }
        app.blocks.push(Block::Agent(
            "[visible link](https://example.com/target)".into(),
        ));

        super::MATERIALIZED_TRANSCRIPT_ROWS.with(|count| count.set(0));
        super::VISITED_TRANSCRIPT_BLOCKS.with(|count| count.set(0));
        let _ = render(&mut app, 50, 10);

        super::MATERIALIZED_TRANSCRIPT_ROWS.with(|count| assert_eq!(count.get(), app.viewport));
        super::VISITED_TRANSCRIPT_BLOCKS.with(|count| {
            assert!(count.get() <= app.viewport);
            assert!(count.get() < app.blocks.len());
        });
        assert_eq!(app.row_links.len(), app.viewport);
        assert_eq!(app.row_calls.len(), app.viewport);
        assert_eq!(app.row_code.len(), app.viewport);
        assert!(
            app.row_links
                .iter()
                .flatten()
                .any(|hit| hit.url == "https://example.com/target")
        );
    }

    #[test]
    fn tail_mutation_does_not_inspect_or_rebuild_unchanged_history() {
        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        for index in 0..99 {
            app.blocks.push(Block::Agent(format!("history {index}")));
        }
        app.apply(Update::test_text("history 99".into()));
        refresh_transcript_cache(&mut app, 12);
        let history_rows = app.transcript_cache[0].as_ref().unwrap().rows.as_ptr();
        let history_revision = app.transcript_cache[0].as_ref().unwrap().revision;
        let tail_rows = app.transcript_cache[99].as_ref().unwrap().rows.as_ptr();

        super::REFRESHED_TRANSCRIPT_BLOCKS.with(|count| count.set(0));
        app.apply(Update::test_text(" changed".into()));
        refresh_transcript_cache(&mut app, 12);

        super::REFRESHED_TRANSCRIPT_BLOCKS.with(|count| assert_eq!(count.get(), 1));
        let history = app.transcript_cache[0].as_ref().unwrap();
        assert_eq!(history.rows.as_ptr(), history_rows);
        assert_eq!(history.revision, history_revision);
        assert_ne!(
            app.transcript_cache[99].as_ref().unwrap().rows.as_ptr(),
            tail_rows
        );
    }

    #[test]
    fn user_attachment_links_remain_clickable() {
        let lines = [user_line(
            "inspect [Image #1](file:///tmp/image_(1).png)",
            true,
        )];
        let links = lines[0]
            .spans
            .iter()
            .filter_map(|span| span.url.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(links, ["file:///tmp/image_(1).png"]);
        let displayed = lines[0]
            .spans
            .iter()
            .map(|span| span.span.content.as_ref())
            .collect::<String>();
        assert_eq!(displayed, "› inspect Image #1");
    }

    #[test]
    fn image_rows_preserve_text_image_text_display_order() {
        let image = UserImage::new("AQID".into(), "image/png".into(), 1).unwrap();
        let message = UserMessage {
            text: "before\n[Image #1]\nafter".into(),
            images: vec![image],
        };

        let (rows, placements) = user_block_rows(&message, 40, true);

        assert_eq!(placements.len(), 1);
        let after = &rows[placements[0].row + usize::from(super::RESERVED_ROWS)].0;
        assert!(
            after
                .spans
                .iter()
                .any(|span| span.content.contains("after"))
        );
    }

    #[test]
    fn image_rows_are_fixed_and_decoding_is_lazy() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(400, 200)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let source = UserImage::new(
            base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
            "image/png".into(),
            0,
        )
        .unwrap();
        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        app.blocks.push(Block::User(UserMessage {
            text: "[Image #1](file:///tmp/image.png)".into(),
            images: vec![source],
        }));
        let mut images = crate::tui::image::ImageRuntime::with_picker(Picker::halfblocks());

        refresh_transcript_cache_with_images(&mut app, &mut images, 12);
        assert_eq!(images.cached_entries(), 0, "layout must not decode images");
        let narrow = app.transcript_cache[0].as_ref().unwrap();
        assert_eq!(narrow.images.len(), 1);
        assert!(narrow.rows.len() > 1);
        let narrow_rows = narrow.rows.len();
        assert_eq!(app.transcript_prefixes.last().copied(), Some(narrow_rows));

        refresh_transcript_cache_with_images(&mut app, &mut images, 40);
        assert_eq!(images.cached_entries(), 0, "width changes stay lazy");
        let wide = app.transcript_cache[0].as_ref().unwrap();
        assert_eq!(wide.images.len(), 1);
        assert_eq!(wide.rows.len(), narrow_rows);

        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .unwrap();
        assert_eq!(images.cached_entries(), 1, "visible image is prepared");
        let reserved_rows = app.transcript_cache[0].as_ref().unwrap().rows.len();

        images.clear();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .unwrap();
        assert_eq!(
            images.cached_entries(),
            1,
            "an evicted visible image is prepared again before rendering"
        );
        assert_eq!(
            app.transcript_cache[0].as_ref().unwrap().rows.len(),
            reserved_rows,
            "cache eviction cannot remove reserved transcript rows"
        );
    }

    #[test]
    fn transcript_width_change_rebuilds_cached_rows() {
        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        app.blocks
            .push(Block::Agent("alpha beta gamma delta epsilon".into()));
        refresh_transcript_cache(&mut app, 12);
        let narrow_rows = app.transcript_cache[0].as_ref().unwrap().rows.len();
        refresh_transcript_cache(&mut app, 40);
        assert_eq!(app.transcript_cache_width, 40);
        assert!(app.transcript_cache[0].as_ref().unwrap().rows.len() < narrow_rows);
    }

    #[test]
    fn shows_the_welcome_screen_before_the_first_prompt() {
        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        let frame = render(&mut app, 90, 24);
        println!("{frame}");
        assert!(frame.contains("send"));
        assert!(frame.contains("message kit"));
    }
}
