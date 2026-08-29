//! Frame drawing: header, transcript, prompt, and status.

use std::ops::Range;

use agent_client_protocol::schema::v2::{ToolCallStatus, ToolKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
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

use crate::events::{GenerationOutcome, SubagentStatus};

use super::{
    app::{
        AgentTreeRow, App, Block, CachedTranscriptBlock, CachedTranscriptImage,
        CachedTranscriptRow, Child, CodeHit, Phase, ToolCall, UserMessage,
    },
    command,
    image::{ImageRuntime, RESERVED_ROWS},
    markdown,
    plan::PlanKind,
    theme,
    wrap::{LinkedLine, LinkedSpan, wrap_linked_tagged},
};

const SIDE_BY_SIDE_WIDTH: u16 = 108;
const AGENTS_WIDTH: u16 = 46;
const MAX_PROMPT_ROWS: usize = 10;
const MAX_PENDING_STEER_ROWS: usize = 3;
const START_MAX_WIDTH: u16 = 96;
const START_LOGO_ROWS: u16 = 3;
const START_LOGO_GAP: u16 = 2;
const START_PROMPT_CHROME_ROWS: u16 = 4;
const START_MIN_PROMPT_ROWS: u16 = START_PROMPT_CHROME_ROWS + 1;
const HEADER_SEPARATOR: &str = "  ·  ";
/// Rows of raw tool output rendered when a card is opened.
const MAX_OUTPUT_ROWS: usize = 400;

type TranscriptTag = (Option<String>, Option<CodeHit>, Option<usize>);
type TaggedTranscriptLine = (LinkedLine, TranscriptTag);

pub fn draw(frame: &mut Frame<'_>, app: &mut App, images: &mut ImageRuntime) {
    // Two border columns plus the `›` gutter; the prompt grows as the wrapped
    // text needs more rows, up to the cap.
    let start_width = frame
        .area()
        .width
        .saturating_sub(4)
        .clamp(1, START_MAX_WIDTH);
    let start_prompt_width = start_width.saturating_sub(4).max(1) as usize;
    let available_start_prompt_rows = frame
        .area()
        .height
        .saturating_sub(START_LOGO_ROWS + START_LOGO_GAP + 1);
    let start_prompt_rows = (app
        .editor
        .display_rows(start_prompt_width)
        .clamp(1, MAX_PROMPT_ROWS) as u16
        + START_PROMPT_CHROME_ROWS)
        .min(available_start_prompt_rows);
    let show_start = frame.area().width >= 20
        && available_start_prompt_rows >= START_MIN_PROMPT_ROWS
        && app.blocks.is_empty()
        && app.pending_steers.is_empty()
        && !app.show_logs;

    if show_start {
        app.prompt_width = start_prompt_width;
        draw_start(frame, app, start_width, start_prompt_rows);
    } else {
        let prompt_width = frame.area().width.saturating_sub(4).max(1) as usize;
        app.prompt_width = prompt_width;
        let prompt_rows = app
            .editor
            .display_rows(prompt_width)
            .clamp(1, MAX_PROMPT_ROWS) as u16
            + 2;
        let logs_rows = if app.show_logs { 9 } else { 0 };
        let pending_rows = app.pending_steers.len().min(MAX_PENDING_STEER_ROWS) as u16;
        let minimum_rows = 1 + 3 + logs_rows + pending_rows + prompt_rows + 1;
        let rainbow_fits = frame.area().height >= minimum_rows.saturating_add(1);
        let header_rows = 1 + u16::from(!app.blocks.is_empty() && rainbow_fits);
        let [header, body, logs, pending, prompt, status] = Layout::vertical([
            Constraint::Length(header_rows),
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
    }
    if app.session_dialog.is_some() {
        draw_session_dialog(frame, app);
    } else if app.model_dialog.is_some() {
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

fn draw_session_dialog(frame: &mut Frame<'_>, app: &App) {
    let outer = frame.area();
    let width = if outer.width > 20 {
        outer.width.saturating_sub(4).min(88)
    } else {
        outer.width
    };
    let height = outer
        .height
        .min((app.session_choices.len() as u16).saturating_add(3).min(22));
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let selected = app
        .session_dialog
        .as_ref()
        .map_or(0, |dialog| dialog.selected);
    let panel = Panel::bordered().title(" sessions ");
    let inner = panel.inner(area);
    let footer_rows = u16::from(inner.height > 1);
    let [list, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(footer_rows)]).areas(inner);
    let visible = list.height as usize;
    let start = selected
        .saturating_sub(visible / 2)
        .min(app.session_choices.len().saturating_sub(visible));
    let lines = app.session_choices[start..]
        .iter()
        .take(visible)
        .enumerate()
        .map(|(offset, entry)| {
            let is_selected = start + offset == selected;
            let label = entry
                .title
                .as_deref()
                .or(entry.preview.as_deref())
                .unwrap_or("untitled");
            let updated = entry.updated_at_rfc3339();
            Line::from(Span::styled(
                format!(
                    "{}{} · {} · {}",
                    if is_selected { "› " } else { "  " },
                    &updated[..10],
                    entry.id,
                    label
                ),
                if is_selected {
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
        Paragraph::new(Span::styled("enter resume · esc close", theme::dim())),
        footer,
    );
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
    let version = format!("v{} ", env!("CARGO_PKG_VERSION"));
    let mut spans = vec![
        Span::styled(" kit ", theme::bold(theme::accent_color())),
        Span::styled(version.clone(), theme::faint()),
        Span::styled("▏ ", theme::faint()),
    ];
    // Keep complete high-value fields and drop low-priority fields rather than
    // clipping a session ID or context-window value into something misleading.
    let mut fields = vec![
        (2, root, theme::text()),
        (3, format!("{} / {}", app.provider, app.model), theme::dim()),
        (1, format!("effort {}", app.reasoning_effort), theme::dim()),
    ];
    if let Some(usage) = app.usage {
        fields.push((
            4,
            format!(
                "{} {}/{}",
                percent(usage.used, usage.size),
                compact(usage.used),
                compact(usage.size)
            ),
            theme::dim(),
        ));
    }
    fields.push((
        5,
        format!(
            "session {}",
            app.session_id.as_deref().unwrap_or("starting")
        ),
        theme::dim(),
    ));
    fields.push((0, format!("a2a {}", app.a2a), theme::dim()));

    let prefix_width = Line::from(spans.clone()).width();
    let header_width = |fields: &[(u8, String, Style)]| {
        prefix_width
            + fields
                .iter()
                .map(|(_, text, _)| UnicodeWidthStr::width(text.as_str()))
                .sum::<usize>()
            + fields.len().saturating_sub(1) * UnicodeWidthStr::width(HEADER_SEPARATOR)
    };
    while header_width(&fields) > area.width as usize {
        let Some(index) = fields
            .iter()
            .enumerate()
            .min_by_key(|(_, (priority, _, _))| *priority)
            .map(|(index, _)| index)
        else {
            break;
        };
        fields.remove(index);
    }
    for (index, (_, text, style)) in fields.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(HEADER_SEPARATOR, theme::faint()));
        }
        spans.push(Span::styled(text, style));
    }
    let metadata = Rect { height: 1, ..area };
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(theme::bar()),
        metadata,
    );
    if area.height > 1 {
        let rainbow = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };
        frame.render_widget(
            Paragraph::new(rainbow_line(area.width as usize, "━")),
            rainbow,
        );
    }
}

fn draw_start(frame: &mut Frame<'_>, app: &App, width: u16, prompt_rows: u16) {
    let area = frame.area();
    let height = START_LOGO_ROWS + START_LOGO_GAP + prompt_rows + 1;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let mut y = area.y + area.height.saturating_sub(height) / 2;

    let logo = Rect::new(x, y, width, START_LOGO_ROWS);
    frame.render_widget(welcome_logo(), logo);
    y += START_LOGO_ROWS + START_LOGO_GAP;

    let prompt = Rect::new(x, y, width, prompt_rows);
    draw_start_prompt(frame, app, prompt);
    let status = Rect::new(x, y + prompt_rows, width, 1);
    draw_status(frame, app, status);
}

fn body_layout(area: Rect, show_agents: bool) -> (Rect, Option<Rect>) {
    if !show_agents {
        return (area, None);
    }
    let [transcript, agents] = if area.width >= SIDE_BY_SIDE_WIDTH {
        Layout::horizontal([Constraint::Min(40), Constraint::Length(AGENTS_WIDTH)]).areas(area)
    } else {
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area)
    };
    (transcript, Some(agents))
}

fn draw_body(frame: &mut Frame<'_>, app: &mut App, images: &mut ImageRuntime, area: Rect) {
    let (transcript, agents) = body_layout(area, app.show_agents());
    draw_transcript(frame, app, images, transcript);
    if let Some(agents) = agents {
        draw_agents(frame, app, agents);
    } else {
        app.set_agents_viewport(Rect::default(), 0);
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
        frame.render_widget(welcome_logo(), inner);
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

fn rainbow_line(width: usize, symbol: &str) -> Line<'static> {
    let colors = theme::brand_rainbow();
    Line::from(
        colors
            .iter()
            .enumerate()
            .filter_map(|(index, color)| {
                let start = index * width / colors.len();
                let end = (index + 1) * width / colors.len();
                (end > start)
                    .then(|| Span::styled(symbol.repeat(end - start), Style::default().fg(*color)))
            })
            .collect::<Vec<_>>(),
    )
}

fn welcome_logo() -> Paragraph<'static> {
    let lines = vec![
        Line::from(Span::styled("█   ▀ ▄█▄              ", theme::text())),
        Line::from(Span::styled("█▄▀ █  █               ", theme::text())),
        Line::from(vec![
            Span::styled("█▀▄ █  █▄", theme::text()),
            Span::styled("  by Speakeasy", theme::dim()),
        ]),
    ];
    Paragraph::new(lines).alignment(Alignment::Center)
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
                wrap_linked_tagged(&transcript_block_lines(app, block_index, width), width),
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
    width: usize,
) -> Vec<TaggedTranscriptLine> {
    let block = &app.blocks[block_index];
    let (block_lines, call) = match block {
        Block::User(_) => unreachable!("user blocks are laid out with image anchors"),
        Block::Agent(text) => (markdown::render_copyable_at_width(text, Some(width)), None),
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
                Span::styled(text.clone(), theme::text()),
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
    let mut spans = vec![LinkedSpan {
        span: Span::styled(
            if first { "› " } else { "  " },
            theme::bold(theme::user_color()),
        ),
        url: None,
    }];
    spans.extend(markdown::inline_spans(
        text,
        theme::bold(theme::text_color()),
    ));
    LinkedLine::new(spans).with_leading_gutter()
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

fn tool_lines(app: &App, call: &ToolCall, active: bool) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(tool_header(app, call, active))];
    let compose = call.title == agentkit_tool_compose::COMPOSE_TOOL_NAME;
    if call.running() {
        if compose && !call.script.is_empty() {
            lines.extend(live_script_lines(app, call));
        } else if let Some(child) = call.children.iter().rev().find(|child| child.running()) {
            lines.push(Line::from(vec![
                Span::styled("   ↳ ", theme::faint()),
                Span::styled(child.summary.clone(), theme::dim()),
            ]));
        }
        return lines;
    }
    if !compose {
        for child in call.children.iter().take(6) {
            lines.push(Line::from(child_spans(app, child, "   ")));
        }
        if call.children.len() > 6 {
            lines.push(Line::from(Span::styled(
                format!("   … {} more calls", call.children.len() - 6),
                theme::faint(),
            )));
        }
    }
    lines.extend(output_lines(call));
    lines
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ProgramState {
    Idle,
    Resolved,
    Failed,
    Running,
}

fn live_script_lines(app: &App, call: &ToolCall) -> Vec<Line<'static>> {
    call.script
        .lines()
        .enumerate()
        .map(|(line_index, source)| {
            let nodes: Vec<usize> = call
                .plan
                .iter()
                .enumerate()
                .filter_map(|(index, node)| (node.source_line == line_index).then_some(index))
                .collect();
            let state = nodes
                .iter()
                .map(|&index| program_state(call, index))
                .max()
                .unwrap_or(ProgramState::Idle);
            let (glyph, style) = match state {
                ProgramState::Running => (
                    theme::pulse(theme::Pulse::Child, app.tick),
                    Style::default().fg(theme::running_color()),
                ),
                ProgramState::Failed => ("✗", Style::default().fg(theme::error_color())),
                ProgramState::Resolved => ("✓", Style::default().fg(theme::success_color())),
                ProgramState::Idle => ("·", theme::faint()),
            };
            let annotations: Vec<String> = nodes
                .iter()
                .map(|&index| program_annotation(call, index))
                .filter(|text| !text.is_empty())
                .collect();
            let mut spans = vec![
                Span::styled("   │ ", theme::faint()),
                Span::styled(format!("{glyph} "), style),
                Span::styled(source.to_string(), style),
            ];
            if !annotations.is_empty() {
                spans.push(Span::styled("  # ", theme::faint()));
                spans.push(Span::styled(annotations.join(" · "), style));
            }
            Line::from(spans)
        })
        .collect()
}

fn program_state(call: &ToolCall, index: usize) -> ProgramState {
    let node = &call.plan[index];
    if node.kind == PlanKind::Binding {
        return ProgramState::Resolved;
    }
    if node.kind == PlanKind::Return {
        return ProgramState::Idle;
    }
    let end = subtree_end(call, index);
    let children: Vec<&Child> = call
        .children
        .iter()
        .filter(|child| {
            child
                .node
                .is_some_and(|owner| owner >= index && owner < end)
        })
        .collect();
    if children.iter().any(|child| child.running()) {
        ProgramState::Running
    } else if children.is_empty() {
        ProgramState::Idle
    } else if children.iter().any(|child| child.ok) {
        ProgramState::Resolved
    } else {
        ProgramState::Failed
    }
}

fn subtree_end(call: &ToolCall, index: usize) -> usize {
    let depth = call.plan[index].depth;
    call.plan
        .iter()
        .enumerate()
        .skip(index + 1)
        .find_map(|(next, node)| (node.depth <= depth).then_some(next))
        .unwrap_or(call.plan.len())
}

fn program_annotation(call: &ToolCall, index: usize) -> String {
    let node = &call.plan[index];
    let state = program_state(call, index);
    let variable = node.binding.as_ref().map(|name| {
        let status = match state {
            ProgramState::Resolved => "resolved",
            ProgramState::Failed => "failed",
            ProgramState::Idle | ProgramState::Running => "waiting",
        };
        format!("{name} {status}")
    });
    let detail = match node.kind {
        PlanKind::Call => {
            let attached: Vec<&Child> = call
                .children
                .iter()
                .filter(|child| child.node == Some(index))
                .collect();
            let status = match state {
                ProgramState::Idle => "idle",
                ProgramState::Running => "running",
                ProgramState::Resolved => "success",
                ProgramState::Failed => "failure",
            };
            let target = node.tool.as_deref().unwrap_or(node.label.as_str());
            let target = if target.is_empty() {
                node.kind.glyph()
            } else {
                target
            };
            if attached.len() <= 1 {
                Some(format!("{target} {status}"))
            } else {
                let running = attached.iter().filter(|child| child.running()).count();
                let success = attached
                    .iter()
                    .filter(|child| !child.running() && child.ok)
                    .count();
                let failure = attached
                    .iter()
                    .filter(|child| !child.running() && !child.ok)
                    .count();
                let states = [
                    (running, "running"),
                    (success, "success"),
                    (failure, "failure"),
                ]
                .into_iter()
                .filter(|(count, _)| *count > 0)
                .map(|(count, state)| format!("{count} {state}"))
                .collect::<Vec<_>>()
                .join(", ");
                Some(format!("{target}: {states}"))
            }
        }
        PlanKind::Loop | PlanKind::Fold => Some(match direct_execution_count(call, index) {
            Some(0) => "iteration waiting".into(),
            Some(count) if state == ProgramState::Running => {
                format!("iteration {count} running")
            }
            Some(count) => format!("{count} {}", plural("iteration", count)),
            None if state == ProgramState::Idle => "iteration waiting".into(),
            None => "iterations active".into(),
        }),
        PlanKind::Boundary => Some(match direct_execution_count(call, index) {
            Some(0) => "attempt waiting".into(),
            Some(attempt) => format!("attempt {attempt}"),
            None if state == ProgramState::Idle => "attempt waiting".into(),
            None => "boundary active".into(),
        }),
        PlanKind::After => Some(if state == ProgramState::Idle {
            "dependency waiting".into()
        } else {
            "dependency ready".into()
        }),
        PlanKind::Branch => Some(if state == ProgramState::Idle {
            "branch waiting".into()
        } else {
            "branch active".into()
        }),
        PlanKind::Return => Some("waiting to return".into()),
        PlanKind::Binding => None,
    };
    [variable, detail]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Counts a construct only when one direct child call makes dispatch count a
/// sound proxy. Nested loops/boundaries and branching bodies stay qualitative.
fn direct_execution_count(call: &ToolCall, index: usize) -> Option<usize> {
    let end = subtree_end(call, index);
    let child_depth = call.plan[index].depth + 1;
    let calls: Vec<usize> = (index + 1..end)
        .filter(|&child| {
            call.plan[child].depth == child_depth && call.plan[child].kind == PlanKind::Call
        })
        .collect();
    (calls.len() == 1).then(|| {
        call.children
            .iter()
            .filter(|child| child.node == calls.first().copied())
            .count()
    })
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
    expanded_output_lines(call)
}

fn expanded_output_lines(call: &ToolCall) -> Vec<Line<'static>> {
    if call.output.is_empty() {
        return Vec::new();
    }
    let count = call.output.len();
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

fn truncate_to_width(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }

    let content_width = width.saturating_sub(1);
    let mut truncated = String::new();
    for character in text.chars() {
        let candidate_width = UnicodeWidthStr::width(truncated.as_str())
            + unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if candidate_width > content_width {
            break;
        }
        truncated.push(character);
    }
    truncated.push('…');
    truncated
}

fn agent_duration(millis: u64) -> String {
    let mut duration = theme::duration(millis);
    if let Some(minutes_end) = duration.rfind('m').map(|index| index + 1)
        && duration[minutes_end..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
    {
        duration.insert(minutes_end, ' ');
    }
    duration
}

fn agent_lines(
    tree_row: &AgentTreeRow<'_>,
    tick: usize,
    now_unix_ms: u64,
    width: usize,
) -> [Line<'static>; 2] {
    let row = tree_row.row;
    let failed = row.outcome == Some(GenerationOutcome::Failed)
        && row
            .generation_finished_at_unix_ms
            .is_some_and(|finished| now_unix_ms.saturating_sub(finished) < 4_000);
    let (glyph, glyph_style) = match row.status {
        SubagentStatus::Starting => (
            theme::pulse(theme::Pulse::Child, tick),
            Style::default().fg(theme::warn_color()),
        ),
        SubagentStatus::Working => (
            theme::pulse(theme::Pulse::Tool, tick),
            Style::default().fg(Color::Cyan),
        ),
        SubagentStatus::Idle | SubagentStatus::Removed if failed => {
            ("✗", Style::default().fg(theme::error_color()))
        }
        SubagentStatus::Idle | SubagentStatus::Removed => ("○", theme::dim()),
    };
    let ancestry = if tree_row.missing_parent {
        row.parent_name
            .as_ref()
            .map(|name| format!(" · via {name}"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let mut first_prefix = String::new();
    let mut second_prefix = String::new();
    if tree_row.depth == 0 {
        second_prefix.push_str(if tree_row.has_next_sibling {
            "│ "
        } else {
            "  "
        });
    } else {
        for has_next in &tree_row.ancestor_has_next_sibling {
            let connector = if *has_next { "│  " } else { "   " };
            first_prefix.push_str(connector);
            second_prefix.push_str(connector);
        }
        first_prefix.push_str(if tree_row.has_next_sibling {
            "├─ "
        } else {
            "└─ "
        });
        second_prefix.push_str(if tree_row.has_next_sibling {
            "│  "
        } else {
            "   "
        });
    }
    let first = Line::from(vec![
        Span::styled(first_prefix, theme::faint()),
        Span::styled(format!("{glyph} "), glyph_style),
        Span::styled(row.name.clone(), theme::text()),
        Span::styled(ancestry, theme::faint()),
    ]);

    let finished = row.generation_finished_at_unix_ms.unwrap_or(now_unix_ms);
    let duration = agent_duration(finished.saturating_sub(row.generation_started_at_unix_ms));
    let duration_full_width = UnicodeWidthStr::width(duration.as_str());
    let duration_width = duration_full_width.min(width);
    let displayed_duration = if duration_width < duration_full_width {
        visible_query_tail(&duration, duration_width).to_string()
    } else {
        duration
    };
    let prefix_width = width.saturating_sub(duration_width);
    let tree_prefix_width = UnicodeWidthStr::width(second_prefix.as_str());
    let second = if prefix_width <= tree_prefix_width {
        Line::from(vec![
            Span::raw(" ".repeat(prefix_width)),
            Span::styled(displayed_duration, theme::faint()),
        ])
    } else {
        let task_width = prefix_width - tree_prefix_width - 1;
        let task = truncate_to_width(&row.task, task_width);
        let task_padding = task_width.saturating_sub(UnicodeWidthStr::width(task.as_str()));
        Line::from(vec![
            Span::styled(second_prefix, theme::faint()),
            Span::styled(task, theme::dim()),
            Span::raw(" ".repeat(task_padding + 1)),
            Span::styled(displayed_duration, theme::faint()),
        ])
    };
    [first, second]
}

fn draw_agents(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let block = Panel::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::faint())
        .title(Span::styled(" agent roster ", theme::accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let row_area_height = inner.height.saturating_sub(1);
    let visible_rows = usize::from(row_area_height / 2);
    app.set_agents_viewport(area, visible_rows);
    let now = crate::events::now_millis();
    let lines = app
        .agent_tree_rows()
        .into_iter()
        .skip(app.agents_scroll())
        .take(visible_rows)
        .flat_map(|row| agent_lines(&row, app.tick, now, inner.width as usize))
        .collect::<Vec<_>>();
    let rows_area = Rect {
        height: row_area_height,
        ..inner
    };
    frame.render_widget(Paragraph::new(lines), rows_area);

    if inner.height > 0 {
        let counts = app.agent_counts();
        let mut parts = vec![format!("{} agents", counts.total)];
        if counts.starting > 0 {
            parts.push(format!("{} starting", counts.starting));
        }
        if counts.working > 0 {
            parts.push(format!("{} working", counts.working));
        }
        if counts.idle > 0 {
            parts.push(format!("{} idle", counts.idle));
        }
        let footer = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(Span::styled(parts.join(" · "), theme::faint())),
            footer,
        );
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
            Span::styled(text, theme::bold(theme::text_color())),
            Span::styled("  · pending", theme::faint()),
        ])
    }));
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_start_prompt(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let inner = draw_start_prompt_frame(frame, area);
    let [input, _, metadata] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    draw_prompt_editor(frame, app, input, theme::dim());

    let metadata = Rect {
        x: metadata.x + 2,
        width: metadata.width.saturating_sub(2),
        ..metadata
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(app.provider.clone(), theme::accent()),
            Span::styled("  ·  ", theme::dim()),
            Span::styled(app.model.clone(), theme::text()),
            Span::styled("  ·  ", theme::dim()),
            Span::styled(format!("effort {}", app.reasoning_effort), theme::dim()),
        ])),
        metadata,
    );
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
    draw_prompt_editor(frame, app, inner, theme::faint());
}

