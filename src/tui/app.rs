//! Client state: the transcript, the live runtime graph, and key handling.

use std::{
    cmp::Reverse,
    collections::BTreeSet,
    ops::Range,
    path::PathBuf,
    time::{Duration, Instant},
};

#[cfg(any(test, target_os = "linux"))]
use std::ffi::OsStr;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Command, Stdio};

use agentkit_acp::{ToolCallStatus, ToolKind};
use agentkit_core::{DataRef, Item, ItemKind, Modality, Part, ToolOutput};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::text::Line;

use crate::{compaction::is_compaction_summary, events::RuntimeEvent};

use super::{
    command::{Parsed, parse},
    editor::Editor,
    plan::{PlanNode, parse as parse_plan},
    wrap::LinkHit,
};

/// Everything the client learns from the agent or its own runtime channel.
#[derive(Debug)]
pub enum Update {
    /// The actual dynamically allocated A2A listen address.
    A2aAddress(String),
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
        backgrounded: bool,
    },
    /// A tool call changed status or produced output.
    ToolUpdated {
        id: String,
        status: Option<ToolCallStatus>,
        script: Option<String>,
        output: Vec<String>,
        backgrounded: bool,
    },
    /// Agent-advertised slash commands for one session.
    AvailableCommands {
        session_id: String,
        commands: Vec<String>,
    },
    /// Context window accounting.
    Usage { used: u64, size: u64 },
    /// A nested tool call started or finished inside a compose run.
    Runtime(RuntimeEvent),
    /// A diagnostic line from the agent process.
    Log(String),
    /// An autonomous turn started after a background result arrived.
    AutonomousTurnStarted(u64),
    /// An autonomous turn ended.
    AutonomousTurnEnded { id: u64, error: Option<String> },
    /// A specific submitted turn ended. `None` identifies a process-wide failure.
    TurnEnded {
        id: Option<u64>,
        error: Option<String>,
    },
}

/// Latest provider-reported occupancy of the main model's context window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsage {
    pub used: u64,
    pub size: u64,
}

#[derive(Clone)]
pub struct CodeHit {
    pub block: usize,
    pub range: Range<usize>,
}

pub(super) type CachedTranscriptRow = (
    Line<'static>,
    (Option<String>, Option<CodeHit>),
    Vec<LinkHit>,
);

/// What the event loop should do after a key press.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelChoice {
    pub id: String,
    pub provider: String,
    pub model: String,
}

pub struct ModelDialog {
    pub query: String,
    pub selected: usize,
    pub save_defaults: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffortChoice {
    pub id: String,
    pub name: String,
}

pub struct EffortDialog {
    pub selected: usize,
    pub save_defaults: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    Audio,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub path: PathBuf,
    pub placeholder: String,
    pub mime_type: &'static str,
    pub kind: AttachmentKind,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmittedPrompt {
    pub text: String,
    pub attachments: Vec<Attachment>,
}

pub enum Action {
    None,
    Redraw,
    Submit(SubmittedPrompt),
    New(Option<String>),
    SelectModel {
        choice: ModelChoice,
        save_defaults: bool,
    },
    SelectEffort {
        effort: String,
        save_defaults: bool,
    },
    Copy(String),
    Cancel,
    CancelBackground(String),
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
    /// The call detached from its originating turn.
    pub backgrounded: bool,
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

