//! Client state: the transcript, the live runtime graph, and key handling.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use agentkit_acp::{ToolCallStatus, ToolKind};
use agentkit_core::{Item, ItemKind, Part, ToolOutput};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::events::RuntimeEvent;

use super::{
    editor::Editor,
    plan::{PlanNode, parse as parse_plan},
};

/// Everything the client learns from the agent or its own runtime channel.
#[derive(Debug)]
pub enum Update {
    /// A chunk of agent prose.
    Text(String),
    /// A chunk of agent reasoning.
    Thought(String),
    /// A tool call was announced.
    ToolStarted {
        id: String,
        title: String,
        kind: ToolKind,
        script: Option<String>,
    },
    /// A tool call changed status or produced output.
    ToolUpdated {
        id: String,
        status: Option<ToolCallStatus>,
        script: Option<String>,
        output: Vec<String>,
    },
    /// Context window accounting.
    Usage { used: u64, size: u64 },
    /// A nested tool call started or finished inside a compose run.
    Runtime(RuntimeEvent),
    /// A diagnostic line from the agent process.
    Log(String),
    /// The turn ended, with an error message when it failed.
    TurnEnded(Option<String>),
}

/// What the event loop should do after a key press.
pub enum Action {
    None,
    Submit(String),
    Cancel,
    Quit,
}

/// One nested tool dispatch inside a compose run.
pub struct Child {
    pub call: String,
    pub tool: String,
    pub summary: String,
    pub result: String,
    pub started: Instant,
    pub millis: Option<u64>,
    pub ok: bool,
    /// Plan node this dispatch was attributed to.
    pub node: Option<usize>,
}

impl Child {
    pub fn running(&self) -> bool {
        self.millis.is_none()
    }

    pub fn elapsed(&self) -> u64 {
        self.millis.unwrap_or_else(|| {
            u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
        })
    }
}

/// A model-visible tool call and, for compose, the program running inside it.
pub struct ToolCall {
    pub id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub plan: Vec<PlanNode>,
    pub children: Vec<Child>,
    /// Raw tool output, kept whole but folded away until asked for.
    pub output: Vec<String>,
    pub expanded: bool,
}

impl ToolCall {
    pub fn running(&self) -> bool {
        matches!(
            self.status,
            ToolCallStatus::Pending | ToolCallStatus::InProgress
        )
    }

    pub fn elapsed(&self) -> u64 {
        let end = self.finished.unwrap_or_else(Instant::now);
        u64::try_from(end.duration_since(self.started).as_millis()).unwrap_or(u64::MAX)
    }

    /// Records a nested dispatch against the plan node most likely to own it.
    ///
    /// Runlet's own node ids stay inside the runtime, so attribution goes by
    /// tool name and load: among the plan nodes calling this tool, the one
    /// carrying the fewest dispatches so far wins. That is exact for the
    /// common program shapes and otherwise degrades to a stable grouping when
    /// one tool is called from several places. Child lifecycle itself stays
    /// exact because start and finish are correlated by the runtime call id.
    pub fn attach(&mut self, call: String, tool: String, summary: String) {
        let node = self
            .plan
            .iter()
            .enumerate()
            .filter(|(_, node)| node.tool.as_deref() == Some(tool.as_str()))
            .map(|(index, _)| {
                let load = self
                    .children
                    .iter()
                    .filter(|child| child.node == Some(index))
                    .count();
                (load, index)
            })
            .min()
            .map(|(_, index)| index);
        self.children.push(Child {
            call,
            tool,
            summary,
            result: String::new(),
            started: Instant::now(),
            millis: None,
            ok: true,
            node,
        });
    }

    pub fn finish_child(&mut self, call: &str, ok: bool, summary: String, millis: u64) {
        if let Some(child) = self
            .children
            .iter_mut()
            .rev()
            .find(|child| child.running() && child.call == call)
        {
            child.millis = Some(millis);
            child.ok = ok;
            child.result = summary;
        }
    }

    pub fn running_children(&self) -> usize {
        self.children.iter().filter(|child| child.running()).count()
    }
}

