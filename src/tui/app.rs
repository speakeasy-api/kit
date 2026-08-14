//! Client state: the transcript, the live runtime graph, and key handling.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use agentkit_acp::{ToolCallStatus, ToolKind};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};

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
        preview: Vec<String>,
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
    pub preview: Vec<String>,
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
    pub toast: Option<(String, Instant)>,
}

impl App {
    pub fn new(root: PathBuf, model: String, a2a: String) -> Self {
        Self {
            root,
            model,
            a2a,
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
            toast: None,
        }
    }

    pub fn working(&self) -> bool {
        self.phase != Phase::Idle
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
                    preview: Vec::new(),
                }));
            }
            Update::ToolUpdated {
                id,
                status,
                script,
                preview,
            } => {
                let Some(call) = self.call_mut(&id) else {
                    return;
                };
                if let Some(script) = script {
                    call.plan = parse_plan(&script);
                }
                if !preview.is_empty() {
                    call.preview = preview;
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

    /// Applies a key press, returning work for the event loop.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
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
            KeyCode::Enter if key.modifiers.is_empty() => {
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
                if !self.editor.move_up() {
                    self.editor.history_prev();
                }
            }
            KeyCode::Down => {
                if !self.editor.move_down() {
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
            self.toast("still stopping…");
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
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agentkit_acp::{ToolCallStatus, ToolKind};

    use super::{App, Block, Update};
    use crate::events::RuntimeEvent;

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
