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
        CachedTranscriptRow, Child, CodeHit, ComposeView, EffortDialog, FilePickerDialog,
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

fn body_layout(area: Rect, show_agents: bool, transcript_empty: bool) -> (Rect, Option<Rect>) {
    if !show_agents || transcript_empty {
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
        app.transcript_prefixes[index + 1] =
            app.transcript_prefixes[index] + rows + usize::from(app.transcript_prefixes[index] > 0);
    }
    if layout_changed {
        app.clear_transcript_interaction();
    }
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
                placements.push(CachedTranscriptImage {
                    source,
                    row,
                    destination: None,
                });
            }
        }
    }
    (rows, placements)
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

fn transcript_block_rows(
    app: &App,
    block_index: usize,
    width: usize,
    reserve_images: bool,
) -> (Vec<CachedTranscriptRow>, Vec<CachedTranscriptImage>) {
    let block = &app.blocks[block_index];
    let (block_lines, call) = match block {
        Block::User(message) => return user_block_rows(message, width, reserve_images),
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
    let compose = call.is_compose();
    if call.running() {
        if compose && !call.script.is_empty() {
            lines.extend(script_lines(call));
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
    if compose {
        lines.extend(completed_compose_lines(call));
    } else {
        lines.extend(output_lines(call));
    }
    lines
}

/// Source lines stay neutral. Exact runtime call spans add qualified counts at
/// their source start line; annotations never assert whole-line/binding state.
fn script_lines(call: &ToolCall) -> Vec<Line<'static>> {
    let annotations = call.progress.labels(&call.script);
    let mut lines: Vec<_> = call
        .script
        .lines()
        .take(MAX_OUTPUT_ROWS)
        .enumerate()
        .map(|(index, source)| {
            let mut spans = vec![
                Span::styled("   │ ", theme::faint()),
                Span::styled(source.to_string(), theme::dim()),
            ];
            if let Some(labels) = annotations.get(&(index + 1)) {
                for label in labels {
                    spans.push(Span::styled(format!("  {label}"), theme::dim()));
                }
            }
            Line::from(spans)
        })
        .collect();
    let count = call.script.lines().count();
    if count > MAX_OUTPUT_ROWS {
        lines.push(Line::from(Span::styled(
            format!("   │ … {} more lines", count - MAX_OUTPUT_ROWS),
            theme::faint(),
        )));
    }
    lines
}

fn completed_compose_lines(call: &ToolCall) -> Vec<Line<'static>> {
    if !call.expanded {
        let mut counts = std::collections::HashMap::<&str, usize>::new();
        for child in &call.children {
            *counts.entry(child.tool.as_str()).or_default() += 1;
        }
        let mut counts = counts.into_iter().collect::<Vec<_>>();
        counts.sort_by(|(left_name, left_count), (right_name, right_count)| {
            right_count
                .cmp(left_count)
                .then_with(|| left_name.cmp(right_name))
        });
        if counts.is_empty() {
            return output_lines(call);
        }
        let summary = counts
            .into_iter()
            .take(4)
            .map(|(name, count)| {
                if count > 1 {
                    format!("{name} x {count}")
                } else {
                    name.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" · ");
        return vec![Line::from(vec![
            Span::styled("   ▸ ", theme::dim()),
            Span::styled(summary, theme::dim()),
            Span::styled("  click or ^o to open", theme::faint()),
        ])];
    }

    let (output_style, script_style, hint) = match call.compose_view {
        ComposeView::Output => (
            theme::bold(theme::accent_color()),
            theme::faint(),
            "  click or ^o for Script",
        ),
        ComposeView::Script => (
            theme::faint(),
            theme::bold(theme::accent_color()),
            "  click or ^o to close",
        ),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("   ", theme::faint()),
        Span::styled("Script", script_style),
        Span::styled("  ", theme::faint()),
        Span::styled("Output", output_style),
        Span::styled(hint, theme::faint()),
    ])];
    match call.compose_view {
        ComposeView::Output => lines.extend(expanded_output_lines(call)),
        ComposeView::Script => {
            lines.extend(script_lines(call));
        }
    }
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
            call.display_title().to_string(),
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
    } else if (call.expanded || call.running() || !call.is_compose()) && !call.children.is_empty() {
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

/// Task text keeps at least this many cells before the usage readout yields.
const MIN_AGENT_TASK_WIDTH: usize = 8;

fn agent_lines(
    tree_row: &AgentTreeRow<'_>,
    show_vendor: bool,
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
    let mut first = vec![
        Span::styled(first_prefix, theme::faint()),
        Span::styled(format!("{glyph} "), glyph_style),
    ];
    if show_vendor {
        let (mark, mark_style) = theme::vendor_mark(row.vendor);
        first.push(Span::styled(format!("{mark} "), mark_style));
    }
    first.push(Span::styled(row.name.clone(), theme::text()));
    first.push(Span::styled(ancestry, theme::faint()));
    let first = Line::from(first);

    let finished = row.generation_finished_at_unix_ms.unwrap_or(now_unix_ms);
    let mut tail = agent_duration(finished.saturating_sub(row.generation_started_at_unix_ms));
    let tree_prefix_width = UnicodeWidthStr::width(second_prefix.as_str());
    if let Some(usage) = row.usage {
        let with_usage = format!(
            "{} {}/{} · {tail}",
            percent(usage.used, usage.size),
            compact(usage.used),
            compact(usage.size)
        );
        let needed = tree_prefix_width
            + MIN_AGENT_TASK_WIDTH
            + 1
            + UnicodeWidthStr::width(with_usage.as_str());
        if needed <= width {
            tail = with_usage;
        }
    }
    let tail_full_width = UnicodeWidthStr::width(tail.as_str());
    let tail_width = tail_full_width.min(width);
    let displayed_tail = if tail_width < tail_full_width {
        visible_query_tail(&tail, tail_width).to_string()
    } else {
        tail
    };
    let prefix_width = width.saturating_sub(tail_width);
    let second = if prefix_width <= tree_prefix_width {
        Line::from(vec![
            Span::raw(" ".repeat(prefix_width)),
            Span::styled(displayed_tail, theme::faint()),
        ])
    } else {
        let task_width = prefix_width - tree_prefix_width - 1;
        let task = truncate_to_width(&row.task, task_width);
        let task_padding = task_width.saturating_sub(UnicodeWidthStr::width(task.as_str()));
        Line::from(vec![
            Span::styled(second_prefix, theme::faint()),
            Span::styled(task, theme::dim()),
            Span::raw(" ".repeat(task_padding + 1)),
            Span::styled(displayed_tail, theme::faint()),
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
    let selected = app
        .pending_steers
        .iter()
        .position(|pending| app.selected_steer.as_deref() == Some(pending.id.as_str()));
    let skip = if let Some(selected) = selected {
        selected.saturating_sub(visible - 1)
    } else if app.pending_steers.len() > visible && visible > 1 {
        let hidden = app.pending_steers.len() - (visible - 1);
        lines.push(Line::from(Span::styled(
            format!("  … {hidden} earlier pending"),
            theme::faint(),
        )));
        hidden
    } else {
        app.pending_steers.len().saturating_sub(visible)
    };
    lines.extend(
        app.pending_steers
            .iter()
            .skip(skip)
            .take(visible)
            .map(|pending| {
                let text = pending
                    .text
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                Line::from(vec![
                    Span::styled(
                        if app.selected_steer.as_deref() == Some(pending.id.as_str()) {
                            "  ▶ "
                        } else {
                            "  › "
                        },
                        theme::bold(theme::user_color()),
                    ),
                    Span::styled(text, theme::bold(theme::text_color())),
                    Span::styled(
                        if app.selected_steer.as_deref() == Some(pending.id.as_str()) {
                            format!(
                                "  · pending [{}/{}]",
                                selected.unwrap_or(0) + 1,
                                app.pending_steers.len()
                            )
                        } else {
                            "  · pending".to_owned()
                        },
                        theme::faint(),
                    ),
                ])
            }),
    );
    frame.render_widget(Paragraph::new(lines), area);
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
    let block = Panel::bordered()
        .title(if app.editing_steer() {
            " editing pending · Enter save · Esc cancel "
        } else {
            ""
        })
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
    } else if !app.pending_steers.is_empty() {
        "F2 queue   ⏎ send   ⇧⏎ newline "
    } else {
        "⏎ send   ⇧⏎ newline   ^l log   ^c quit "
    };
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
    fn agent_rows_render_two_lines_ancestry_duration_and_palette() {
        let top = test_agent(
            "Scout",
            SubagentStatus::Working,
            None,
            None,
            "Trace ACP lifecycle",
        );
        let lines = agent_lines(&tree_row(&top, vec![], false, false), false, 0, 74_000, 48);
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
        let lines = agent_lines(
            &tree_row(&nested, vec![true], false, false),
            false,
            0,
            74_000,
            48,
        );
        assert_eq!(line_text(&lines[0]), "│  └─ ⠁ Scout");
        assert!(line_text(&lines[1]).starts_with("│     Trace ACP lifecycle"));

        let lines = agent_lines(
            &tree_row(&nested, vec![false], true, false),
            false,
            0,
            74_000,
            48,
        );
        assert_eq!(line_text(&lines[0]), "   ├─ ⠁ Scout");
        assert!(line_text(&lines[1]).starts_with("   │  Trace ACP lifecycle"));

        let lines = agent_lines(&tree_row(&nested, vec![], true, true), false, 0, 74_000, 48);
        assert_eq!(line_text(&lines[0]), "⠁ Scout · via Pip");
        assert!(line_text(&lines[1]).starts_with("│ Trace ACP lifecycle"));
        assert_eq!(
            lines[0].spans[1].style.fg,
            Some(ratatui::style::Color::Yellow)
        );

        let idle = test_agent("Scout", SubagentStatus::Idle, None, None, "done");
        let idle_lines = agent_lines(&tree_row(&idle, vec![], false, false), false, 0, 74_000, 20);
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
        let failed_lines = agent_lines(
            &tree_row(&failed, vec![], false, false),
            false,
            0,
            74_000,
            20,
        );
        assert_eq!(line_text(&failed_lines[0]), "✗ Scout");
        assert_eq!(
            failed_lines[0].spans[1].style.fg,
            Some(ratatui::style::Color::Red)
        );
        assert_eq!(
            line_text(
                &agent_lines(
                    &tree_row(&failed, vec![], false, false),
                    false,
                    0,
                    77_000,
                    20,
                )[0]
            ),
            "○ Scout"
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
    fn agent_rows_show_usage_before_duration_and_drop_it_when_narrow() {
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
        assert_eq!(second, "  Trace ACP lifecycle    20.6% 41k/200k · 1m 12s");
        assert_eq!(unicode_width::UnicodeWidthStr::width(second.as_str()), 48);

        let lines = agent_lines(&tree_row(&row, vec![], false, false), false, 0, 74_000, 30);
        assert_eq!(line_text(&lines[1]), "  Trace ACP lifecycle   1m 12s");
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
        assert_eq!(line_text(&plain[0]), "○ Designer");
        let marked = agent_lines(&tree_row(&row, vec![], false, false), true, 0, 74_000, 48);
        assert_eq!(line_text(&marked[0]), "○ ✱ Designer");
        assert_eq!(
            marked[0].spans[2].style,
            super::theme::vendor_mark(crate::events::HarnessVendor::Claude).1
        );
    }

    #[test]
    fn agents_panel_marks_vendors_only_for_mixed_rosters() {
        let mut app = panel_app(1);
        let mut terminal = Terminal::new(TestBackend::new(46, 8)).expect("terminal");
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
        assert_eq!(buffer_cells(buffer, 3, 1..12), "○ ▙ Scout 0");
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
        let lines = agent_lines(&tree_row(&row, vec![], false, false), false, 0, 3_500, 12);
        assert_eq!(line_text(&lines[1]), "  🦀🦀… 1.5s");
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(line_text(&lines[1]).as_str()),
            12
        );

        let lines = agent_lines(&tree_row(&row, vec![], false, false), false, 0, 3_500, 4);
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(line_text(&lines[1]).as_str()),
            4
        );
    }

    #[test]
    fn agents_layout_obeys_hidden_and_107_108_boundaries() {
        let area_107 = ratatui::layout::Rect::new(2, 3, 107, 20);
        assert_eq!(body_layout(area_107, false, false), (area_107, None));
        assert_eq!(body_layout(area_107, true, true), (area_107, None));
        assert_eq!(
            body_layout(area_107, true, false),
            (
                ratatui::layout::Rect::new(2, 3, 107, 11),
                Some(ratatui::layout::Rect::new(2, 14, 107, 9)),
            )
        );

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

    include!("progress_tests.rs");

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
        assert!(first.contains("▶ queued text 0"), "{first}");
        assert!(first.contains("⏎ edit"), "{first}");
        assert!(first.contains("del remove"), "{first}");
        for _ in 0..7 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let last = render(&mut app, 100, 18);
        assert!(last.contains("▶ queued text 7"), "{last}");
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
    fn compose_script_stays_neutral_with_running_and_successful_calls() {
        let mut app = sample();

        let frame = render(&mut app, 120, 40);

        assert!(
            frame.contains("files = shell({ command: \"ls src\" })"),
            "{frame}"
        );
        assert!(frame.contains("1 in flight"), "{frame}");
        assert!(!frame.contains(" # "), "{frame}");
        assert!(!frame.contains("resolved"), "{frame}");
        assert!(!frame.contains("iteration 1 running"), "{frame}");
        assert!(!frame.contains("shell running"), "{frame}");
        assert!(app.transcript_width > 100, "{}", app.transcript_width);
    }

    #[test]
    fn compose_script_does_not_infer_failure_retry_or_waiting_state() {
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
        assert!(!frame.contains(" # "), "{frame}");
        assert!(!frame.contains("value failed"), "{frame}");
        assert!(!frame.contains("attempt 1"), "{frame}");
        assert!(!frame.contains("shell failure"), "{frame}");
        assert!(!frame.contains("later waiting"), "{frame}");
    }

    #[test]
    fn compose_script_does_not_attribute_descendants_to_a_dependent_review() {
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        let script = "a = subagent({name: \"implementation\", prompt: input.task})\n\
            r = subagent({name: \"review\", prompt: json.encode(a.output)})\n\
            return r";
        app.apply(Update::ToolStarted {
            id: "root".into(),
            title: "compose".into(),
            kind: ToolKind::Other,
            script: Some(script.into()),
            backgrounded: true,
        });
        for call in [
            "root:compose:implementation",
            "child:compose:storage",
            "child:compose:backfill",
            "child:compose:transport",
            "child:compose:tests",
        ] {
            app.apply(Update::Runtime(RuntimeEvent::ChildStarted {
                call: call.into(),
                tool: "subagent".into(),
                summary: "working".into(),
                at: 0,
            }));
        }
        let frame = render(&mut app, 160, 30);
        assert!(frame.contains("1 in flight"), "{frame}");
        let source_rows: Vec<_> = frame
            .lines()
            .filter_map(|line| line.split_once("│ ").map(|(_, source)| source.trim_end()))
            .collect();
        assert_eq!(source_rows, script.lines().collect::<Vec<_>>());
        assert!(!frame.contains("subagent: 2 running"), "{frame}");
        assert!(!frame.contains("subagent: 3 running"), "{frame}");
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
    fn collapsed_compose_groups_sorts_and_caps_child_tool_names() {
        let mut app = sample();
        let Block::Tool(call) = app.blocks.last_mut().expect("compose call") else {
            panic!("last block was not a tool");
        };
        for (index, tool) in [
            "shell", "docs", "shell", "alpha", "edit", "docs", "fork", "shell", "alpha",
        ]
        .into_iter()
        .enumerate()
        {
            call.attach(format!("extra-{index}"), tool.into(), tool.into());
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
            output: Some(vec!["done".into()]),
            backgrounded: false,
        });

        let frame = render(&mut app, 120, 35);
        let shell = frame.find("shell x 5").expect("shell summary");
        let alpha = frame.find("alpha x 2").expect("alpha summary");
        let docs = frame.find("docs x 2").expect("docs summary");
        let edit = frame.find("edit").expect("edit summary");
        assert!(shell < alpha && alpha < docs && docs < edit, "{frame}");
        assert!(!frame.contains("edit x 1"), "{frame}");
        assert!(!frame.contains("fork"), "{frame}");
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
        assert!(collapsed.contains("shell x 2"), "{collapsed}");
        assert!(!collapsed.contains("compose result"), "{collapsed}");
        assert!(!collapsed.contains("files = shell"), "{collapsed}");

        app.toggle_last_output();
        let output = render(&mut app, 100, 30);
        assert!(output.contains("Output"), "{output}");
        assert!(output.contains("Script"), "{output}");
        assert!(output.contains("compose result"), "{output}");
        assert!(!output.contains("files = shell"), "{output}");

        app.toggle_last_output();
        let script = render(&mut app, 100, 30);
        assert!(script.contains("Output"), "{script}");
        assert!(script.contains("Script"), "{script}");
        assert!(script.contains("files = shell"), "{script}");
        assert!(!script.contains("compose result"), "{script}");

        app.toggle_last_output();
        let collapsed_again = render(&mut app, 100, 30);
        assert!(collapsed_again.contains("shell x 2"), "{collapsed_again}");
        assert!(!collapsed_again.contains("Output"), "{collapsed_again}");
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
    fn non_compose_child_summary_requires_a_matching_parent() {
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

        assert!(!frame.contains("↳ cargo check"), "{frame}");

        app.apply(Update::Runtime(RuntimeEvent::ChildStarted {
            call: "call-1:compose:child".into(),
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
        assert_eq!(images.cached_entries(), 0, "visible image decode is queued");
        wait_for_image_decode(&mut images);
        terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .unwrap();
        assert_eq!(images.cached_entries(), 1, "visible image is rendered");
        assert!(buffer_contains_black_image_cell(
            terminal.backend().buffer()
        ));
        let reserved_rows = app.transcript_cache[0].as_ref().unwrap().rows.len();

        images.clear();
        assert_eq!(
            images.cached_entries(),
            0,
            "decoded image cache was evicted"
        );
        terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .unwrap();
        assert_eq!(images.cached_entries(), 0, "evicted image decode is queued");
        wait_for_image_decode(&mut images);
        terminal
            .draw(|frame| draw(frame, &mut app, &mut images))
            .unwrap();
        assert_eq!(
            images.cached_entries(),
            1,
            "an evicted visible image is rendered again after decoding"
        );
        assert!(buffer_contains_black_image_cell(
            terminal.backend().buffer()
        ));
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