/// One entry in the transcript.
pub enum Block {
    User(String),
    Agent(String),
    Thought {
        text: String,
        started: Instant,
        millis: Option<u64>,
    },
    Tool(ToolCall),
    Notice(String),
    Error(String),
}

/// What the client is doing right now.
#[derive(PartialEq, Eq)]
pub enum Phase {
    Idle,
    Working,
    Cancelling,
}

pub struct App {
    pub root: PathBuf,
    pub model: String,
    pub a2a: String,
    pub session_id: Option<String>,
    pub blocks: Vec<Block>,
    pub editor: Editor,
    pub phase: Phase,
    pub turn_started: Option<Instant>,
    pub usage: Option<(u64, u64)>,
    pub logs: Vec<String>,
    pub show_logs: bool,
    pub show_thoughts: bool,
    /// `None` shows the graph while a tool runs; `Some` pins it open or shut.
    pub graph_pinned: Option<bool>,
    pub tick: usize,
    pub scroll: usize,
    pub follow: bool,
    /// Rendered transcript height and total line count from the last frame.
    pub viewport: usize,
    pub total_lines: usize,
    /// Prompt field width from the last frame, for row-wise cursor movement.
    pub prompt_width: usize,
    /// Which tool call owns each rendered transcript row, and where that area
    /// started, so a click can be mapped back to a card.
    pub row_calls: Vec<Option<String>>,
    pub transcript_top: usize,
    pub transcript_left: usize,
    pub transcript_width: usize,
    pub toast: Option<(String, Instant)>,
    /// When the last key arrived, for telling a paste from typing.
    pub last_key: Option<Instant>,
}

/// A key arriving this soon after the previous one is machine-fast: pasted
/// text in a terminal that cannot bracket a paste, not someone typing. A
/// return in such a burst is a line break in the pasted text, not a send.
const PASTE_GAP: Duration = Duration::from_millis(8);

fn persisted_output(output: &ToolOutput) -> Vec<String> {
    let text = match output {
        ToolOutput::Text(text) => text.clone(),
        ToolOutput::Structured(value) => {
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        }
        ToolOutput::Parts(parts) => {
            serde_json::to_string_pretty(parts).unwrap_or_else(|_| format!("{} parts", parts.len()))
        }
        ToolOutput::Files(files) => format!("{} files", files.len()),
    };
    text.lines().map(str::to_string).collect()
}

impl App {
    pub fn new(root: PathBuf, model: String, a2a: String) -> Self {
        Self {
            root,
            model,
            a2a,
            session_id: None,
            blocks: Vec::new(),
            editor: Editor::default(),
            phase: Phase::Idle,
            turn_started: None,
            usage: None,
            logs: Vec::new(),
            show_logs: false,
            show_thoughts: false,
            graph_pinned: None,
            tick: 0,
            scroll: 0,
            follow: true,
            viewport: 0,
            total_lines: 0,
            prompt_width: 80,
            row_calls: Vec::new(),
            transcript_top: 0,
            transcript_left: 0,
            transcript_width: 0,
            toast: None,
            last_key: None,
        }
    }

