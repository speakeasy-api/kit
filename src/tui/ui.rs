//! Frame drawing: header, transcript, runtime graph, prompt, status.

use agentkit_acp::{ToolCallStatus, ToolKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block as Panel, BorderType, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    },
};

use super::{
    app::{App, Block, Child, Phase, ToolCall},
    markdown,
    plan::PlanKind,
    theme,
    wrap::{wrap, wrap_tagged},
};

/// Width at which the graph moves beside the transcript instead of below it.
const SIDE_BY_SIDE_WIDTH: u16 = 108;
const GRAPH_WIDTH: u16 = 46;
const MAX_PROMPT_ROWS: usize = 10;
/// Rows of raw tool output rendered when a card is opened.
const MAX_OUTPUT_ROWS: usize = 400;

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
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
    let [header, body, logs, prompt, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(logs_rows),
        Constraint::Length(prompt_rows),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    draw_body(frame, app, body);
    if app.show_logs {
        draw_logs(frame, app, logs);
    }
    draw_prompt(frame, app, prompt);
    draw_status(frame, app, status);
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let root = app.root.file_name().map_or_else(
        || app.root.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut spans = vec![
        Span::styled(" kit ", theme::bold(theme::ACCENT)),
        Span::styled("▏ ", theme::faint()),
        Span::styled(root, theme::text()),
        Span::styled("  ·  ", theme::faint()),
        Span::styled(app.model.clone(), theme::dim()),
        Span::styled("  ·  ", theme::faint()),
        Span::styled(
            format!(
                "session {}",
                app.session_id.as_deref().unwrap_or("starting")
            ),
            theme::dim(),
        ),
        Span::styled("  ·  ", theme::faint()),
        Span::styled(format!("a2a {}", app.a2a), theme::dim()),
    ];
    if let Some((used, size)) = app.usage {
        spans.push(Span::styled("  ·  ", theme::faint()));
        spans.push(Span::styled(
            format!("ctx {}/{}", compact(used), compact(size)),
            theme::dim(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme::bar()), area);
}

fn draw_body(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if !app.show_graph() {
        draw_transcript(frame, app, area);
        return;
    }
    if area.width >= SIDE_BY_SIDE_WIDTH {
        let [transcript, graph] =
            Layout::horizontal([Constraint::Min(40), Constraint::Length(GRAPH_WIDTH)]).areas(area);
        draw_transcript(frame, app, transcript);
        draw_graph(frame, app, graph);
    } else {
        let [transcript, graph] =
            Layout::vertical([Constraint::Min(6), Constraint::Percentage(45)]).areas(area);
        draw_transcript(frame, app, transcript);
        draw_graph(frame, app, graph);
    }
}

fn draw_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let [text_area, bar_area] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let inner = Rect {
        x: text_area.x + 1,
        width: text_area.width.saturating_sub(2),
        ..text_area
    };
    if app.blocks.is_empty() {
        frame.render_widget(welcome(), inner);
        return;
    }

    let lines = wrap_tagged(&transcript_lines(app), inner.width.max(1) as usize);
    let total = lines.len();
    let height = inner.height as usize;
    app.total_lines = total;
    app.viewport = height;
    app.transcript_top = inner.y as usize;
    app.transcript_left = inner.x as usize;
    app.transcript_width = inner.width as usize;
    app.row_calls = lines.iter().map(|(_, call)| call.clone()).collect();
    let bottom = total.saturating_sub(height);
    let offset = if app.follow {
        bottom
    } else {
        app.scroll.min(bottom)
    };
    app.scroll = offset;

    let visible: Vec<Line<'static>> = lines
        .into_iter()
        .skip(offset)
        .take(height)
        .map(|(line, _)| line)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
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

fn welcome() -> Paragraph<'static> {
    let lines = vec![
        Line::default(),
        Line::from(Span::styled("kit", theme::bold(theme::ACCENT))),
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
fn transcript_lines(app: &App) -> Vec<(Line<'static>, Option<String>)> {
    let mut lines: Vec<(Line<'static>, Option<String>)> = Vec::new();
    for block in &app.blocks {
        if !lines.is_empty() {
            lines.push((Line::default(), None));
        }
        let (block_lines, call) = match block {
            Block::User(text) => (user_lines(text), None),
            Block::Agent(text) => (markdown::render(text), None),
            Block::Thought {
                text,
                started,
                millis,
            } => (
                thought_lines(app, text, started.elapsed().as_millis(), *millis),
                None,
            ),
            Block::Tool(call) => (tool_lines(app, call), Some(call.id.clone())),
            Block::Notice(text) => (
                vec![Line::from(Span::styled(
                    format!("· {text}"),
                    theme::faint(),
                ))],
                None,
            ),
            Block::Error(text) => (
                vec![Line::from(vec![
                    Span::styled("✗ ", theme::bold(theme::ERROR)),
                    Span::styled(text.clone(), Style::default().fg(theme::ERROR)),
                ])],
                None,
            ),
        };
        lines.extend(block_lines.into_iter().map(|line| (line, call.clone())));
    }
    if app.working() {
        lines.push((Line::default(), None));
        lines.push((working_line(app), None));
    }
    lines
}

fn user_lines(text: &str) -> Vec<Line<'static>> {
    text.split('\n')
        .enumerate()
        .map(|(index, line)| {
            Line::from(vec![
                Span::styled(
                    if index == 0 { "› " } else { "  " },
                    theme::bold(theme::USER),
                ),
                Span::styled(line.to_string(), theme::bold(theme::USER)),
            ])
        })
        .collect()
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

fn tool_lines(app: &App, call: &ToolCall) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(tool_header(app, call))];
    if call.running() {
        // The live detail belongs in the graph pane, which is open while a
        // call runs; repeating it here would just churn the transcript.
        if !app.show_graph()
            && let Some(child) = call.children.iter().rev().find(|child| child.running())
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

fn tool_header(app: &App, call: &ToolCall) -> Vec<Span<'static>> {
    let (glyph, style) = match call.status {
        _ if call.running() => (
            theme::pulse(theme::Pulse::Tool, app.tick).to_string(),
            theme::bold(theme::RUNNING),
        ),
        ToolCallStatus::Failed => ("✗".into(), theme::bold(theme::ERROR)),
        _ => ("✓".into(), theme::bold(theme::SUCCESS)),
    };
    let mut spans = vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(call.title.clone(), theme::bold(theme::TEXT)),
        Span::styled(kind_label(call.kind).to_string(), theme::faint()),
        Span::styled(
            format!("  {}", theme::duration(call.elapsed())),
            theme::dim(),
        ),
    ];
    let running = call.running_children();
    if running > 0 {
        spans.push(Span::styled(
            format!("  · {running} in flight"),
            Style::default().fg(theme::RUNNING),
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
const fn kind_label(kind: ToolKind) -> &'static str {
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
            Style::default().fg(theme::RUNNING),
        )
    } else if child.ok {
        ("✓".into(), Style::default().fg(theme::SUCCESS))
    } else {
        ("✗".into(), Style::default().fg(theme::ERROR))
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
        _ if app.focus_call().is_some_and(ToolCall::running) => "running tools",
        _ => "thinking",
    };
    Line::from(vec![
        Span::styled(
            format!("{} ", theme::pulse(theme::Pulse::Turn, app.tick)),
            theme::bold(theme::ACCENT),
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
        Span::styled(call.title.clone(), theme::bold(theme::TEXT)),
        Span::styled(
            format!("  {}", theme::duration(call.elapsed())),
            theme::dim(),
        ),
    ])];
    let done = call.children.len() - call.running_children();
    let failed = call.children.iter().filter(|child| !child.ok).count();
    lines.push(Line::from(Span::styled(
        format!(
            "{} nodes · {done} done · {} running{}",
            call.plan.len(),
            call.running_children(),
            if failed > 0 {
                format!(" · {failed} failed")
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
        return theme::bold(theme::RUNNING);
    }
    if attached.iter().any(|child| !child.ok) {
        return Style::default().fg(theme::ERROR);
    }
    if !attached.is_empty() {
        return Style::default().fg(theme::SUCCESS);
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

fn draw_prompt(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let border = if app.working() {
        theme::faint()
    } else {
        Style::default().fg(theme::ACCENT)
    };
    let block = Panel::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [gutter, field] =
        Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Span::styled("›", theme::bold(theme::ACCENT))),
        gutter,
    );

    let height = field.height as usize;
    let (rows, (cursor_row, cursor_column)) = app.editor.wrapped(field.width as usize);
    // Keep the cursor's row on screen when the prompt is taller than the box.
    let first = cursor_row.saturating_sub(height.saturating_sub(1));
    let lines: Vec<Line<'static>> = if app.editor.text().is_empty() {
        vec![Line::from(Span::styled("message kit…", theme::faint()))]
    } else {
        rows.into_iter()
            .skip(first)
            .take(height)
            .map(|row| Line::from(Span::styled(row, theme::text())))
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

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut left = match app.phase {
        Phase::Idle => vec![Span::styled(" ready", theme::dim())],
        Phase::Cancelling => vec![
            Span::styled(
                format!(" {} ", theme::pulse(theme::Pulse::Status, app.tick)),
                theme::bar(),
            ),
            Span::styled("stopping", Style::default().fg(theme::WARN)),
        ],
        Phase::Working => vec![
            Span::styled(
                format!(" {} ", theme::pulse(theme::Pulse::Status, app.tick)),
                Style::default().fg(theme::ACCENT).bg(theme::BAR_BG),
            ),
            Span::styled(
                format!("working {}", theme::duration(app.elapsed())),
                Style::default().fg(theme::ACCENT).bg(theme::BAR_BG),
            ),
        ],
    };
    if let Some(toast) = app.toast_text() {
        left.push(Span::styled("  ", theme::bar()));
        left.push(Span::styled(
            toast.to_string(),
            Style::default().fg(theme::WARN).bg(theme::BAR_BG),
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

    use agentkit_acp::ToolKind;
    use ratatui::{Terminal, backend::TestBackend};

    use super::{MAX_PROMPT_ROWS, draw};
    use crate::{
        events::RuntimeEvent,
        tui::app::{App, Update},
    };

    const SCRIPT: &str = "files = shell({ command: \"ls src\" })\n\
        checked = for file in files.lines {\n\
            return shell({ command: \"cargo check\" })\n\
        }\n\
        return checked";

    fn sample() -> App {
        let mut app = App::new(
            PathBuf::from("/Users/dev/projects/kit"),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        );
        app.push_user("check every source file".into());
        app.apply(Update::Text(
            "Reading the tree first.\n\n- one\n- two\n\n```sh\ncargo check\n```".into(),
        ));
        app.apply(Update::ToolStarted {
            id: "call-1".into(),
            title: "compose".into(),
            kind: ToolKind::Other,
            script: Some(SCRIPT.into()),
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
        terminal
            .draw(|frame| draw(frame, app))
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
    fn scrolls_back_through_a_long_transcript_with_the_log_pane_open() {
        let mut app = sample();
        app.apply(Update::ToolUpdated {
            id: "call-1".into(),
            status: Some(agentkit_acp::ToolCallStatus::Failed),
            script: None,
            output: vec!["exit code 1".into()],
        });
        app.apply(Update::TurnEnded(Some("model refused the request".into())));
        app.apply(Update::Log("warn: retrying provider request".into()));
        app.show_logs = true;
        for index in 0..12 {
            app.push_user(format!("follow-up number {index}"));
            app.apply(Update::Text(format!("answer number {index}")));
        }
        app.apply(Update::TurnEnded(None));
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
        let mut app = App::new(PathBuf::from("/tmp/kit"), "gpt-5.4".into(), "0:0".into());
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
    fn folds_raw_tool_output_until_the_card_is_clicked() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mut app = sample();
        app.apply(Update::ToolUpdated {
            id: "call-1".into(),
            status: Some(agentkit_acp::ToolCallStatus::Completed),
            script: None,
            output: (0..40)
                .map(|index| format!("output line {index}"))
                .collect(),
        });
        let frame = render(&mut app, 100, 24);
        println!("{frame}");
        assert!(frame.contains("40 lines of output"));
        assert!(!frame.contains("output line 39"));

        let row = frame
            .lines()
            .position(|line| line.contains("lines of output"))
            .expect("fold row is on screen");
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 6,
            row: u16::try_from(row).unwrap(),
            modifiers: KeyModifiers::NONE,
        });
        let frame = render(&mut app, 100, 24);
        println!("{frame}");
        assert!(frame.contains("output line 39"));
    }

    #[test]
    fn shows_the_welcome_screen_before_the_first_prompt() {
        let mut app = App::new(PathBuf::from("/tmp/kit"), "gpt-5.4".into(), "0:0".into());
        let frame = render(&mut app, 90, 24);
        println!("{frame}");
        assert!(frame.contains("send"));
        assert!(frame.contains("message kit"));
    }
}
