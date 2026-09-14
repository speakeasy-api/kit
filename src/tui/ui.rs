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
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::events::{GenerationOutcome, SubagentStatus};

use super::{
    app::{
        AgentPart, AgentTreeRow, App, Block, CachedTranscriptBlock, CachedTranscriptImage,
        CachedTranscriptRow, CodeHit, ComposeView, EffortDialog, FilePickerDialog,
        FilePickerStatus, ModelDialog, Phase, SessionRename, ToolCall, UserMessage,
    },
    command,
    image::{ImageRuntime, RESERVED_ROWS},
    markdown, theme,
    wrap::{LinkedLine, LinkedSpan, wrap_linked_tagged},
};

const SIDE_BY_SIDE_WIDTH: u16 = 108;
const AGENTS_WIDTH: u16 = 46;
const MAX_PROMPT_ROWS: usize = 10;

pub(super) fn native_links_obscured(app: &App) -> bool {
    app.model_switch.is_some()
        || app.file_picker.is_some()
        || app.session_dialog.is_some()
        || app.model_dialog.is_some()
        || app.effort_dialog.is_some()
        || !app.command_completions().is_empty()
}
const START_MAX_WIDTH: u16 = 96;
const START_LOGO_ROWS: u16 = 3;
const START_LOGO_GAP: u16 = 2;
const START_PROMPT_CHROME_ROWS: u16 = 4;
const START_MIN_PROMPT_ROWS: u16 = START_PROMPT_CHROME_ROWS + 1;
/// Rows of raw tool output rendered when a card is opened.
const MAX_OUTPUT_ROWS: usize = 400;

type TranscriptTag = (Option<String>, Option<CodeHit>, Option<usize>);
type TaggedTranscriptLine = (LinkedLine, TranscriptTag);

pub fn draw(frame: &mut Frame<'_>, app: &mut App, images: &mut ImageRuntime) {
    images.poll();
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
        && !app.editing_steer()
        && !app.queue_focused
        && !app.show_logs;

    let (prompt_area, prompt_viewport, picker_below) = if show_start {
        app.prompt_width = start_prompt_width;
        let (prompt, viewport) = draw_start(frame, app, start_width, start_prompt_rows);
        (prompt, viewport, true)
    } else {
        let prompt_width = frame.area().width.saturating_sub(4).max(1) as usize;
        app.prompt_width = prompt_width;
        let prompt_rows = app
            .editor
            .display_rows(prompt_width)
            .clamp(1, MAX_PROMPT_ROWS) as u16
            + 2;
        let logs_rows = if app.show_logs { 9 } else { 0 };
        let narrow = frame.area().width < SIDE_BY_SIDE_WIDTH;
        let pending_rows = dock_rows(app, narrow) as u16;
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

        app.session_area = draw_header(frame, app, header).unwrap_or_default();

        draw_body(frame, app, images, body);
        if app.show_logs {
            draw_logs(frame, app, logs);
        }
        draw_dock(frame, app, pending, narrow);
        let viewport = draw_prompt(frame, app, prompt);
        draw_command_popup(frame, app, prompt);
        draw_status(frame, app, status);
        (prompt, viewport, false)
    };
    if let Some(pending) = &app.model_switch {
        draw_model_switch_dialog(frame, pending);
    } else if let Some(dialog) = &app.file_picker {
        draw_file_picker(
            frame,
            app,
            dialog,
            prompt_area,
            prompt_viewport,
            picker_below,
        );
    } else if app.session_dialog.is_some() {
        draw_session_dialog(frame, app);
    } else if let Some(dialog) = &app.model_dialog {
        draw_model_dialog(frame, app, dialog);
    } else if let Some(dialog) = &app.effort_dialog {
        draw_effort_dialog(frame, app, dialog);
    }
    // Durability stays visible on the start screen and over session pickers.
    // Pending data belongs to the process, not the currently selected session.
    if app.runtime_unavailable() || app.storage_pending || app.storage_exhausted {
        let area = frame.area();
        let warning = if app.runtime_unavailable() {
            " Runtime status unavailable: agent, child, compaction and storage state unknown"
        } else if app.storage_exhausted {
            " Storage exhausted: shutting down; unpersisted data is at risk"
        } else {
            " Memory-only storage: awaiting disk recovery; data at risk on exit"
        };
        frame.render_widget(
            Paragraph::new(warning).style(Style::default().fg(theme::warn_color())),
            Rect::new(
                area.x,
                area.bottom().saturating_sub(1),
                area.width,
                u16::from(area.height > 0),
            ),
        );
    }
}

#[derive(Clone, Copy)]
struct PromptViewport {
    field: Rect,
    first_row: usize,
}

struct CompletionPopup {
    title: &'static str,
    rows: Vec<Line<'static>>,
    selected: Option<usize>,
    footer: Option<Line<'static>>,
    max_rows: usize,
    max_width: u16,
    prefer_below: bool,
}

fn draw_completion_popup(frame: &mut Frame<'_>, anchor: Rect, popup: CompletionPopup) {
    let outer = frame.area();
    let width = anchor.width.min(popup.max_width);
    let below_y = anchor.y.saturating_add(anchor.height);
    let below_height = outer.bottom().saturating_sub(below_y);
    let above_height = anchor.y.saturating_sub(outer.y);
    let below = popup.prefer_below && below_height >= 3;
    let available = if below { below_height } else { above_height };
    if popup.rows.is_empty() || available < 3 || width < 4 {
        return;
    }
    let footer_rows = u16::from(popup.footer.is_some() && available >= 4);
    let visible = popup
        .rows
        .len()
        .min(popup.max_rows)
        .min(available.saturating_sub(2 + footer_rows) as usize);
    if visible == 0 {
        return;
    }
    let height = visible as u16 + 2 + footer_rows;
    let y = if below {
        below_y
    } else {
        anchor.y.saturating_sub(height)
    };
    let area = Rect::new(
        anchor.x + anchor.width.saturating_sub(width) / 2,
        y,
        width,
        height,
    );
    let panel = Panel::bordered()
        .title(popup.title)
        .border_type(BorderType::Rounded)
        .border_style(theme::accent());
    let inner = panel.inner(area);
    let [list, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(footer_rows)]).areas(inner);
    let selected = popup
        .selected
        .map(|selected| selected.min(popup.rows.len().saturating_sub(1)));
    let first = selected
        .unwrap_or_default()
        .saturating_sub(visible.saturating_sub(1))
        .min(popup.rows.len().saturating_sub(visible));
    let lines = popup
        .rows
        .into_iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(index, line)| {
            let is_selected = selected == Some(index);
            let mut spans = vec![Span::styled(
                if is_selected { "› " } else { "  " },
                theme::accent(),
            )];
            spans.extend(line.spans);
            let line = Line::from(spans);
            if is_selected {
                line.style(theme::selection())
            } else {
                line
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(panel, area);
    frame.render_widget(Paragraph::new(lines), list);
    if footer_rows != 0
        && let Some(footer_line) = popup.footer
    {
        frame.render_widget(Paragraph::new(footer_line), footer);
    }
}

fn draw_file_picker(
    frame: &mut Frame<'_>,
    app: &App,
    dialog: &FilePickerDialog,
    prompt: Rect,
    viewport: PromptViewport,
    prefer_below: bool,
) {
    let Some((row, column)) = app
        .editor
        .display_position(dialog.query_range.start, viewport.field.width as usize)
    else {
        draw_completion_popup(
            frame,
            prompt,
            CompletionPopup {
                title: " files ",
                rows: vec![Line::from(Span::styled(
                    "invalid file picker position",
                    theme::bold(theme::error_color()),
                ))],
                selected: None,
                footer: Some(Line::from("esc close")),
                max_rows: 1,
                max_width: 88,
                prefer_below,
            },
        );
        return;
    };
    let trigger_x = viewport.field.x + u16::try_from(column).unwrap_or(0);
    let max_width = prompt.width.min(88).min(frame.area().width);
    let min_width = max_width.min(24);
    let available_width = frame.area().right().saturating_sub(trigger_x);
    let width = max_width.min(available_width.max(min_width));
    let x = trigger_x.min(frame.area().right().saturating_sub(width));
    let path_width = width.saturating_sub(4) as usize;
    let lines = match dialog.status {
        FilePickerStatus::Loading => {
            vec![Line::from(Span::styled(
                "indexing workspace files…",
                theme::dim(),
            ))]
        }
        FilePickerStatus::Ready if dialog.matches.is_empty() => {
            vec![Line::from(Span::styled("no matching files", theme::dim()))]
        }
        FilePickerStatus::Ready => dialog
            .matches
            .iter()
            .map(|item| {
                let path_style = theme::text();
                let mut spans = Vec::new();
                let mut end = 0;
                let mut used = 0;
                for (index, grapheme) in item.relative_path.grapheme_indices(true) {
                    let width = UnicodeWidthStr::width(grapheme);
                    if used + width > path_width {
                        break;
                    }
                    used += width;
                    end = index + grapheme.len();
                }
                let mut cursor = 0;
                for range in &item.match_byte_offsets {
                    let start = range.start.max(cursor).min(end);
                    let range_end = range.end.max(start).min(end);
                    if cursor < start {
                        spans.push(Span::styled(
                            item.relative_path[cursor..start].to_string(),
                            path_style,
                        ));
                    }
                    if start < range_end {
                        spans.push(Span::styled(
                            item.relative_path[start..range_end].to_string(),
                            theme::accent(),
                        ));
                    }
                    cursor = range_end;
                }
                if cursor < end {
                    spans.push(Span::styled(
                        item.relative_path[cursor..end].to_string(),
                        path_style,
                    ));
                }
                Line::from(spans)
            })
            .collect(),
    };
    let y = if row < viewport.first_row {
        if prefer_below {
            prompt.bottom().saturating_sub(1)
        } else {
            prompt.y
        }
    } else {
        let visible_row =
            (row - viewport.first_row).min(viewport.field.height.saturating_sub(1) as usize);
        viewport.field.y + u16::try_from(visible_row).unwrap_or(0)
    };
    let anchor = Rect::new(x, y, width, 1);
    draw_completion_popup(
        frame,
        anchor,
        CompletionPopup {
            title: " files ",
            rows: lines,
            selected: (dialog.status == FilePickerStatus::Ready && !dialog.matches.is_empty())
                .then_some(dialog.selected),
            footer: Some(Line::from(Span::styled(
                "↑/↓ navigate · tab complete · esc close",
                theme::dim(),
            ))),
            max_rows: 19,
            max_width: 88,
            prefer_below,
        },
    );
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
        let Some((index, _)) = tail.grapheme_indices(true).nth(1) else {
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
    let rename = app
        .session_dialog
        .as_ref()
        .and_then(|dialog| dialog.rename.as_ref());
    let prompt_rows = u16::from(rename.is_some());
    let height = outer.height.min(
        (app.session_choices.len() as u16)
            .saturating_add(3 + prompt_rows)
            .min(23),
    );
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
    let footer_rows = u16::from(inner.height > prompt_rows.saturating_add(1));
    let [list, prompt, footer] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(prompt_rows),
        Constraint::Length(footer_rows),
    ])
    .areas(inner);
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
            let detail = entry
                .preview
                .as_deref()
                .filter(|preview| *preview != label)
                .map_or_else(
                    || label.to_string(),
                    |preview| format!("{label} — {preview}"),
                );
            let updated = entry.updated_at_rfc3339();
            let text = format!(
                "{}{} · {} · {}",
                if is_selected { "› " } else { "  " },
                &updated[..10],
                entry.id,
                detail
            );
            Line::from(Span::styled(
                truncate_to_width(&text, list.width as usize),
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

    let footer_text = match rename {
        Some(SessionRename::Editing(_)) => "enter save · esc cancel",
        Some(SessionRename::ConfirmClear) => "enter clear name · esc cancel",
        Some(SessionRename::Saving) => "saving…",
        None => "↑/↓ select · enter resume · r rename · esc close",
    };
    frame.render_widget(
        Paragraph::new(Span::styled(footer_text, theme::dim())),
        footer,
    );

    match rename {
        Some(SessionRename::Editing(input)) if prompt.width > 0 => {
            let prefix = "rename: ";
            let prefix_width = UnicodeWidthStr::width(prefix).min(prompt.width as usize);
            let value = visible_query_tail(
                input,
                (prompt.width as usize)
                    .saturating_sub(prefix_width)
                    .saturating_sub(1),
            );
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(prefix, theme::dim()),
                    Span::styled(value.to_string(), theme::text()),
                ])),
                prompt,
            );
            let column = prefix_width + UnicodeWidthStr::width(value);
            frame.set_cursor_position(Position::new(
                prompt.x
                    + u16::try_from(column)
                        .unwrap_or(u16::MAX)
                        .min(prompt.width.saturating_sub(1)),
                prompt.y,
            ));
        }
        Some(SessionRename::ConfirmClear) => {
            frame.render_widget(
                Paragraph::new(Span::styled("Clear the custom name?", theme::text())),
                prompt,
            );
        }
        _ => {}
    }
}