    fn finish_running_children(&mut self) {
        let ok = self.status != ToolCallStatus::Failed;
        for child in self.children.iter_mut().filter(|child| child.running()) {
            child.millis = Some(child.elapsed());
            child.ok = ok;
        }
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

pub(super) struct CachedTranscriptBlock {
    pub revision: u64,
    pub rows: Vec<CachedTranscriptRow>,
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
    pub provider: String,
    pub model: String,
    pub model_choices: Vec<ModelChoice>,
    pub model_dialog: Option<ModelDialog>,
    pub reasoning_effort: String,
    pub effort_choices: Vec<EffortChoice>,
    pub effort_dialog: Option<EffortDialog>,
    pub available_commands: Vec<String>,
    pub a2a: String,
    pub session_id: Option<String>,
    pub blocks: Vec<Block>,
    pub(super) transcript_cache: Vec<Option<CachedTranscriptBlock>>,
    pub(super) transcript_revisions: Vec<u64>,
    pub(super) transcript_dirty: BTreeSet<usize>,
    pub(super) transcript_dynamic: BTreeSet<usize>,
    pub(super) transcript_thoughts: BTreeSet<usize>,
    pub(super) transcript_prefixes: Vec<usize>,
    pub(super) transcript_cache_width: usize,
    next_transcript_revision: u64,
    transcript_focus_index: Option<usize>,
    pub editor: Editor,
    pub attachments: Vec<Attachment>,
    next_attachment: usize,
    pub phase: Phase,
    pub turn_started: Option<Instant>,
    next_turn_id: u64,
    active_turn_id: Option<u64>,
    active_autonomous_turn_id: Option<u64>,
    /// The previous assistant stream ended; the next text starts a new block.
    agent_stream_sealed: bool,
    /// Exact source bytes in the latest assistant stream, before TUI rendering.
    latest_agent_source: String,
    pub compacting: bool,
    pub usage: Option<ContextUsage>,
    pub logs: Vec<String>,
    pub show_logs: bool,
    pub show_thoughts: bool,
    /// `None` shows the graph while a tool runs; `Some` pins it open or shut.
    pub graph_pinned: Option<bool>,
    /// Tool card selected for the runtime graph.
    pub focused_call_id: Option<String>,
    pub tick: usize,
    pub scroll: usize,
    pub follow: bool,
    /// Rendered transcript height and total line count from the last frame.
    pub viewport: usize,
    pub total_lines: usize,
    /// Prompt field width from the last frame, for row-wise cursor movement.
    pub prompt_width: usize,
    /// Which tool call owns each visible transcript row, and where that area
    /// started, so a click can be mapped back to a card.
    pub row_calls: Vec<Option<String>>,
    pub row_links: Vec<Vec<LinkHit>>,
    /// Exact fenced-code content owned by each visible transcript row.
    pub row_code: Vec<Option<CodeHit>>,
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

fn edit_distance(left: &str, right: &str) -> usize {
    let mut costs = (0..=right.chars().count()).collect::<Vec<_>>();
    for (row, a) in left.chars().enumerate() {
        let mut previous = costs[0];
        costs[0] = row + 1;
        for (column, b) in right.chars().enumerate() {
            let old = costs[column + 1];
            costs[column + 1] = if a == b {
                previous
            } else {
                1 + previous.min(costs[column]).min(old)
            };
            previous = old;
        }
    }
    *costs.last().unwrap_or(&0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ModelScore {
    tier: u8,
    distance: usize,
    unmatched: usize,
    gaps: usize,
    start: usize,
}

fn model_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn typo_limit(term: &str) -> usize {
    match term.chars().count() {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

fn ordered_token_score(candidate: &[String], query: &[String]) -> Option<ModelScore> {
    let mut previous = vec![None; candidate.len()];
    for (term_index, term) in query.iter().enumerate() {
        let mut next = vec![None; candidate.len()];
        for (index, token) in candidate.iter().enumerate() {
            let edit = edit_distance(token, term);
            if !token.starts_with(term) && edit > typo_limit(term) {
                continue;
            }
            next[index] = if term_index == 0 {
                Some((edit, index))
            } else {
                previous[..index]
                    .iter()
                    .flatten()
                    .map(|(distance, start)| (distance + edit, *start))
                    .min_by_key(|(distance, start)| (*distance, Reverse(*start)))
            };
        }
        previous = next;
    }

    previous
        .into_iter()
        .enumerate()
        .filter_map(|(end, state)| {
            state.map(|(distance, start)| ModelScore {
                tier: 3,
                distance,
                unmatched: candidate.len().saturating_sub(query.len()),
                gaps: (end - start + 1).saturating_sub(query.len()),
                start,
            })
        })
        .min()
}

fn model_score(choice: &ModelChoice, query: &str) -> Option<ModelScore> {
    let query = query.trim().to_lowercase();
    let model = choice.model.to_lowercase();
    let basename = model.rsplit('/').next().unwrap_or(&model);
    let query_tokens = model_tokens(&query);
    if query_tokens.is_empty() {
        return None;
    }

    if query == model || query == basename {
        return Some(ModelScore {
            tier: 0,
            distance: 0,
            unmatched: 0,
            gaps: 0,
            start: 0,
        });
    }

    let basename_tokens = model_tokens(basename);
    let candidate_tokens = model_tokens(&model);
    if basename_tokens == query_tokens || candidate_tokens == query_tokens {
        return Some(ModelScore {
            tier: 1,
            distance: 0,
            unmatched: 0,
            gaps: 0,
            start: 0,
        });
    }
    if let Some(start) = candidate_tokens
        .windows(query_tokens.len())
        .position(|tokens| tokens == query_tokens)
    {
        return Some(ModelScore {
            tier: 2,
            distance: 0,
            unmatched: candidate_tokens.len() - query_tokens.len(),
            gaps: 0,
            start,
        });
    }
    if let Some(score) = ordered_token_score(&candidate_tokens, &query_tokens) {
        return Some(score);
    }

    let mut all_tokens = model_tokens(&choice.provider);
    all_tokens.extend(candidate_tokens);
    ordered_token_score(&all_tokens, &query_tokens).map(|score| ModelScore { tier: 4, ..score })
}

fn media_label(media: &agentkit_core::MediaPart, index: usize) -> String {
    let kind = match media.modality {
        Modality::Image => "Image",
        Modality::Audio => "Audio",
        Modality::Video => "Video",
        Modality::Binary => "Media",
    };
    match &media.data {
        DataRef::Uri(uri) if safe_media_uri(uri) => format!("[{kind} #{index}]({uri})"),
        _ => format!("[{kind} #{index}]"),
    }
}

fn safe_media_uri(uri: &str) -> bool {
    uri.len() <= 2_048
        && url::Url::parse(uri).is_ok_and(|uri| matches!(uri.scheme(), "file" | "http" | "https"))
}

fn persisted_output(output: &ToolOutput) -> Vec<String> {
    let text = match output {
        ToolOutput::Text(text) => text.clone(),
        ToolOutput::Structured(value) => {
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        }
        ToolOutput::Parts(parts) => {
            let mut next_media = 0;
            let mut next_file = 0;
            parts
                .iter()
                .filter_map(|part| match part {
                    Part::Text(text) => Some(text.text.clone()),
                    Part::Media(media) => {
                        next_media += 1;
                        Some(media_label(media, next_media))
                    }
                    Part::File(file) => {
                        next_file += 1;
                        Some(match &file.data {
                            DataRef::Uri(uri) if safe_media_uri(uri) => {
                                format!("[File #{}]({uri})", next_file)
                            }
                            _ => format!("[File #{}]", next_file),
                        })
                    }
                    Part::Structured(value) => Some(value.value.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        ToolOutput::Files(files) => format!("{} files", files.len()),
    };
    text.lines().map(str::to_string).collect()
}

impl App {
    pub fn new(root: PathBuf, provider: String, model: String, a2a: String) -> Self {
        Self {
            root,
            provider,
            model,
            model_choices: Vec::new(),
            model_dialog: None,
            reasoning_effort: "default".into(),
            effort_choices: Vec::new(),
            effort_dialog: None,
            available_commands: Vec::new(),
            a2a,
            session_id: None,
            blocks: Vec::new(),
            transcript_cache: Vec::new(),
            transcript_revisions: Vec::new(),
            transcript_dirty: BTreeSet::new(),
            transcript_dynamic: BTreeSet::new(),
            transcript_thoughts: BTreeSet::new(),
            transcript_prefixes: vec![0],
            transcript_cache_width: 0,
            next_transcript_revision: 0,
            transcript_focus_index: None,
            editor: Editor::default(),
            attachments: Vec::new(),
            next_attachment: 0,
            phase: Phase::Idle,
            turn_started: None,
            next_turn_id: 0,
            active_turn_id: None,
            active_autonomous_turn_id: None,
            agent_stream_sealed: false,
            latest_agent_source: String::new(),
            compacting: false,
            usage: None,
            logs: Vec::new(),
            show_logs: false,
            show_thoughts: false,
            graph_pinned: None,
            focused_call_id: None,
            tick: 0,
            scroll: 0,
            follow: true,
            viewport: 0,
            total_lines: 0,
            prompt_width: 80,
            row_calls: Vec::new(),
            row_links: Vec::new(),
            row_code: Vec::new(),
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

    fn push_block(&mut self, block: Block) {
        let index = self.blocks.len();
        let thought = matches!(block, Block::Thought { .. });
        let dynamic = Self::block_is_dynamic(&block);
        let tool_id = match &block {
            Block::Tool(call) => Some(call.id.clone()),
            _ => None,
        };
        self.blocks.push(block);
        self.next_transcript_revision = self.next_transcript_revision.wrapping_add(1);
        self.transcript_revisions
            .push(self.next_transcript_revision);
        self.transcript_cache.push(None);
        self.transcript_prefixes
            .push(self.transcript_prefixes.last().copied().unwrap_or(0));
        self.transcript_dirty.insert(index);
        if thought {
            self.transcript_thoughts.insert(index);
        }
        if dynamic {
            self.transcript_dynamic.insert(index);
        }
        if tool_id.as_deref() == self.focused_call_id.as_deref()
            || tool_id.is_some() && self.focused_call_id.is_none()
        {
            self.set_focus_index(Some(index));
        }
    }

    fn block_is_dynamic(block: &Block) -> bool {
        match block {
            Block::Thought { millis, .. } => millis.is_none(),
            Block::Tool(call) => call.running() || call.running_children() > 0,
            _ => false,
        }
    }

    fn reclassify_dynamic(&mut self, index: usize) {
        if self.blocks.get(index).is_some_and(Self::block_is_dynamic) {
            self.transcript_dynamic.insert(index);
        } else {
            self.transcript_dynamic.remove(&index);
        }
    }

    fn mark_block_dirty(&mut self, index: usize) {
        if let Some(revision) = self.transcript_revisions.get_mut(index) {
            self.next_transcript_revision = self.next_transcript_revision.wrapping_add(1);
            *revision = self.next_transcript_revision;
            self.transcript_dirty.insert(index);
        }
    }

    /// Aligns cache bookkeeping for tests and other direct transcript setup.
    pub(super) fn sync_transcript_cache(&mut self) {
        self.transcript_cache.truncate(self.blocks.len());
        self.transcript_revisions.truncate(self.blocks.len());
        self.transcript_dirty
            .retain(|index| *index < self.blocks.len());
        self.transcript_dynamic
            .retain(|index| *index < self.blocks.len());
        self.transcript_thoughts
            .retain(|index| *index < self.blocks.len());
        while self.transcript_revisions.len() < self.blocks.len() {
            let index = self.transcript_revisions.len();
            self.next_transcript_revision = self.next_transcript_revision.wrapping_add(1);
            self.transcript_revisions
                .push(self.next_transcript_revision);
            self.transcript_cache.push(None);
            self.transcript_dirty.insert(index);
            if matches!(self.blocks[index], Block::Thought { .. }) {
                self.transcript_thoughts.insert(index);
            }
            if Self::block_is_dynamic(&self.blocks[index]) {
                self.transcript_dynamic.insert(index);
            }
            if matches!(self.blocks[index], Block::Tool(_))
                && (self.focused_call_id.is_none()
                    || matches!(
                        &self.blocks[index],
                        Block::Tool(call) if Some(call.id.as_str()) == self.focused_call_id.as_deref()
                    ))
            {
                self.set_focus_index(Some(index));
            }
        }
        if let Some(id) = &self.focused_call_id
            && !self.transcript_focus_index.is_some_and(
                |index| matches!(&self.blocks[index], Block::Tool(call) if &call.id == id),
            )
        {
            let focus = self
                .blocks
                .iter()
                .rposition(|block| matches!(block, Block::Tool(call) if &call.id == id));
            self.set_focus_index(focus);
        }
        self.transcript_prefixes
            .resize(self.blocks.len().saturating_add(1), 0);
    }

    fn set_focus_index(&mut self, index: Option<usize>) {
        let old = self.transcript_focus_index;
        if old == index {
            return;
        }
        self.transcript_focus_index = index;
        if let Some(old) = old {
            self.mark_block_dirty(old);
        }
        if let Some(index) = index {
            self.mark_block_dirty(index);
        }
    }

    fn prepare_focused_call(&mut self, id: String) {
        self.focused_call_id = Some(id);
        self.set_focus_index(None);
    }

    fn focus_call_by_id(&mut self, id: String) {
        let index = self
            .blocks
            .iter()
            .rposition(|block| matches!(block, Block::Tool(call) if call.id == id));
        self.focused_call_id = Some(id);
        self.set_focus_index(index);
    }

    pub(super) fn transcript_call_is_focused(&self, index: usize) -> bool {
        self.transcript_focus_index == Some(index)
    }

    fn toggle_thoughts(&mut self) {
        self.show_thoughts = !self.show_thoughts;
        let thoughts: Vec<_> = self.transcript_thoughts.iter().copied().collect();
        for index in thoughts {
            self.mark_block_dirty(index);
        }
    }

    /// Whether the periodic animation clock can change anything on screen.
    pub fn needs_redraw_tick(&self) -> bool {
        self.working() || !self.transcript_dynamic.is_empty() || self.toast.is_some()
    }

    /// Advances animations and removes an expired toast on its final redraw.
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(4))
        {
            self.toast = None;
        }
    }

    /// Rebuilds the visible history from the same Items preloaded into the model.
    pub fn restore_transcript(&mut self, session_id: String, transcript: &[Item]) {
        self.session_id = Some(session_id);
        for item in transcript {
            match item.kind {
                ItemKind::Developer if is_compaction_summary(item) => {
                    self.push_block(Block::Notice("context compacted".into()));
                }
                ItemKind::System | ItemKind::Developer | ItemKind::Context => continue,
                ItemKind::User | ItemKind::Notification => {
                    let mut next_media = 0;
                    let text = item
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            Part::Text(text) => Some(text.text.clone()),
                            Part::Media(media) => {
                                next_media += 1;
                                Some(media_label(media, next_media))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        if item.kind == ItemKind::User {
                            self.latest_agent_source.clear();
                            self.push_block(Block::User(text));
                        } else {
                            self.push_block(Block::Notice(text));
                        }
                    }
                }
                ItemKind::Assistant => {
                    let mut next_media = 0;
                    for part in &item.parts {
                        match part {
                            Part::Text(text) if !text.text.is_empty() => {
                                self.latest_agent_source.push_str(&text.text);
                                self.push_block(Block::Agent(text.text.clone()))
                            }
                            Part::Media(media) => {
                                next_media += 1;
                                self.push_block(Block::Agent(media_label(media, next_media)));
                            }
                            Part::Reasoning(reasoning) if reasoning.summary.is_some() => self
                                .push_block(Block::Thought {
                                    text: reasoning.summary.clone().unwrap_or_default(),
                                    started: Instant::now(),
                                    millis: Some(0),
                                }),
                            Part::ToolCall(call) => self.push_block(Block::Tool(ToolCall {
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
                                backgrounded: false,
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
        if let Some(index) = self.transcript_focus_index
            && let Some(Block::Tool(call)) = self.blocks.get(index)
            && self
                .focused_call_id
                .as_ref()
                .is_none_or(|id| id == &call.id)
        {
            return Some(call);
        }
        // Public transcript fields are used by tests for direct setup. Normal
        // production mutations keep the index above aligned.
        if self.transcript_revisions.len() != self.blocks.len() || self.focused_call_id.is_some() {
            if let Some(id) = &self.focused_call_id
                && let Some(call) = self.blocks.iter().rev().find_map(|block| match block {
                    Block::Tool(call) if &call.id == id => Some(call),
                    _ => None,
                })
            {
                return Some(call);
            }
            return self.blocks.iter().rev().find_map(|block| match block {
                Block::Tool(call) => Some(call),
                _ => None,
            });
        }
        None
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
            Update::A2aAddress(address) => self.a2a = address,
            Update::AvailableCommands {
                session_id,
                commands,
            } => {
                if self.session_id.as_deref() == Some(session_id.as_str()) {
                    self.available_commands = commands;
                }
            }
            Update::Text(text) => {
                self.close_thought();
                if self.agent_stream_sealed {
                    self.latest_agent_source.clear();
                    self.latest_agent_source.push_str(&text);
                    self.push_block(Block::Agent(text));
                    self.agent_stream_sealed = false;
                } else {
                    self.latest_agent_source.push_str(&text);
                    match self.blocks.last_mut() {
                        Some(Block::Agent(existing)) => {
                            existing.push_str(&text);
                            self.mark_block_dirty(self.blocks.len() - 1);
                        }
                        _ => self.push_block(Block::Agent(text)),
                    }
                }
            }
            Update::Thought(text) => match self.blocks.last_mut() {
                Some(Block::Thought {
                    text: existing,
                    millis: None,
                    ..
                }) => {
                    existing.push_str(&text);
                    self.mark_block_dirty(self.blocks.len() - 1);
                }
                _ => self.push_block(Block::Thought {
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
                backgrounded,
            } => {
                self.close_thought();
                self.prepare_focused_call(id.clone());
                self.push_block(Block::Tool(ToolCall {
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
                    backgrounded,
                }));
            }
            Update::ToolUpdated {
                id,
                status,
                script,
                output,
                backgrounded,
            } => {
                let completed_background = {
                    let Some(call) = self.call_mut(&id) else {
                        return;
                    };
                    let was_running = call.running();
                    if let Some(script) = script {
                        call.plan = parse_plan(&script);
                    }
                    if !output.is_empty() {
                        call.output = output;
                    }
                    call.backgrounded |= backgrounded;
                    if let Some(status) = status {
                        call.status = status;
                        if !call.running() {
                            call.finished = Some(Instant::now());
                            call.finish_running_children();
                        }
                    }
                    was_running && !call.running() && call.backgrounded
                };
                // Autonomous model output follows the terminal update for a
                // detached call without a new ACP prompt/TurnEnded pair. Seal
                // the current stream so that output starts a new agent block.
                self.agent_stream_sealed |= completed_background;
                if let Some(index) = self.call_index(&id) {
                    self.reclassify_dynamic(index);
                }
            }
            Update::Usage { used, size } => {
                self.usage = Some(ContextUsage { used, size });
            }
            Update::Runtime(event) => self.apply_runtime(event),
            Update::Log(line) => {
                self.logs.push(line);
                if self.logs.len() > 500 {
                    self.logs.drain(..self.logs.len() - 500);
                }
            }
            Update::AutonomousTurnStarted(id) => {
                if self.active_turn_id.is_none() {
                    self.active_autonomous_turn_id = Some(id);
                    self.agent_stream_sealed = true;
                    self.phase = Phase::Working;
                    self.turn_started = Some(Instant::now());
                    self.follow = true;
                    self.scroll = usize::MAX;
                }
            }
            Update::AutonomousTurnEnded { id, error } => {
                if self.active_autonomous_turn_id != Some(id) || self.active_turn_id.is_some() {
                    return;
                }
                self.close_thought();
                self.agent_stream_sealed = true;
                let interrupted = self.phase == Phase::Cancelling;
                self.phase = Phase::Idle;
                self.turn_started = None;
                self.active_autonomous_turn_id = None;
                match (interrupted, error) {
                    (true, _) => self.note("turn interrupted"),
                    (false, Some(error)) => self.push_block(Block::Error(error)),
                    (false, None) => {}
                }
            }
            Update::TurnEnded { id, error } => {
                if id.is_some() && id != self.active_turn_id {
                    return;
                }
                self.close_thought();
                self.agent_stream_sealed = true;
                let interrupted = self.phase == Phase::Cancelling;
                self.phase = Phase::Idle;
                self.turn_started = None;
                self.active_turn_id = None;
                self.active_autonomous_turn_id = None;
                self.compacting = false;
                let mut finished = Vec::new();
                for (index, block) in self.blocks.iter_mut().enumerate() {
                    if let Block::Tool(call) = block
                        && call.running()
                        && !call.backgrounded
                    {
                        call.status = ToolCallStatus::Completed;
                        call.finished = Some(Instant::now());
                        call.finish_running_children();
                        finished.push(index);
                    }
                }
                for index in finished {
                    self.mark_block_dirty(index);
                    self.reclassify_dynamic(index);
                }
                match (interrupted, error) {
                    (true, _) => self.note("turn interrupted"),
                    (false, Some(error)) => self.push_block(Block::Error(error)),
                    (false, None) => {}
                }
            }
        }
        if self.follow {
            self.scroll = usize::MAX;
        }
    }

    fn apply_runtime(&mut self, event: RuntimeEvent) {
        let event = match event {
            RuntimeEvent::SessionStarted { .. } => return,
            RuntimeEvent::CompactionStarted { .. } => {
                self.compacting = true;
                return;
            }
            RuntimeEvent::CompactionFinished { ok, compacted, .. } => {
                self.compacting = false;
                if ok && compacted {
                    self.usage = None;
                    self.note("context compacted");
                }
                return;
            }
            event => event,
        };
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
        let owner_id = call.id.clone();
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
            RuntimeEvent::SessionStarted { .. }
            | RuntimeEvent::CompactionStarted { .. }
            | RuntimeEvent::CompactionFinished { .. } => unreachable!("handled above"),
        }
        if let Some(index) = self.call_index(&owner_id) {
            self.reclassify_dynamic(index);
        }
    }

    fn call_index(&self, id: &str) -> Option<usize> {
        self.blocks
            .iter()
            .rposition(|block| matches!(block, Block::Tool(call) if call.id == id))
    }

    fn call_mut(&mut self, id: &str) -> Option<&mut ToolCall> {
        let index = self.call_index(id)?;
        self.mark_block_dirty(index);
        match &mut self.blocks[index] {
            Block::Tool(call) => Some(call),
            _ => unreachable!(),
        }
    }

    fn running_call_mut(&mut self) -> Option<&mut ToolCall> {
        let index = self
            .blocks
            .iter()
            .rposition(|block| matches!(block, Block::Tool(call) if call.running()))?;
        self.mark_block_dirty(index);
        match &mut self.blocks[index] {
            Block::Tool(call) => Some(call),
            _ => unreachable!(),
        }
    }

    fn close_thought(&mut self) {
        if let Some(Block::Thought {
            started,
            millis: millis @ None,
            ..
        }) = self.blocks.last_mut()
        {
            *millis = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            let index = self.blocks.len() - 1;
            self.mark_block_dirty(index);
            self.reclassify_dynamic(index);
        }
    }

    /// Switches the visible client state to a fresh persisted session. Editor
    /// history and diagnostics remain useful, while transcript-derived state
    /// starts empty.
    pub fn start_session(&mut self, session_id: String) {
        self.session_id = Some(session_id);
        self.available_commands.clear();
        self.blocks.clear();
        self.transcript_cache.clear();
        self.transcript_revisions.clear();
        self.transcript_dirty.clear();
        self.transcript_dynamic.clear();
        self.transcript_thoughts.clear();
        self.transcript_prefixes.clear();
        self.transcript_prefixes.push(0);
        self.transcript_cache_width = 0;
        self.transcript_focus_index = None;
        self.clear_attachments();
        self.latest_agent_source.clear();
        self.phase = Phase::Idle;
        self.turn_started = None;
        self.active_turn_id = None;
        self.active_autonomous_turn_id = None;
        self.compacting = false;
        self.usage = None;
        self.scroll = usize::MAX;
        self.follow = true;
        self.focused_call_id = None;
        self.viewport = 0;
        self.total_lines = 0;
        self.transcript_top = 0;
        self.transcript_left = 0;
        self.transcript_width = 0;
        self.row_calls.clear();
        self.row_links.clear();
        self.row_code.clear();
    }

    pub fn push_user(&mut self, prompt: String) -> u64 {
        self.push_block(Block::User(prompt));
        self.begin_turn()
    }

    fn begin_turn(&mut self) -> u64 {
        self.next_turn_id = self.next_turn_id.wrapping_add(1);
        self.active_turn_id = Some(self.next_turn_id);
        self.active_autonomous_turn_id = None;
        self.agent_stream_sealed = true;
        self.phase = Phase::Working;
        self.turn_started = Some(Instant::now());
        self.follow = true;
        self.scroll = usize::MAX;
        self.next_turn_id
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
        self.push_block(Block::Notice(text.into()));
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

    pub fn attach(
        &mut self,
        path: PathBuf,
        mime_type: &'static str,
        kind: AttachmentKind,
        size: u64,
    ) {
        self.next_attachment += 1;
        let label = match kind {
            AttachmentKind::Image => "Image",
            AttachmentKind::Audio => "Audio",
        };
        let placeholder = format!("[{label} #{}]", self.next_attachment);
        if self
            .editor
            .text()
            .chars()
            .last()
            .is_some_and(|character| !character.is_whitespace())
        {
            self.editor.insert_char(' ');
        }
        self.editor.insert_str(&placeholder);
        self.attachments.push(Attachment {
            path,
            placeholder,
            mime_type,
            kind,
            size,
        });
        self.toast(format!("attached {label} #{}", self.next_attachment));
    }

    pub fn clear_attachments(&mut self) {
        self.attachments.clear();
        self.next_attachment = 0;
    }

    pub fn prune_attachments(&mut self) {
        let prompt = self.editor.text();
        self.attachments
            .retain(|attachment| prompt.contains(&attachment.placeholder));
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

    pub fn set_model_choices(&mut self, choices: Vec<ModelChoice>) {
        self.model_choices = choices;
    }

    pub fn set_effort(&mut self, current: String, choices: Vec<EffortChoice>) {
        self.reasoning_effort = current;
        self.effort_choices = choices;
    }

    pub fn selected_model_choices(&self) -> Vec<&ModelChoice> {
        let Some(dialog) = &self.model_dialog else {
            return Vec::new();
        };
        if dialog.query.trim().is_empty() {
            return self.model_choices.iter().collect();
        }
        let mut choices = self
            .model_choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                model_score(choice, &dialog.query).map(|score| (score, index, choice))
            })
            .collect::<Vec<_>>();
        choices.sort_by_key(|(score, index, _)| (*score, *index));
        choices.into_iter().map(|(_, _, choice)| choice).collect()
    }

    fn closest_model(&self, query: &str) -> Option<ModelChoice> {
        self.model_choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                model_score(choice, query).map(|score| (score, index, choice))
            })
            .min_by_key(|(score, index, _)| (*score, *index))
            .map(|(_, _, choice)| choice.clone())
    }

    fn handle_model_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => self.model_dialog = None,
            KeyCode::Tab => {
                if let Some(dialog) = &mut self.model_dialog {
                    dialog.save_defaults = !dialog.save_defaults;
                }
            }
            KeyCode::Up => {
                if let Some(dialog) = &mut self.model_dialog {
                    dialog.selected = dialog.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                let count = self.selected_model_choices().len();
                if let Some(dialog) = &mut self.model_dialog {
                    dialog.selected = (dialog.selected + 1).min(count.saturating_sub(1));
                }
            }
            KeyCode::Backspace => {
                if let Some(dialog) = &mut self.model_dialog {
                    dialog.query.pop();
                    dialog.selected = 0;
                }
            }
            KeyCode::Enter => {
                let choice = self
                    .selected_model_choices()
                    .get(self.model_dialog.as_ref().map_or(0, |value| value.selected))
                    .cloned()
                    .cloned();
                let save_defaults = self
                    .model_dialog
                    .as_ref()
                    .is_some_and(|value| value.save_defaults);
                if let Some(choice) = choice {
                    self.model_dialog = None;
                    return Action::SelectModel {
                        choice,
                        save_defaults,
                    };
                }
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(dialog) = &mut self.model_dialog {
                    dialog.query.push(character);
                    dialog.selected = 0;
                }
            }
            _ => {}
        }
        Action::None
    }

    fn handle_effort_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => self.effort_dialog = None,
            KeyCode::Tab => {
                if let Some(dialog) = &mut self.effort_dialog {
                    dialog.save_defaults = !dialog.save_defaults;
                }
            }
            KeyCode::Up => {
                if let Some(dialog) = &mut self.effort_dialog {
                    dialog.selected = dialog.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(dialog) = &mut self.effort_dialog {
                    dialog.selected =
                        (dialog.selected + 1).min(self.effort_choices.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                let selected = self
                    .effort_dialog
                    .as_ref()
                    .map_or(0, |dialog| dialog.selected);
                let save_defaults = self
                    .effort_dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.save_defaults);
                if let Some(choice) = self.effort_choices.get(selected) {
                    let effort = choice.id.clone();
                    self.effort_dialog = None;
                    return Action::SelectEffort {
                        effort,
                        save_defaults,
                    };
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Applies a key press, returning work for the event loop.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if self.model_dialog.is_some() {
            return self.handle_model_key(key);
        }
        if self.effort_dialog.is_some() {
            return self.handle_effort_key(key);
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
            KeyCode::Char('k')
                if control
                    && self
                        .focus_call()
                        .is_some_and(|call| call.backgrounded && call.running()) =>
            {
                return Action::CancelBackground(
                    self.focus_call()
                        .map(|call| call.id.clone())
                        .unwrap_or_default(),
                );
            }
            KeyCode::Char('d') if control && self.editor.is_empty() => return Action::Quit,
            KeyCode::Char('y') if control => {
                let Some(text) = self.latest_agent_text() else {
                    self.toast("no agent response to copy");
                    return Action::None;
                };
                self.toast("copied latest agent response as Markdown");
                return Action::Copy(text);
            }
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
                let input = self.editor.submit();
                return match parse(&input) {
                    Parsed::New { prompt } => Action::New(prompt.map(str::to_string)),
                    Parsed::Model { query: Some(query) } => match self.closest_model(query) {
                        Some(choice) => Action::SelectModel {
                            choice,
                            save_defaults: false,
                        },
                        None => {
                            self.toast(format!("no model matches {query:?}"));
                            Action::None
                        }
                    },
                    Parsed::Model { query: None } => {
                        if self.model_choices.is_empty() {
                            self.toast("no models are available");
                        } else {
                            self.model_dialog = Some(ModelDialog {
                                query: String::new(),
                                selected: 0,
                                save_defaults: false,
                            });
                        }
                        Action::None
                    }
                    Parsed::Effort { value: Some(value) } => {
                        if self.effort_choices.iter().any(|choice| choice.id == value) {
                            Action::SelectEffort {
                                effort: value.to_string(),
                                save_defaults: false,
                            }
                        } else {
                            self.toast(format!("unknown reasoning effort {value:?}"));
                            Action::None
                        }
                    }
                    Parsed::Effort { value: None } => {
                        if self.effort_choices.is_empty() {
                            self.toast("reasoning effort is not available");
                        } else {
                            let selected = self
                                .effort_choices
                                .iter()
                                .position(|choice| choice.id == self.reasoning_effort)
                                .unwrap_or(0);
                            self.effort_dialog = Some(EffortDialog {
                                selected,
                                save_defaults: false,
                            });
                        }
                        Action::None
                    }
                    Parsed::Prompt(prompt) => Action::Submit(SubmittedPrompt {
                        text: prompt.to_string(),
                        attachments: self
                            .attachments
                            .iter()
                            .filter(|attachment| prompt.contains(&attachment.placeholder))
                            .cloned()
                            .collect(),
                    }),
                };
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
            // Legacy terminal encoding reports option+left/right as alt+b/f.
            KeyCode::Char('b') if alt => self.editor.move_word_left(),
            KeyCode::Char('f') if alt => self.editor.move_word_right(),
            KeyCode::Char('a') if control => self.editor.move_line_start(),
            KeyCode::Char('e') if control => self.editor.move_line_end(),
            KeyCode::Char('g') if control => {
                self.graph_pinned = Some(!self.show_graph());
            }
            KeyCode::Char('l') if control => self.show_logs = !self.show_logs,
            KeyCode::Char('o') if control => self.toggle_last_output(),
            KeyCode::Char('t') if control => self.toggle_thoughts(),
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

    /// Returns the latest agent response exactly as received, including text
    /// split around tool calls. Rendering and wrapping never touch this source.
    fn latest_agent_text(&self) -> Option<String> {
        (!self.latest_agent_source.is_empty()).then(|| self.latest_agent_source.clone())
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

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_by(-3);
                return Action::Redraw;
            }
            MouseEventKind::ScrollDown => {
                self.scroll_by(3);
                return Action::Redraw;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                return self.click(mouse.column as usize, mouse.row as usize);
            }
            _ => {}
        }
        Action::None
    }

    /// Copies code, opens links, or folds tool output at the clicked row.
    fn click(&mut self, column: usize, row: usize) -> Action {
        if self.scroll == usize::MAX {
            return Action::None;
        }
        let Some(offset) = row.checked_sub(self.transcript_top) else {
            return Action::None;
        };
        let inside = offset < self.viewport
            && column >= self.transcript_left
            && column < self.transcript_left + self.transcript_width;
        if !inside {
            return Action::None;
        }
        if let Some(url) = self.clicked_link(column, offset) {
            open_url(&url);
            return Action::None;
        }
        let code = self
            .row_code
            .get(offset)
            .and_then(Option::as_ref)
            .and_then(|hit| self.blocks.get(hit.block).map(|block| (block, hit)))
            .and_then(|(block, hit)| match block {
                Block::Agent(source) => source.get(hit.range.clone()),
                _ => None,
            })
            .map(str::to_string);
        if let Some(code) = code {
            self.toast("copied code block");
            return Action::Copy(code);
        }
        if let Some(Some(id)) = self.row_calls.get(offset).cloned() {
            self.focus_call_by_id(id.clone());
            self.graph_pinned = Some(true);
            self.toggle_output(&id);
        }
        Action::None
    }

    fn clicked_link(&self, column: usize, offset: usize) -> Option<String> {
        if self.scroll == usize::MAX {
            return None;
        }
        let column = column.checked_sub(self.transcript_left)?;
        self.row_links
            .get(offset)?
            .iter()
            .find(|link| column >= link.start && column < link.end)
            .map(|link| link.url.clone())
    }
}

#[cfg(target_os = "macos")]
fn open_url(url: &str) {
    let _ = Command::new("open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(target_os = "linux")]
fn open_url(url: &str) {
    let display = std::env::var_os("DISPLAY");
    let wayland_display = std::env::var_os("WAYLAND_DISPLAY");
    if !has_graphical_session(display.as_deref(), wayland_display.as_deref()) {
        return;
    }
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(any(test, target_os = "linux"))]
fn has_graphical_session(display: Option<&OsStr>, wayland_display: Option<&OsStr>) -> bool {
    [display, wayland_display]
        .into_iter()
        .flatten()
        .any(|value| !value.is_empty())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_url(_url: &str) {}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };

    use agentkit_acp::{ToolCallStatus, ToolKind};
    use agentkit_core::{DataRef, Item, ItemKind, MediaPart, MetadataMap, Modality, Part};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::{Action, App, AttachmentKind, Block, Phase, Update};
    use crate::{events::RuntimeEvent, tui::wrap::LinkHit};

    fn press(code: KeyCode) -> KeyEvent {
        modified_press(code, KeyModifiers::NONE)
    }

    fn modified_press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn app() -> App {
        App::new(
            PathBuf::from("/tmp"),
            "openai-subscription".into(),
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
            backgrounded: false,
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
    fn surfaces_compaction_lifecycle_without_a_tool_call() {
        let mut app = app();
        app.push_user("continue".into());
        app.usage = Some(super::ContextUsage {
            used: 80,
            size: 100,
        });
        app.apply(Update::Runtime(RuntimeEvent::CompactionStarted {
            reason: "TokenThreshold".into(),
            at: 0,
        }));
        assert!(app.compacting);

        app.apply(Update::Runtime(RuntimeEvent::CompactionFinished {
            reason: "TokenThreshold".into(),
            ok: true,
            compacted: true,
            millis: 12,
        }));
        assert!(!app.compacting);
        assert!(app.usage.is_none());
        assert!(
            matches!(app.blocks.last(), Some(Block::Notice(text)) if text == "context compacted")
        );
    }

    #[test]
    fn turn_end_clears_compaction_state() {
        let mut app = app();
        app.compacting = true;
        app.apply(Update::TurnEnded {
            id: None,
            error: None,
        });
        assert!(!app.compacting);
    }

    #[test]
    fn restores_only_tagged_developer_items_as_compaction_markers() {
        let mut metadata = MetadataMap::new();
        metadata.insert(
            crate::compaction::COMPACTION_SUMMARY_METADATA_KEY.into(),
            true.into(),
        );
        let transcript = vec![
            Item::text(ItemKind::Developer, "ordinary instruction"),
            Item::text(ItemKind::Developer, "summary").with_metadata(metadata),
        ];
        let mut app = app();
        app.restore_transcript("session".into(), &transcript);
        assert_eq!(app.blocks.len(), 1);
        assert!(
            matches!(app.blocks.first(), Some(Block::Notice(text)) if text == "context compacted")
        );
    }

    #[test]
    fn restored_media_uses_safe_links_and_never_exposes_data_urls() {
        let transcript = vec![
            Item::new(
                ItemKind::User,
                vec![
                    Part::text("inspect these"),
                    Part::Media(MediaPart::new(
                        Modality::Image,
                        "image/png",
                        DataRef::Uri("file:///tmp/image.png".into()),
                    )),
                    Part::Media(MediaPart::new(
                        Modality::Image,
                        "image/png",
                        DataRef::Uri("data:image/png;base64,c2VjcmV0".into()),
                    )),
                ],
            ),
            Item::new(
                ItemKind::Assistant,
                vec![
                    Part::text("done"),
                    Part::Media(MediaPart::new(
                        Modality::Image,
                        "image/png",
                        DataRef::Uri("https://example.com/result.png".into()),
                    )),
                ],
            ),
        ];
        let mut app = app();

        app.restore_transcript("session".into(), &transcript);

        assert!(matches!(
            &app.blocks[0],
            Block::User(text)
                if text == "inspect these\n[Image #1](file:///tmp/image.png)\n[Image #2]"
                    && !text.contains("data:")
        ));
        assert!(matches!(&app.blocks[1], Block::Agent(text) if text == "done"));
        assert!(matches!(
            &app.blocks[2],
            Block::Agent(text) if text == "[Image #1](https://example.com/result.png)"
        ));
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
    fn terminal_parent_finishes_children_missing_completion_events() {
        let mut app = app();
        compose(&mut app, "a = shell({ command: \"sleep 60\" })\nreturn a");
        app.apply(Update::Runtime(child("call-1:compose:shell", "shell")));

        app.apply(Update::ToolUpdated {
            id: "call-1".into(),
            status: Some(ToolCallStatus::Failed),
            script: None,
            output: Vec::new(),
            backgrounded: false,
        });

        let Some(Block::Tool(call)) = app.blocks.last() else {
            panic!("expected a tool block");
        };
        assert_eq!(call.status, ToolCallStatus::Failed);
        assert_eq!(call.running_children(), 0);
        assert!(call.children[0].millis.is_some());
        assert!(!call.children[0].ok);
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
        app.apply(Update::TurnEnded {
            id: None,
            error: None,
        });
        let Some(Block::Tool(call)) = app.blocks.last() else {
            panic!("expected a tool block");
        };
        assert_eq!(call.status, ToolCallStatus::Completed);
        assert!(!app.working());
    }

    #[test]
    fn stale_turn_end_cannot_finish_a_newer_turn() {
        let mut app = app();
        let first = app.push_user("first".into());
        app.apply(Update::TurnEnded {
            id: Some(first),
            error: None,
        });
        let second = app.push_user("second".into());

        app.apply(Update::TurnEnded {
            id: Some(first),
            error: None,
        });

        assert!(app.working());
        assert_eq!(app.active_turn_id, Some(second));
    }

    #[test]
    fn autonomous_turn_is_visible_and_cancellable() {
        let mut app = app();

        app.apply(Update::AutonomousTurnStarted(7));

        assert!(app.working());
        assert!(matches!(app.request_cancel(), Action::Cancel));
        assert!(app.phase == Phase::Cancelling);

        app.apply(Update::AutonomousTurnEnded { id: 7, error: None });

        assert!(!app.working());
        assert!(
            matches!(app.blocks.last(), Some(Block::Notice(text)) if text == "turn interrupted")
        );
    }

    #[test]
    fn background_calls_remain_running_after_the_turn_ends() {
        let mut app = app();
        app.apply(Update::ToolStarted {
            id: "background-1".into(),
            title: "compose".into(),
            kind: ToolKind::Other,
            script: Some("return shell({ command: \"sleep 1\" })".into()),
            backgrounded: true,
        });
        app.apply(Update::ToolStarted {
            id: "background-2".into(),
            title: "compose".into(),
            kind: ToolKind::Other,
            script: Some("return shell({ command: \"sleep 2\" })".into()),
            backgrounded: true,
        });

        app.apply(Update::TurnEnded {
            id: None,
            error: None,
        });

        let running = app
            .blocks
            .iter()
            .filter(|block| matches!(block, Block::Tool(call) if call.running()))
            .count();
        assert_eq!(running, 2);
        assert_eq!(
            app.focus_call().map(|call| call.id.as_str()),
            Some("background-2")
        );
        assert!(!app.working());
    }

    #[test]
    fn control_k_kills_the_focused_background_call() {
        let mut app = app();
        app.apply(Update::ToolStarted {
            id: "background".into(),
            title: "compose".into(),
            kind: ToolKind::Other,
            script: Some("return 1".into()),
            backgrounded: true,
        });
        app.graph_pinned = Some(false);

        let action = app.handle_key(modified_press(KeyCode::Char('k'), KeyModifiers::CONTROL));

        assert!(matches!(
            action,
            Action::CancelBackground(id) if id == "background"
        ));
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
    fn deleting_an_attachment_placeholder_omits_it_from_submission() {
        let mut app = app();
        app.attach(
            PathBuf::from("/tmp/image.png"),
            "image/png",
            AttachmentKind::Image,
            3,
        );
        app.editor.submit();
        app.paste("describe without it");
        app.last_key = Some(Instant::now() - Duration::from_millis(500));

        let Action::Submit(prompt) = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected the prompt to be sent");
        };

        assert!(prompt.attachments.is_empty());
    }

    #[test]
    fn available_command_updates_replace_the_session_set_and_clear_on_switch() {
        let mut app = app();
        app.start_session("one".into());
        app.apply(Update::AvailableCommands {
            session_id: "one".into(),
            commands: vec!["compact".into(), "review".into()],
        });
        app.apply(Update::AvailableCommands {
            session_id: "one".into(),
            commands: vec!["compact".into()],
        });
        assert_eq!(app.available_commands, ["compact"]);

        app.apply(Update::AvailableCommands {
            session_id: "stale".into(),
            commands: vec!["ignored".into()],
        });
        assert_eq!(app.available_commands, ["compact"]);

        app.apply(Update::AvailableCommands {
            session_id: "one".into(),
            commands: Vec::new(),
        });
        assert!(app.available_commands.is_empty());
        app.apply(Update::AvailableCommands {
            session_id: "one".into(),
            commands: vec!["compact".into()],
        });

        app.start_session("two".into());
        assert!(app.available_commands.is_empty());
    }

    #[test]
    fn switching_sessions_clears_pending_attachments() {
        let mut app = app();
        app.attach(
            PathBuf::from("/tmp/image.png"),
            "image/png",
            AttachmentKind::Image,
            3,
        );

        app.start_session("fresh".into());

        assert!(app.attachments.is_empty());
    }

    #[test]
    fn a_return_after_a_pause_still_sends() {
        let mut app = app();
        app.paste("first line");
        app.last_key = Some(Instant::now() - Duration::from_millis(500));
        let Action::Submit(prompt) = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected the prompt to be sent");
        };
        assert_eq!(prompt.text, "first line");
    }

    #[test]
    fn advertised_compact_command_is_submitted_unchanged() {
        let mut app = app();
        app.available_commands = vec!["compact".into()];
        app.paste("/compact continue with this");
        app.last_key = Some(Instant::now() - Duration::from_millis(500));
        let Action::Submit(prompt) = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected an ordinary prompt");
        };
        assert_eq!(prompt.text, "/compact continue with this");
    }

    #[test]
    fn local_commands_win_advertised_name_collisions() {
        let mut app = app();
        app.available_commands = vec!["new".into()];
        app.paste("/new begin fresh");
        app.last_key = Some(Instant::now() - Duration::from_millis(500));
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::New(Some(prompt)) if prompt == "begin fresh"
        ));
    }

    #[test]
    fn new_command_carries_an_optional_first_prompt() {
        let mut app = app();
        app.paste("/new begin fresh");
        app.last_key = Some(Instant::now() - Duration::from_millis(500));
        let Action::New(prompt) = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected a new session");
        };
        assert_eq!(prompt.as_deref(), Some("begin fresh"));
    }

    #[test]
    fn unknown_slash_commands_are_submitted_unchanged() {
        let mut app = app();
        app.paste("/newer keep this");
        app.last_key = Some(Instant::now() - Duration::from_millis(500));
        let Action::Submit(prompt) = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected a model prompt");
        };
        assert_eq!(prompt.text, "/newer keep this");
    }

    #[test]
    fn new_command_waits_for_the_active_turn_to_be_idle() {
        let mut app = app();
        app.push_user("working".into());
        app.paste("/new");
        app.last_key = Some(Instant::now() - Duration::from_millis(500));
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert_eq!(app.editor.text(), "/new");
    }

    #[test]
    fn switching_sessions_clears_only_transcript_derived_state() {
        let mut app = app();
        app.blocks.push(Block::User("old transcript".into()));
        app.logs.push("diagnostic".into());
        app.usage = Some(super::ContextUsage { used: 1, size: 2 });
        app.start_session("fresh".into());
        assert_eq!(app.session_id.as_deref(), Some("fresh"));
        assert!(app.blocks.is_empty());
        assert!(app.usage.is_none());
        assert_eq!(app.logs, ["diagnostic"]);
    }

    #[test]
    fn option_arrow_legacy_sequences_move_by_word() {
        let mut app = app();
        app.paste("first second");

        app.handle_key(modified_press(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(app.editor.wrapped(80).1, (0, 6));
        assert_eq!(app.editor.text(), "first second");

        app.handle_key(modified_press(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(app.editor.wrapped(80).1, (0, 12));
        assert_eq!(app.editor.text(), "first second");
    }

    #[test]
    fn maps_clicked_link_columns_in_the_visible_viewport() {
        let mut app = app();
        app.transcript_left = 4;
        app.scroll = 20;
        app.row_links = vec![vec![LinkHit {
            start: 2,
            end: 8,
            url: "https://example.com".into(),
        }]];

        assert_eq!(
            app.clicked_link(7, 0).as_deref(),
            Some("https://example.com")
        );
        assert!(app.clicked_link(12, 0).is_none());
    }

    #[test]
    fn ignores_clicks_while_follow_scroll_is_waiting_for_a_redraw() {
        let mut app = app();
        app.scroll = usize::MAX;
        app.viewport = 2;
        app.transcript_width = 10;
        app.row_calls = vec![None, None];
        app.row_links = vec![Vec::new(), Vec::new()];

        app.click(0, 1);

        assert!(app.clicked_link(0, 1).is_none());
    }

    #[test]
    fn detects_graphical_sessions() {
        use std::ffi::OsStr;

        assert!(!super::has_graphical_session(None, None));
        assert!(!super::has_graphical_session(Some(OsStr::new("")), None));
        assert!(super::has_graphical_session(Some(OsStr::new(":0")), None));
        assert!(super::has_graphical_session(
            None,
            Some(OsStr::new("wayland-0"))
        ));
    }

    #[test]
    fn copies_latest_agent_source_across_tool_boundaries() {
        let mut app = app();
        app.push_user("first".into());
        app.apply(Update::Text("# Heading\n\tindented  ".into()));
        compose(&mut app, "value = tool({})");
        app.apply(Update::Text("\n\n- item".into()));

        let action = app.handle_key(modified_press(KeyCode::Char('y'), KeyModifiers::CONTROL));
        let Action::Copy(text) = action else {
            panic!("expected clipboard action");
        };
        assert_eq!(text, "# Heading\n\tindented  \n\n- item");
    }

    #[test]
    fn copies_only_agent_text_after_the_latest_user_message() {
        let mut app = app();
        app.apply(Update::Text("old".into()));
        app.apply(Update::TurnEnded {
            id: None,
            error: None,
        });
        app.push_user("next".into());
        app.apply(Update::Text("new".into()));

        assert_eq!(app.latest_agent_text().as_deref(), Some("new"));
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

    #[test]
    fn autonomous_text_starts_a_new_block_after_turn_end() {
        let mut app = app();
        app.apply(Update::Text("Started.".into()));
        app.apply(Update::TurnEnded {
            id: None,
            error: None,
        });
        app.apply(Update::Text("RAVENS_".into()));
        app.apply(Update::Text("HARBOR_INEVITABLE".into()));

        let agents = app
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Agent(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(agents, ["Started.", "RAVENS_HARBOR_INEVITABLE"]);
        assert_eq!(
            app.latest_agent_text().as_deref(),
            Some("RAVENS_HARBOR_INEVITABLE")
        );
    }

    #[test]
    fn each_background_completion_seals_the_previous_agent_stream() {
        let mut app = app();
        app.apply(Update::ToolStarted {
            id: "background".into(),
            title: "compose".into(),
            kind: ToolKind::Other,
            script: None,
            backgrounded: true,
        });
        app.apply(Update::Text("first completion".into()));
        app.apply(Update::ToolUpdated {
            id: "background".into(),
            status: Some(ToolCallStatus::Completed),
            script: None,
            output: Vec::new(),
            backgrounded: false,
        });
        app.apply(Update::Text("second completion".into()));
        app.apply(Update::Text(" continued".into()));

        let agents = app
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Agent(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(agents, ["first completion", "second completion continued"]);
    }

    fn model_choice(provider: &str, model: &str) -> super::ModelChoice {
        super::ModelChoice {
            id: format!("{provider}:{model}"),
            provider: provider.into(),
            model: model.into(),
        }
    }

    fn similar_models() -> Vec<super::ModelChoice> {
        vec![
            model_choice("openai-subscription", "gpt-4o"),
            model_choice("openai-subscription", "gpt-4o-mini"),
            model_choice("openai-subscription", "gpt-5.4"),
            model_choice("anthropic", "claude-3-5-haiku"),
            model_choice("anthropic", "claude-3-7-sonnet"),
            model_choice("openrouter", "anthropic/claude-sonnet-4"),
        ]
    }

    #[test]
    fn effort_command_selects_directly_and_dialog_keys_toggle_and_select() {
        let mut app = app();
        app.set_effort(
            "medium".into(),
            ["default", "low", "medium", "high"]
                .into_iter()
                .map(|id| super::EffortChoice {
                    id: id.into(),
                    name: id.into(),
                })
                .collect(),
        );
        app.editor.insert_str("/effort high");
        let Action::SelectEffort {
            effort,
            save_defaults,
        } = app.handle_key(press(KeyCode::Enter))
        else {
            panic!("expected effort selection");
        };
        assert_eq!(effort, "high");
        assert!(!save_defaults);

        app.editor.insert_str("/effort");
        app.last_key = None;
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        app.handle_key(press(KeyCode::Tab));
        app.handle_key(press(KeyCode::Down));
        let Action::SelectEffort {
            effort,
            save_defaults,
        } = app.handle_key(press(KeyCode::Enter))
        else {
            panic!("expected dialog effort selection");
        };
        assert_eq!(effort, "high");
        assert!(save_defaults);
    }

    #[test]
    fn model_search_ranks_the_full_query_and_only_keeps_meaningful_matches() {
        let mut app = app();
        app.set_model_choices(similar_models());
        app.model_dialog = Some(super::ModelDialog {
            query: "claude sonet".into(),
            selected: 0,
            save_defaults: false,
        });

        let ranked = app.selected_model_choices();
        assert_eq!(
            ranked
                .iter()
                .map(|choice| choice.model.as_str())
                .collect::<Vec<_>>(),
            ["anthropic/claude-sonnet-4", "claude-3-7-sonnet"]
        );

        app.model_dialog.as_mut().unwrap().query = "totally unrelated".into();
        assert!(app.selected_model_choices().is_empty());
    }

    #[test]
    fn model_command_uses_multi_token_relevance_and_does_not_guess_on_no_match() {
        let mut app = app();
        app.set_model_choices(similar_models());
        app.editor.insert_str("/model claude 3 7 sonnet");

        let Action::SelectModel { choice, .. } = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected model selection");
        };
        assert_eq!(choice.model, "claude-3-7-sonnet");

        app.editor.insert_str("/model gpt mini");
        app.last_key = None;
        let Action::SelectModel { choice, .. } = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected model selection");
        };
        assert_eq!(choice.model, "gpt-4o-mini");

        app.editor.insert_str("/model completely unknown");
        app.last_key = None;
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert_eq!(
            app.toast_text(),
            Some("no model matches \"completely unknown\"")
        );
    }

    #[test]
    fn redraw_ticks_only_while_time_dependent_ui_is_visible() {
        let mut app = app();
        assert!(!app.needs_redraw_tick());

        app.phase = Phase::Working;
        assert!(app.needs_redraw_tick());
        app.phase = Phase::Idle;

        app.apply(Update::ToolStarted {
            id: "background".into(),
            title: "shell".into(),
            kind: ToolKind::Execute,
            script: None,
            backgrounded: true,
        });
        app.phase = Phase::Idle;
        assert!(app.needs_redraw_tick());

        app.apply(Update::ToolUpdated {
            id: "background".into(),
            status: Some(ToolCallStatus::Completed),
            script: None,
            output: Vec::new(),
            backgrounded: true,
        });
        assert!(!app.needs_redraw_tick());

        app.toast = Some(("done".into(), Instant::now() - Duration::from_secs(5)));
        assert!(app.needs_redraw_tick());
        app.tick();
        assert!(app.toast.is_none());
        assert!(!app.needs_redraw_tick());
    }

    #[test]
    fn exact_basename_and_contiguous_phrase_beat_broader_token_matches() {
        let mut app = app();
        app.set_model_choices(vec![
            model_choice("second", "gpt-4o-mini"),
            model_choice("second", "claude-sonnet-old"),
            model_choice("first", "family/claude-sonnet"),
            model_choice("second", "claude-new-sonnet"),
            model_choice("third", "other/claude-sonnet"),
            model_choice("first", "gpt-4o"),
        ]);

        assert_eq!(
            app.closest_model("claude-sonnet").unwrap().provider,
            "first"
        );
        assert_eq!(
            app.closest_model("claude sonnet").unwrap().provider,
            "first"
        );
        assert_eq!(app.closest_model("gpt 4o").unwrap().model, "gpt-4o");

        app.set_model_choices(vec![model_choice("first", "fooo-bar-foo")]);
        assert_eq!(app.closest_model("foo bar").unwrap().model, "fooo-bar-foo");
    }
}