fn draw_start_prompt_frame(frame: &mut Frame<'_>, area: Rect) -> Rect {
    let surface = theme::composer();
    frame.render_widget(Panel::default().style(surface), area);
    if area.width >= 2
        && area.height >= 2
        && let Some(color) = surface.bg
    {
        let corner = Style::default().fg(color).bg(Color::Reset);
        let y = area.y + area.height - 1;
        frame.render_widget(
            Paragraph::new(Span::styled("▜", corner)),
            Rect::new(area.x, y, 1, 1),
        );
        frame.render_widget(
            Paragraph::new(Span::styled("▛", corner)),
            Rect::new(area.x + area.width - 1, y, 1, 1),
        );
    }
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let rainbow = Rect { height: 1, ..area };
    frame.render_widget(
        Paragraph::new(rainbow_line(area.width as usize, "▔")),
        rainbow,
    );
    inner
}

fn draw_prompt_editor(frame: &mut Frame<'_>, app: &App, area: Rect, placeholder_style: Style) {
    let [gutter, field] =
        Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(area);
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
            placeholder_style,
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
                Style::default().fg(theme::warn_color()),
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
                Style::default().fg(theme::accent_color()),
            ),
            Span::styled(
                format!("working {}", theme::duration(app.elapsed())),
                Style::default().fg(theme::accent_color()),
            ),
        ],
    };
    if let Some(toast) = app.toast_text() {
        left.push(Span::styled("  ", theme::dim()));
        left.push(Span::styled(
            toast.to_string(),
            Style::default().fg(theme::warn_color()),
        ));
    }
    let hints = "⏎ send   ⇧⏎ newline   ^l log   ^c quit ";
    let used: usize = left.iter().map(|span| span.content.chars().count()).sum();
    let gap = (area.width as usize)
        .saturating_sub(used + hints.chars().count())
        .max(1);
    left.push(Span::styled(" ".repeat(gap), theme::dim()));
    left.push(Span::styled(hints, theme::dim()));
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::picker::Picker;

    use super::{
        MAX_PROMPT_ROWS, ModelDialogRow, agent_lines, body_layout, draw, draw_agents,
        model_dialog_rows, model_dialog_viewport, prompt_lines, refresh_transcript_cache,
        refresh_transcript_cache_with_images, user_block_rows, user_line,
    };
    use crate::{
        events::{GenerationOutcome, RuntimeEvent, SubagentStatus},
        tui::app::{
            Action, AgentRow, AgentTreeRow, App, Block, EffortChoice, EffortDialog, ModelDialog,
            Update, UserImage, UserMessage,
        },
    };

    fn test_agent(
        name: &str,
        status: SubagentStatus,
        outcome: Option<GenerationOutcome>,
        parent_name: Option<&str>,
        task: &str,
    ) -> AgentRow {
        AgentRow {
            id: format!("id-{name}"),
            name: name.into(),
            status,
            outcome,
            generation: 1,
            task: task.into(),
            parent_id: parent_name.map(|_| "parent-id".into()),
            parent_name: parent_name.map(Into::into),
            harness: "acp.kit".into(),
            model: Some("test".into()),
            created_at_unix_ms: 1_000,
            generation_started_at_unix_ms: 2_000,
            generation_finished_at_unix_ms: None,
        }
    }

    fn tree_row(
        row: &AgentRow,
        ancestor_has_next_sibling: Vec<bool>,
        has_next_sibling: bool,
        missing_parent: bool,
    ) -> AgentTreeRow<'_> {
        AgentTreeRow {
            row,
            depth: ancestor_has_next_sibling.len(),
            ancestor_has_next_sibling,
            has_next_sibling,
            missing_parent,
        }
    }

    fn line_text(line: &ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn agent_rows_render_two_lines_ancestry_duration_and_palette() {
        let top = test_agent(
            "Scout",
            SubagentStatus::Working,
            None,
            None,
            "Trace ACP lifecycle",
        );
        let lines = agent_lines(&tree_row(&top, vec![], false, false), 0, 74_000, 48);
        assert_eq!(line_text(&lines[0]), "⠋ Scout");
        let second = line_text(&lines[1]);
        assert_eq!(second, "  Trace ACP lifecycle                     1m 12s");
        assert_eq!(&second[42..48], "1m 12s");
        assert_eq!(unicode_width::UnicodeWidthStr::width(second.as_str()), 48);
        assert_eq!(
            lines[0].spans[1].style.fg,
            Some(ratatui::style::Color::Cyan)
        );

        let nested = test_agent(
            "Scout",
            SubagentStatus::Starting,
            None,
            Some("Pip"),
            "Trace ACP lifecycle",
        );
        let lines = agent_lines(&tree_row(&nested, vec![true], false, false), 0, 74_000, 48);
        assert_eq!(line_text(&lines[0]), "│  └─ ⠁ Scout");
        assert!(line_text(&lines[1]).starts_with("│     Trace ACP lifecycle"));

        let lines = agent_lines(&tree_row(&nested, vec![false], true, false), 0, 74_000, 48);
        assert_eq!(line_text(&lines[0]), "   ├─ ⠁ Scout");
        assert!(line_text(&lines[1]).starts_with("   │  Trace ACP lifecycle"));

        let lines = agent_lines(&tree_row(&nested, vec![], true, true), 0, 74_000, 48);
        assert_eq!(line_text(&lines[0]), "⠁ Scout · via Pip");
        assert!(line_text(&lines[1]).starts_with("│ Trace ACP lifecycle"));
        assert_eq!(
            lines[0].spans[1].style.fg,
            Some(ratatui::style::Color::Yellow)
        );

        let idle = test_agent("Scout", SubagentStatus::Idle, None, None, "done");
        let idle_lines = agent_lines(&tree_row(&idle, vec![], false, false), 0, 74_000, 20);
        assert_eq!(line_text(&idle_lines[0]), "○ Scout");
        assert!(
            idle_lines[0].spans[1]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );
        let mut failed = test_agent(
            "Scout",
            SubagentStatus::Idle,
            Some(GenerationOutcome::Failed),
            None,
            "failed",
        );
        failed.generation_finished_at_unix_ms = Some(72_000);
        let failed_lines = agent_lines(&tree_row(&failed, vec![], false, false), 0, 74_000, 20);
        assert_eq!(line_text(&failed_lines[0]), "✗ Scout");
        assert_eq!(
            failed_lines[0].spans[1].style.fg,
            Some(ratatui::style::Color::Red)
        );
        assert_eq!(
            line_text(&agent_lines(&tree_row(&failed, vec![], false, false), 0, 77_000, 20,)[0]),
            "○ Scout"
        );
    }

    #[test]
    fn agent_rows_truncate_unicode_before_reserved_duration() {
        let row = test_agent(
            "Scout",
            SubagentStatus::Working,
            None,
            None,
            "🦀🦀 lifecycle work",
        );
        let lines = agent_lines(&tree_row(&row, vec![], false, false), 0, 3_500, 12);
        assert_eq!(line_text(&lines[1]), "  🦀🦀… 1.5s");
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(line_text(&lines[1]).as_str()),
            12
        );

        let lines = agent_lines(&tree_row(&row, vec![], false, false), 0, 3_500, 4);
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(line_text(&lines[1]).as_str()),
            4
        );
    }

    #[test]
    fn agents_layout_obeys_hidden_and_107_108_boundaries() {
        let area_107 = ratatui::layout::Rect::new(2, 3, 107, 20);
        assert_eq!(body_layout(area_107, false), (area_107, None));
        assert_eq!(
            body_layout(area_107, true),
            (
                ratatui::layout::Rect::new(2, 3, 107, 11),
                Some(ratatui::layout::Rect::new(2, 14, 107, 9)),
            )
        );

        let area_108 = ratatui::layout::Rect::new(2, 3, 108, 20);
        assert_eq!(
            body_layout(area_108, true),
            (
                ratatui::layout::Rect::new(2, 3, 62, 20),
                Some(ratatui::layout::Rect::new(64, 3, 46, 20)),
            )
        );
    }

    fn panel_app(agent_count: usize) -> App {
        let mut app = App::new(
            PathBuf::from("/tmp/project"),
            "provider".into(),
            "model".into(),
            "127.0.0.1:7331".into(),
        );
        for index in 0..agent_count {
            app.apply(Update::Runtime(RuntimeEvent::SubagentStateChanged {
                id: format!("agent-{index}"),
                name: format!("Scout {index}"),
                status: SubagentStatus::Idle,
                outcome: Some(GenerationOutcome::Success),
                generation: 1,
                task: format!("Task {index}"),
                parent_id: None,
                parent_name: None,
                harness: "acp.kit".into(),
                model: Some("test".into()),
                created_at_unix_ms: 1_000 + index as u64,
                generation_started_at_unix_ms: 2_000,
                generation_finished_at_unix_ms: Some(74_000),
            }));
        }
        app
    }

    fn buffer_row(buffer: &ratatui::buffer::Buffer, row: u16) -> String {
        (0..buffer.area.width)
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    fn buffer_cells(
        buffer: &ratatui::buffer::Buffer,
        row: u16,
        columns: std::ops::Range<u16>,
    ) -> String {
        columns
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    #[test]
    fn agents_panel_keeps_footer_fixed_while_overflowing_rows_scroll() {
        let mut app = panel_app(5);
        let mut terminal = Terminal::new(TestBackend::new(46, 8)).expect("terminal");
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .expect("draw succeeds");
        let initial = terminal.backend().buffer();
        assert_eq!(buffer_cells(initial, 0, 1..15), " agent roster ");
        assert_eq!(buffer_cells(initial, 1, 1..10), "○ Scout 0");
        assert_eq!(buffer_cells(initial, 2, 3..9), "Task 0");
        assert_eq!(buffer_cells(initial, 3, 1..10), "○ Scout 1");
        assert_eq!(buffer_cells(initial, 6, 1..18), "5 agents · 5 idle");

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.agents_scroll(), 3);
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .expect("draw succeeds");
        let scrolled = terminal.backend().buffer();
        assert_eq!(buffer_cells(scrolled, 1, 1..10), "○ Scout 3");
        assert_eq!(buffer_cells(scrolled, 3, 1..10), "○ Scout 4");
        assert_eq!(buffer_cells(scrolled, 6, 1..18), "5 agents · 5 idle");
        assert_eq!(
            buffer_row(scrolled, 7),
            "╰────────────────────────────────────────────╯"
        );
    }

    #[test]
    fn agents_panel_draws_zero_and_tiny_rectangles_without_panicking() {
        let mut app = panel_app(5);
        app.set_agents_viewport(ratatui::layout::Rect::new(0, 0, 2, 4), 1);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.agents_scroll(), 3);

        let mut terminal = Terminal::new(TestBackend::new(1, 1)).expect("terminal");
        terminal
            .draw(|frame| {
                draw_agents(frame, &mut app, ratatui::layout::Rect::default());
                draw_agents(frame, &mut app, ratatui::layout::Rect::new(0, 0, 1, 1));
            })
            .expect("zero and tiny draws succeed");
        assert_eq!(app.agents_scroll(), 3);
    }

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
    fn active_compose_keeps_the_full_transcript_layout() {
        let mut app = sample();

        render(&mut app, 120, 40);

        assert!(app.transcript_width > 100, "{}", app.transcript_width);
    }

    #[test]
    fn ctrl_g_does_not_toggle_a_runtime_graph_layout() {
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        app.push_user("keep the transcript full width".into());

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        render(&mut app, 120, 20);

        assert!(app.transcript_width > 100, "{}", app.transcript_width);
    }

    #[test]
    fn compose_script_runs_inline_with_live_annotations_at_full_width() {
        let mut app = sample();

        let frame = render(&mut app, 120, 40);

        assert!(
            frame.contains("files = shell({ command: \"ls src\" })"),
            "{frame}"
        );
        assert!(frame.contains("files resolved · shell success"), "{frame}");
        assert!(
            frame.contains("checked waiting · iteration 1 running"),
            "{frame}"
        );
        assert!(frame.contains("shell running"), "{frame}");
        assert!(app.transcript_width > 100, "{}", app.transcript_width);
    }

    #[test]
    fn compose_script_annotates_idle_and_failed_calls_and_boundary_attempts() {
        let script = "value = boundary retry 2 {\n\
            return shell({ command: \"false\" })\n\
        } catch err {\n\
            return fail(\"FAILED\", err.message)\n\
        }\n\
        later = docs({ query: \"next\" })\n\
        return value";
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        app.apply(Update::ToolStarted {
            id: "call-1".into(),
            title: "compose".into(),
            kind: ToolKind::Other,
            script: Some(script.into()),
            backgrounded: false,
        });
        app.apply(Update::Runtime(RuntimeEvent::ChildStarted {
            call: "call-1:compose:failed".into(),
            tool: "shell".into(),
            summary: "false".into(),
            at: 0,
        }));
        app.apply(Update::Runtime(RuntimeEvent::ChildFinished {
            call: "call-1:compose:failed".into(),
            tool: "shell".into(),
            ok: false,
            summary: "exit code 1".into(),
            millis: 10,
        }));

        let frame = render(&mut app, 100, 30);

        assert!(frame.contains("value = boundary retry 2 {"), "{frame}");
        assert!(frame.contains("value failed · attempt 1"), "{frame}");
        assert!(frame.contains("shell failure"), "{frame}");
        assert!(frame.contains("later waiting · docs idle"), "{frame}");
    }

    #[test]
    fn completed_compose_replaces_the_script_with_output() {
        let mut app = sample();
        app.apply(Update::ToolUpdated {
            id: "call-1".into(),
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
            script: None,
            output: vec!["compose result".into()],
            backgrounded: false,
        });

        let frame = render(&mut app, 100, 30);

        assert!(!frame.contains("files = shell"), "{frame}");
        assert!(frame.contains("compose result"), "{frame}");

        app.apply(Update::AgentMessage {
            id: "test-agent".into(),
            text: "Moving on.".into(),
            append: true,
        });
        let continued = render(&mut app, 100, 30);
        assert!(continued.contains("Moving on."), "{continued}");
        assert!(continued.contains("1 line of output"), "{continued}");
        assert!(!continued.contains("compose result"), "{continued}");
    }

    #[test]
    fn explicit_output_choice_survives_later_messages_and_tools() {
        let mut app = sample();
        app.apply(Update::ToolUpdated {
            id: "call-1".into(),
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
            script: None,
            output: vec!["compose result".into()],
            backgrounded: false,
        });

        app.toggle_last_output();
        app.apply(Update::AgentMessage {
            id: "after-compose".into(),
            text: "Moving on.".into(),
            append: true,
        });
        let compose_expanded = |app: &App| {
            app.blocks.iter().any(
                |block| matches!(block, Block::Tool(call) if call.id == "call-1" && call.expanded),
            )
        };
        assert!(!compose_expanded(&app));

        app.toggle_last_output();
        app.apply(Update::ToolStarted {
            id: "call-2".into(),
            title: "shell".into(),
            kind: ToolKind::Execute,
            script: None,
            backgrounded: false,
        });
        assert!(compose_expanded(&app));
    }

    #[test]
    fn a_new_tool_collapses_the_previous_compose_output() {
        let mut app = sample();
        app.apply(Update::ToolUpdated {
            id: "call-1".into(),
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
            script: None,
            output: vec!["compose result".into()],
            backgrounded: false,
        });
        app.apply(Update::ToolStarted {
            id: "call-2".into(),
            title: "shell".into(),
            kind: ToolKind::Execute,
            script: None,
            backgrounded: false,
        });

        let previous = app.blocks.iter().find_map(|block| match block {
            Block::Tool(call) if call.id == "call-1" => Some(call),
            _ => None,
        });
        assert!(previous.is_some_and(|call| !call.expanded));
    }

    #[test]
    fn non_compose_running_child_summary_remains_inline() {
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        app.apply(Update::ToolStarted {
            id: "call-1".into(),
            title: "shell".into(),
            kind: ToolKind::Execute,
            script: None,
            backgrounded: false,
        });
        app.apply(Update::Runtime(RuntimeEvent::ChildStarted {
            call: "call-1:child".into(),
            tool: "shell".into(),
            summary: "cargo check".into(),
            at: 0,
        }));

        let frame = render(&mut app, 80, 20);

        assert!(frame.contains("↳ cargo check"), "{frame}");
    }

    #[test]
    fn only_the_focused_call_shows_the_kill_hint() {
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
        assert!(frame.contains("explain how the compose tool dispatches hidden"));
        assert!(frame.contains("children and everything internal to it"));

        // A prompt taller than the cap scrolls inside the box instead of
        // pushing the transcript off the screen.
        for _ in 0..40 {
            app.editor.insert_str("more text to type ");
        }
        let frame = render(&mut app, 60, 20);
        let rows = frame.lines().collect::<Vec<_>>();
        let rainbow = rows
            .iter()
            .rposition(|row| row.trim_start().starts_with('▔'))
            .expect("prompt rainbow");
        let status = rows
            .iter()
            .skip(rainbow + 1)
            .position(|row| row.contains("send"))
            .map(|offset| rainbow + 1 + offset)
            .expect("status row");
        assert_eq!(status - rainbow - 4, MAX_PROMPT_ROWS);
        assert!(frame.contains("message kit") || frame.contains("more text"));
    }

    #[test]
    fn start_screen_stays_stable_as_the_prompt_grows() {
        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        let initial = render(&mut app, 60, 11);
        let prompt_width = app.prompt_width;
        app.editor.insert_str(&"wrapped prompt ".repeat(80));
        let grown = render(&mut app, 60, 11);

        assert!(initial.contains("by Speakeasy"));
        assert!(grown.contains("by Speakeasy"));
        assert!(grown.contains('▔'));
        assert_eq!(app.prompt_width, prompt_width);
    }

    #[test]
    fn compact_prompt_still_wraps_and_caps_after_submission() {
        let mut app = sample();
        app.editor.insert_str(&"more text to type ".repeat(80));
        let frame = render(&mut app, 60, 20);
        let rows = frame.lines().collect::<Vec<_>>();
        let status = rows
            .iter()
            .position(|row| row.contains("send"))
            .expect("status row");
        let bottom = status - 1;
        let top = rows[..bottom]
            .iter()
            .rposition(|row| row.starts_with('╭'))
            .expect("prompt top border");
        let prompt = &rows[top + 1..bottom];

        assert_eq!(prompt.len(), MAX_PROMPT_ROWS);
        assert!(
            prompt
                .iter()
                .all(|row| row.starts_with('│') && row.ends_with('│'))
        );
        assert!(rows[bottom].starts_with('╰'));
    }

    #[test]
    fn clicking_a_code_block_copies_exact_content() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mut app = sample();
        let frame = render(&mut app, 100, 24);
        let row = frame
            .lines()
            .position(|line| line.contains("│ cargo check"))
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
        if let Some(Block::Tool(call)) = app.blocks.last_mut() {
            call.title = "shell".into();
            call.expanded = false;
        }
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
            .position(|line| line.contains("│ cargo check"))
            .expect("code row is on screen");
        let line = app.scroll + (row - app.transcript_top);
        app.selection = Some(Selection {
            anchor: (line, 0),
            head: (line, app.transcript_width.saturating_sub(1)),
        });
        assert_eq!(app.selection_text().as_deref(), Some("cargo check"));
    }

    #[test]
    fn selection_copy_strips_empty_and_wrapped_code_gutters() {
        use crate::tui::app::Selection;

        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        let code = "one\n\n  abcdefghijklmnopqrstuvwxyz0123456789\ntwo";
        app.blocks
            .push(Block::Agent(format!("```text\n{code}\n```")));
        let frame = render(&mut app, 24, 24);
        let first = frame
            .lines()
            .position(|line| line.contains("│ one"))
            .expect("first code row is on screen");
        let closing = frame
            .lines()
            .position(|line| line.contains("└─ text"))
            .expect("closing fence is on screen");
        app.selection = Some(Selection {
            anchor: (app.scroll + first - app.transcript_top, 0),
            head: (
                app.scroll + closing - app.transcript_top - 1,
                app.transcript_width.saturating_sub(1),
            ),
        });

        assert_eq!(app.selection_text().as_deref(), Some(code));
    }

    #[test]
    fn narrow_code_selection_preserves_source_indentation() {
        use crate::tui::app::Selection;

        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        app.blocks.push(Block::Agent("```text\n 界x\n```".into()));
        let _ = render(&mut app, 5, 24);
        let rows = &app.transcript_cache[0]
            .as_ref()
            .expect("agent block is cached")
            .rows;
        let selected = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.1.2 == Some(1))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        app.selection = Some(Selection {
            anchor: (*selected.first().expect("code has a first row"), 0),
            head: (
                *selected.last().expect("code has a last row"),
                app.transcript_width.saturating_sub(1),
            ),
        });

        assert_eq!(app.selection_text().as_deref(), Some(" 界x"));
    }

    #[test]
    fn wrapped_prose_selection_preserves_space_before_styled_text() {
        use crate::tui::app::Selection;

        let mut app = App::new(
            PathBuf::from("/tmp/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "0:0".into(),
        );
        app.blocks.push(Block::Agent("hello `x`".into()));
        let _ = render(&mut app, 9, 24);
        let rows = &app.transcript_cache[0]
            .as_ref()
            .expect("agent block is cached")
            .rows;
        app.selection = Some(Selection {
            anchor: (0, 0),
            head: (rows.len() - 1, app.transcript_width.saturating_sub(1)),
        });

        assert_eq!(app.selection_text().as_deref(), Some("hello `x`"));
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
        app.session_id = Some("s-1770000000000-12345-0".into());

        let frame = render(&mut app, 120, 24);

        assert!(frame.contains(concat!("kit v", env!("CARGO_PKG_VERSION"))));
        assert!(frame.contains("openai-subscription / gpt-5.4"));
        assert!(frame.contains("0.5% 1k/272k"));
        assert!(frame.contains("session s-1770000000000-12345-0"));
        assert!(frame.lines().any(|line| line == "━".repeat(120)));
        assert!(!frame.contains("ctx "));
    }

    #[test]
    fn header_fitting_uses_terminal_column_width() {
        let mut app = sample();
        app.provider = "p".into();
        app.model = "模型".into();
        app.session_id = Some("s".into());

        let frame = render(&mut app, 35, 24);
        let header = frame.lines().next().expect("header row");

        assert!(header.contains("session s"));
        assert!(!header.contains("p / 模型"));
    }

    #[test]
    fn short_terminals_keep_the_prompt_instead_of_the_header_rainbow() {
        let mut app = sample();

        let frame = render(&mut app, 80, 8);

        assert!(frame.contains("message kit…"));
        assert!(!frame.lines().any(|line| line == "━".repeat(80)));
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
    fn restores_the_compact_prompt_after_the_first_submission() {
        let mut app = sample();
        let frame = render(&mut app, 90, 24);
        let rows = frame.lines().collect::<Vec<_>>();
        let input = rows
            .iter()
            .position(|row| row.starts_with("│› message kit…"))
            .expect("prompt input");

        assert!(rows[input - 1].starts_with('╭'));
        assert!(rows[input + 1].starts_with('╰'));
        assert!(!frame.contains('▜'));
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
        assert!(frame.contains("█▀▄ █  █▄  by Speakeasy"));
        assert!(!frame.contains("╭──────────────╮"));
        assert!(frame.contains("openai-subscription  ·  gpt-5.4  ·  effort default"));
        assert!(frame.contains(
            "▜                                                                                    ▛"
        ));
        assert!(frame.lines().any(|line| line.trim() == "▔".repeat(86)));
        assert!(frame.contains(" ready"));
        assert!(frame.contains("⏎ send   ⇧⏎ newline   ^l log   ^c quit"));
        assert!(frame.contains("message kit"));
    }
}