fn draw_model_switch_dialog(frame: &mut Frame<'_>, pending: &super::app::ModelSwitch) {
    let outer = frame.area();
    let width = outer.width.min(76);
    let height = outer.height.min(12);
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let panel = Panel::bordered().title(" model context warning ");
    let inner = panel.inner(area);
    let mut lines = match &pending.warning {
        Some(warning) => vec![
            Line::from(format!(
                "{} via {}",
                pending.choice.model, pending.choice.provider
            )),
            Line::from(format!(
                "Estimated {} / {} tokens.",
                warning.guarded_tokens, warning.target_window
            )),
            Line::from("At least 80% of the target context is occupied."),
            Line::from("Compact uses the current model before switching."),
            Line::from(""),
        ],
        None => vec![
            Line::from(if pending.cancelling {
                "Cancelling model change…"
            } else {
                "Preparing model change…"
            }),
            Line::from("Esc / Ctrl+C cancels; input is preserved."),
        ],
    };
    if pending.warning.is_some() {
        for (index, label) in ["Continue anyway", "Compact", "Cancel"].iter().enumerate() {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} {label}",
                    if index == pending.selected {
                        "›"
                    } else {
                        " "
                    }
                ),
                if index == pending.selected {
                    theme::accent()
                } else {
                    theme::text()
                },
            )));
        }
        lines.push(Line::from("↑/↓ select · enter confirm · esc cancel"));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(panel, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_effort_dialog(frame: &mut Frame<'_>, app: &App, dialog: &EffortDialog) {
    let outer = frame.area();
    let width = outer.width.min(48);
    let height = outer.height.min(app.effort_choices.len() as u16 + 3);
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
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

fn draw_model_dialog(frame: &mut Frame<'_>, app: &App, dialog: &ModelDialog) {
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

fn cost_label(amount: f64, currency: &str) -> String {
    let prefix = if currency == "USD" {
        "$".to_owned()
    } else {
        format!("{currency} ")
    };
    if amount > 0.0 && amount < 0.0001 {
        format!("{prefix}<0.0001")
    } else {
        format!("{prefix}{amount:.4}")
    }
}

/// Ten-cell context gauge with the automatic compaction threshold marked.
fn gauge_spans(used: u64, size: u64) -> Vec<Span<'static>> {
    const CELLS: u64 = 10;
    const COMPACT_AT: u64 = 8;
    let filled = if size == 0 {
        0
    } else {
        (used.saturating_mul(CELLS) / size).min(CELLS)
    };
    let fill = Style::default().fg(if filled >= COMPACT_AT {
        theme::warn_color()
    } else {
        theme::running_color()
    });
    let mut spans = Vec::with_capacity(12);
    for cell in 0..CELLS {
        if cell == COMPACT_AT {
            spans.push(Span::styled("┆", theme::faint()));
        }
        if cell < filled {
            spans.push(Span::styled("▰", fill));
        } else {
            spans.push(Span::styled("▱", theme::faint()));
        }
    }
    spans
}

fn header_field(priority: u8, spans: Vec<Span<'static>>) -> (u8, Vec<Span<'static>>) {
    (priority, spans)
}

/// Where, then which model, then how much room. Lowest-priority fields drop
/// first rather than clipping a value into something misleading.
fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) -> Option<Rect> {
    let root = app.root.file_name().map_or_else(
        || app.root.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let prefix = vec![
        Span::styled(" kit", theme::bold(theme::accent_color())),
        Span::styled(" ▏ ", theme::faint()),
    ];
    let mut left = vec![header_field(9, vec![Span::styled(root, theme::text())])];
    if let Some(title) = app.session_title() {
        left.push(header_field(
            4,
            vec![
                Span::styled("▸ ", theme::faint()),
                Span::styled(truncate_to_width(&title, 48), theme::text()),
            ],
        ));
    }
    let mut right = vec![header_field(
        6,
        vec![
            Span::styled(app.model.clone(), theme::text()),
            Span::styled(" · ", theme::faint()),
            Span::styled(app.reasoning_effort.clone(), theme::dim()),
        ],
    )];
    if let Some(usage) = app.usage {
        let mut spans = gauge_spans(usage.used, usage.size);
        spans.push(Span::styled(
            format!(" {}", percent(usage.used, usage.size)),
            theme::dim(),
        ));
        spans.push(Span::styled(
            format!(" {}", compact(usage.used)),
            theme::text(),
        ));
        spans.push(Span::styled(
            format!("/{}", compact(usage.size)),
            theme::dim(),
        ));
        right.push(header_field(8, spans));
    }
    if let Some(cost) = &app.cost {
        right.push(header_field(
            5,
            vec![Span::styled(
                cost_label(cost.amount, &cost.currency),
                theme::text(),
            )],
        ));
    }
    let session_field = right.len();
    if let Some(id) = &app.session_id {
        right.push(header_field(
            3,
            vec![Span::styled(id.clone(), theme::dim())],
        ));
    }
    right.push(header_field(
        0,
        vec![Span::styled(format!("a2a {}", app.a2a), theme::faint())],
    ));

    let join = |fields: &[(u8, Vec<Span<'static>>)], separator: &str| {
        let mut spans = Vec::new();
        for (index, (_, field)) in fields.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(separator.to_owned(), theme::faint()));
            }
            spans.extend(field.iter().cloned());
        }
        spans
    };
    let width = |left: &[(u8, Vec<Span<'static>>)], right: &[(u8, Vec<Span<'static>>)]| {
        Line::from(prefix.clone()).width()
            + Line::from(join(left, " ")).width()
            + 2
            + Line::from(join(right, " ▏ ")).width()
            + 1
    };
    while width(&left, &right) > area.width as usize {
        let lowest = |fields: &[(u8, Vec<Span<'static>>)]| {
            fields
                .iter()
                .enumerate()
                .min_by_key(|(_, (priority, _))| *priority)
                .map(|(index, (priority, _))| (*priority, index))
        };
        match (lowest(&left), lowest(&right)) {
            (Some((l, li)), Some((r, ri))) => {
                if l <= r {
                    left.remove(li);
                } else {
                    right.remove(ri);
                }
            }
            (Some((_, li)), None) => {
                left.remove(li);
            }
            (None, Some((_, ri))) => {
                right.remove(ri);
            }
            (None, None) => break,
        }
    }
    let mut spans = prefix;
    spans.extend(join(&left, " "));
    let right_spans = join(&right, " ▏ ");
    let gap = (area.width as usize)
        .saturating_sub(
            Line::from(spans.clone()).width() + Line::from(right_spans.clone()).width() + 1,
        )
        .max(1);
    spans.push(Span::raw(" ".repeat(gap)));
    // The session id is a click target: where it starts is where the fields
    // before it end, if it survived the width fitting.
    let session_area = right
        .get(session_field)
        .filter(|(priority, _)| *priority == 3)
        .map(|(_, field)| {
            let before = Line::from(spans.clone()).width()
                + Line::from(join(&right[..session_field], " ▏ ")).width()
                + if session_field > 0 { 3 } else { 0 };
            Rect::new(
                area.x + u16::try_from(before).unwrap_or(u16::MAX),
                area.y,
                u16::try_from(Line::from(field.clone()).width()).unwrap_or(u16::MAX),
                1,
            )
        });
    spans.extend(right_spans);
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
    session_area
}

fn draw_start(
    frame: &mut Frame<'_>,
    app: &App,
    width: u16,
    prompt_rows: u16,
) -> (Rect, PromptViewport) {
    let area = frame.area();
    let height = START_LOGO_ROWS + START_LOGO_GAP + prompt_rows + 1;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let mut y = area.y + area.height.saturating_sub(height) / 2;

    let logo = Rect::new(x, y, width, START_LOGO_ROWS);
    frame.render_widget(welcome_logo(), logo);
    y += START_LOGO_ROWS + START_LOGO_GAP;

    let prompt = Rect::new(x, y, width, prompt_rows);
    let viewport = draw_start_prompt(frame, app, prompt);
    draw_command_popup(frame, app, prompt);
    let status = Rect::new(x, y + prompt_rows, width, 1);
    draw_status(frame, app, status);
    (prompt, viewport)
}

/// Narrow terminals keep the whole height for the transcript; the dock
/// summarises agents there instead of stacking the panel.
fn body_layout(area: Rect, show_agents: bool, transcript_empty: bool) -> (Rect, Option<Rect>) {
    if !show_agents || transcript_empty || area.width < SIDE_BY_SIDE_WIDTH {
        return (area, None);
    }
    let [transcript, agents] =
        Layout::horizontal([Constraint::Min(40), Constraint::Length(AGENTS_WIDTH)]).areas(area);
    (transcript, Some(agents))
}

fn draw_body(frame: &mut Frame<'_>, app: &mut App, images: &mut ImageRuntime, area: Rect) {
    let (transcript, agents) = body_layout(area, app.show_agents(), app.blocks.is_empty());
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
    let mut visible_images: Vec<(usize, usize, Option<String>, i16)> = Vec::new();
    let mut materialize = |row: &crate::tui::app::CachedTranscriptRow| {
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
            let span_start = app.transcript_prefixes[block_index];
            if span_start >= end {
                break;
            }
            if app.transcript_prefixes[block_index + 1] == span_start {
                block_index += 1;
                continue;
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
                            placement.block.unwrap_or(block_index),
                            placement.source,
                            placement.destination.clone(),
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
    for (block_index, source_index, destination, y) in visible_images {
        if let Some(destination) = destination {
            if let Some(image) =
                images.prepare_destination(&destination, &app.root, inner.width.max(1))
            {
                images.render(frame, image, inner, y);
            }
            continue;
        }
        if let Some(Block::AgentParts(parts)) = app.blocks.get(block_index) {
            if let Some(AgentPart::Image(source)) = parts.get(source_index)
                && let Some(image) = images.prepare_assistant(source, inner.width.max(1))
            {
                images.render(frame, image, inner, y);
            }
            continue;
        }
        let sources = match app.blocks.get(block_index) {
            Some(Block::User(message)) => &message.images,
            Some(Block::Tool(call)) => &call.images,
            _ => continue,
        };
        let Some(source) = sources.get(source_index) else {
            continue;
        };
        if let Some(image) = images.prepare_assistant(source, inner.width.max(1)) {
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

/// The block-glyph mark as plain text, for the exit message.
pub(super) fn banner_lines() -> [&'static str; 3] {
    ["█   ▀ ▄█▄", "█▄▀ █  █", "█▀▄ █  █▄  by Speakeasy"]
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
    let animated_owners: std::collections::BTreeSet<_> = app
        .transcript_dynamic
        .iter()
        .filter_map(|index| app.tool_owners.get(index).copied())
        .collect();
    app.transcript_dirty.extend(animated_owners.iter().copied());
    let dirty = std::mem::take(&mut app.transcript_dirty);
    let mut first_changed_count = app.blocks.len();
    for block_index in dirty {
        let dynamic = match &app.blocks[block_index] {
            Block::Thought { millis, .. } => millis.is_none(),
            Block::Tool(call) => call.running(),
            _ => false,
        };
        let revision = app.transcript_revisions[block_index];
        if !width_changed
            && !dynamic
            && !animated_owners.contains(&block_index)
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
        let (rows, cached_images) =
            transcript_block_rows(app, block_index, width, images.enabled());
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
        app.transcript_prefixes[index + 1] = app.transcript_prefixes[index]
            + rows
            + usize::from(rows > 0 && app.transcript_prefixes[index] > 0);
    }
    if layout_changed {
        app.clear_transcript_interaction();
    }
}

fn user_block_rows(
    message: &UserMessage,
    width: usize,
) -> (Vec<CachedTranscriptRow>, Vec<CachedTranscriptImage>) {
    let mut rows = Vec::new();
    for (line_index, text) in message.text.split('\n').enumerate() {
        rows.extend(wrap_linked_tagged(
            &[(
                user_line(text, line_index == 0),
                (None, None, Some(line_index)),
            )],
            width,
        ));
    }
    (rows, Vec::new())
}

/// Place viewports at complete image boundaries before wrapping following prose.
/// Preserve inline style context and keep tables intact as layout units.
fn agent_block_rows(
    text: &str,
    block_index: usize,
    width: usize,
    reserve_images: bool,
) -> (Vec<CachedTranscriptRow>, Vec<CachedTranscriptImage>) {
    let mut rows = Vec::new();
    let mut placements = Vec::new();
    if !reserve_images {
        append_agent_rows(&mut rows, text, 0, block_index, width);
        return (rows, placements);
    }
    let mut references = markdown::image_references(text).into_iter().peekable();
    for (line_index, (line, code, source)) in
        markdown::render_copyable_with_sources(text, Some(width), true)
            .into_iter()
            .enumerate()
    {
        let code = code.map(|range| CodeHit {
            block: block_index,
            range,
        });
        rows.extend(wrap_linked_tagged(
            &[(line, (None, code, Some(line_index)))],
            width,
        ));
        while references
            .peek()
            .is_some_and(|(range, _)| range.end <= source.end)
        {
            let Some((_, destination)) = references.next() else {
                break;
            };
            let row = rows.len();
            rows.extend((0..RESERVED_ROWS).map(|_| {
                (
                    Line::default(),
                    (None, None, None),
                    Vec::new(),
                    String::new(),
                )
            }));
            placements.push(CachedTranscriptImage {
                block: None,
                source: 0,
                row,
                destination: Some(destination),
            });
        }
    }
    (rows, placements)
}

fn append_agent_rows(
    rows: &mut Vec<CachedTranscriptRow>,
    text: &str,
    offset: usize,
    block: usize,
    width: usize,
) {
    let lines = markdown::render_copyable_at_width(text, Some(width))
        .into_iter()
        .enumerate()
        .map(|(line_index, (line, code))| {
            let code = code.map(|range| CodeHit {
                block,
                range: range.start + offset..range.end + offset,
            });
            (line, (None, code, Some(line_index)))
        })
        .collect::<Vec<TaggedTranscriptLine>>();
    rows.extend(wrap_linked_tagged(&lines, width));
}

fn agent_parts_rows(
    parts: &[AgentPart],
    block_index: usize,
    width: usize,
    reserve_images: bool,
) -> (Vec<CachedTranscriptRow>, Vec<CachedTranscriptImage>) {
    let mut rows = Vec::new();
    let mut placements = Vec::new();
    let mut text_offset = 0;
    for (source, part) in parts.iter().enumerate() {
        match part {
            AgentPart::Text(text) => {
                let (mut part_rows, part_images) =
                    agent_block_rows(text, block_index, width, reserve_images);
                for row in &mut part_rows {
                    if let Some(hit) = &mut row.1.1 {
                        hit.range.start += text_offset;
                        hit.range.end += text_offset;
                    }
                }
                placements.extend(part_images.into_iter().map(|mut image| {
                    image.row += rows.len();
                    image
                }));
                rows.extend(part_rows);
                text_offset += text.len();
            }
            AgentPart::Image(image) => {
                append_agent_rows(
                    &mut rows,
                    &format!("[Image: {}]", image.mime_type),
                    text_offset,
                    block_index,
                    width,
                );
                if reserve_images {
                    let row = rows.len();
                    rows.extend((0..RESERVED_ROWS).map(|_| {
                        (
                            Line::default(),
                            (None, None, None),
                            Vec::new(),
                            String::new(),
                        )
                    }));
                    placements.push(CachedTranscriptImage {
                        block: None,
                        source,
                        row,
                        destination: None,
                    });
                }
            }
        }
    }
    (rows, placements)
}

/// Canonical children keep their own selection tags and image sources. Only a
/// bounded page is materialized, before wrapping rows or reserving image cells.
fn transcript_block_rows(
    app: &App,
    block_index: usize,
    width: usize,
    reserve_images: bool,
) -> (Vec<CachedTranscriptRow>, Vec<CachedTranscriptImage>) {
    if app.tool_owners.contains_key(&block_index) {
        return (Vec::new(), Vec::new());
    }
    let (mut rows, mut images) =
        single_transcript_block_rows(app, block_index, width, reserve_images);
    let Block::Tool(call) = &app.blocks[block_index] else {
        return (rows, images);
    };
    let (children, start, total) = app.child_window(block_index);
    if total == 0 {
        return (rows, images);
    }
    // Lanes stay visible while the program runs; a finished program folds
    // them behind its call count until opened.
    let collapsed_by_user = call.expansion_explicit && !call.expanded;
    let show = (call.running() && !collapsed_by_user)
        || (call.expanded && call.compose_view == ComposeView::Output);
    if show {
        if total > children.len() {
            let label = format!(
                "   ┃ calls {}–{} of {total} · alt+PgUp/PgDn",
                start + 1,
                start + children.len()
            );
            rows.extend(wrap_linked_tagged(
                &[(
                    LinkedLine::plain(Line::from(Span::styled(label, theme::faint()))),
                    (Some(call.id.clone()), None, None),
                )],
                width,
            ));
        }
        for &child in children {
            let (mut child_rows, child_images) = single_transcript_block_rows(
                app,
                child,
                width.saturating_sub(3).max(1),
                reserve_images,
            );
            images.extend(child_images.into_iter().map(|mut image| {
                image.block = Some(child);
                image.row += rows.len();
                image
            }));
            for row in &mut child_rows {
                row.0.spans.insert(0, Span::styled("   ", theme::faint()));
            }
            rows.extend(child_rows);
        }
    }
    (rows, images)
}

fn single_transcript_block_rows(
    app: &App,
    block_index: usize,
    width: usize,
    reserve_images: bool,
) -> (Vec<CachedTranscriptRow>, Vec<CachedTranscriptImage>) {
    let block = &app.blocks[block_index];
    let (block_lines, call) = match block {
        Block::User(message) => return user_block_rows(message, width),
        Block::Agent(text) => return agent_block_rows(text, block_index, width, reserve_images),
        Block::AgentParts(parts) => {
            return agent_parts_rows(parts, block_index, width, reserve_images);
        }
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
                width,
                block_index,
            ))),
            Some(call.id.clone()),
        ),
        Block::TurnDuration {
            background,
            since_prompt,
            ..
        } => (
            uncopyable(plain_lines(vec![turn_end_line(*background, *since_prompt)])),
            None,
        ),
        Block::BackgroundResult {
            title,
            millis,
            failed,
        } => (
            uncopyable(plain_lines(vec![spread(
                vec![
                    Span::styled("↩ ", theme::faint()),
                    Span::styled(
                        truncate_to_width(title, width.saturating_sub(40).max(8)),
                        theme::bold(theme::text_color()),
                    ),
                ],
                vec![
                    Span::styled(
                        format!("background result · {}", theme::duration(*millis)),
                        theme::dim(),
                    ),
                    Span::styled(
                        if *failed { " failed" } else { "" },
                        Style::default().fg(theme::error_color()),
                    ),
                ],
                width,
            )])),
            None,
        ),
        Block::Compacted { reason, millis } => (
            uncopyable(plain_lines(vec![Line::from(vec![
                Span::styled("⇅ ", theme::faint()),
                Span::styled("compacted context", theme::bold(theme::text_color())),
                Span::styled(
                    format!(
                        "  {} · {}",
                        theme::duration(*millis),
                        if reason.contains("Threshold") {
                            "reached 80% of the window".to_owned()
                        } else {
                            reason.to_lowercase()
                        }
                    ),
                    theme::dim(),
                ),
            ])])),
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
    let lines = block_lines
        .into_iter()
        .enumerate()
        .map(|(line_index, (line, code))| {
            let code = code.map(|range| CodeHit {
                block: block_index,
                range,
            });
            (line, (call.clone(), code, Some(line_index)))
        })
        .collect::<Vec<TaggedTranscriptLine>>();
    let mut rows = wrap_linked_tagged(&lines, width);
    let mut placements = Vec::new();
    if let Block::Tool(call) = block
        && call.expanded
        && (!call.is_compose() || call.compose_view == ComposeView::Output)
    {
        for (source, image) in call.images.iter().enumerate() {
            rows.extend(wrap_linked_tagged(
                &[(
                    LinkedLine::plain(Line::from(Span::styled(
                        format!("   [Image: {}]", image.mime_type),
                        theme::dim(),
                    ))),
                    (Some(call.id.clone()), None, None),
                )],
                width,
            ));
            if !reserve_images {
                continue;
            }
            let row = rows.len();
            rows.extend((0..RESERVED_ROWS).map(|_| {
                (
                    Line::default(),
                    (Some(call.id.clone()), None, None),
                    Vec::new(),
                    String::new(),
                )
            }));
            placements.push(CachedTranscriptImage {
                block: None,
                source,
                row,
                destination: None,
            });
        }
    }
    (rows, placements)
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
        image_end: false,
    }];
    spans.extend(markdown::inline_spans(
        text,
        theme::bold(theme::text_color()),
    ));
    LinkedLine::new(spans).with_leading_gutter()
}

/// Left-aligned content with meta pushed to the right edge; falls back to a
/// plain run when the row is too narrow to spread.
fn spread(mut left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    if right.is_empty() {
        return Line::from(left);
    }
    let used = Line::from(left.clone()).width() + Line::from(right.clone()).width();
    let gap = if used + 2 <= width { width - used } else { 2 };
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    Line::from(left)
}

/// A turn's end: wall time since the user's prompt, and whether work is
/// still running in the background. Turns restart on their own after a
/// detached result lands, so per-turn figures would say little; the clock
/// keeps running until nothing is left in the background.
fn turn_end_line(background: usize, since_prompt: u64) -> Line<'static> {
    let mut spans = vec![Span::styled("· ", theme::faint())];
    if background > 0 {
        spans.push(Span::styled("◔ ", Style::default().fg(theme::warn_color())));
        spans.push(Span::styled(
            format!(
                "{background} in background · {} so far",
                theme::duration(since_prompt)
            ),
            theme::faint(),
        ));
    } else {
        spans.push(Span::styled(theme::duration(since_prompt), theme::faint()));
    }
    Line::from(spans)
}

/// The most recent reasoning heading, as the fold's label.
fn thought_heading(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(|line| line.trim().trim_matches('*').trim())
        .find(|line| !line.is_empty())
        .map(|line| truncate_to_width(line, 72))
}

fn thought_lines(
    app: &App,
    text: &str,
    running_millis: u128,
    millis: Option<u64>,
) -> Vec<Line<'static>> {
    let elapsed = millis.unwrap_or(u64::try_from(running_millis).unwrap_or(u64::MAX));
    if !app.show_thoughts {
        return vec![if millis.is_some() {
            Line::from(vec![
                Span::styled("⋮ ", theme::faint()),
                Span::styled(
                    format!("thought {} · ^t", theme::duration(elapsed)),
                    theme::faint(),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled("⋮ ", Style::default().fg(theme::running_color())),
                Span::styled("thinking", theme::dim()),
                Span::styled(
                    thought_heading(text).map_or(String::new(), |heading| format!(" · {heading}")),
                    theme::text(),
                ),
                Span::styled(format!(" · {}", theme::duration(elapsed)), theme::dim()),
            ])
        }];
    }
    let style = theme::dim().add_modifier(Modifier::ITALIC);
    let all: Vec<&str> = text.split('\n').collect();
    let shown = all.as_slice();
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

fn tool_lines(
    app: &App,
    call: &ToolCall,
    active: bool,
    width: usize,
    block_index: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![tool_header(app, call, active, width, block_index)];
    let compose = call.is_compose();
    if call.running() {
        if compose
            && ((call.expanded && call.compose_view == ComposeView::Script)
                || (call.parent_id.is_none()
                    && !app.has_grouped_tools(&call.id)
                    && !call.script.is_empty()))
        {
            lines.extend(script_lines(call));
        }
        // A running program's own output is a placeholder until it finishes;
        // its lanes say more, unless the user asked for the output.
        if call.expanded && (call.expansion_explicit || !app.has_grouped_tools(&call.id)) {
            lines.extend(expanded_output_lines(call));
        }
        return lines;
    }
    if compose {
        lines.extend(completed_compose_lines(call));
    } else if call.expanded {
        lines.extend(expanded_output_lines(call));
    }
    lines
}

/// Where a nested call sits on its program's clock.
struct LaneClock {
    started: std::time::Instant,
    total_millis: u64,
}

fn lane_clock(app: &App, call: &ToolCall) -> Option<LaneClock> {
    let parent = app.tool_call(call.parent_id.as_deref()?)?;
    Some(LaneClock {
        started: parent.started,
        total_millis: parent.elapsed().max(1),
    })
}

/// A bar on the program's clock: where the call started, how long it ran,
/// and whether it is still going.
fn lane_bar(call: &ToolCall, clock: &LaneClock, track: usize) -> Vec<Span<'static>> {
    let offset = u64::try_from(
        call.started
            .saturating_duration_since(clock.started)
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    let end = offset.saturating_add(call.elapsed());
    let total = clock.total_millis.max(end).max(1);
    let cell = |millis: u64| ((millis as f64 / total as f64) * track as f64) as usize;
    let mut start = cell(offset).min(track.saturating_sub(2));
    let mut stop = if call.running() {
        track
    } else {
        cell(end).max(start + 2).min(track)
    };
    if stop - start < 2 {
        start = stop.saturating_sub(2);
        stop = start + 2;
    }
    let (style, cap) = if call.running() {
        (Style::default().fg(theme::running_color()), "▶")
    } else if call.status == ToolCallStatus::Failed {
        (Style::default().fg(theme::error_color()), "╴")
    } else {
        (Style::default().fg(theme::success_color()), "╴")
    };
    vec![
        Span::raw(" ".repeat(start)),
        Span::styled(format!("╶{}{cap}", "━".repeat(stop - start - 2)), style),
        Span::raw(" ".repeat(track - stop)),
    ]
}

/// The one-word kind of a nested call, from the runtime's canonical title.
fn lane_kind(call: &ToolCall) -> (&'static str, bool) {
    let kind = match call.title.as_str() {
        "Running shell command" => "shell",
        "Editing file" => "edit",
        "Reading image" => "read",
        "Searching available tools" => "tool_search",
        "Fetching Kit documentation" => "docs",
        "Starting subagent" => "subagent",
        "Prompting subagent" => "prompt",
        "Forking session" => "fork",
        "Calling tool" => "tool",
        _ => return ("", false),
    };
    (kind, true)
}

/// A nested call as one lane of its program.
fn lane_line(
    app: &App,
    call: &ToolCall,
    active: bool,
    width: usize,
    clock: Option<LaneClock>,
) -> Line<'static> {
    const TRACK: usize = 16;
    let (glyph, style) = status_glyph(app, call);
    let (kind, canonical) = lane_kind(call);
    let mut left = vec![
        Span::styled("┃ ", theme::faint()),
        Span::styled(format!("{glyph} "), style),
    ];
    let title_style = theme::bold(if active {
        theme::accent_color()
    } else {
        theme::text_color()
    });
    if canonical {
        left.push(Span::styled(format!("{kind:<11}"), title_style));
    } else {
        left.push(Span::styled(
            format!("{:<11}", kind_label(&call.kind).trim()),
            theme::dim(),
        ));
        left.push(Span::styled(call.display_title().to_string(), title_style));
    }
    let mut right = Vec::new();
    if let Some(clock) = clock.filter(|_| width >= 56) {
        right.extend(lane_bar(call, &clock, TRACK));
        right.push(Span::raw("  "));
    }
    right.push(Span::styled(theme::duration(call.elapsed()), theme::dim()));
    if !call.running() && call.status == ToolCallStatus::Failed {
        right.push(Span::styled(
            " failed",
            Style::default().fg(theme::error_color()),
        ));
    }
    if !call.output.is_empty() && !call.expanded {
        right.push(Span::styled(
            format!(" ▸ {}", call.output.len()),
            theme::faint(),
        ));
    }
    spread(left, right, width)
}

fn status_glyph(app: &App, call: &ToolCall) -> (String, Style) {
    match call.status {
        _ if call.running() => (
            theme::pulse(theme::Pulse::Tool, app.tick).to_string(),
            theme::bold(theme::running_color()),
        ),
        ToolCallStatus::Failed => ("✗".into(), theme::bold(theme::error_color())),
        _ => ("✓".into(), theme::bold(theme::success_color())),
    }
}

/// Source lines stay neutral; ACP owns tool lifecycle display.
fn script_lines(call: &ToolCall) -> Vec<Line<'static>> {
    let mut lines: Vec<_> = call
        .script
        .lines()
        .take(MAX_OUTPUT_ROWS)
        .map(|source| {
            let spans = vec![
                Span::styled("   ┃ ", theme::faint()),
                Span::styled(source.to_string(), theme::dim()),
            ];
            Line::from(spans)
        })
        .collect();
    let count = call.script.lines().count();
    if count > MAX_OUTPUT_ROWS {
        lines.push(Line::from(Span::styled(
            format!("   ┃ … {} more lines", count - MAX_OUTPUT_ROWS),
            theme::faint(),
        )));
    }
    lines
}