    pub fn working(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// Rebuilds the visible history from the same Items preloaded into the model.
    pub fn restore_transcript(&mut self, session_id: String, transcript: &[Item]) {
        self.session_id = Some(session_id);
        for item in transcript {
            match item.kind {
                ItemKind::System | ItemKind::Developer | ItemKind::Context => continue,
                ItemKind::User | ItemKind::Notification => {
                    let text = item
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            Part::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.is_empty() {
                        if item.kind == ItemKind::User {
                            self.blocks.push(Block::User(text));
                        } else {
                            self.blocks.push(Block::Notice(text));
                        }
                    }
                }
                ItemKind::Assistant => {
                    for part in &item.parts {
                        match part {
                            Part::Text(text) if !text.text.is_empty() => {
                                self.blocks.push(Block::Agent(text.text.clone()))
                            }
                            Part::Reasoning(reasoning) if reasoning.summary.is_some() => {
                                self.blocks.push(Block::Thought {
                                    text: reasoning.summary.clone().unwrap_or_default(),
                                    started: Instant::now(),
                                    millis: Some(0),
                                })
                            }
                            Part::ToolCall(call) => self.blocks.push(Block::Tool(ToolCall {
                                id: call.id.to_string(),
                                title: call.name.clone(),
                                kind: ToolKind::Other,
                                status: ToolCallStatus::Completed,
                                started: Instant::now(),
                                finished: Some(Instant::now()),
                                plan: call
                                    .input
                                    .get("script")
                                    .and_then(serde_json::Value::as_str)
                                    .map(parse_plan)
                                    .unwrap_or_default(),
                                children: Vec::new(),
                                output: Vec::new(),
                                expanded: false,
                            })),
                            _ => {}
                        }
                    }
                }
                ItemKind::Tool => {
                    for part in &item.parts {
                        if let Part::ToolResult(result) = part
                            && let Some(call) = self.call_mut(&result.call_id.to_string())
                        {
                            call.output = persisted_output(&result.output);
                            if result.is_error {
                                call.status = ToolCallStatus::Failed;
                            }
                        }
                    }
                }
            }
        }
        self.follow = true;
        self.scroll = usize::MAX;
    }

    /// The tool call the graph pane should show: the running one, else the
    /// most recent, so a finished program stays readable.
    pub fn focus_call(&self) -> Option<&ToolCall> {
        let calls = self.blocks.iter().rev().filter_map(|block| match block {
            Block::Tool(call) => Some(call),
            _ => None,
        });
        let mut latest = None;
        for call in calls {
            if call.running() {
                return Some(call);
            }
            latest = latest.or(Some(call));
        }
        latest
    }

    pub fn show_graph(&self) -> bool {
        match self.graph_pinned {
            Some(pinned) => pinned,
            None => self.focus_call().is_some_and(ToolCall::running),
        }
    }

    pub fn elapsed(&self) -> u64 {
        self.turn_started.map_or(0, |started| {
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
        })
    }

    pub fn toast_text(&self) -> Option<&str> {
        self.toast
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(4))
            .map(|(text, _)| text.as_str())
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Text(text) => {
                self.close_thought();
                match self.blocks.last_mut() {
                    Some(Block::Agent(existing)) => existing.push_str(&text),
                    _ => self.blocks.push(Block::Agent(text)),
                }
            }
            Update::Thought(text) => match self.blocks.last_mut() {
                Some(Block::Thought {
                    text: existing,
                    millis: None,
                    ..
                }) => existing.push_str(&text),
                _ => self.blocks.push(Block::Thought {
                    text,
                    started: Instant::now(),
                    millis: None,
                }),
            },
            Update::ToolStarted {
                id,
                title,
                kind,
                script,
            } => {
                self.close_thought();
                self.blocks.push(Block::Tool(ToolCall {
                    id,
                    title,
                    kind,
                    status: ToolCallStatus::Pending,
                    started: Instant::now(),
                    finished: None,
                    plan: script.as_deref().map(parse_plan).unwrap_or_default(),
                    children: Vec::new(),
                    output: Vec::new(),
                    expanded: false,
                }));
            }
            Update::ToolUpdated {
                id,
                status,
                script,
                output,
            } => {
                let Some(call) = self.call_mut(&id) else {
                    return;
                };
                if let Some(script) = script {
                    call.plan = parse_plan(&script);
                }
                if !output.is_empty() {
                    call.output = output;
                }
                if let Some(status) = status {
                    call.status = status;
                    if !call.running() {
                        call.finished = Some(Instant::now());
                    }
                }
            }
            Update::Usage { used, size } => self.usage = Some((used, size)),
            Update::Runtime(event) => self.apply_runtime(event),
            Update::Log(line) => {
                self.logs.push(line);
                if self.logs.len() > 500 {
                    self.logs.drain(..self.logs.len() - 500);
                }
            }
            Update::TurnEnded(error) => {
                self.close_thought();
                let interrupted = self.phase == Phase::Cancelling;
                self.phase = Phase::Idle;
                self.turn_started = None;
                for block in &mut self.blocks {
                    if let Block::Tool(call) = block
                        && call.running()
                    {
                        call.status = ToolCallStatus::Completed;
                        call.finished = Some(Instant::now());
                    }
                }
                match (interrupted, error) {
                    (true, _) => self.note("turn interrupted"),
                    (false, Some(error)) => self.blocks.push(Block::Error(error)),
                    (false, None) => {}
                }
            }
        }
        if self.follow {
            self.scroll = usize::MAX;
        }
    }

    fn apply_runtime(&mut self, event: RuntimeEvent) {
        let parent = event.parent_call().map(str::to_string);
        let call = match parent.and_then(|parent| self.call_mut(&parent)) {
            Some(call) => call,
            // Compose runs started by a subagent report against a call this
            // client never saw; fold them into the visible run instead.
            None => match self.running_call_mut() {
                Some(call) => call,
                None => return,
            },
        };
        match event {
            RuntimeEvent::ChildStarted {
                call: child_call,
                tool,
                summary,
                ..
            } => call.attach(child_call, tool, summary),
            RuntimeEvent::ChildFinished {
                call: child_call,
                ok,
                summary,
                millis,
                ..
            } => call.finish_child(&child_call, ok, summary, millis),
        }
    }

    fn call_mut(&mut self, id: &str) -> Option<&mut ToolCall> {
        self.blocks.iter_mut().rev().find_map(|block| match block {
            Block::Tool(call) if call.id == id => Some(call),
            _ => None,
        })
    }

    fn running_call_mut(&mut self) -> Option<&mut ToolCall> {
        self.blocks.iter_mut().rev().find_map(|block| match block {
            Block::Tool(call) if call.running() => Some(call),
            _ => None,
        })
    }

    fn close_thought(&mut self) {
        if let Some(Block::Thought {
            started,
            millis: millis @ None,
            ..
        }) = self.blocks.last_mut()
        {
            *millis = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        }
    }

    pub fn push_user(&mut self, prompt: String) {
        self.blocks.push(Block::User(prompt));
        self.phase = Phase::Working;
        self.turn_started = Some(Instant::now());
        self.follow = true;
        self.scroll = usize::MAX;
    }

    /// Folds a tool call's raw output open or shut.
    pub fn toggle_output(&mut self, id: &str) {
        if let Some(call) = self.call_mut(id) {
            call.expanded = !call.expanded;
        }
    }

    /// Folds the most recent tool call, for keyboard use.
    pub fn toggle_last_output(&mut self) {
        if let Some(id) = self.blocks.iter().rev().find_map(|block| match block {
            Block::Tool(call) => Some(call.id.clone()),
            _ => None,
        }) {
            self.toggle_output(&id);
        }
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::Notice(text.into()));
    }

    pub fn scroll_by(&mut self, lines: isize) {
        let top = self.total_lines.saturating_sub(self.viewport);
        let current = self.scroll.min(top);
        self.scroll = current.saturating_add_signed(lines).min(top);
        self.follow = self.scroll >= top;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.follow = true;
        self.scroll = usize::MAX;
    }

    /// Inserts pasted text into the prompt.
    ///
    /// A paste never sends: the newlines in it are part of the text. Multi-line
    /// pastes say so, because the prompt box shows only its last rows and the
    /// rest is easy to miss.
    pub fn paste(&mut self, text: &str) {
        self.editor.insert_str(text);
        let lines = text.lines().count();
        if lines > 1 {
            self.toast(format!("pasted {lines} lines"));
        }
    }

    /// Applies a key press, returning work for the event loop.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        // Terminals without bracketed paste deliver a paste as a key burst, so
        // the arrival gap is the only thing separating it from typing.
        let pasted = self.last_key.is_some_and(|last| last.elapsed() < PASTE_GAP);
        self.last_key = Some(Instant::now());
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        // `cmd` only reaches the client in terminals that speak the Kitty
        // keyboard protocol; the control equivalents cover the rest.
        let command = key.modifiers.contains(KeyModifiers::SUPER);
        let word = alt || control;
        let line = command || control;

        match key.code {
            KeyCode::Char('c') if control => {
                // A turn that will not stop must still be escapable: the second
                // ctrl+c leaves, which takes the agent process with it.
                if self.phase == Phase::Cancelling {
                    return Action::Quit;
                }
                if self.working() {
                    return self.request_cancel();
                }
                // A stray ctrl+c should not throw away a half-written prompt.
                if self.editor.text().is_empty() {
                    return Action::Quit;
                }
                self.editor.clear();
                self.toast("prompt cleared — ctrl+c again to quit");
            }
            KeyCode::Char('d') if control && self.editor.is_empty() => return Action::Quit,
            KeyCode::Esc => {
                if self.working() {
                    return self.request_cancel();
                }
                self.toast = None;
            }
            KeyCode::Enter if key.modifiers.is_empty() && !pasted => {
                if self.editor.is_empty() {
                    return Action::None;
                }
                if self.working() {
                    self.toast("a turn is already running — esc interrupts it");
                    return Action::None;
                }
                return Action::Submit(self.editor.submit());
            }
            KeyCode::Enter => self.editor.insert_char('\n'),
            KeyCode::Char('j') if control => self.editor.insert_char('\n'),
            KeyCode::Tab => self.editor.insert_str("    "),
            KeyCode::Backspace if command => self.editor.delete_to_line_start(),
            KeyCode::Backspace if alt || control => self.editor.delete_word_back(),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete if word => self.editor.delete_word_forward(),
            KeyCode::Delete => self.editor.delete_forward(),
            KeyCode::Char('w') if control => self.editor.delete_word_back(),
            KeyCode::Char('u') if control => self.editor.delete_to_line_start(),
            KeyCode::Char('k') if control => self.editor.delete_to_line_end(),
            KeyCode::Char('d') if alt => self.editor.delete_word_forward(),
            KeyCode::Char('a') if control => self.editor.move_line_start(),
            KeyCode::Char('e') if control => self.editor.move_line_end(),
            KeyCode::Char('g') if control => {
                self.graph_pinned = Some(!self.show_graph());
            }
            KeyCode::Char('l') if control => self.show_logs = !self.show_logs,
            KeyCode::Char('o') if control => self.toggle_last_output(),
            KeyCode::Char('t') if control => self.show_thoughts = !self.show_thoughts,
            KeyCode::Left if line => self.editor.move_line_start(),
            KeyCode::Right if line => self.editor.move_line_end(),
            KeyCode::Left if alt => self.editor.move_word_left(),
            KeyCode::Right if alt => self.editor.move_word_right(),
            KeyCode::Left => self.editor.move_left(),
            KeyCode::Right => self.editor.move_right(),
            KeyCode::Up if shift => self.scroll_by(-1),
            KeyCode::Down if shift => self.scroll_by(1),
            KeyCode::Up => {
                if !self.editor.move_row_up(self.prompt_width) {
                    self.editor.history_prev();
                }
            }
            KeyCode::Down => {
                if !self.editor.move_row_down(self.prompt_width) {
                    self.editor.history_next();
                }
            }
            KeyCode::Home if control => self.scroll = 0,
            KeyCode::End if control => self.scroll_to_bottom(),
            KeyCode::Home => self.editor.move_line_start(),
            KeyCode::End => self.editor.move_line_end(),
            KeyCode::PageUp => self.scroll_by(-(self.viewport.max(2) as isize - 1)),
            KeyCode::PageDown => self.scroll_by(self.viewport.max(2) as isize - 1),
            KeyCode::Char(character) if !control && !command => self.editor.insert_char(character),
            _ => {}
        }
        if self.follow {
            self.scroll = usize::MAX;
        }
        Action::None
    }

    fn request_cancel(&mut self) -> Action {
        if self.phase == Phase::Cancelling {
            self.toast("still stopping — ctrl+c leaves");
            return Action::None;
        }
        self.phase = Phase::Cancelling;
        self.toast("interrupting the turn");
        Action::Cancel
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::Down(MouseButton::Left) => {
                self.click(mouse.column as usize, mouse.row as usize);
            }
            _ => {}
        }
    }

    /// A click on a tool card folds its output open or shut.
    fn click(&mut self, column: usize, row: usize) {
        let Some(offset) = row.checked_sub(self.transcript_top) else {
            return;
        };
        let inside = offset < self.viewport
            && column >= self.transcript_left
            && column < self.transcript_left + self.transcript_width;
        if !inside {
            return;
        }
        if let Some(Some(id)) = self.row_calls.get(self.scroll + offset).cloned() {
            self.toggle_output(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };

    use agentkit_acp::{ToolCallStatus, ToolKind};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::{Action, App, Block, Update};
    use crate::events::RuntimeEvent;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn app() -> App {
        App::new(
            PathBuf::from("/tmp"),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        )
    }

    fn compose(app: &mut App, script: &str) {
        app.apply(Update::ToolStarted {
            id: "call-1".into(),
            title: "compose".into(),
            kind: ToolKind::Other,
            script: Some(script.into()),
        });
    }

    fn child(call: &str, tool: &str) -> RuntimeEvent {
        RuntimeEvent::ChildStarted {
            call: call.into(),
            tool: tool.into(),
            summary: "ls".into(),
            at: 0,
        }
    }

    #[test]
    fn attributes_nested_calls_to_the_owning_tool_call() {
        let mut app = app();
        compose(&mut app, "a = shell({ command: \"ls\" })\nreturn a");
        app.apply(Update::Runtime(child("call-1:compose:abc", "shell")));
        let Some(Block::Tool(call)) = app.blocks.last() else {
            panic!("expected a tool block");
        };
        assert_eq!(call.children.len(), 1);
        assert_eq!(call.children[0].node, Some(0));
        assert_eq!(call.running_children(), 1);
    }

    #[test]
    fn spreads_repeated_dispatches_across_matching_plan_nodes() {
        let mut app = app();
        compose(
            &mut app,
            "a = shell({ command: \"one\" })\nb = shell({ command: \"two\" })\nreturn [a, b]",
        );
        app.apply(Update::Runtime(child("call-1:compose:a", "shell")));
        app.apply(Update::Runtime(child("call-1:compose:b", "shell")));
        let Some(Block::Tool(call)) = app.blocks.last() else {
            panic!("expected a tool block");
        };
        let nodes: Vec<_> = call.children.iter().map(|child| child.node).collect();
        assert_eq!(nodes, [Some(0), Some(1)]);
    }

    #[test]
    fn closes_running_calls_when_the_turn_ends() {
        let mut app = app();
        compose(&mut app, "a = shell({ command: \"ls\" })\nreturn a");
        app.apply(Update::TurnEnded(None));
        let Some(Block::Tool(call)) = app.blocks.last() else {
            panic!("expected a tool block");
        };
        assert_eq!(call.status, ToolCallStatus::Completed);
        assert!(!app.working());
    }

    #[test]
    fn a_paste_becomes_prompt_text_rather_than_a_send() {
        let mut app = app();
        app.paste("first line\nsecond line");
        assert_eq!(app.editor.text(), "first line\nsecond line");
        assert_eq!(app.toast_text(), Some("pasted 2 lines"));
    }

    #[test]
    fn a_return_inside_a_key_burst_breaks_the_line() {
        let mut app = app();
        app.paste("first line");
        app.last_key = Some(Instant::now());
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert_eq!(app.editor.text(), "first line\n");
    }

    #[test]
    fn a_return_after_a_pause_still_sends() {
        let mut app = app();
        app.paste("first line");
        app.last_key = Some(Instant::now() - Duration::from_millis(500));
        let Action::Submit(prompt) = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected the prompt to be sent");
        };
        assert_eq!(prompt, "first line");
    }

    #[test]
    fn streams_agent_text_into_one_block() {
        let mut app = app();
        app.apply(Update::Text("he".into()));
        app.apply(Update::Text("llo".into()));
        assert_eq!(app.blocks.len(), 1);
        let Some(Block::Agent(text)) = app.blocks.last() else {
            panic!("expected an agent block");
        };
        assert_eq!(text, "hello");
    }
}