/// The output/script view chips for an opened program.
fn view_chips(view: ComposeView) -> Vec<Span<'static>> {
    let chip = |label: &'static str, selected: bool| {
        if selected {
            Span::styled(format!("[{label}]"), theme::bold(theme::accent_color()))
        } else {
            Span::styled(format!(" {label} "), theme::faint())
        }
    };
    vec![
        chip("output", view == ComposeView::Output),
        chip("script", view == ComposeView::Script),
    ]
}

fn completed_compose_lines(call: &ToolCall) -> Vec<Line<'static>> {
    if !call.expanded {
        return Vec::new();
    }
    let count = call.output.len();
    let mut left = vec![Span::styled("   ┃ ▾ ", theme::dim())];
    left.push(Span::styled(
        match call.compose_view {
            ComposeView::Output => format!("output · {count} {}", plural("line", count)),
            ComposeView::Script => "script".to_owned(),
        },
        theme::dim(),
    ));
    left.push(Span::raw("   "));
    left.extend(view_chips(call.compose_view));
    let mut lines = vec![Line::from(left)];
    match call.compose_view {
        ComposeView::Output => lines.extend(output_body(call)),
        ComposeView::Script => lines.extend(script_lines(call)),
    }
    lines
}

/// Raw tool output stays folded behind the header's line count: it is
/// machine-shaped, often thousands of lines, and unreadable inline. A click
/// or `^o` opens it.
fn expanded_output_lines(call: &ToolCall) -> Vec<Line<'static>> {
    if call.output.is_empty() {
        return Vec::new();
    }
    let count = call.output.len();
    let mut lines = vec![Line::from(vec![
        Span::styled("   ┃ ▾ ", theme::dim()),
        Span::styled(
            format!("output · {count} {}", plural("line", count)),
            theme::dim(),
        ),
    ])];
    lines.extend(output_body(call));
    lines
}

fn output_body(call: &ToolCall) -> Vec<Line<'static>> {
    let count = call.output.len();
    let mut lines: Vec<_> = call
        .output
        .iter()
        .take(MAX_OUTPUT_ROWS)
        .map(|line| {
            Line::from(vec![
                Span::styled("   ┃ ", theme::faint()),
                Span::styled(line.clone(), theme::dim()),
            ])
        })
        .collect();
    if count > MAX_OUTPUT_ROWS {
        lines.push(Line::from(Span::styled(
            format!("   ┃ … {} more lines", count - MAX_OUTPUT_ROWS),
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

/// Glyph and title on the left; counts, timing, and the output fold on the
/// right. Nested calls render as lanes of their program instead.
fn tool_header(
    app: &App,
    call: &ToolCall,
    active: bool,
    width: usize,
    block_index: usize,
) -> Line<'static> {
    if call.parent_id.is_some() {
        return lane_line(app, call, active, width, lane_clock(app, call));
    }
    let (glyph, style) = status_glyph(app, call);
    let mut left = vec![Span::styled(format!("{glyph} "), style)];
    let kind = if call.is_compose() {
        ""
    } else {
        kind_label(&call.kind)
    };
    let mut meta = Vec::new();
    let (_, _, calls) = app.child_window(block_index);
    if calls > 0 {
        meta.push(format!("{calls} {}", plural("call", calls)));
    }
    meta.push(theme::duration(call.elapsed()));
    let mut right = Vec::new();
    if call.backgrounded {
        if call.running() {
            right.push(Span::styled("◔ ", Style::default().fg(theme::warn_color())));
            right.push(Span::styled("background · ", theme::dim()));
        } else {
            right.push(Span::styled("↩ ", theme::faint()));
            right.push(Span::styled("background · ".to_owned(), theme::dim()));
        }
    }
    right.push(Span::styled(meta.join(" · "), theme::dim()));
    if !call.running() && call.status == ToolCallStatus::Failed {
        right.push(Span::styled(
            " failed",
            Style::default().fg(theme::error_color()),
        ));
    }
    if !call.output.is_empty() && !call.expanded {
        let count = call.output.len();
        right.push(Span::styled(
            format!(" ▸ {count} {}", plural("line", count)),
            theme::faint(),
        ));
    }
    // The title gives way before the meta wraps onto a second row.
    let room = width
        .saturating_sub(2 + UnicodeWidthStr::width(kind) + 2 + Line::from(right.clone()).width())
        .max(8);
    left.push(Span::styled(
        truncate_to_width(call.display_title(), room),
        theme::bold(if active {
            theme::accent_color()
        } else {
            theme::text_color()
        }),
    ));
    if !kind.is_empty() {
        left.push(Span::styled(kind.to_string(), theme::faint()));
    }
    spread(left, right, width)
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

fn working_line(app: &App) -> Line<'static> {
    let label = match app.phase {
        Phase::Cancelling => "stopping",
        _ if app.compacting => "compacting context",
        _ if app
            .focus_call()
            .is_some_and(|call| call.running() && !call.backgrounded) =>
        {
            "running tools"
        }
        _ => "thinking",
    };
    Line::from(vec![
        Span::styled(
            format!("{} ", theme::pulse(theme::Pulse::Turn, app.tick)),
            theme::bold(theme::accent_color()),
        ),
        Span::styled(label.to_string(), theme::accent()),
        Span::styled(
            format!(" · {}", theme::duration(app.elapsed())),
            theme::dim(),
        ),
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
    for grapheme in text.graphemes(true) {
        let previous_len = truncated.len();
        truncated.push_str(grapheme);
        if UnicodeWidthStr::width(truncated.as_str()) > content_width {
            truncated.truncate(previous_len);
            break;
        }
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
    show_vendor: bool,
    tick: usize,
    now_unix_ms: u64,
    width: usize,
) -> [Line<'static>; 3] {
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
    let mut first = vec![
        Span::styled(first_prefix.clone(), theme::faint()),
        Span::styled(format!("{glyph} "), glyph_style),
    ];
    if show_vendor {
        let (mark, mark_style) = theme::vendor_mark(row.vendor);
        first.push(Span::styled(format!("{mark} "), mark_style));
    }
    // Identity on the right: which harness and model, and which generation
    // the model can continue or fork.
    let mut identity = Vec::new();
    let harness = row.harness.trim_start_matches("acp.");
    if !harness.is_empty() && (harness != "kit" || show_vendor) {
        identity.push(harness.to_owned());
    }
    if let Some(model) = row.model.as_deref().filter(|model| !model.is_empty()) {
        identity.push(model.to_owned());
    }
    let generation = truncate_to_width(&format!("g{}", row.generation), width);
    let generation_width = UnicodeWidthStr::width(generation.as_str());
    // Reserve generation and the inter-column gap before budgeting either side.
    let gap = width.saturating_sub(generation_width).min(2);
    let mut prefix_room = width.saturating_sub(generation_width + gap);
    for span in &mut first {
        span.content = truncate_to_width(&span.content, prefix_room).into();
        prefix_room = prefix_room.saturating_sub(span.width());
    }
    let prefix_width = Line::from(first.clone()).width();
    let identity_room = width.saturating_sub(prefix_width + 4 + gap + generation_width + 2);
    let identity = truncate_to_width(&identity.join(" · "), identity_room);
    let mut right = Vec::new();
    if !identity.is_empty() {
        right.push(Span::styled(identity, theme::faint()));
        right.push(Span::raw("  "));
    }
    right.push(Span::styled(generation, theme::faint()));
    let name_room = width.saturating_sub(prefix_width + Line::from(right.clone()).width() + gap);
    first.push(Span::styled(
        truncate_to_width(&format!("{}{ancestry}", row.name), name_room),
        theme::text(),
    ));
    let used = Line::from(first.clone()).width() + Line::from(right.clone()).width();
    first.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    first.extend(right);
    let first = Line::from(first);

    let finished = row.generation_finished_at_unix_ms.unwrap_or(now_unix_ms);
    let elapsed = agent_duration(finished.saturating_sub(row.generation_started_at_unix_ms));
    let second_prefix = truncate_to_width(&second_prefix, width);
    let prefix_width = UnicodeWidthStr::width(second_prefix.as_str());
    let idle = matches!(row.status, SubagentStatus::Idle) && !failed;
    let excerpt = if idle {
        "idle · resumable".to_owned()
    } else if failed {
        "failed".to_owned()
    } else {
        row.excerpt()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let second = Line::from(vec![
        Span::styled(second_prefix.clone(), theme::faint()),
        Span::styled(
            truncate_to_width(&excerpt, width.saturating_sub(prefix_width)),
            if failed {
                Style::default().fg(theme::error_color())
            } else {
                theme::dim()
            },
        ),
    ]);
    let mut third = vec![Span::styled(second_prefix, theme::faint())];
    let mut tail = Vec::new();
    if let Some(usage) = row.usage {
        third.extend(agent_gauge(usage.used, usage.size));
        tail.push(compact(usage.used));
    }
    if !idle {
        tail.push(elapsed);
    }
    if let Some(cost) = &row.cost {
        tail.push(cost_label(cost.amount, &cost.currency));
    }
    let used = Line::from(third.clone()).width();
    third.push(Span::styled(
        truncate_to_width(&tail.join(" · "), width.saturating_sub(used)),
        theme::faint(),
    ));
    [first, second, Line::from(third)]
}

/// Six-cell context gauge for an agent row, followed by a space.
fn agent_gauge(used: u64, size: u64) -> Vec<Span<'static>> {
    const CELLS: u64 = 6;
    let filled = if size == 0 {
        0
    } else {
        (used.saturating_mul(CELLS) / size).min(CELLS)
    };
    let fill = Style::default().fg(if used.saturating_mul(10) >= size.saturating_mul(8) {
        theme::warn_color()
    } else {
        theme::running_color()
    });
    vec![
        Span::styled("▰".repeat(filled as usize), fill),
        Span::styled("▱".repeat((CELLS - filled) as usize), theme::faint()),
        Span::raw(" "),
    ]
}

fn draw_agents(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let block = Panel::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::faint())
        .title(Span::styled(" agents ", theme::accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let row_area_height = inner.height.saturating_sub(1);
    let visible_rows = usize::from(row_area_height / 3);
    app.set_agents_viewport(area, visible_rows);
    let now = crate::events::now_millis();
    let show_vendor = !app.agents_all_kit();
    let lines = app
        .agent_tree_rows()
        .into_iter()
        .skip(app.agents_scroll())
        .take(visible_rows)
        .flat_map(|row| agent_lines(&row, show_vendor, app.tick, now, inner.width as usize))
        .collect::<Vec<_>>();
    let rows_area = Rect {
        height: row_area_height,
        ..inner
    };
    frame.render_widget(Paragraph::new(lines), rows_area);

    if inner.height > 0 {
        let counts = app.agent_counts();
        let mut parts = vec![format!(
            "{} {}",
            counts.total,
            plural("agent", counts.total)
        )];
        if counts.starting > 0 {
            parts.push(format!("{} starting", counts.starting));
        }
        if counts.working > 0 {
            parts.push(format!("{} working", counts.working));
        }
        if counts.idle > 0 {
            parts.push(format!("{} idle", counts.idle));
        }
        let costs = app
            .subagent_cost_totals()
            .into_iter()
            .map(|(currency, amount)| cost_label(amount, &currency))
            .collect::<Vec<_>>();
        let footer = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(spread(
                vec![Span::styled(parts.join(" · "), theme::faint())],
                if costs.is_empty() {
                    Vec::new()
                } else {
                    vec![Span::styled(costs.join(" + "), theme::dim())]
                },
                inner.width as usize,
            )),
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

/// Whether the dock shows an agents summary: the panel is hidden by width.
fn dock_agents_row(app: &App, narrow: bool) -> bool {
    narrow && app.show_agents() && app.agent_counts().total > 0
}

/// Content rows the dock may take before it windows its entries.
const MAX_DOCK_ROWS: usize = 4;

fn dock_entry_count(app: &App, narrow: bool) -> usize {
    app.background_calls().len()
        + usize::from(dock_agents_row(app, narrow))
        + app.pending_steers.len()
}

/// Rows for everything alive outside the current turn, plus a hairline.
fn dock_rows(app: &App, narrow: bool) -> usize {
    let rows = dock_entry_count(app, narrow).min(MAX_DOCK_ROWS);
    if rows == 0 { 0 } else { rows + 1 }
}

/// Background programs, an agents summary when the panel cannot fit, and
/// steers waiting for injection sit above the composer so nothing that is
/// still running can scroll away. The dock is capped; when it overflows it
/// windows around the selected entry and says how many rows are hidden.
fn draw_dock(frame: &mut Frame<'_>, app: &App, area: Rect, narrow: bool) {
    let visible = area.height as usize;
    if visible == 0 {
        return;
    }
    let width = area.width as usize;
    let mut entries: Vec<(Line<'static>, bool)> = Vec::new();
    for call in app.background_calls() {
        let focused = app.focus_call().is_some_and(|focus| focus.id == call.id);
        let mut right = vec![Span::styled(theme::duration(call.elapsed()), theme::dim())];
        if focused {
            right.push(Span::styled("   ^k stop", theme::faint()));
        }
        right.push(Span::raw(" "));
        let room = width
            .saturating_sub(15 + Line::from(right.clone()).width() + 2)
            .max(8);
        let left = vec![
            Span::styled(" ◔ ", Style::default().fg(theme::warn_color())),
            Span::styled("background  ", theme::dim()),
            Span::styled(
                truncate_to_width(call.display_title(), room),
                theme::bold(if focused {
                    theme::accent_color()
                } else {
                    theme::text_color()
                }),
            ),
        ];
        // While the queue has focus the selected steer is what keys act on.
        entries.push((spread(left, right, width), focused && !app.queue_focused));
    }
    if dock_agents_row(app, narrow) {
        let counts = app.agent_counts();
        let active = counts.starting + counts.working;
        let (glyph, style) = if active > 0 {
            (
                theme::pulse(theme::Pulse::Child, app.tick),
                Style::default().fg(theme::running_color()),
            )
        } else {
            ("○", theme::dim())
        };
        let mut parts = vec![format!(
            "{} {}",
            counts.total,
            plural("agent", counts.total)
        )];
        if counts.working > 0 {
            parts.push(format!("{} working", counts.working));
        }
        if counts.starting > 0 {
            parts.push(format!("{} starting", counts.starting));
        }
        if counts.idle > 0 {
            parts.push(format!("{} idle", counts.idle));
        }
        entries.push((
            spread(
                vec![
                    Span::styled(format!(" {glyph} "), style),
                    Span::styled("agents  ", theme::dim()),
                    Span::styled(parts.join(" · "), theme::text()),
                ],
                vec![Span::styled("^r panel ", theme::faint())],
                width,
            ),
            false,
        ));
    }
    entries.extend(pending_steer_lines(app, width));

    let mut lines = vec![Line::from(Span::styled("╌".repeat(width), theme::faint()))];
    let cap = visible.saturating_sub(1);
    let total = entries.len();
    if total <= cap {
        lines.extend(entries.into_iter().map(|(line, _)| line));
    } else if cap > 0 {
        // Keep the selected entry on screen; otherwise favour the newest,
        // which is what the keys act on. One row says what is hidden.
        let selected = entries
            .iter()
            .position(|(_, selected)| *selected)
            .unwrap_or(total - 1);
        let start = selected.saturating_sub(cap - 1).min(total - cap);
        let end = start + cap;
        let hidden = total - cap + usize::from(cap > 1);
        let indicator = Line::from(Span::styled(
            format!("   … {hidden} more in the dock"),
            theme::faint(),
        ));
        let mut window: Vec<Line<'static>> = entries
            .into_iter()
            .skip(start)
            .take(cap)
            .map(|(line, _)| line)
            .collect();
        if cap > 1 {
            let slot = if selected == end - 1 { 0 } else { cap - 1 };
            window[slot] = indicator;
        }
        lines.extend(window);
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn pending_steer_lines(app: &App, width: usize) -> Vec<(Line<'static>, bool)> {
    let selected = app
        .pending_steers
        .iter()
        .position(|pending| app.selected_steer.as_deref() == Some(pending.id.as_str()));
    app.pending_steers
        .iter()
        .enumerate()
        .map(|(index, pending)| {
            let text = pending
                .text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let is_selected = selected == Some(index);
            let left = vec![
                Span::styled(
                    if is_selected { " ▶ " } else { " ⇥ " },
                    theme::bold(theme::user_color()),
                ),
                Span::styled(format!("steer {}  ", index + 1), theme::dim()),
                Span::styled(text, theme::bold(theme::text_color())),
            ];
            let right = vec![Span::styled(
                if is_selected {
                    format!("pending [{}/{}] ", index + 1, app.pending_steers.len())
                } else if app.queue_focused {
                    "pending ".to_owned()
                } else {
                    "pending · f2 ".to_owned()
                },
                theme::faint(),
            )];
            (spread(left, right, width), is_selected)
        })
        .collect()
}

fn draw_start_prompt(frame: &mut Frame<'_>, app: &App, area: Rect) -> PromptViewport {
    let inner = draw_start_prompt_frame(frame, area);
    let [input, _, metadata] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let viewport = draw_prompt_editor(frame, app, input, theme::dim());

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
    viewport
}

fn draw_command_popup(frame: &mut Frame<'_>, app: &App, anchor: Rect) {
    let commands = app.command_completions();
    let rows = commands
        .iter()
        .map(|command| {
            Line::from(vec![
                Span::styled(command.name.clone(), theme::accent()),
                Span::styled("  ".to_string(), theme::text()),
                Span::styled(command.description.clone(), theme::faint()),
            ])
        })
        .collect();
    draw_completion_popup(
        frame,
        anchor,
        CompletionPopup {
            title: " commands ",
            rows,
            selected: Some(app.command_completion_selected),
            footer: None,
            max_rows: 7,
            max_width: u16::MAX,
            prefer_below: false,
        },
    );
}

fn draw_prompt(frame: &mut Frame<'_>, app: &App, area: Rect) -> PromptViewport {
    let border = if app.phase == Phase::Working && app.can_steer || app.phase == Phase::Idle {
        Style::default().fg(theme::accent_color())
    } else {
        theme::faint()
    };
    let title = if app.editing_steer() {
        Line::from(Span::styled(
            " editing pending · Enter save · Esc cancel ",
            theme::dim(),
        ))
    } else if app.phase == Phase::Working && app.can_steer {
        Line::from(Span::styled(" steer ", theme::accent()))
    } else if app.phase == Phase::Idle {
        Line::from(Span::styled(" message ", theme::faint()))
    } else {
        Line::default()
    };
    let block = Panel::bordered()
        .title(title)
        .border_type(BorderType::Rounded)
        .border_style(border);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    draw_prompt_editor(frame, app, inner, theme::faint())
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

fn draw_prompt_editor(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    placeholder_style: Style,
) -> PromptViewport {
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
        prompt_lines(
            rows,
            app.editor.text(),
            &app.available_commands,
            !app.auth_methods.is_empty(),
        )
        .into_iter()
        .skip(first)
        .take(height)
        .collect()
    };
    frame.render_widget(Paragraph::new(lines), field);
    if !app.queue_focused {
        frame.set_cursor_position(Position::new(
            field.x
                + u16::try_from(cursor_column)
                    .unwrap_or(0)
                    .min(field.width.saturating_sub(1)),
            field.y + u16::try_from(cursor_row - first).unwrap_or(0),
        ));
    }
    PromptViewport {
        field,
        first_row: first,
    }
}

fn prompt_lines(
    rows: Vec<String>,
    input: &str,
    available_commands: &[command::Command],
    login_available: bool,
) -> Vec<Line<'static>> {
    let mut highlights = command::known_token(input, available_commands, login_available)
        .into_iter()
        .collect::<Vec<_>>();
    highlights.extend(input.char_indices().filter_map(|(start, character)| {
        if character != '@'
            || start > 0
                && !input[..start]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace)
        {
            return None;
        }
        let end = input[start..]
            .char_indices()
            .skip(1)
            .find_map(|(offset, character)| character.is_whitespace().then_some(start + offset))
            .unwrap_or(input.len());
        Some(start..end)
    }));
    let mut offset = 0;
    rows.into_iter()
        .map(|row| {
            let start = input[offset..].find(&row).map_or(offset, |at| offset + at);
            let end = start + row.len();
            offset = end;
            let mut spans = Vec::new();
            let mut cursor = start;
            for range in highlights
                .iter()
                .filter(|range| range.start < end && range.end > start)
            {
                let highlighted_start = range.start.max(start).max(cursor);
                let highlighted_end = range.end.min(end);
                if cursor < highlighted_start {
                    spans.push(Span::styled(
                        input[cursor..highlighted_start].to_string(),
                        theme::text(),
                    ));
                }
                if highlighted_start < highlighted_end {
                    spans.push(Span::styled(
                        input[highlighted_start..highlighted_end].to_string(),
                        theme::accent(),
                    ));
                }
                cursor = highlighted_end;
            }
            if cursor < end {
                spans.push(Span::styled(input[cursor..end].to_string(), theme::text()));
            }
            Line::from(spans)
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
            Span::styled("working", theme::bold(theme::accent_color())),
            Span::styled(format!(" {}", theme::duration(app.elapsed())), theme::dim()),
        ],
    };
    let counts = app.agent_counts();
    let active_agents = counts.starting + counts.working;
    if active_agents > 0 {
        left.push(Span::styled(
            format!(" · {active_agents} {}", plural("agent", active_agents)),
            theme::dim(),
        ));
    }
    let background = app.background_calls().len();
    if background > 0 {
        left.push(Span::styled(
            format!(" · {background} background"),
            theme::dim(),
        ));
    }
    if let Some(toast) = app.toast_text() {
        left.push(Span::styled("  ", theme::dim()));
        left.push(Span::styled(
            toast.to_string(),
            Style::default().fg(theme::warn_color()),
        ));
    }
    let hints = if app.editing_steer() {
        "⏎ save edit   esc restore draft "
    } else if app.queue_focused && !app.pending_steers.is_empty() {
        if app.can_replace_steer
            && app.pending_steers.iter().any(|pending| {
                app.selected_steer.as_deref() == Some(pending.id.as_str()) && pending.editable
            })
        {
            "↑/↓ select   ⏎ edit   ⌫/del remove   esc back "
        } else {
            "↑/↓ select   ⌫/del remove   esc back · edit unavailable "
        }
    } else if app.queue_handoff {
        "queue closed · type / ←→ / esc to continue "
    } else if app.phase == Phase::Working {
        let stop = app
            .focus_call()
            .is_some_and(|call| call.backgrounded && call.running());
        let mut keys = Vec::new();
        if app.can_steer {
            keys.push("⏎ steer");
        }
        if !app.pending_steers.is_empty() {
            keys.push("F2 queue");
        }
        keys.push("esc interrupt");
        if stop {
            keys.push("^k stop");
        }
        keys.extend(["⌘b background", "^t reasoning", "^r agents"]);
        &format!("{} ", keys.join("   "))
    } else if !app.pending_steers.is_empty() {
        "F2 queue   ⏎ send   ⇧⏎ newline "
    } else if app.phase == Phase::Idle
        && app
            .focus_call()
            .is_some_and(|call| call.backgrounded && call.running())
    {
        "⏎ send   ^k stop   ^r agents   ^t reasoning   ^l log "
    } else {
        "⏎ send   ⇧⏎ newline   ^r agents   ^t reasoning   ^l log   ^c quit "
    };
    let used: usize = left.iter().map(|span| span.content.chars().count()).sum();

    let hint_width = hints.chars().count();
    let hints = if used + hint_width + 1 > area.width as usize {
        // Drop hints from the right until the phase stays readable.
        let mut trimmed = hints.trim_end().to_owned();
        while used + trimmed.chars().count() + 2 > area.width as usize
            && let Some(cut) = trimmed.rfind("   ")
        {
            trimmed.truncate(cut);
        }
        format!("{trimmed} ")
    } else {
        hints.to_owned()
    };
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use agent_client_protocol::schema::v2::{
        IdleStateUpdate, RunningStateUpdate, StateUpdate, StopReason,
    };
    use std::{
        cell::RefCell,
        io::{self, Write},
        path::PathBuf,
        rc::Rc,
        time::Duration,
    };

    use agent_client_protocol::schema::v2::ToolKind;
    use base64::Engine as _;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::{
        Terminal,
        backend::{CrosstermBackend, TestBackend},
    };
    use ratatui_image::picker::Picker;
    use unicode_width::UnicodeWidthStr;

    use super::{
        ImageRuntime, MAX_PROMPT_ROWS, ModelDialogRow, agent_lines, body_layout, draw, draw_agents,
        model_dialog_rows, model_dialog_viewport, native_links_obscured, prompt_lines,
        refresh_transcript_cache_with_images, truncate_to_width, user_block_rows, user_line,
    };
    use crate::{
        events::{GenerationOutcome, RuntimeEvent, SubagentStatus},
        file_search::FileMatch,
        tui::{
            app::{
                Action, AgentRow, AgentTreeRow, App, Block, EffortChoice, EffortDialog,
                FilePickerDialog, FilePickerStatus, ModelDialog, Phase, SessionDialog,
                SessionRename, Update, UserImage, UserMessage,
            },
            hyperlinks::HyperlinkRenderer,
        },
    };

    #[derive(Clone, Default)]
    struct Capture(Rc<RefCell<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn bytes(&self) -> Vec<u8> {
            self.0.borrow().clone()
        }

        fn clear(&self) {
            self.0.borrow_mut().clear();
        }
    }

    fn refresh_transcript_cache(app: &mut App, width: usize) {
        let mut images = ImageRuntime::disabled();
        refresh_transcript_cache_with_images(app, &mut images, width);
    }

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
            vendor: crate::events::HarnessVendor::Kit,
            model: Some("test".into()),
            created_at_unix_ms: 1_000,
            generation_started_at_unix_ms: 2_000,
            generation_finished_at_unix_ms: None,
            usage: None,
            cost: None,
            activity: Default::default(),
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
    fn structured_assistant_images_share_viewports_with_markdown_images() {
        let parts = vec![
            super::AgentPart::Text("before".into()),
            super::AgentPart::Image(UserImage::new("AQID".into(), "image/png".into(), 0).unwrap()),
            super::AgentPart::Text("after ![markdown](chart.png) end".into()),
        ];
        let (rows, placements) = super::agent_parts_rows(&parts, 0, 120, true);
        assert_eq!(placements.len(), 2);
        assert_eq!(placements[0].source, 1);
        assert!(placements[0].destination.is_none());
        assert_eq!(placements[1].destination.as_deref(), Some("chart.png"));
        assert!(line_text(&rows[0].0).contains("before"));
        assert!(line_text(&rows[placements[1].row - 1].0).contains("after"));
        assert!(!line_text(&rows[placements[1].row - 1].0).contains("end"));
        assert_eq!(
            line_text(&rows[placements[1].row + super::RESERVED_ROWS as usize].0),
            " end"
        );
        assert!(super::agent_parts_rows(&parts, 0, 120, false).1.is_empty());
    }

    #[test]
    fn assistant_inline_images_preserve_text_rows_and_emphasis() {
        for source in [
            "before ![alt](image.png) after",
            "**before ![alt](image.png) after**",
        ] {
            for width in [12, 120] {
                let mut expected = Vec::new();
                super::append_agent_rows(&mut expected, source, 0, 0, width);
                let (plain, disabled) = super::agent_block_rows(source, 0, width, false);
                assert!(disabled.is_empty());
                assert_eq!(plain.len(), expected.len());
                let (images, placements) = super::agent_block_rows(source, 0, width, true);
                assert_eq!(placements.len(), 1);
                let preview = placements[0].row;
                let after = preview + super::RESERVED_ROWS as usize;
                assert!(line_text(&images[preview - 1].0).ends_with(")"));
                assert_eq!(
                    images[..preview]
                        .iter()
                        .map(|row| line_text(&row.0))
                        .collect::<String>()
                        .split_whitespace()
                        .collect::<String>(),
                    "before![alt](image.png)"
                );
                assert_eq!(line_text(&images[after].0).trim(), "after");
                assert!(
                    !images[..preview]
                        .iter()
                        .any(|row| line_text(&row.0).contains("after"))
                );
                for (expected, plain) in expected.iter().zip(&plain) {
                    assert_eq!(plain.0, expected.0);
                }
                if source.starts_with("**") {
                    for row in images[..preview].iter().chain(&images[after..]) {
                        assert!(
                            row.0
                                .spans
                                .iter()
                                .filter(|span| !span.content.trim().is_empty())
                                .all(|span| {
                                    span.style.add_modifier.contains(super::Modifier::BOLD)
                                })
                        );
                    }
                }
                if width == 120 {
                    assert_eq!(plain.len(), 1);
                    assert_eq!(line_text(&plain[0].0), "before ![alt](image.png) after");
                    if source.starts_with("**") {
                        assert!(plain[0].0.spans.iter().all(|span| {
                            span.style.add_modifier.contains(super::Modifier::BOLD)
                        }));
                    }
                }
            }
        }
    }

    #[test]
    fn assistant_image_viewports_follow_layout_units() {
        for source in [
            "# Heading\n**before ![alt](a.png)**\nnext ![other](b.png)\n```rust\nlet x = 1;\n```",
            "| A | B |\n| --- | --- |\n| ![alt](a.png) | **after** |\nnext ![other](b.png)",
        ] {
            for width in [18, 120] {
                let (plain, _) = super::agent_block_rows(source, 3, width, false);
                let (images, placements) = super::agent_block_rows(source, 3, width, true);
                assert_eq!(placements.len(), 2);
                assert_eq!(placements[0].destination.as_deref(), Some("a.png"));
                assert_eq!(placements[1].destination.as_deref(), Some("b.png"));
                let text_rows: Vec<_> = images
                    .iter()
                    .enumerate()
                    .filter(|(row, _)| {
                        !placements.iter().any(|image| {
                            (image.row..image.row + super::RESERVED_ROWS as usize).contains(row)
                        })
                    })
                    .map(|(_, row)| row)
                    .collect();
                assert_eq!(text_rows.len(), plain.len());
                for (actual, expected) in text_rows.iter().zip(&plain) {
                    assert_eq!(actual.0, expected.0);
                    assert_eq!(
                        actual.1.1.as_ref().map(|hit| &hit.range),
                        expected.1.1.as_ref().map(|hit| &hit.range)
                    );
                }
            }
        }
    }

    #[test]
    fn assistant_markdown_images_keep_prose_order_and_code_literal() {
        let source =
            "before ![first](a.png) between ![second](https://example.invalid/b.png) after";
        let (rows, placements) = super::agent_block_rows(source, 0, 120, true);
        assert_eq!(placements.len(), 2);
        assert_eq!(placements[0].destination.as_deref(), Some("a.png"));
        assert_eq!(
            placements[1].destination.as_deref(),
            Some("https://example.invalid/b.png")
        );
        assert!(line_text(&rows[placements[0].row - 1].0).contains("before"));
        assert!(!line_text(&rows[placements[0].row - 1].0).contains("between"));
        assert!(line_text(&rows[placements[1].row - 1].0).contains("between"));
        assert!(!line_text(&rows[placements[1].row - 1].0).contains("after"));
        assert_eq!(
            line_text(&rows[placements[1].row + super::RESERVED_ROWS as usize].0),
            " after"
        );
        assert!(placements[1].row >= placements[0].row + super::RESERVED_ROWS as usize);
        let (_, disabled) = super::agent_block_rows(source, 0, 120, false);
        assert!(disabled.is_empty());
        for source in [
            "`![literal](a.png)`",
            "```md\n![literal](a.png)\n```",
            "![incomplete](a.png",
        ] {
            assert!(super::agent_block_rows(source, 0, 120, true).1.is_empty());
        }
    }

    fn assert_same_line_image_order(width: usize) {
        let source =
            "**before ![first](a.png) between ![second](b.png) after**\n```rust\nlet x = 1;\n```";
        let (rows, images) = super::agent_block_rows(source, 3, width, true);
        assert_eq!(images.len(), 2);
        let end_first = images[0].row + super::RESERVED_ROWS as usize;
        let end_second = images[1].row + super::RESERVED_ROWS as usize;
        let text = |range: std::ops::Range<usize>| {
            rows[range]
                .iter()
                .map(|row| line_text(&row.0))
                .collect::<String>()
                .split_whitespace()
                .collect::<String>()
        };
        assert_eq!(text(0..images[0].row), "before![first](a.png)");
        assert_eq!(text(end_first..images[1].row), "between![second](b.png)");
        assert_eq!(line_text(&rows[end_second].0), " after");
        for row in rows[..images[0].row]
            .iter()
            .chain(&rows[end_first..images[1].row])
            .chain(&rows[end_second..end_second + 1])
        {
            assert!(
                row.0
                    .spans
                    .iter()
                    .filter(|span| !span.content.trim().is_empty())
                    .all(|span| span.style.add_modifier.contains(super::Modifier::BOLD))
            );
        }
        let (plain, _) = super::agent_block_rows(source, 3, width, false);
        let code_rows = |rows: Vec<super::CachedTranscriptRow>| {
            rows.into_iter()
                .filter_map(|row| row.1.1.map(|hit| (row.0, hit.block, hit.range)))
                .collect::<Vec<_>>()
        };
        let actual = code_rows(rows);
        assert!(!actual.is_empty());
        assert!(
            actual
                .iter()
                .all(|(_, block, range)| *block == 3 && &source[range.clone()] == "let x = 1;")
        );
        assert_eq!(actual, code_rows(plain));
    }

    fn assert_image_continuation_gutter(prefix: &str, indent: usize) {
        for two_images in [false, true] {
            let prose = "alpha bravo charlie delta echo foxtrot";
            let tail = if two_images {
                format!("{prose} ![second](b.png) {prose}")
            } else {
                prose.to_string()
            };
            let source = format!("{prefix}![alt](a.png) {tail}");
            let (rows, images) = super::agent_block_rows(&source, 0, 20, true);
            assert_eq!(images.len(), if two_images { 2 } else { 1 });
            for (index, image) in images.iter().enumerate() {
                let start = image.row + super::RESERVED_ROWS as usize;
                let end = images.get(index + 1).map_or(rows.len(), |next| next.row);
                let text: Vec<_> = rows[start..end]
                    .iter()
                    .map(|row| line_text(&row.0))
                    .collect();
                assert!(text.len() > 1);
                for line in &text {
                    assert_eq!(
                        line.len() - line.trim_start().len(),
                        indent,
                        "{source}: {line:?}"
                    );
                    assert!(unicode_width::UnicodeWidthStr::width(line.as_str()) <= 20);
                }
                let actual = text
                    .iter()
                    .map(|line| line.trim())
                    .collect::<Vec<_>>()
                    .join(" ");
                let expected = if index + 1 < images.len() {
                    format!("{prose} ![second](b.png)")
                } else {
                    prose.to_string()
                };
                assert_eq!(actual, expected, "{source}");
                // Words fit this width: the gutter must not make prose atomic
                // and force character wrapping or half-width indentation.
                assert_eq!(text[0], format!("{}alpha bravo", " ".repeat(indent)));
            }
        }
    }

    #[test]
    fn assistant_list_images_preserve_wrapped_continuation_gutter() {
        assert_image_continuation_gutter("- ", 2);
        assert_image_continuation_gutter("  - ", 4);
    }

    #[test]
    fn assistant_quote_images_preserve_wrapped_continuation_gutter() {
        assert_image_continuation_gutter("> ", 2);
        assert_image_continuation_gutter("  > ", 4);
    }

    #[test]
    fn assistant_same_line_images_order_wide() {
        assert_same_line_image_order(120);
    }

    #[test]
    fn assistant_same_line_images_order_wrapped() {
        assert_same_line_image_order(12);
    }

    #[test]
    fn agent_rows_render_three_lines_ancestry_duration_and_palette() {
        let top = test_agent(
            "Scout",
            SubagentStatus::Working,
            None,
            None,
            "Trace ACP lifecycle",
        );
        let lines = agent_lines(&tree_row(&top, vec![], false, false), false, 0, 74_000, 48);
        assert!(
            line_text(&lines[0]).starts_with("⠋ Scout"),
            "{}",
            line_text(&lines[0])
        );
        assert!(
            line_text(&lines[0]).ends_with("test  g1"),
            "{}",
            line_text(&lines[0])
        );
        let second = line_text(&lines[1]);
        assert_eq!(second, "  Trace ACP lifecycle");
        assert_eq!(line_text(&lines[2]), "  1m 12s");
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
        let lines = agent_lines(
            &tree_row(&nested, vec![true], false, false),
            false,
            0,
            74_000,
            48,
        );
        assert!(
            line_text(&lines[0]).starts_with("│  └─ ⠁ Scout "),
            "{}",
            line_text(&lines[0])
        );
        assert!(line_text(&lines[1]).starts_with("│     Trace ACP lifecycle"));

        let lines = agent_lines(
            &tree_row(&nested, vec![false], true, false),
            false,
            0,
            74_000,
            48,
        );
        assert!(
            line_text(&lines[0]).starts_with("   ├─ ⠁ Scout "),
            "{}",
            line_text(&lines[0])
        );
        assert!(line_text(&lines[1]).starts_with("   │  Trace ACP lifecycle"));

        let lines = agent_lines(&tree_row(&nested, vec![], true, true), false, 0, 74_000, 48);
        assert!(
            line_text(&lines[0]).starts_with("⠁ Scout · via Pip "),
            "{}",
            line_text(&lines[0])
        );
        assert!(line_text(&lines[1]).starts_with("│ Trace ACP lifecycle"));
        assert_eq!(
            lines[0].spans[1].style.fg,
            Some(ratatui::style::Color::Yellow)
        );

        let idle = test_agent("Scout", SubagentStatus::Idle, None, None, "done");
        let idle_lines = agent_lines(&tree_row(&idle, vec![], false, false), false, 0, 74_000, 20);
        assert!(
            line_text(&idle_lines[0]).starts_with("○ Scout "),
            "{}",
            line_text(&idle_lines[0])
        );
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
        let failed_lines = agent_lines(
            &tree_row(&failed, vec![], false, false),
            false,
            0,
            74_000,
            20,
        );
        assert!(
            line_text(&failed_lines[0]).starts_with("✗ Scout "),
            "{}",
            line_text(&failed_lines[0])
        );
        assert_eq!(
            failed_lines[0].spans[1].style.fg,
            Some(ratatui::style::Color::Red)
        );
        assert!(
            line_text(
                &agent_lines(
                    &tree_row(&failed, vec![], false, false),
                    false,
                    0,
                    77_000,
                    20,
                )[0]
            )
            .starts_with("○ Scout ")
        );
    }

    #[test]
    fn truncate_to_width_preserves_extended_grapheme_clusters() {
        for (text, width, expected) in [
            ("❤️x", 2, "…"),
            ("❤️xx", 3, "❤️…"),
            ("🇺🇸xx", 3, "🇺🇸…"),
            ("e\u{301}xx", 2, "e\u{301}…"),
            ("👩‍💻xx", 3, "👩‍💻…"),
        ] {
            assert_eq!(truncate_to_width(text, width), expected);
            assert!(UnicodeWidthStr::width(expected) <= width);
        }
    }

    #[test]
    fn agent_rows_show_usage_below_excerpt() {
        let mut row = test_agent(
            "Scout",
            SubagentStatus::Working,
            None,
            None,
            "Trace ACP lifecycle",
        );
        row.usage = Some(super::super::app::ContextUsage {
            used: 41_200,
            size: 200_000,
        });
        let lines = agent_lines(&tree_row(&row, vec![], false, false), false, 0, 74_000, 48);
        let second = line_text(&lines[1]);
        assert_eq!(second, "  Trace ACP lifecycle");
        assert_eq!(line_text(&lines[2]), "  ▰▱▱▱▱▱ 41k · 1m 12s");

        let lines = agent_lines(&tree_row(&row, vec![], false, false), false, 0, 74_000, 30);
        assert_eq!(line_text(&lines[1]), "  Trace ACP lifecycle");
        assert_eq!(line_text(&lines[2]), "  ▰▱▱▱▱▱ 41k · 1m 12s");
    }

    #[test]
    fn agent_rows_mark_the_harness_vendor_only_when_asked() {
        let mut row = test_agent(
            "Designer",
            SubagentStatus::Idle,
            None,
            None,
            "Propose a palette",
        );
        row.vendor = crate::events::HarnessVendor::Claude;
        let plain = agent_lines(&tree_row(&row, vec![], false, false), false, 0, 74_000, 48);
        assert!(
            line_text(&plain[0]).starts_with("○ Designer "),
            "{}",
            line_text(&plain[0])
        );
        assert_eq!(line_text(&plain[1]), "  idle · resumable");
        let marked = agent_lines(&tree_row(&row, vec![], false, false), true, 0, 74_000, 48);
        assert!(
            line_text(&marked[0]).starts_with("○ ✱ Designer "),
            "{}",
            line_text(&marked[0])
        );
        assert_eq!(
            marked[0].spans[2].style,
            super::theme::vendor_mark(crate::events::HarnessVendor::Claude).1
        );
    }

    #[test]
    fn agents_panel_marks_vendors_only_for_mixed_rosters() {
        let mut app = panel_app(1);
        let mut terminal = Terminal::new(TestBackend::new(46, 9)).expect("terminal");
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .expect("draw succeeds");
        assert_eq!(
            buffer_cells(terminal.backend().buffer(), 1, 1..10),
            "○ Scout 0"
        );

        app.apply(Update::Runtime(RuntimeEvent::SubagentStateChanged {
            id: "codex".into(),
            name: "Fixer".into(),
            status: SubagentStatus::Working,
            outcome: None,
            generation: 1,
            task: "Patch the parser".into(),
            parent_id: None,
            parent_name: None,
            harness: "acp.codex".into(),
            vendor: crate::events::HarnessVendor::Codex,
            model: None,
            created_at_unix_ms: 3_000,
            generation_started_at_unix_ms: 3_000,
            generation_finished_at_unix_ms: None,
        }));
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .expect("draw succeeds");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer_cells(buffer, 1, 1..10), "⠋ ◎ Fixer");
        assert_eq!(buffer_cells(buffer, 4, 1..12), "○ k Scout 0");
    }

    #[test]
    fn agents_panel_fits_nested_long_identity_and_preserves_generation() {
        let mut app = panel_app(1);
        app.apply(Update::Runtime(RuntimeEvent::SubagentStateChanged {
            id: "child".into(),
            name: "Nested agent with a long name".into(),
            status: SubagentStatus::Working,
            outcome: None,
            generation: 123,
            task: "Patch the parser".into(),
            parent_id: Some("agent-0".into()),
            parent_name: Some("Scout 0".into()),
            harness: "acp.codex".into(),
            vendor: crate::events::HarnessVendor::Codex,
            model: Some("very-long-model-模型-identifier-that-exceeds-panel-width".into()),
            created_at_unix_ms: 3_000,
            generation_started_at_unix_ms: 3_000,
            generation_finished_at_unix_ms: None,
        }));
        for width in [30, 46, 60] {
            let mut terminal = Terminal::new(TestBackend::new(width, 12)).expect("terminal");
            terminal
                .draw(|frame| draw_agents(frame, &mut app, frame.area()))
                .expect("draw succeeds");
            let row = buffer_row(terminal.backend().buffer(), 4);
            assert!(row.contains("└─ ⠋ ◎ Nes"), "{row}");
            assert!(row.contains("…"), "{row}");
            assert!(row.ends_with("g123│"), "{row}");
        }
    }

    #[test]
    fn agent_rows_truncate_unicode_excerpt_independently_of_usage() {
        let row = test_agent(
            "Scout",
            SubagentStatus::Working,
            None,
            None,
            "🦀🦀 lifecycle work",
        );
        let lines = agent_lines(&tree_row(&row, vec![], false, false), false, 0, 3_500, 12);
        assert_eq!(line_text(&lines[1]), "  🦀🦀 life…");
        assert_eq!(line_text(&lines[2]), "  1.5s");
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(line_text(&lines[1]).as_str()),
            12
        );

        let lines = agent_lines(&tree_row(&row, vec![], false, false), false, 0, 3_500, 4);
        assert!(unicode_width::UnicodeWidthStr::width(line_text(&lines[1]).as_str()) <= 4);
    }

    #[test]
    fn agents_layout_obeys_hidden_and_107_108_boundaries() {
        let area_107 = ratatui::layout::Rect::new(2, 3, 107, 20);
        assert_eq!(body_layout(area_107, false, false), (area_107, None));
        assert_eq!(body_layout(area_107, true, true), (area_107, None));
        assert_eq!(body_layout(area_107, true, false), (area_107, None));

        let area_108 = ratatui::layout::Rect::new(2, 3, 108, 20);
        assert_eq!(
            body_layout(area_108, true, false),
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
                vendor: crate::events::HarnessVendor::Kit,
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

    fn wait_for_image_decode(images: &mut ImageRuntime) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while images.pending() && std::time::Instant::now() < deadline {
            images.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        images.poll();
        assert!(!images.pending(), "background image decode did not finish");
    }

    fn buffer_contains_black_image_cell(buffer: &ratatui::buffer::Buffer) -> bool {
        (0..buffer.area.height).any(|row| {
            (0..buffer.area.width).any(|column| {
                let cell = &buffer[(column, row)];
                cell.fg == ratatui::style::Color::Rgb(0, 0, 0)
                    && cell.bg == ratatui::style::Color::Rgb(0, 0, 0)
            })
        })
    }

    #[test]
    fn agents_panel_shows_accumulated_reported_cost_by_currency() {
        let mut app = panel_app(3);
        for (id, amount, currency) in [
            ("agent-0", 1.25, "USD"),
            ("agent-0", 1.25, "USD"),
            ("agent-1", 0.5, "USD"),
            ("agent-2", 2.0, "EUR"),
        ] {
            app.apply(Update::Runtime(RuntimeEvent::SubagentUsage {
                id: id.into(),
                used: 1,
                size: 2,
                cost: Some(agent_client_protocol::schema::v2::Cost::new(
                    amount, currency,
                )),
            }));
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 9)).expect("terminal");
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .expect("draw succeeds");
        assert!(buffer_row(terminal.backend().buffer(), 7).contains("EUR 2.0000 + $1.7500"));
    }

    #[test]
    fn agents_panel_keeps_footer_fixed_while_overflowing_rows_scroll() {
        let mut app = panel_app(5);
        let mut terminal = Terminal::new(TestBackend::new(46, 9)).expect("terminal");
        terminal
            .draw(|frame| draw_agents(frame, &mut app, frame.area()))
            .expect("draw succeeds");
        let initial = terminal.backend().buffer();
        assert_eq!(buffer_cells(initial, 0, 1..9), " agents ");
        assert_eq!(buffer_cells(initial, 1, 1..10), "○ Scout 0");
        assert_eq!(buffer_cells(initial, 2, 3..19), "idle · resumable");

        assert_eq!(buffer_cells(initial, 4, 1..10), "○ Scout 1");
        assert_eq!(buffer_cells(initial, 7, 1..18), "5 agents · 5 idle");

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
        assert_eq!(buffer_cells(scrolled, 4, 1..10), "○ Scout 4");
        assert_eq!(buffer_cells(scrolled, 7, 1..18), "5 agents · 5 idle");
        assert_eq!(
            buffer_row(scrolled, 8),
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

    #[test]
    fn file_picker_renders_indexing_empty_and_selected_states() {
        let mut app = sample();
        app.file_picker = Some(FilePickerDialog {
            query_range: 0..1,
            revision: 1,
            activation: 1,
            selected: 0,
            matches: Vec::new(),
            status: FilePickerStatus::Loading,
        });
        let indexing = render(&mut app, 60, 12);
        assert!(indexing.contains("files"), "{indexing}");
        assert!(indexing.contains("indexing workspace files"), "{indexing}");

        app.file_picker.as_mut().unwrap().status = FilePickerStatus::Ready;
        let empty = render(&mut app, 60, 12);
        assert!(empty.contains("no matching files"), "{empty}");

        let dialog = app.file_picker.as_mut().unwrap();
        dialog.matches = (0..10)
            .map(|index| FileMatch {
                relative_path: format!("src/path-{index}.rs"),
                match_byte_offsets: std::iter::once(4..8).collect(),
            })
            .collect();
        dialog.selected = 9;
        let selected = render(&mut app, 24, 8);
        assert!(selected.contains("› src/path-9.rs"), "{selected}");
        let tiny = render(&mut app, 12, 3);
        assert!(tiny.contains("files"), "{tiny}");
    }

    #[test]
    fn file_picker_reports_invalid_query_offsets() {
        for offset in [1, usize::MAX] {
            let mut app = sample();
            app.editor.insert_str("界");
            app.file_picker = Some(FilePickerDialog {
                query_range: offset..offset,
                revision: 1,
                activation: 1,
                selected: 0,
                matches: Vec::new(),
                status: FilePickerStatus::Loading,
            });
            let frame = render(&mut app, 60, 12);
            assert!(frame.contains("invalid file picker position"), "{frame}");
            assert!(!frame.contains("indexing workspace files"), "{frame}");
        }
    }

    #[test]
    fn file_picker_anchors_to_the_at_sign() {
        let mut start = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        start.editor.insert_str("open\n      @app");
        start.file_picker = Some(FilePickerDialog {
            query_range: 11..15,
            revision: 1,
            activation: 1,
            selected: 0,
            matches: vec![FileMatch {
                relative_path: "src/tui/app.rs".into(),
                match_byte_offsets: std::iter::once(8..11).collect(),
            }],
            status: FilePickerStatus::Ready,
        });
        let screen = render(&mut start, 120, 24);
        let lines = screen.lines().collect::<Vec<_>>();
        let prompt_row = lines.iter().position(|line| line.contains('@')).unwrap();
        let prompt_column = lines[prompt_row]
            .chars()
            .position(|cell| cell == '@')
            .unwrap();
        let picker_row = lines
            .iter()
            .position(|line| line.contains(" files "))
            .unwrap();
        let picker_column = lines[picker_row]
            .chars()
            .position(|cell| cell == '╭')
            .unwrap();
        assert_eq!((picker_row, picker_column), (prompt_row + 1, prompt_column));

        let mut active = sample();
        active.editor.insert_str("open\n      @app");
        active.file_picker = start.file_picker;
        let screen = render(&mut active, 120, 24);
        let lines = screen.lines().collect::<Vec<_>>();
        let prompt_row = lines.iter().position(|line| line.contains('@')).unwrap();
        let prompt_column = lines[prompt_row]
            .chars()
            .position(|cell| cell == '@')
            .unwrap();
        let picker_row = (0..prompt_row)
            .rev()
            .find(|row| lines[*row].contains('╰'))
            .unwrap();
        let picker_column = lines[picker_row]
            .chars()
            .position(|cell| cell == '╰')
            .unwrap();
        assert_eq!((picker_row + 1, picker_column), (prompt_row, prompt_column));
    }

    #[test]
    fn file_picker_stays_outside_prompt_when_the_at_sign_scrolls_offscreen() {
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        let input = format!("@{}", "a".repeat(1_000));
        app.editor.insert_str(&input);
        app.file_picker = Some(FilePickerDialog {
            query_range: 0..input.len(),
            revision: 1,
            activation: 1,
            selected: 0,
            matches: vec![FileMatch {
                relative_path: "src/tui/app.rs".into(),
                match_byte_offsets: std::iter::once(8..11).collect(),
            }],
            status: FilePickerStatus::Ready,
        });

        let screen = render(&mut app, 120, 40);
        let lines = screen.lines().collect::<Vec<_>>();
        let last_prompt_row = lines
            .iter()
            .rposition(|line| line.contains("aaaaaaaaaa"))
            .unwrap();
        let picker_row = lines
            .iter()
            .position(|line| line.contains('╭') && line.contains(" files "))
            .unwrap();
        assert!(picker_row > last_prompt_row, "{screen}");
    }

    #[test]
    fn file_picker_stays_visible_near_the_right_edge() {
        let mut app = sample();
        let input = format!("{} @app", "x".repeat(70));
        let query_start = input.find('@').unwrap();
        app.editor.insert_str(&input);
        app.file_picker = Some(FilePickerDialog {
            query_range: query_start..input.len(),
            revision: 1,
            activation: 1,
            selected: 0,
            matches: vec![FileMatch {
                relative_path: "src/tui/app.rs".into(),
                match_byte_offsets: std::iter::once(8..11).collect(),
            }],
            status: FilePickerStatus::Ready,
        });

        let screen = render(&mut app, 80, 24);
        assert!(screen.contains("src/tui/app.rs"), "{screen}");
        let picker_border = screen
            .lines()
            .find(|line| line.contains('╭') && line.contains(" files "))
            .unwrap();
        let left = picker_border.chars().position(|cell| cell == '╭').unwrap();
        let right = picker_border.chars().position(|cell| cell == '╮').unwrap();
        assert_eq!(right - left + 1, 24);
        assert_eq!(right, 79);
    }

    include!("runtime_health_tests.rs");

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
    fn model_switch_popup_renders_all_choices_and_defaults_to_cancel() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "openrouter".into(),
            "original".into(),
            String::new(),
        );
        app.begin_model_switch(
            crate::tui::app::ModelChoice {
                id: "openrouter:target".into(),
                provider: "openrouter".into(),
                model: "target".into(),
            },
            false,
        )
        .unwrap();
        app.model_switch.as_mut().unwrap().warning =
            Some(crate::protocols::acp::model_switch::Warning {
                token: 1,
                guarded_tokens: "120000".into(),
                target_window: 150000,
            });
        let screen = render(&mut app, 90, 24);
        for label in [
            "model context warning",
            "Estimated 120000 / 150000 tokens.",
            "80%",
            "Continue anyway",
            "Compact",
            "› Cancel",
        ] {
            assert!(screen.contains(label), "missing {label}");
        }
        assert!(!screen.contains("margin"));
        // Small terminals must not panic when a dialog is clipped.
        let _ = render(&mut app, 1, 1);
    }

    #[test]
    fn storage_warning_is_visible_without_logs_and_clears_on_recovery() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "openai".into(),
            "gpt".into(),
            "127.0.0.1:7331".into(),
        );
        app.apply(Update::Runtime(RuntimeEvent::StorageStatus {
            pending: true,
            exhausted: false,
        }));
        assert!(!app.show_logs);
        assert!(render(&mut app, 80, 24).contains("Memory-only storage"));
        app.blocks.push(Block::Agent("response".into()));
        assert!(render(&mut app, 80, 24).contains("Memory-only storage"));
        app.start_session("next".into());
        assert!(render(&mut app, 80, 24).contains("Memory-only storage"));
        app.apply(Update::Runtime(RuntimeEvent::StorageStatus {
            pending: false,
            exhausted: false,
        }));
        assert!(!render(&mut app, 80, 24).contains("Memory-only storage"));
        app.apply(Update::Runtime(RuntimeEvent::StorageStatus {
            pending: false,
            exhausted: true,
        }));
        assert!(render(&mut app, 80, 24).contains("Storage exhausted"));
    }

    #[test]
    fn command_popup_renders_in_start_and_compact_layouts() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "openai".into(),
            "gpt".into(),
            "127.0.0.1:7331".into(),
        );
        app.paste("/mo");
        let start = render(&mut app, 80, 24);
        assert!(start.contains("commands"), "{start}");
        assert!(start.contains("/model"), "{start}");
        assert!(start.contains("Choose a model"), "{start}");

        app.blocks.push(Block::Agent("session response".into()));
        app.phase = Phase::Working;
        let compact = render(&mut app, 80, 24);
        assert!(compact.contains("/model"), "{compact}");
        assert!(compact.contains("Choose a model"), "{compact}");
    }

    #[test]
    fn command_popup_obscures_native_links_and_clears_stale_footprints() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "openai".into(),
            "gpt".into(),
            "127.0.0.1:7331".into(),
        );
        app.blocks.push(Block::Agent(
            "[visible link](https://example.com/target)".into(),
        ));
        app.phase = Phase::Working;
        let mut terminal = Terminal::new(TestBackend::new(50, 10)).expect("terminal");
        let mut images = ImageRuntime::disabled();
        let mut renderer = HyperlinkRenderer::default();
        let capture = Capture::default();
        let mut native_backend = CrosstermBackend::new(capture.clone());

        let linked = terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .expect("draw linked frame");
        assert!(!native_links_obscured(&app));
        let (row, hit) = app
            .row_links
            .iter()
            .enumerate()
            .find_map(|(row, hits)| hits.first().map(|hit| (row, hit)))
            .expect("visible transcript link");
        let link_x = u16::try_from(app.transcript_left + hit.start).unwrap();
        let link_y = u16::try_from(app.transcript_top + row).unwrap();
        let linked_symbol = linked.buffer[(link_x, link_y)].symbol().to_string();
        let prepared = renderer.prepare(
            &linked,
            &app.row_links,
            app.transcript_left,
            app.transcript_top,
            false,
        );
        renderer.draw(&mut native_backend, prepared).unwrap();
        let open = b"\x1b]8;;https://example.com/target\x1b\\";
        assert!(capture.bytes().windows(open.len()).any(|part| part == open));
        capture.clear();

        app.paste("/");
        let popup = terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .expect("draw popup frame");
        assert!(native_links_obscured(&app));
        assert!(app.row_links.iter().flatten().next().is_some());
        assert_ne!(
            popup.buffer[(link_x, link_y)].symbol(),
            linked_symbol,
            "command popup should cover the transcript link"
        );
        let prepared = renderer.prepare(
            &popup,
            &app.row_links,
            app.transcript_left,
            app.transcript_top,
            native_links_obscured(&app),
        );
        renderer.draw(&mut native_backend, prepared).unwrap();

        let cleared = capture.bytes();
        assert!(cleared.starts_with(b"\x1b7"));
        assert!(cleared.ends_with(b"\x1b8"));
        // Clearing a stale footprint still emits OSC-8 close; it must not
        // reopen the hidden transcript destination over the popup cells.
        assert!(!cleared.windows(open.len()).any(|part| part == open));
        capture.clear();

        let prepared = renderer.prepare(
            &popup,
            &app.row_links,
            app.transcript_left,
            app.transcript_top,
            true,
        );
        renderer.draw(&mut native_backend, prepared).unwrap();
        assert!(capture.bytes().is_empty());
    }

    #[test]
    fn command_popup_handles_tiny_terminals_and_disappears_after_dismissal() {
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "openai".into(),
            "gpt".into(),
            "127.0.0.1:7331".into(),
        );
        app.paste("/");
        let _ = render(&mut app, 8, 2);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let dismissed = render(&mut app, 80, 24);
        assert!(!dismissed.contains(" commands "), "{dismissed}");
    }

    #[test]
    fn session_dialog_renders_custom_names_and_inline_rename_states() {
        let mut app = App::new(
            PathBuf::from("/tmp/project"),
            "provider".into(),
            "model".into(),
            "127.0.0.1:7331".into(),
        );
        app.session_choices = vec![crate::session::CatalogEntry {
            additional_directories: Vec::new(),
            id: "s-abc123".into(),
            title: Some("OAuth token bug".into()),
            preview: Some("Preview remains available".into()),
            is_subagent: false,
            updated_at: 0,
        }];
        app.session_dialog = Some(SessionDialog {
            selected: 0,
            rename: None,
        });

        let browsing = render(&mut app, 88, 10);
        assert!(browsing.contains("OAuth token bug"), "{browsing}");
        assert!(browsing.contains("Preview remains available"), "{browsing}");

        assert!(browsing.contains("r rename"), "{browsing}");
        let tiny_browsing = render(&mut app, 40, 3);
        assert!(tiny_browsing.contains("s-abc123"), "{tiny_browsing}");

        app.session_dialog.as_mut().unwrap().rename =
            Some(SessionRename::Editing("New name".into()));
        let editing = render(&mut app, 54, 10);
        assert!(editing.contains("rename: New name"), "{editing}");
        assert!(editing.contains("enter save"), "{editing}");
        let tiny_editing = render(&mut app, 40, 4);
        assert!(tiny_editing.contains("s-abc123"), "{tiny_editing}");
        assert!(tiny_editing.contains("rename: New name"), "{tiny_editing}");

        app.session_dialog.as_mut().unwrap().rename = Some(SessionRename::ConfirmClear);
        let confirming = render(&mut app, 32, 8);
        assert!(
            confirming.contains("Clear the custom name?"),
            "{confirming}"
        );
        assert!(confirming.contains("enter clear name"), "{confirming}");

        app.session_choices[0].title = Some("界".repeat(100));
        app.session_dialog.as_mut().unwrap().rename = None;
        let narrow = render(&mut app, 20, 6);
        assert!(narrow.contains('…'), "{narrow}");
    }

    #[test]
    fn agents_panel_is_hidden_before_the_first_transcript_block() {
        let mut app = panel_app(1);
        assert!(app.blocks.is_empty());
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));

        for (width, height) in [(120, 30), (80, 12)] {
            let screen = render(&mut app, width, height);
            assert!(screen.contains("█▀▄ █  █▄"), "{screen}");
            assert!(!screen.contains("agent roster"), "{screen}");
            assert!(!screen.contains("Scout 0"), "{screen}");
        }
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
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        app.apply(Update::SteerAccepted {
            editable: true,
            id: "first".into(),
            text: "first pending".into(),
        });
        app.apply(Update::SteerAccepted {
            editable: true,
            id: "second".into(),
            text: "second pending".into(),
        });

        let frame = render(&mut app, 80, 18);
        let first = frame.find("first pending").expect("first steer");
        let second = frame.find("second pending").expect("second steer");
        let input = frame.find("steer kit…").expect("steering input");
        assert!(first < second && second < input, "{frame}");
        assert_eq!(frame.matches("⇥ steer").count(), 2, "{frame}");
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
        assert_eq!(frame.matches("⇥ steer").count(), 1, "{frame}");
        assert_eq!(app.pending_steers.len(), 1);
        assert!(
            matches!(app.blocks.last(), Some(Block::User(message)) if message.text == "first pending")
        );
    }

    #[test]
    fn queued_controls_are_discoverable_and_selection_scrolls_into_view() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );
        app.can_replace_steer = true;
        for index in 0..8 {
            app.apply(Update::SteerAccepted {
                editable: true,
                id: index.to_string(),
                text: format!("queued text {index}"),
            });
        }
        assert!(render(&mut app, 100, 18).contains("F2 queue"));
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        let first = render(&mut app, 100, 18);
        assert!(first.contains("▶ steer 1  queued text 0"), "{first}");
        assert!(first.contains("⏎ edit"), "{first}");
        assert!(first.contains("del remove"), "{first}");
        for _ in 0..7 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let last = render(&mut app, 100, 18);
        assert!(last.contains("▶ steer 8  queued text 7"), "{last}");
        assert!(last.contains("[8/8]"), "{last}");
        app.pending_steers[7].editable = false;
        assert!(render(&mut app, 100, 18).contains("edit unavailable"));
        app.pending_steers[7].editable = true;
        app.can_replace_steer = false;
        assert!(render(&mut app, 100, 18).contains("edit unavailable"));
        app.can_replace_steer = true;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let editing = render(&mut app, 100, 18);
        assert!(editing.contains("editing pending"), "{editing}");
        assert!(editing.contains("esc restore draft"), "{editing}");
    }

    #[test]
    fn drained_queue_shows_composer_handoff_hint_without_selector_focus() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );
        app.paste("draft");
        app.apply(Update::SteerAccepted {
            id: "pending".into(),
            text: "queued".into(),
            editable: true,
        });
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        app.apply(Update::UserMessage {
            id: "pending".into(),
            text: "queued".into(),
            images: Vec::new(),
            append: false,
        });
        assert!(!app.queue_focused);
        let frame = render(&mut app, 100, 18);
        assert!(frame.contains("queue closed · type / ←→ / esc"), "{frame}");
        assert!(!frame.contains("esc back"), "{frame}");
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(!render(&mut app, 100, 18).contains("queue closed"));
    }

    #[test]
    fn empty_queue_keeps_the_composer_hint() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        let frame = render(&mut app, 100, 18);
        assert!(!app.queue_focused);
        assert!(frame.contains("no pending messages"), "{frame}");
        assert!(frame.contains("⏎ send"), "{frame}");
        assert!(!frame.contains("esc back"), "{frame}");
    }

    #[test]
    fn completed_turn_duration_is_rendered() {
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        app.blocks.push(Block::TurnDuration {
            background: 0,
            since_prompt: 788_645_000,
        });

        let frame = render(&mut app, 80, 12);

        assert!(frame.contains("· 1w2d 3h04m05s"), "{frame}");
        assert!(!frame.contains("so far"), "{frame}");

        app.blocks.push(Block::TurnDuration {
            background: 2,
            since_prompt: 788_646_000,
        });
        let frame = render(&mut app, 80, 12);
        assert!(
            frame.contains("· ◔ 2 in background · 1w2d 3h04m06s so far"),
            "{frame}"
        );
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
    fn compose_source_stays_neutral() {
        let mut app = sample();

        let frame = render(&mut app, 120, 40);

        assert!(
            frame.contains("files = shell({ command: \"ls src\" })"),
            "{frame}"
        );
        assert!(!frame.contains(" # "), "{frame}");
        assert!(!frame.contains("resolved"), "{frame}");
        assert!(!frame.contains("iteration 1 running"), "{frame}");
        assert!(!frame.contains("shell running"), "{frame}");
        assert!(app.transcript_width > 100, "{}", app.transcript_width);
    }

    #[test]
    fn compose_title_uses_trimmed_intent_or_running_tools_fallback() {
        let mut app = sample();
        let fallback = render(&mut app, 100, 30);
        assert!(fallback.contains("Running tools."), "{fallback}");
        assert!(!fallback.contains("compose"), "{fallback}");

        app.apply(Update::ToolPatched {
            id: "call-1".into(),
            title: None,
            kind: None,
            status: None,
            script: None,
            output: None,
            images: None,
            append_output: false,
            intent: Some(Some("  Check every source file.  ".into())),
            backgrounded: false,
        });
        let intent = render(&mut app, 100, 30);
        assert!(intent.contains("Check every source file."), "{intent}");
        assert!(!intent.contains("Running tools."), "{intent}");
    }

    #[test]
    fn completed_compose_stays_collapsed_when_its_title_arrives_late() {
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        app.apply(Update::ToolPatched {
            id: "late-title".into(),
            title: None,
            kind: None,
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
            script: Some("return 1".into()),
            output: Some(vec!["1".into()]),
            images: None,
            append_output: false,
            intent: None,
            backgrounded: false,
        });
        app.apply(Update::ToolPatched {
            id: "late-title".into(),
            title: Some("compose".into()),
            kind: None,
            status: None,
            script: None,
            output: None,
            images: None,
            append_output: false,
            intent: None,
            backgrounded: false,
        });

        assert!(app.blocks.iter().any(|block| {
            matches!(block, Block::Tool(call) if call.id == "late-title" && call.is_compose() && !call.expanded)
        }));
    }

    #[test]
    fn completed_compose_defaults_collapsed_and_cycles_output_script_and_closed() {
        let mut app = sample();
        app.apply(Update::ToolPatched {
            title: None,
            kind: None,
            images: None,
            append_output: false,
            intent: None,
            id: "call-1".into(),
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
            script: None,
            output: Some(vec!["compose result".into()]),
            backgrounded: false,
        });

        let collapsed = render(&mut app, 100, 30);
        assert!(collapsed.contains("▸ 1 line"), "{collapsed}");
        assert!(!collapsed.contains("compose result"), "{collapsed}");
        assert!(!collapsed.contains("files = shell"), "{collapsed}");

        app.toggle_last_output();
        let output = render(&mut app, 100, 30);
        assert!(output.contains("[output]"), "{output}");
        assert!(output.contains(" script "), "{output}");
        assert!(output.contains("compose result"), "{output}");
        assert!(!output.contains("files = shell"), "{output}");

        app.toggle_last_output();
        let script = render(&mut app, 100, 30);
        assert!(script.contains(" output "), "{script}");
        assert!(script.contains("[script]"), "{script}");
        assert!(script.contains("files = shell"), "{script}");
        assert!(!script.contains("compose result"), "{script}");

        app.toggle_last_output();
        let collapsed_again = render(&mut app, 100, 30);
        assert!(collapsed_again.contains("▸ 1 line"), "{collapsed_again}");
        assert!(!collapsed_again.contains("[output]"), "{collapsed_again}");
    }

    #[test]
    fn explicit_compose_view_survives_completion_and_later_messages() {
        let mut app = sample();
        app.toggle_last_output();
        app.toggle_last_output();
        app.apply(Update::ToolPatched {
            title: None,
            kind: None,
            images: None,
            append_output: false,
            intent: None,
            id: "call-1".into(),
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
            script: None,
            output: Some(vec!["compose result".into()]),
            backgrounded: false,
        });

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
        assert!(compose_expanded(&app));

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
        app.apply(Update::ToolPatched {
            title: None,
            kind: None,
            images: None,
            append_output: false,
            intent: None,
            id: "call-1".into(),
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
            script: None,
            output: Some(vec!["compose result".into()]),
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
    fn dock_is_capped_and_keeps_the_selected_entry_visible() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        for index in 0..6 {
            if index == 5 {
                let frame = render(&mut app, 100, 24);
                assert!(frame.contains("… 2 more in the dock"), "{frame}");
                assert!(frame.contains("program 4"), "{frame}");
            }
            app.apply(Update::ToolStarted {
                id: format!("bg-{index}"),
                title: format!("program {index}"),
                kind: ToolKind::Other,
                script: Some("return 1".into()),
                backgrounded: true,
            });
        }
        app.can_replace_steer = true;
        for index in 0..3 {
            app.apply(Update::SteerAccepted {
                editable: true,
                id: format!("steer-{index}"),
                text: format!("queued {index}"),
            });
        }
        let dock = |frame: &str| {
            frame
                .lines()
                .filter(|line| line.contains("background  ") || line.contains("steer "))
                .count()
        };

        // Newest background call is focused by default and stays visible.
        let frame = render(&mut app, 100, 24);
        assert!(frame.contains("program 5"), "{frame}");
        assert!(frame.contains("… 6 more in the dock"), "{frame}");
        assert!(dock(&frame) <= super::MAX_DOCK_ROWS, "{frame}");

        app.focused_call_id = Some("bg-0".into());
        let frame = render(&mut app, 100, 24);
        assert!(frame.contains("program 0"), "{frame}");
        assert!(frame.contains("^k stop"), "{frame}");
        assert!(!frame.contains("background  program 5"), "{frame}");

        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        for _ in 0..2 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let frame = render(&mut app, 100, 24);
        assert!(frame.contains("▶ steer 3  queued 2"), "{frame}");
        assert!(frame.contains("[3/3]"), "{frame}");
        assert!(dock(&frame) <= super::MAX_DOCK_ROWS, "{frame}");
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
        fn dock(app: &mut App) -> Vec<String> {
            render(app, 100, 20)
                .lines()
                .filter(|line| line.trim_start().starts_with("◔ background  "))
                .map(str::to_owned)
                .collect()
        }

        let initial = dock(&mut app);
        let first = initial
            .iter()
            .find(|line| line.contains("compose first"))
            .unwrap();
        let second = initial
            .iter()
            .find(|line| line.contains("compose second"))
            .unwrap();
        assert!(!first.contains("^k stop"), "{initial:?}");
        assert!(second.contains("^k stop"), "{initial:?}");

        app.focused_call_id = Some("first".into());
        let selected = dock(&mut app);
        let first = selected
            .iter()
            .find(|line| line.contains("compose first"))
            .unwrap();
        let second = selected
            .iter()
            .find(|line| line.contains("compose second"))
            .unwrap();
        assert!(first.contains("^k stop"), "{selected:?}");
        assert!(!second.contains("^k stop"), "{selected:?}");
    }

    #[test]
    fn idle_stop_hint_requires_a_running_focused_background_call() {
        let mut app = sample();
        app.apply(Update::ToolStarted {
            id: "background".into(),
            title: "background program".into(),
            kind: ToolKind::Other,
            script: Some("return 1".into()),
            backgrounded: true,
        });
        app.apply(Update::ToolPatched {
            id: "call-1".into(),
            title: None,
            kind: None,
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
            script: None,
            output: None,
            images: None,
            append_output: false,
            intent: None,
            backgrounded: false,
        });
        app.phase = Phase::Idle;
        app.focused_call_id = Some("call-1".into());
        assert!(!app.focus_call().unwrap().running());
        assert_eq!(app.background_calls().len(), 1);
        let frame = render(&mut app, 160, 24);
        assert!(!frame.contains("^k stop"), "{frame}");

        app.focused_call_id = Some("background".into());
        let frame = render(&mut app, 160, 24);
        assert!(frame.lines().last().unwrap().contains("^k stop"), "{frame}");
    }

    #[test]
    fn scrolls_back_through_a_long_transcript_with_the_log_pane_open() {
        let mut app = sample();
        app.apply(Update::ToolPatched {
            title: None,
            kind: None,
            images: None,
            append_output: false,
            intent: None,
            id: "call-1".into(),
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Failed),
            script: None,
            output: Some(vec!["exit code 1".into()]),
            backgrounded: false,
        });
        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));
        app.apply(Update::Log("warn: retrying provider request".into()));
        app.show_logs = true;
        for index in 0..12 {
            app.push_user(format!("follow-up number {index}"));
            app.apply(Update::test_text(format!("answer number {index}")));
        }
        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));
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
            .position(|row| row.contains("working"))
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
        app.apply(Update::ToolPatched {
            title: None,
            kind: None,
            images: None,
            append_output: false,
            intent: None,
            id: "call-1".into(),
            status: Some(agent_client_protocol::schema::v2::ToolCallStatus::Completed),
            script: None,
            output: Some(
                (0..40)
                    .map(|index| format!("output line {index}"))
                    .collect(),
            ),
            backgrounded: false,
        });
        let frame = render(&mut app, 100, 24);
        println!("{frame}");
        assert!(frame.contains("▸ 40 lines"));
        assert!(!frame.contains("output line 39"));

        let row = frame
            .lines()
            .position(|line| line.contains("▸ 40 lines"))
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
    fn session_cost_is_shown_in_header_only_when_reported() {
        use agent_client_protocol::schema::v2::Cost;
        let mut app = sample();
        assert!(
            !render(&mut app, 160, 24)
                .lines()
                .next()
                .unwrap()
                .contains('$')
        );
        app.apply(Update::Usage {
            used: 1,
            size: 2,
            cost: Some(Cost::new(0.0, "USD")),
        });
        assert!(
            render(&mut app, 160, 24)
                .lines()
                .next()
                .unwrap()
                .contains("$0.0000")
        );
        app.apply(Update::Usage {
            used: 1,
            size: 2,
            cost: Some(Cost::new(1.2345, "EUR")),
        });
        assert!(
            render(&mut app, 160, 24)
                .lines()
                .next()
                .unwrap()
                .contains("EUR 1.2345")
        );
        assert_eq!(super::cost_label(0.000001, "USD"), "$<0.0001");
    }

    #[test]
    fn agent_rows_show_reported_cost() {
        let mut row = test_agent("Scout", SubagentStatus::Working, None, None, "Explore");
        row.cost = Some(agent_client_protocol::schema::v2::Cost::new(0.125, "USD"));
        let lines = agent_lines(&tree_row(&row, vec![], false, false), false, 0, 74_000, 48);
        assert!(line_text(&lines[2]).contains("$0.1250"));
    }

    #[test]
    fn shows_reported_context_usage_in_the_header() {
        let mut app = sample();
        app.apply(Update::Usage {
            used: 1_360,
            size: 272_000,
            cost: None,
        });
        app.session_id = Some("s-1770000000000-12345-0".into());

        let frame = render(&mut app, 120, 24);

        assert!(frame.contains(" kit ▏ kit"), "{frame}");
        assert!(frame.contains("gpt-5.4 · default"), "{frame}");
        assert!(frame.contains("▱▱▱▱▱▱▱▱┆▱▱ 0.5% 1k/272k"), "{frame}");
        assert!(frame.contains("s-1770000000000-12345-0"), "{frame}");
        assert!(frame.lines().any(|line| line == "━".repeat(120)));
        assert!(!frame.contains("ctx "));
    }

    #[test]
    fn clicking_the_session_id_copies_the_resume_command() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut app = sample();
        app.session_id = Some("s-1770000000000-12345-0".into());
        let frame = render(&mut app, 120, 24);
        let header = frame.lines().next().expect("header row");
        let column = header
            .find("s-1770000000000-12345-0")
            .expect("session id in header");
        let column = u16::try_from(header[..column].chars().count()).unwrap();

        let mut action = crate::tui::app::Action::None;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            action = app.handle_mouse(MouseEvent {
                kind,
                column: column + 3,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
        }
        let crate::tui::app::Action::Copy(command) = action else {
            panic!("clicking the session id should copy");
        };
        assert_eq!(
            command,
            "kit tui --root /Users/dev/projects/kit --resume s-1770000000000-12345-0"
        );
        assert_eq!(app.toast_text(), Some("copied resume command"));

        app.root = PathBuf::from("/Users/dev/my projects/kit");
        assert_eq!(
            app.resume_command().as_deref(),
            Some("kit tui --root '/Users/dev/my projects/kit' --resume s-1770000000000-12345-0")
        );
    }

    #[test]
    fn resume_command_quotes_shell_metacharacters_without_whitespace() {
        let mut app = sample();
        app.session_id = Some("session-1".into());
        for root in ["/tmp/it's", "/tmp/$HOME", "/tmp/$(pwd)", "/tmp/a;b", ""] {
            app.root = PathBuf::from(root);
            let command = app.resume_command().unwrap();
            assert!(command.starts_with("kit tui --root '"), "{command}");
            assert_eq!(
                shlex::split(&command).unwrap(),
                ["kit", "tui", "--root", root, "--resume", "session-1"]
            );
        }
    }

    #[test]
    fn header_fitting_uses_terminal_column_width() {
        let mut app = sample();
        app.provider = "p".into();
        app.model = "模型".into();
        app.session_id = Some("s".into());

        let frame = render(&mut app, 35, 24);
        let header = frame.lines().next().expect("header row");

        assert!(header.contains("模"), "{header}");
        assert!(!header.contains("a2a"), "{header}");
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
        let known = prompt_lines(vec!["/new prompt".into()], "/new prompt", &[], false);
        assert_eq!(known[0].spans[0].content, "/new");
        assert_eq!(known[0].spans[0].style, crate::tui::theme::accent());
        assert_eq!(known[0].spans[1].content, " prompt");

        let unknown = prompt_lines(vec!["/newer prompt".into()], "/newer prompt", &[], false);
        assert_eq!(unknown[0].spans.len(), 1);
        assert_eq!(unknown[0].spans[0].style, crate::tui::theme::text());

        let advertised = vec![crate::tui::command::Command::new(
            "compact",
            "Compact context",
        )];
        let dynamic = prompt_lines(
            vec!["/compact prompt".into()],
            "/compact prompt",
            &advertised,
            false,
        );
        assert_eq!(dynamic[0].spans[0].content, "/compact");
        assert_eq!(dynamic[0].spans[0].style, crate::tui::theme::accent());
    }

    #[test]
    fn keeps_new_highlighted_when_the_real_editor_wraps_the_token() {
        let mut editor = crate::tui::editor::Editor::default();
        editor.insert_str("/new prompt");
        let (rows, _) = editor.wrapped(2);
        let lines = prompt_lines(rows, editor.text(), &[], false);
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
    fn highlights_file_paths_in_the_prompt() {
        let input = "review @src/tui/app.rs and name@example.com";
        let lines = prompt_lines(vec![input.into()], input, &[], false);
        let highlighted = lines[0]
            .spans
            .iter()
            .filter(|span| span.style == crate::tui::theme::accent())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(highlighted, "@src/tui/app.rs");
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
    fn click_metadata_matches_visible_rows() {
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

        let _ = render(&mut app, 50, 10);

        assert_eq!(app.row_links.len(), app.viewport);
        assert_eq!(app.row_calls.len(), app.viewport);
        assert_eq!(app.row_code.len(), app.viewport);
        assert!(
            app.row_links
                .iter()
                .flatten()
                .any(|hit| hit.url == "https://example.com/target")
        );

        app.scroll_by(-1000);
        let frame = render(&mut app, 50, 10);
        assert!(frame.contains("old row 0"), "{frame}");
        assert!(!frame.contains("visible link"), "{frame}");
        assert_eq!(app.row_links.len(), app.viewport);
        assert!(app.row_links.iter().all(Vec::is_empty));
    }

    #[test]
    fn tail_mutation_preserves_cached_history() {
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

        app.apply(Update::test_text(" changed".into()));
        refresh_transcript_cache(&mut app, 12);

        let history = app.transcript_cache[0].as_ref().unwrap();
        assert_eq!(history.rows.as_ptr(), history_rows);
        assert_eq!(history.revision, history_revision);
        assert_ne!(
            app.transcript_cache[99].as_ref().unwrap().rows.as_ptr(),
            tail_rows
        );
        let tail = app.transcript_cache[99].as_ref().unwrap();
        let text = tail
            .rows
            .iter()
            .map(|row| line_text(&row.0))
            .collect::<String>();
        assert!(text.contains("history 99"), "{text}");
        assert!(text.contains("changed"), "{text}");
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
    fn user_image_placeholders_preserve_explicit_text_newlines() {
        let image = UserImage::new("AQID".into(), "image/png".into(), 1).unwrap();
        let message = UserMessage {
            text: "before\n[Image #1](file:///tmp/image.png)\n\nafter\n".into(),
            images: vec![image],
        };

        let (rows, placements) = user_block_rows(&message, 40);

        assert!(placements.is_empty());
        assert_eq!(
            rows.iter().map(|row| line_text(&row.0)).collect::<Vec<_>>(),
            ["› before", "  Image #1", "  ", "  after", "  "]
        );
        assert_eq!(rows[1].2.len(), 1);
        assert_eq!(rows[1].2[0].url, "file:///tmp/image.png");
    }

    #[test]
    fn tool_images_render_with_shared_runtime_and_text_fallback() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(4, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let source = UserImage::new(
            base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
            "image/png".into(),
            0,
        )
        .unwrap();
        let mut app = App::new(
            PathBuf::from("/tmp"),
            "provider".into(),
            "model".into(),
            "a2a".into(),
        );
        app.apply(Update::ToolPatched {
            id: "tool".into(),
            title: Some("compose".into()),
            kind: None,
            status: None,
            script: None,
            output: Some(vec!["[Image]".into()]),
            images: Some(vec![source]),
            append_output: false,
            intent: None,
            backgrounded: false,
        });
        let mut images = ImageRuntime::with_picker(Picker::halfblocks());
        refresh_transcript_cache_with_images(&mut app, &mut images, 40);
        assert_eq!(app.transcript_cache[0].as_ref().unwrap().images.len(), 1);
        assert_eq!(images.cached_entries(), 0);
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .unwrap();
        assert_eq!(images.cached_entries(), 0);
        wait_for_image_decode(&mut images);
        terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .unwrap();
        assert_eq!(images.cached_entries(), 1);
        assert!(buffer_contains_black_image_cell(
            terminal.backend().buffer()
        ));

        let mut disabled = ImageRuntime::disabled();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut disabled))
            .unwrap();
        let cached = app.transcript_cache[0].as_ref().unwrap();
        assert!(cached.images.is_empty());
        assert!(
            cached
                .rows
                .iter()
                .any(|row| line_text(&row.0).contains("[Image: image/png]"))
        );
        if let Block::Tool(call) = &mut app.blocks[0] {
            call.expanded = false;
        }
        let (_, placements) = super::transcript_block_rows(&app, 0, 40, true);
        assert!(placements.is_empty());
    }

    #[test]
    fn user_images_remain_clickable_placeholders_with_image_runtime() {
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

        for width in [12, 40] {
            refresh_transcript_cache_with_images(&mut app, &mut images, width);
            let cached = app.transcript_cache[0].as_ref().unwrap();
            assert!(cached.images.is_empty());
            assert_eq!(cached.rows.len(), 1);
            assert_eq!(line_text(&cached.rows[0].0), "› Image #1");
            assert_eq!(cached.rows[0].2.len(), 1);
            assert_eq!(cached.rows[0].2[0].url, "file:///tmp/image.png");
            assert_eq!(app.transcript_prefixes.last().copied(), Some(1));
        }

        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .unwrap();
        assert!(!images.pending(), "user images must not queue decoding");
        assert_eq!(images.cached_entries(), 0);
        assert!(!buffer_contains_black_image_cell(
            terminal.backend().buffer()
        ));
        assert!(
            app.row_links
                .iter()
                .flatten()
                .any(|hit| hit.url == "file:///tmp/image.png")
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
        assert!(
            frame.contains("⏎ send   ⇧⏎ newline   ^r agents   ^t reasoning   ^l log   ^c quit")
        );
        assert!(frame.contains("message kit"));
    }

    mod canonical_group_tests {
        use super::super::*;
        use crate::tui::{app::Update, translate_for_session};
        use agent_client_protocol::schema::v2::{
            self as wire, SessionUpdate, UpdateSessionNotification,
        };
        use serde_json::json;

        fn app() -> App {
            App::new(
                std::path::PathBuf::from("/tmp"),
                "provider".into(),
                "model".into(),
                "a2a".into(),
            )
        }
        fn patch(app: &mut App, value: serde_json::Value) {
            let patch: wire::ToolCallUpdate = serde_json::from_value(value).unwrap();
            for update in translate_for_session(
                UpdateSessionNotification::new("session", SessionUpdate::ToolCallUpdate(patch)),
                "session",
            ) {
                app.apply(update);
            }
        }
        fn text(rows: &[CachedTranscriptRow]) -> String {
            rows.iter()
                .map(|row| {
                    row.0
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        fn refresh(app: &mut App) {
            refresh_transcript_cache_with_images(app, &mut ImageRuntime::disabled(), 100);
        }

        #[test]
        fn canonical_first_child_restores_implicit_expansion_but_not_explicit_collapse() {
            for explicitly_collapsed in [false, true] {
                let mut app = app();
                patch(&mut app, json!({"toolCallId":"root", "title":"compose"}));
                if explicitly_collapsed {
                    app.toggle_output("root");
                }
                // Exercise wire translation and its ordered ToolPatched / ToolParent
                // updates, including creation of the previously unseen child.
                patch(
                    &mut app,
                    json!({"toolCallId":"child", "title":"first canonical child", "_meta":{"kit/parentToolCallId":"root"}}),
                );
                refresh(&mut app);
                let root = &app.transcript_cache[0].as_ref().unwrap().rows;
                assert_eq!(
                    text(root).contains("first canonical child"),
                    !explicitly_collapsed
                );
                assert!(app.transcript_cache[1].as_ref().unwrap().rows.is_empty());
                assert!(app.transcript_call_is_focused(if explicitly_collapsed { 0 } else { 1 }));
                let Block::Tool(call) = &app.blocks[0] else {
                    panic!("root")
                };
                assert_eq!(call.expanded, !explicitly_collapsed);
                assert_eq!(call.expansion_explicit, explicitly_collapsed);
                if explicitly_collapsed {
                    app.toggle_output("root");
                    refresh(&mut app);
                    assert!(
                        text(&app.transcript_cache[0].as_ref().unwrap().rows)
                            .contains("first canonical child")
                    );
                }
            }
        }

        #[test]
        fn canonical_rows_attach_late_preserve_tags_images_and_absent_metadata() {
            let mut app = app();
            patch(
                &mut app,
                json!({"toolCallId":"child", "title":"unique-child", "_meta":{"kit/parentToolCallId":"root"}}),
            );
            refresh(&mut app);
            assert!(text(&app.transcript_cache[0].as_ref().unwrap().rows).contains("unique-child"));
            patch(
                &mut app,
                json!({"toolCallId":"unrelated", "title":"other-compose"}),
            );
            patch(&mut app, json!({"toolCallId":"root", "title":"compose"}));
            app.toggle_output("child");
            app.apply(Update::ToolPatched {
                id: "child".into(),
                title: None,
                kind: None,
                status: None,
                script: None,
                output: Some(vec!["live content".into()]),
                images: Some(vec![
                    crate::tui::app::UserImage::new("AQID".into(), "image/png".into(), 0).unwrap(),
                ]),
                append_output: false,
                intent: None,
                backgrounded: false,
            });
            refresh(&mut app);
            assert!(app.transcript_cache[0].as_ref().unwrap().rows.is_empty());
            assert_eq!(app.transcript_prefixes[0], app.transcript_prefixes[1]);
            let root = &app.transcript_cache[2].as_ref().unwrap().rows;
            assert!(text(root).contains("unique-child"));
            assert!(text(root).contains("live content"));
            assert!(root.iter().any(|row| row.1.0.as_deref() == Some("child")));
            let (_, images) = transcript_block_rows(&app, 2, 100, true);
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].block, Some(0));
            patch(
                &mut app,
                json!({"toolCallId":"child", "status":"completed", "_meta":{"unrelated":true}}),
            );
            assert_eq!(app.tool_owners.get(&0), Some(&2));
            patch(
                &mut app,
                json!({"toolCallId":"child", "rawOutput":"late content"}),
            );
            refresh(&mut app);
            assert!(text(&app.transcript_cache[2].as_ref().unwrap().rows).contains("late content"));
            app.toggle_output("root");
            refresh(&mut app);
            assert!(
                !text(&app.transcript_cache[2].as_ref().unwrap().rows).contains("unique-child")
            );
            patch(&mut app, json!({"toolCallId":"child", "_meta":null}));
            refresh(&mut app);
            assert!(text(&app.transcript_cache[0].as_ref().unwrap().rows).contains("unique-child"));
        }

        #[test]
        fn canonical_completed_owner_refreshes_for_running_child_and_pages_before_rows() {
            let mut app = app();
            patch(&mut app, json!({"toolCallId":"root", "title":"compose"}));
            for i in 0..70 {
                patch(
                    &mut app,
                    json!({"toolCallId":format!("child-{i}"), "title":format!("unique-{i:03}"), "_meta":{"kit/parentToolCallId":"root"}}),
                );
            }
            patch(&mut app, json!({"toolCallId":"root", "status":"completed"}));
            app.toggle_output("root");
            refresh(&mut app);
            let root = &app.transcript_cache[0].as_ref().unwrap().rows;
            assert!(text(root).contains("unique-069"));
            assert!(!text(root).contains("unique-000"));
            // Expanding the canonical child dirties its displayed owner even after
            // that owner is terminal; late output still remains on the same card.
            app.toggle_output("child-69");
            patch(
                &mut app,
                json!({"toolCallId":"child-69", "rawOutput":"still running"}),
            );
            refresh(&mut app);
            assert!(
                text(&app.transcript_cache[0].as_ref().unwrap().rows).contains("still running")
            );
            app.tick();
            refresh(&mut app);
            assert!(
                text(&app.transcript_cache[0].as_ref().unwrap().rows).contains("still running")
            );
            let (children, _, _) = app.child_window(0);
            assert_eq!(children.len(), 6);
            for index in 1..app.blocks.len() {
                assert!(
                    app.transcript_cache[index]
                        .as_ref()
                        .unwrap()
                        .rows
                        .is_empty()
                );
            }
        }

        #[test]
        fn canonical_selection_separates_children_but_rejoins_wrapped_source_lines() {
            use crate::tui::app::Selection;
            for (width, titles) in [
                (100, ["first-child", "second-child"]),
                (
                    24,
                    [
                        "first child with a genuinely wrapped long title",
                        "second child with another genuinely wrapped long title",
                    ],
                ),
            ] {
                let mut app = app();
                patch(&mut app, json!({"toolCallId":"root", "title":"compose"}));
                for (id, title) in ["first", "second"].into_iter().zip(titles) {
                    patch(
                        &mut app,
                        json!({"toolCallId":id, "title":title, "_meta":{"kit/parentToolCallId":"root"}}),
                    );
                }
                refresh_transcript_cache_with_images(
                    &mut app,
                    &mut ImageRuntime::disabled(),
                    width,
                );
                let rows = &app.transcript_cache[0].as_ref().unwrap().rows;
                let selected: Vec<_> = rows
                    .iter()
                    .enumerate()
                    .filter_map(|(i, row)| {
                        matches!(row.1.0.as_deref(), Some("first" | "second")).then_some(i)
                    })
                    .collect();
                if width == 24 {
                    assert!(selected.len() > 2, "exercise actual row wrapping");
                }
                app.selection = Some(Selection {
                    anchor: (selected[0], 0),
                    head: (*selected.last().unwrap(), width - 1),
                });
                let copied = app.selection_text().unwrap();
                let lines: Vec<_> = copied.lines().collect();
                assert_eq!(
                    lines.len(),
                    2,
                    "distinct canonical source headers: {copied}"
                );
                assert!(
                    lines[0].contains(titles[0]),
                    "first header rejoins: {copied}"
                );
                assert!(
                    lines[1].contains(titles[1]),
                    "second header rejoins: {copied}"
                );
            }
        }

        #[test]
        fn canonical_terminal_reference_keeps_parallel_raw_output() {
            let mut app = app();
            patch(
                &mut app,
                json!({"toolCallId":"child", "title":"shell", "content":[{"type":"terminal","terminalId":"terminal"}], "rawOutput":"useful terminal result"}),
            );
            let Block::Tool(call) = &app.blocks[0] else {
                panic!("tool")
            };
            assert_eq!(call.output, ["useful terminal result"]);
            patch(&mut app, json!({"toolCallId":"child", "content":[]}));
            let Block::Tool(call) = &app.blocks[0] else {
                panic!("tool")
            };
            assert!(call.output.is_empty());
        }

        #[test]
        fn canonical_foreign_session_metadata_is_not_applied() {
            let patch: wire::ToolCallUpdate = serde_json::from_value(
                json!({"toolCallId":"child", "_meta":{"kit/parentToolCallId":"root"}}),
            )
            .unwrap();
            assert!(
                translate_for_session(
                    UpdateSessionNotification::new("other", SessionUpdate::ToolCallUpdate(patch)),
                    "session"
                )
                .is_empty()
            );
        }
    }
}
