//! Client state: the transcript, live tool activity, and key handling.

use std::{
    cmp::Reverse,
    collections::{BTreeSet, HashMap, VecDeque},
    ops::Range,
    path::PathBuf,
    time::{Duration, Instant},
};

#[cfg(any(test, target_os = "linux"))]
use std::ffi::OsStr;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Command, Stdio};

use agent_client_protocol::schema::v2::{StopReason, ToolCallStatus, ToolKind};
#[cfg(test)]
use agentkit_core::{DataRef, Item, ItemKind, Modality, Part, ToolOutput};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::text::Line;

#[cfg(test)]
use crate::compaction::is_compaction_summary;
use crate::events::RuntimeEvent;

const MAX_TOOL_OUTPUT_LINES: usize = 5_000;
const MAX_IMAGE_BASE64_BYTES: usize = 14 * 1024 * 1024;
const MAX_IMAGE_SOURCE_BYTES: usize = 10 * 1024 * 1024;
const MAX_RETAINED_IMAGE_SOURCE_BYTES: usize = 32 * 1024 * 1024;

use super::{
    command::{Parsed, known_token, parse},
    editor::Editor,
    plan::{PlanNode, parse as parse_plan},
    wrap::LinkHit,
};

/// Everything the client learns from the agent or its own runtime channel.
#[derive(Debug)]
pub enum Update {
    /// The actual dynamically allocated A2A listen address.
    A2aAddress(String),
    /// Result of listing sessions without blocking the terminal event loop.
    SessionCatalog(Result<Vec<crate::session::CatalogEntry>, String>),
    /// A steer was accepted but has not been delivered into the transcript yet.
    SteerAccepted { id: String, text: String },
    /// A user message delivered or replayed by the agent.
    UserMessage {
        id: String,
        text: String,
        images: Vec<UserImage>,
        append: bool,
    },
    /// Agent prose, either appended as a chunk or replaced by an upsert.
    AgentMessage {
        id: String,
        text: String,
        append: bool,
    },
    /// Agent reasoning, either appended as a chunk or replaced by an upsert.
    AgentThought {
        id: String,
        text: String,
        append: bool,
    },
    /// A tool call was announced.
    ToolStarted {
        id: String,
        title: String,
        kind: ToolKind,
        script: Option<String>,
        backgrounded: bool,
    },
    /// A tool call changed status or produced output.
    #[cfg(test)]
    ToolUpdated {
        id: String,
        status: Option<ToolCallStatus>,
        script: Option<String>,
        output: Vec<String>,
        backgrounded: bool,
    },
    /// A patchable ACP v2 tool call update or content chunk.
    ToolPatched {
        id: String,
        title: Option<String>,
        kind: Option<ToolKind>,
        status: Option<ToolCallStatus>,
        script: Option<String>,
        output: Option<Vec<String>>,
        append_output: bool,
        backgrounded: bool,
    },
    /// Agent-advertised slash commands for one session.
    AvailableCommands {
        session_id: String,
        commands: Vec<String>,
    },
    /// Full session configuration snapshot.
    ConfigOptions(Vec<agent_client_protocol::schema::v2::SessionConfigOption>),
    /// Context window accounting.
    Usage { used: u64, size: u64 },
    /// Standard ACP v2 foreground state.
    State {
        active: bool,
        steerable: bool,
        cancelled: bool,
    },
    /// An ACP v2 turn became idle with its exact terminal reason.
    Stopped(Option<StopReason>),
    /// A nested tool call started or finished inside a compose run.
    Runtime(RuntimeEvent),
    /// A diagnostic line from the agent process.
    Log(String),
    /// The ACP process exited while work could still be active.
    ProcessExited(String),
}

#[cfg(test)]
impl Update {
    pub(super) fn test_text(text: String) -> Self {
        Self::AgentMessage {
            id: "test-agent".into(),
            text,
            append: true,
        }
    }

    pub(super) fn test_thought(text: String) -> Self {
        Self::AgentThought {
            id: "test-thought".into(),
            text,
            append: true,
        }
    }
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

/// A display row's tags: owning tool call, fenced-code content, the logical
/// source line it was wrapped from, and source whitespace removed before this
/// row. The latter lets copy distinguish word wraps from hard token wraps.
pub(super) type CachedTranscriptRow = (
    Line<'static>,
    (Option<String>, Option<CodeHit>, Option<usize>),
    Vec<LinkHit>,
    String,
);

/// A drag selection over the transcript, in absolute display-line coordinates
/// so it stays anchored to content while the transcript scrolls. Both cells
/// are inclusive; `anchor` is where the drag began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl Selection {
    /// The selection's cells in reading order, both ends inclusive.
    pub fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

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

pub struct SessionDialog {
    pub selected: usize,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingSteer {
    pub id: String,
    pub text: String,
}

pub enum Action {
    None,
    Redraw,
    Submit {
        prompt: SubmittedPrompt,
        inject: bool,
    },
    New(Option<String>),
    ListSessions,
    Resume(String),
    Close,
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
    DetachCompose(String),
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
    /// Runlet source shown inline while this compose call is running.
    pub script: String,
    pub plan: Vec<PlanNode>,
    pub children: Vec<Child>,
    /// Raw tool output, kept whole but folded away until asked for.
    pub output: Vec<String>,
    pub expanded: bool,
    /// The user explicitly chose the output's expanded or collapsed state.
    pub expansion_explicit: bool,
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
#[derive(Clone, Debug)]
pub struct UserImage {
    pub(super) key: [u8; 32],
    pub(super) data: String,
    pub(super) mime_type: String,
    /// Source line after which the fixed image viewport is reserved.
    pub(super) line: usize,
}

impl UserImage {
    pub(super) fn new(data: String, mime_type: String, line: usize) -> Option<Self> {
        // Check the encoded and maximum decoded lengths before hashing or retaining
        // attacker-controlled ACP payloads. The exact decode stays lazy.
        if data.len() > MAX_IMAGE_BASE64_BYTES {
            return None;
        }
        let padding = data
            .as_bytes()
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'=')
            .take(2)
            .count();
        let decoded_upper_bound = data
            .len()
            .div_ceil(4)
            .saturating_mul(3)
            .saturating_sub(padding);
        if decoded_upper_bound > MAX_IMAGE_SOURCE_BYTES {
            return None;
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(mime_type.as_bytes());
        hasher.update(&[0]);
        hasher.update(data.as_bytes());
        Some(Self {
            key: *hasher.finalize().as_bytes(),
            data,
            mime_type,
            line,
        })
    }
}

#[derive(Clone, Debug)]
pub struct UserMessage {
    pub(super) text: String,
    pub(super) images: Vec<UserImage>,
}

impl From<String> for UserMessage {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

pub enum Block {
    User(UserMessage),
    Agent(String),
    Thought {
        text: String,
        started: Instant,
        millis: Option<u64>,
    },
    Tool(ToolCall),
    TurnDuration(u64),
    Notice(String),
    Error(String),
}

pub(super) struct CachedTranscriptImage {
    pub source: usize,
    pub row: usize,
}

pub(super) struct CachedTranscriptBlock {
    pub revision: u64,
    pub rows: Vec<CachedTranscriptRow>,
    pub images: Vec<CachedTranscriptImage>,
}

/// What the client is doing right now.
#[derive(PartialEq, Eq)]
pub enum Phase {
    Idle,
    Working,
    Blocked,
    Cancelling,
}

#[derive(Clone, Copy)]
enum MessageRole {
    User,
    Agent,
    Thought,
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
    pub session_choices: Vec<crate::session::CatalogEntry>,
    pub session_dialog: Option<SessionDialog>,
    pub available_commands: Vec<String>,
    pub a2a: String,
    pub session_id: Option<String>,
    /// Session currently associated with the ordered runtime side channel.
    runtime_session_id: Option<String>,
    pub blocks: Vec<Block>,
    pub(super) transcript_cache: Vec<Option<CachedTranscriptBlock>>,
    pub(super) transcript_revisions: Vec<u64>,
    pub(super) transcript_dirty: BTreeSet<usize>,
    pub(super) transcript_dynamic: BTreeSet<usize>,
    pub(super) transcript_thoughts: BTreeSet<usize>,
    pub(super) transcript_prefixes: Vec<usize>,
    pub(super) transcript_cache_width: usize,
    retained_image_source_bytes: usize,
    next_transcript_revision: u64,
    transcript_focus_index: Option<usize>,
    pub editor: Editor,
    pub attachments: Vec<Attachment>,
    next_attachment: usize,
    pub phase: Phase,
    pub turn_started: Option<Instant>,
    pub can_steer: bool,
    pub(super) pending_steers: VecDeque<PendingSteer>,
    message_blocks: HashMap<String, usize>,
    /// The previous assistant stream ended; the next text starts a new block.
    agent_stream_sealed: bool,
    /// Exact source bytes in the latest assistant stream, before TUI rendering.
    latest_agent_source: String,
    pub compacting: bool,
    pub usage: Option<ContextUsage>,
    pub logs: Vec<String>,
    pub show_logs: bool,
    pub show_thoughts: bool,
    /// Tool card selected for output toggling or background cancellation.
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
    pub selection: Option<Selection>,
    /// Pending left press; the flag suppresses a release-click when this press
    /// dismissed an older selection, while still allowing it to start a drag.
    press: Option<(usize, usize, bool)>,
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

#[cfg(test)]
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

#[cfg(test)]
fn safe_media_uri(uri: &str) -> bool {
    uri.len() <= 2_048
        && url::Url::parse(uri).is_ok_and(|uri| matches!(uri.scheme(), "file" | "http" | "https"))
}

#[cfg(test)]
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
            session_choices: Vec::new(),
            session_dialog: None,
            available_commands: Vec::new(),
            a2a,
            session_id: None,
            runtime_session_id: None,
            blocks: Vec::new(),
            transcript_cache: Vec::new(),
            transcript_revisions: Vec::new(),
            transcript_dirty: BTreeSet::new(),
            transcript_dynamic: BTreeSet::new(),
            transcript_thoughts: BTreeSet::new(),
            transcript_prefixes: vec![0],
            transcript_cache_width: 0,
            retained_image_source_bytes: 0,
            next_transcript_revision: 0,
            transcript_focus_index: None,
            editor: Editor::default(),
            attachments: Vec::new(),
            next_attachment: 0,
            phase: Phase::Idle,
            turn_started: None,
            can_steer: false,
            pending_steers: VecDeque::new(),
            message_blocks: HashMap::new(),
            agent_stream_sealed: false,
            latest_agent_source: String::new(),
            compacting: false,
            usage: None,
            logs: Vec::new(),
            show_logs: false,
            show_thoughts: false,
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
            selection: None,
            press: None,
            toast: None,
            last_key: None,
        }
    }

    pub fn working(&self) -> bool {
        self.phase != Phase::Idle
    }

    fn collapse_last_tool_output(&mut self) {
        let previous = self.blocks.len().checked_sub(1).filter(|&index| {
            matches!(
                &self.blocks[index],
                Block::Tool(call) if call.expanded && !call.expansion_explicit
            )
        });
        if let Some(index) = previous {
            if let Block::Tool(call) = &mut self.blocks[index] {
                call.expanded = false;
            }
            self.mark_block_dirty(index);
        }
    }

    fn push_block(&mut self, block: Block) {
        if !matches!(block, Block::TurnDuration(_)) {
            self.collapse_last_tool_output();
        }
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

    fn call_is_latest_message(&self, id: &str) -> bool {
        self.call_index(id).is_some_and(|index| {
            self.blocks[index + 1..]
                .iter()
                .all(|block| matches!(block, Block::TurnDuration(_)))
        })
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
    #[cfg(test)]
    pub fn restore_transcript(&mut self, session_id: String, transcript: &[Item]) {
        self.session_id = Some(session_id);
        for item in transcript {
            match item.kind {
                ItemKind::Developer if is_compaction_summary(item) => {
                    self.push_block(Block::Notice("context compacted".into()));
                }
                ItemKind::System
                | ItemKind::Developer
                | ItemKind::Context
                | ItemKind::Notification => continue,
                ItemKind::User => {
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
                            self.push_block(Block::User(UserMessage {
                                text,
                                images: Vec::new(),
                            }));
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
                                script: call
                                    .input
                                    .get("script")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                plan: call
                                    .input
                                    .get("script")
                                    .and_then(serde_json::Value::as_str)
                                    .map(parse_plan)
                                    .unwrap_or_default(),
                                children: Vec::new(),
                                output: Vec::new(),
                                expanded: call.name == agentkit_tool_compose::COMPOSE_TOOL_NAME,
                                expansion_explicit: false,
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

    fn newest_foreground_compose(&self) -> Option<&ToolCall> {
        self.blocks.iter().rev().find_map(|block| match block {
            Block::Tool(call)
                if call.title == agentkit_tool_compose::COMPOSE_TOOL_NAME
                    && call.running()
                    && !call.backgrounded =>
            {
                Some(call)
            }
            _ => None,
        })
    }

    pub fn elapsed(&self) -> u64 {
        self.turn_started.map_or(0, |started| {
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
        })
    }

    fn stop_turn_timer(&mut self) -> Option<u64> {
        self.turn_started
            .take()
            .map(|started| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
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

    fn apply_message(
        &mut self,
        id: String,
        text: String,
        images: Vec<UserImage>,
        append: bool,
        role: MessageRole,
    ) {
        if !text.is_empty() || !images.is_empty() {
            self.collapse_last_tool_output();
        }
        let mut images = images;
        let existing_index = self.message_blocks.get(&id).copied();
        if !append
            && matches!(role, MessageRole::User)
            && let Some(index) = existing_index
            && let Block::User(existing) = &self.blocks[index]
        {
            let replaced = existing
                .images
                .iter()
                .map(|image| image.data.len())
                .sum::<usize>();
            self.retained_image_source_bytes =
                self.retained_image_source_bytes.saturating_sub(replaced);
        }
        images.retain(|image| {
            let retained = self
                .retained_image_source_bytes
                .saturating_add(image.data.len());
            if retained > MAX_RETAINED_IMAGE_SOURCE_BYTES {
                false
            } else {
                self.retained_image_source_bytes = retained;
                true
            }
        });
        if let Some(index) = existing_index {
            let mut changed = false;
            match (&mut self.blocks[index], role) {
                (Block::User(existing), MessageRole::User) => {
                    if append {
                        let last_line = existing.text.bytes().filter(|&byte| byte == b'\n').count();
                        let follows_image =
                            existing.images.iter().any(|image| image.line == last_line);
                        let starts_image = !images.is_empty();
                        if !existing.text.is_empty()
                            && !existing.text.ends_with('\n')
                            && !text.starts_with('\n')
                            && !text.is_empty()
                            && (follows_image || starts_image)
                        {
                            existing.text.push('\n');
                        }
                        let line_offset =
                            existing.text.bytes().filter(|&byte| byte == b'\n').count();
                        existing.text.push_str(&text);
                        for image in &mut images {
                            image.line += line_offset;
                        }
                        existing.images.extend(std::mem::take(&mut images));
                    } else {
                        existing.text = text.clone();
                        existing.images = std::mem::take(&mut images);
                    }
                    changed = true;
                }
                (Block::Agent(existing), MessageRole::Agent) => {
                    if append {
                        existing.push_str(&text);
                        self.latest_agent_source.push_str(&text);
                    } else {
                        if self.latest_agent_source.ends_with(existing.as_str()) {
                            self.latest_agent_source
                                .truncate(self.latest_agent_source.len() - existing.len());
                            self.latest_agent_source.push_str(&text);
                        } else {
                            self.latest_agent_source = text.clone();
                        }
                        *existing = text.clone();
                    }
                    changed = true;
                }
                (Block::Thought { text: existing, .. }, MessageRole::Thought) => {
                    if append {
                        existing.push_str(&text);
                    } else {
                        *existing = text.clone();
                    }
                    changed = true;
                }
                _ => {}
            }
            if changed {
                self.mark_block_dirty(index);
                return;
            }
        }

        match role {
            MessageRole::User => {
                self.close_thought();
                self.agent_stream_sealed = true;
                self.push_block(Block::User(UserMessage { text, images }));
            }
            MessageRole::Agent => {
                self.close_thought();
                if self.agent_stream_sealed {
                    self.latest_agent_source = text.clone();
                } else {
                    self.latest_agent_source.push_str(&text);
                }
                self.agent_stream_sealed = false;
                self.push_block(Block::Agent(text));
            }
            MessageRole::Thought => self.push_block(Block::Thought {
                text,
                started: Instant::now(),
                millis: None,
            }),
        }
        self.message_blocks.insert(id, self.blocks.len() - 1);
    }

    fn finish_turn(&mut self, cancelled: bool) {
        self.finish_turn_with_outcome(!cancelled, cancelled.then_some("turn interrupted".into()));
    }

    fn finish_with_stop_reason(&mut self, reason: Option<StopReason>) {
        let (successful, notice) = match reason {
            Some(StopReason::EndTurn) => (true, None),
            Some(StopReason::Cancelled) => (false, Some("turn interrupted".into())),
            Some(StopReason::MaxTokens) => (
                false,
                Some("turn stopped: maximum token limit reached".into()),
            ),
            Some(StopReason::MaxTurnRequests) => (
                false,
                Some("turn stopped: maximum turn-request limit reached".into()),
            ),
            Some(StopReason::Refusal) => (false, Some("turn refused".into())),
            Some(StopReason::Other(reason)) if reason == "_error" => {
                (false, Some("turn failed".into()))
            }
            Some(StopReason::Other(reason)) => (false, Some(format!("turn stopped: {reason}"))),
            Some(_) => (false, Some("turn stopped for an unknown reason".into())),
            None => (false, Some("turn stopped without a reason".into())),
        };
        self.finish_turn_with_outcome(successful, notice);
    }

    fn finish_turn_with_outcome(&mut self, successful: bool, notice: Option<String>) {
        self.pending_steers.clear();
        if self.phase == Phase::Idle {
            self.agent_stream_sealed = true;
            return;
        }
        self.close_thought();
        self.agent_stream_sealed = true;
        let interrupted = self.phase == Phase::Cancelling;
        let turn_millis = self.stop_turn_timer();
        self.phase = Phase::Idle;
        self.compacting = false;
        let mut finished = Vec::new();
        for (index, block) in self.blocks.iter_mut().enumerate() {
            if let Block::Tool(call) = block
                && call.running()
                && !call.backgrounded
            {
                call.status = if successful {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                };
                call.finished = Some(Instant::now());
                call.finish_running_children();
                finished.push(index);
            }
        }
        for index in finished {
            self.mark_block_dirty(index);
            self.reclassify_dynamic(index);
        }
        if interrupted {
            self.note("turn interrupted");
        } else if let Some(notice) = notice {
            self.note(notice);
        }
        if let Some(millis) = turn_millis {
            self.push_block(Block::TurnDuration(millis));
        }
    }

    pub fn apply(&mut self, update: Update) {
        match update {
            Update::A2aAddress(address) => self.a2a = address,
            Update::SessionCatalog(result) => match result {
                Ok(entries) if entries.is_empty() => {
                    self.toast("no sessions found for this workspace");
                }
                Ok(entries) => {
                    self.session_choices = entries;
                    self.session_dialog = Some(SessionDialog { selected: 0 });
                }
                Err(error) => self.toast(format!("could not list sessions: {error}")),
            },
            Update::AvailableCommands {
                session_id,
                commands,
            } => {
                if self.session_id.as_deref() == Some(session_id.as_str()) {
                    self.available_commands = commands;
                }
            }
            Update::SteerAccepted { id, text } => {
                if self.message_blocks.contains_key(&id) {
                    return;
                }
                if let Some(pending) = self
                    .pending_steers
                    .iter_mut()
                    .find(|pending| pending.id == id)
                {
                    pending.text = text;
                } else {
                    self.pending_steers.push_back(PendingSteer { id, text });
                }
            }
            Update::UserMessage {
                id,
                text,
                images,
                append,
            } => {
                self.pending_steers.retain(|pending| pending.id != id);
                self.apply_message(id, text, images, append, MessageRole::User);
            }
            Update::AgentMessage { id, text, append } => {
                self.apply_message(id, text, Vec::new(), append, MessageRole::Agent);
            }
            Update::AgentThought { id, text, append } => {
                self.apply_message(id, text, Vec::new(), append, MessageRole::Thought);
            }
            Update::ToolStarted {
                id,
                title,
                kind,
                script,
                backgrounded,
            } => {
                self.close_thought();
                self.prepare_focused_call(id.clone());
                let expanded = title == agentkit_tool_compose::COMPOSE_TOOL_NAME;
                self.push_block(Block::Tool(ToolCall {
                    id,
                    title,
                    kind,
                    status: ToolCallStatus::Pending,
                    started: Instant::now(),
                    finished: None,
                    plan: script.as_deref().map(parse_plan).unwrap_or_default(),
                    script: script.unwrap_or_default(),
                    children: Vec::new(),
                    output: Vec::new(),
                    expanded,
                    expansion_explicit: false,
                    backgrounded,
                }));
            }
            #[cfg(test)]
            Update::ToolUpdated {
                id,
                status,
                script,
                output,
                backgrounded,
            } => {
                let expand_compose = self.call_is_latest_message(&id);
                let completed_background = {
                    let Some(call) = self.call_mut(&id) else {
                        return;
                    };
                    let was_running = call.running();
                    if let Some(script) = script {
                        call.plan = parse_plan(&script);
                        call.script = script;
                    }
                    if !output.is_empty() {
                        call.output = output;
                    }
                    call.backgrounded |= backgrounded;
                    if let Some(status) = status {
                        call.status = status;
                        if !call.running() {
                            if call.title == agentkit_tool_compose::COMPOSE_TOOL_NAME
                                && !call.expansion_explicit
                            {
                                call.expanded = expand_compose;
                            }
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
            Update::ToolPatched {
                id,
                title,
                kind,
                status,
                script,
                output,
                append_output,
                backgrounded,
            } => {
                if self.call_index(&id).is_none() {
                    self.apply(Update::ToolStarted {
                        id: id.clone(),
                        title: title.clone().unwrap_or_else(|| "Tool".into()),
                        kind: kind.clone().unwrap_or_default(),
                        script: script.clone(),
                        backgrounded,
                    });
                }
                let expand_compose = self.call_is_latest_message(&id);
                let Some(call) = self.call_mut(&id) else {
                    return;
                };
                if let Some(title) = title {
                    if title == agentkit_tool_compose::COMPOSE_TOOL_NAME
                        && call.title != agentkit_tool_compose::COMPOSE_TOOL_NAME
                        && !call.expansion_explicit
                    {
                        call.expanded = true;
                    }
                    call.title = title;
                }
                if let Some(kind) = kind {
                    call.kind = kind;
                }
                if let Some(script) = script {
                    call.plan = parse_plan(&script);
                    call.script = script;
                }
                if let Some(output) = output {
                    if append_output {
                        call.output.extend(output);
                    } else {
                        call.output = output;
                    }
                    call.output.truncate(MAX_TOOL_OUTPUT_LINES);
                }
                call.backgrounded |= backgrounded;
                if let Some(status) = status {
                    call.status = status;
                    if !call.running() {
                        if call.title == agentkit_tool_compose::COMPOSE_TOOL_NAME
                            && !call.expansion_explicit
                        {
                            call.expanded = expand_compose;
                        }
                        call.finished = Some(Instant::now());
                        call.finish_running_children();
                    }
                }
                if let Some(index) = self.call_index(&id) {
                    self.mark_block_dirty(index);
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
            Update::ConfigOptions(_) => {}
            Update::State {
                active,
                steerable,
                cancelled,
            } => {
                if active {
                    if self.phase == Phase::Idle {
                        self.agent_stream_sealed = true;
                        self.turn_started = Some(Instant::now());
                    }
                    if self.phase != Phase::Cancelling {
                        self.phase = if steerable {
                            Phase::Working
                        } else {
                            Phase::Blocked
                        };
                    }
                    self.follow = true;
                    self.scroll = usize::MAX;
                } else {
                    self.finish_turn(cancelled);
                }
            }
            Update::Stopped(reason) => self.finish_with_stop_reason(reason),
            Update::ProcessExited(error) => {
                self.finish_turn_with_outcome(false, None);
                self.push_block(Block::Error(error));
            }
        }
        if self.follow {
            self.scroll = usize::MAX;
        }
    }

    fn apply_runtime(&mut self, event: RuntimeEvent) {
        if let RuntimeEvent::SessionStarted { session_id } = event {
            self.runtime_session_id = Some(session_id);
            return;
        }
        if self.session_id.is_some() && self.runtime_session_id != self.session_id {
            return;
        }
        let event = match event {
            RuntimeEvent::SessionStarted { .. } => unreachable!("handled above"),
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
            RuntimeEvent::SubagentStateChanged { .. }
            | RuntimeEvent::SubagentDescendantsRemoved { .. } => return,
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
            | RuntimeEvent::CompactionFinished { .. }
            | RuntimeEvent::SubagentStateChanged { .. }
            | RuntimeEvent::SubagentDescendantsRemoved { .. } => unreachable!("handled above"),
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
        self.retained_image_source_bytes = 0;
        self.transcript_focus_index = None;
        self.clear_attachments();
        self.latest_agent_source.clear();
        self.phase = Phase::Idle;
        self.turn_started = None;
        self.message_blocks.clear();
        self.pending_steers.clear();
        self.compacting = false;
        self.usage = None;
        self.show_logs = false;
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
        self.selection = None;
        self.press = None;
    }

    #[cfg(test)]
    pub fn push_user(&mut self, prompt: String) -> u64 {
        let id = format!("test-user-{}", self.blocks.len());
        self.apply(Update::UserMessage {
            id,
            text: prompt,
            images: Vec::new(),
            append: false,
        });
        self.apply(Update::State {
            active: true,
            steerable: true,
            cancelled: false,
        });
        self.blocks.len() as u64
    }

    /// Folds a tool call's raw output open or shut.
    pub fn toggle_output(&mut self, id: &str) {
        if let Some(call) = self.call_mut(id) {
            call.expanded = !call.expanded;
            call.expansion_explicit = true;
        }
        if let Some(index) = self.call_index(id) {
            self.mark_block_dirty(index);
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
        self.press = None;
        let top = self.total_lines.saturating_sub(self.viewport);
        let current = self.scroll.min(top);
        self.scroll = current.saturating_add_signed(lines).min(top);
        self.follow = self.scroll >= top;
    }

    fn scroll_to_top(&mut self) {
        self.press = None;
        self.follow = false;
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.press = None;
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

    pub fn restore_attachments(&mut self, attachments: Vec<Attachment>) {
        self.next_attachment = attachments
            .iter()
            .filter_map(|attachment| {
                attachment
                    .placeholder
                    .strip_suffix(']')
                    .and_then(|placeholder| placeholder.rsplit_once('#'))
                    .and_then(|(_, number)| number.parse().ok())
            })
            .max()
            .unwrap_or(0);
        self.attachments = attachments;
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

    fn handle_session_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => self.session_dialog = None,
            KeyCode::Up => {
                if let Some(dialog) = &mut self.session_dialog {
                    dialog.selected = dialog.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(dialog) = &mut self.session_dialog {
                    dialog.selected =
                        (dialog.selected + 1).min(self.session_choices.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                let selected = self
                    .session_dialog
                    .as_ref()
                    .map_or(0, |dialog| dialog.selected);
                if let Some(entry) = self.session_choices.get(selected) {
                    let id = entry.id.clone();
                    self.session_dialog = None;
                    return Action::Resume(id);
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
        if self.session_dialog.is_some() {
            return self.handle_session_key(key);
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
            KeyCode::Char('b') if key.modifiers == KeyModifiers::SUPER => {
                let Some(call_id) = self.newest_foreground_compose().map(|call| call.id.clone())
                else {
                    self.toast("no foreground compose call is running");
                    return Action::None;
                };
                return Action::DetachCompose(call_id);
            }
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
                if let Some(text) = self.selection_text() {
                    self.toast("copied selection");
                    return Action::Copy(text);
                }
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
                let inject = self.working();
                if inject {
                    if self.phase != Phase::Working {
                        self.toast("the agent is waiting for required input");
                        return Action::None;
                    }
                    let input = self.editor.text();
                    if !matches!(parse(input), Parsed::Prompt(_))
                        || known_token(input, &self.available_commands).is_some()
                    {
                        self.toast("commands are available only while idle");
                        return Action::None;
                    }
                    if !self.can_steer {
                        self.toast("this agent does not support active steering");
                        return Action::None;
                    }
                }
                let input = self.editor.submit();
                return match parse(&input) {
                    Parsed::New { prompt } => Action::New(prompt.map(str::to_string)),
                    Parsed::Resume {
                        session_id: Some(session_id),
                    } => Action::Resume(session_id.to_string()),
                    Parsed::Resume { session_id: None } => {
                        self.toast("usage: /resume <session-id>");
                        Action::None
                    }
                    Parsed::Sessions => Action::ListSessions,
                    Parsed::Close => Action::Close,
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
                    Parsed::Prompt(prompt) => Action::Submit {
                        prompt: SubmittedPrompt {
                            text: prompt.to_string(),
                            attachments: self
                                .attachments
                                .iter()
                                .filter(|attachment| prompt.contains(&attachment.placeholder))
                                .cloned()
                                .collect(),
                        },
                        inject,
                    },
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
            KeyCode::Home if control => self.scroll_to_top(),
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
                let dismissed = self.selection.take().is_some();
                self.press = Some((mouse.column as usize, mouse.row as usize, dismissed));
                if dismissed {
                    return Action::Redraw;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((column, row, _)) = self.press else {
                    return Action::None;
                };
                let Some(anchor) = self.transcript_position(column, row) else {
                    return Action::None;
                };
                let head =
                    self.transcript_position_clamped(mouse.column as usize, mouse.row as usize);
                self.selection = Some(Selection { anchor, head });
                return Action::Redraw;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let press = self.press.take();
                // A drag that produced a selection is not a click.
                if self.selection.is_some() {
                    return Action::None;
                }
                if let Some((column, row, false)) = press {
                    return self.click(column, row);
                }
            }
            _ => {}
        }
        Action::None
    }

    pub(super) fn clear_transcript_interaction(&mut self) {
        self.selection = None;
        self.press = None;
    }

    /// Maps a screen cell to (absolute transcript line, column), if it is
    /// inside the transcript area.
    fn transcript_position(&self, column: usize, row: usize) -> Option<(usize, usize)> {
        if self.scroll == usize::MAX || self.viewport == 0 {
            return None;
        }
        let offset = row.checked_sub(self.transcript_top)?;
        let inside = offset < self.viewport
            && column >= self.transcript_left
            && column < self.transcript_left + self.transcript_width;
        inside.then(|| (self.scroll + offset, column - self.transcript_left))
    }

    /// Like [`Self::transcript_position`], but clamps a cell outside the
    /// transcript to its nearest edge so a drag can leave the area.
    fn transcript_position_clamped(&self, column: usize, row: usize) -> (usize, usize) {
        let last_row = self.transcript_top + self.viewport.saturating_sub(1);
        let row = row.clamp(self.transcript_top, last_row);
        let last_column = self.transcript_left + self.transcript_width.saturating_sub(1);
        let column = column.clamp(self.transcript_left, last_column);
        (
            self.scroll + (row - self.transcript_top),
            column - self.transcript_left,
        )
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

    /// The cached row behind an absolute transcript line. `None` covers the
    /// separator rows between blocks and lines outside the cache.
    fn transcript_row(&self, line: usize) -> Option<(usize, &CachedTranscriptRow)> {
        let total = self.transcript_prefixes.last().copied()?;
        if line >= total || self.blocks.is_empty() {
            return None;
        }
        let block = self
            .transcript_prefixes
            .partition_point(|prefix| *prefix <= line)
            .saturating_sub(1)
            .min(self.blocks.len() - 1);
        let span_start = self.transcript_prefixes[block];
        let content_start = span_start + usize::from(span_start > 0);
        let row = line.checked_sub(content_start)?;
        self.transcript_cache
            .get(block)?
            .as_ref()?
            .rows
            .get(row)
            .map(|cached| (block, cached))
    }

    /// The selected text, reconstructed from the rendered rows: rows wrapped
    /// from one logical line rejoin, trailing padding is dropped, and fenced
    /// code loses the two-column display gutter it is drawn with.
    pub fn selection_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (start, end) = selection.ordered();
        let mut lines: Vec<String> = Vec::new();
        let mut last_logical: Option<(usize, usize)> = None;
        for line in start.0..=end.0 {
            let Some((block, row)) = self.transcript_row(line) else {
                lines.push(String::new());
                last_logical = None;
                continue;
            };
            let text: String = row
                .0
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            let from = if line == start.0 { start.1 } else { 0 };
            let to = if line == end.0 { end.1 + 1 } else { usize::MAX };
            let mut fragment = column_slice(&text, from, to);
            if row.1.1.is_some() && from < 2 && (text.starts_with('│') || text.starts_with("  "))
            {
                fragment = column_slice(fragment, 2 - from, usize::MAX);
            }
            let fragment = fragment.trim_end().to_string();
            let logical = row.1.2.map(|index| (block, index));
            match (logical, last_logical) {
                (Some(current), Some(previous)) if current == previous => {
                    let joined = lines.last_mut().expect("a wrapped row follows its first");
                    let fragment = fragment.trim_start();
                    if !fragment.is_empty() {
                        joined.push_str(&row.3);
                        joined.push_str(fragment);
                    }
                }
                _ => lines.push(fragment),
            }
            last_logical = logical;
        }
        let text = lines.join("\n");
        let text = text.trim_matches('\n');
        (!text.trim().is_empty()).then(|| text.to_string())
    }
}

/// The substring of `text` covering display columns `[from, to)`.
fn column_slice(text: &str, from: usize, to: usize) -> &str {
    use unicode_width::UnicodeWidthChar;

    let mut column = 0;
    let mut start = None;
    let mut end = text.len();
    for (index, character) in text.char_indices() {
        let width = character.width().unwrap_or(0);
        if width > 0 && column >= to {
            end = index;
            break;
        }
        if start.is_none() && column + width > from {
            start = Some(index);
        }
        column += width;
    }
    start.and_then(|start| text.get(start..end)).unwrap_or("")
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

    use agent_client_protocol::schema::v2::{StopReason, ToolCallStatus, ToolKind};
    use agentkit_core::{DataRef, Item, ItemKind, MediaPart, MetadataMap, Modality, Part};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    use super::{
        Action, App, AttachmentKind, Block, MAX_IMAGE_BASE64_BYTES, MAX_IMAGE_SOURCE_BYTES,
        MAX_RETAINED_IMAGE_SOURCE_BYTES, Phase, Update, UserImage,
    };
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

    #[test]
    fn column_slice_includes_a_selected_wide_character_and_its_combining_marks() {
        assert_eq!(super::column_slice("界a", 1, 2), "界");
        assert_eq!(super::column_slice("e\u{301}x", 0, 1), "e\u{301}");
    }

    #[test]
    fn scrolling_or_dismissing_a_selection_cancels_a_pending_mouse_press() {
        let mut app = app();
        app.total_lines = 20;
        app.viewport = 5;
        app.follow = false;
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };

        app.handle_mouse(down);
        assert!(app.press.is_some());
        app.scroll_by(1);
        assert!(app.press.is_none());

        app.handle_mouse(down);
        app.scroll_to_bottom();
        assert!(app.press.is_none());

        app.handle_mouse(down);
        app.scroll_to_top();
        assert!(app.press.is_none());

        app.transcript_width = 10;
        app.selection = Some(super::Selection {
            anchor: (0, 0),
            head: (0, 1),
        });
        assert!(matches!(app.handle_mouse(down), Action::Redraw));
        assert!(app.selection.is_none());
        assert!(app.press.is_some());

        assert!(matches!(
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 3,
                ..down
            }),
            Action::Redraw
        ));
        assert!(app.selection.is_some());
        assert!(matches!(
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 3,
                ..down
            }),
            Action::None
        ));
        assert!(app.press.is_none());
    }

    fn app() -> App {
        App::new(
            PathBuf::from("/tmp"),
            "openai-subscription".into(),
            "gpt-5.4".into(),
            "127.0.0.1:7331".into(),
        )
    }

    #[test]
    fn oversized_user_image_payload_is_rejected_before_retention() {
        let encoded_too_large = "A".repeat(MAX_IMAGE_BASE64_BYTES + 1);
        assert!(UserImage::new(encoded_too_large, "image/png".into(), 0).is_none());

        let decoded_too_large = "A".repeat((MAX_IMAGE_SOURCE_BYTES + 1).div_ceil(3) * 4);
        assert!(UserImage::new(decoded_too_large, "image/png".into(), 0).is_none());
    }

    #[test]
    fn retained_user_image_sources_have_an_aggregate_bound() {
        let source_bytes = 9 * 1024 * 1024;
        let mut app = app();
        for index in 0..4 {
            let image = UserImage::new("A".repeat(source_bytes), "image/png".into(), 0)
                .expect("source is within the per-image limit");
            app.apply(Update::UserMessage {
                id: format!("image-{index}"),
                text: format!("[Image #{index}]"),
                images: vec![image],
                append: false,
            });
        }

        assert_eq!(app.retained_image_source_bytes, source_bytes * 3);
        assert!(app.retained_image_source_bytes <= MAX_RETAINED_IMAGE_SOURCE_BYTES);
        assert!(matches!(
            app.blocks.last(),
            Some(Block::User(message)) if message.images.is_empty()
        ));
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
    fn runtime_events_do_not_cross_session_transitions() {
        let mut app = app();
        app.start_session("new-session".into());
        app.apply(Update::Runtime(RuntimeEvent::SessionStarted {
            session_id: "old-session".into(),
        }));
        app.apply(Update::Runtime(RuntimeEvent::CompactionStarted {
            reason: "TokenThreshold".into(),
            at: 0,
        }));
        assert!(!app.compacting);

        app.apply(Update::Runtime(RuntimeEvent::SessionStarted {
            session_id: "new-session".into(),
        }));
        app.apply(Update::Runtime(RuntimeEvent::CompactionStarted {
            reason: "TokenThreshold".into(),
            at: 0,
        }));
        assert!(app.compacting);
    }

    #[test]
    fn turn_end_clears_compaction_state() {
        let mut app = app();
        app.compacting = true;
        app.apply(Update::State {
            active: true,
            steerable: true,
            cancelled: false,
        });
        app.apply(Update::State {
            active: false,
            steerable: false,
            cancelled: false,
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
    fn restored_transcript_hides_internal_notifications() {
        let transcript = vec![
            Item::text(ItemKind::User, "run the build"),
            Item::notification("Background tool call completed: very long raw output"),
            Item::text(ItemKind::Assistant, "the build passed"),
        ];
        let mut app = app();

        app.restore_transcript("session".into(), &transcript);

        assert_eq!(app.blocks.len(), 2);
        assert!(matches!(&app.blocks[0], Block::User(message) if message.text == "run the build"));
        assert!(matches!(&app.blocks[1], Block::Agent(text) if text == "the build passed"));
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
            Block::User(message)
                if message.text == "inspect these\n[Image #1](file:///tmp/image.png)\n[Image #2]"
                    && !message.text.contains("data:")
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
        app.apply(Update::State {
            active: true,
            steerable: true,
            cancelled: false,
        });
        app.apply(Update::Stopped(Some(StopReason::EndTurn)));
        let call = app
            .blocks
            .iter()
            .rev()
            .find_map(|block| match block {
                Block::Tool(call) => Some(call),
                _ => None,
            })
            .expect("tool block");
        assert_eq!(call.status, ToolCallStatus::Completed);
        assert!(!app.working());
    }

    #[test]
    fn abnormal_idle_reasons_fail_unresolved_foreground_tools() {
        for (reason, expected_notice) in [
            (StopReason::Cancelled, "turn interrupted"),
            (
                StopReason::MaxTokens,
                "turn stopped: maximum token limit reached",
            ),
            (
                StopReason::MaxTurnRequests,
                "turn stopped: maximum turn-request limit reached",
            ),
            (StopReason::Refusal, "turn refused"),
            (StopReason::Other("_error".into()), "turn failed"),
            (StopReason::Other("custom".into()), "turn stopped: custom"),
        ] {
            let mut app = app();
            compose(&mut app, "a = shell({ command: \"ls\" })\nreturn a");
            app.apply(Update::State {
                active: true,
                steerable: true,
                cancelled: false,
            });
            app.apply(Update::Stopped(Some(reason)));

            let notice = app
                .blocks
                .iter()
                .rev()
                .find_map(|block| match block {
                    Block::Notice(notice) => Some(notice),
                    _ => None,
                })
                .expect("terminal notice");
            assert_eq!(notice, expected_notice);
            let call = app
                .blocks
                .iter()
                .find_map(|block| match block {
                    Block::Tool(call) => Some(call),
                    _ => None,
                })
                .expect("tool call");
            assert_eq!(call.status, ToolCallStatus::Failed);
        }
    }

    #[test]
    fn duplicate_state_updates_are_idempotent() {
        let mut app = app();
        app.apply(Update::State {
            active: true,
            steerable: true,
            cancelled: false,
        });
        let started = app.turn_started;
        app.apply(Update::State {
            active: true,
            steerable: true,
            cancelled: false,
        });
        assert_eq!(app.turn_started, started);
        app.apply(Update::State {
            active: false,
            steerable: false,
            cancelled: false,
        });
        app.apply(Update::State {
            active: false,
            steerable: false,
            cancelled: false,
        });
        assert!(!app.working());
    }

    #[test]
    fn completed_turn_duration_is_recorded_at_the_end() {
        let mut app = app();
        app.push_user("hello".into());
        app.turn_started = Some(Instant::now() - Duration::from_secs(65));

        app.apply(Update::State {
            active: false,
            steerable: false,
            cancelled: false,
        });

        assert!(matches!(
            app.blocks.last(),
            Some(Block::TurnDuration(millis)) if *millis >= 65_000
        ));
    }

    #[test]
    fn autonomous_turn_is_visible_and_cancellable() {
        let mut app = app();

        app.apply(Update::State {
            active: true,
            steerable: true,
            cancelled: false,
        });

        assert!(app.working());
        assert!(matches!(app.request_cancel(), Action::Cancel));
        assert!(app.phase == Phase::Cancelling);

        app.apply(Update::State {
            active: false,
            steerable: false,
            cancelled: false,
        });

        assert!(!app.working());
        assert!(matches!(
            app.blocks.as_slice(),
            [.., Block::Notice(text), Block::TurnDuration(_)] if text == "turn interrupted"
        ));
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

        app.apply(Update::State {
            active: false,
            steerable: false,
            cancelled: false,
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
    fn command_b_detaches_the_newest_running_foreground_compose_only() {
        let mut app = app();
        for (id, title, backgrounded) in [
            ("older", "compose", false),
            ("other", "shell", false),
            ("newest", "compose", false),
            ("background", "compose", true),
        ] {
            app.apply(Update::ToolStarted {
                id: id.into(),
                title: title.into(),
                kind: ToolKind::Other,
                script: None,
                backgrounded,
            });
        }

        let action = app.handle_key(modified_press(KeyCode::Char('b'), KeyModifiers::SUPER));
        assert!(matches!(action, Action::DetachCompose(id) if id == "newest"));
        assert!(matches!(
            app.handle_key(modified_press(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            Action::None
        ));
    }

    #[test]
    fn command_b_reports_when_no_foreground_compose_is_running() {
        let mut app = app();

        assert!(matches!(
            app.handle_key(modified_press(KeyCode::Char('b'), KeyModifiers::SUPER)),
            Action::None
        ));
        assert_eq!(
            app.toast_text(),
            Some("no foreground compose call is running")
        );
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

        let Action::Submit { prompt, .. } = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected the prompt to be sent");
        };

        assert!(prompt.attachments.is_empty());
    }

    #[test]
    fn rejected_prompt_restores_unique_attachment_numbering() {
        let mut app = app();
        for name in ["one.png", "two.png"] {
            app.attach(
                PathBuf::from(format!("/tmp/{name}")),
                "image/png",
                AttachmentKind::Image,
                3,
            );
        }
        let rejected = std::mem::take(&mut app.attachments);
        app.clear_attachments();
        app.restore_attachments(rejected);
        app.attach(
            PathBuf::from("/tmp/three.png"),
            "image/png",
            AttachmentKind::Image,
            3,
        );

        assert_eq!(
            app.attachments
                .iter()
                .map(|attachment| attachment.placeholder.as_str())
                .collect::<Vec<_>>(),
            ["[Image #1]", "[Image #2]", "[Image #3]"]
        );
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
        let Action::Submit { prompt, .. } = app.handle_key(press(KeyCode::Enter)) else {
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
        let Action::Submit { prompt, .. } = app.handle_key(press(KeyCode::Enter)) else {
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
        let Action::Submit { prompt, .. } = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected a model prompt");
        };
        assert_eq!(prompt.text, "/newer keep this");
    }

    #[test]
    fn active_plain_text_is_submitted_as_steering_when_advertised() {
        let mut app = app();
        app.can_steer = true;
        app.apply(Update::State {
            active: true,
            steerable: true,
            cancelled: false,
        });
        app.paste("change direction");
        app.last_key = Some(Instant::now() - Duration::from_millis(500));

        let Action::Submit { prompt, inject } = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected steering submission");
        };
        assert!(inject);
        assert_eq!(prompt.text, "change direction");
        assert!(
            !app.blocks
                .iter()
                .any(|block| matches!(block, Block::User(_)))
        );

        app.apply(Update::SteerAccepted {
            id: "injected-1".into(),
            text: prompt.text,
        });
        assert_eq!(app.pending_steers.len(), 1);
        assert!(
            !app.blocks
                .iter()
                .any(|block| matches!(block, Block::User(_)))
        );

        app.apply(Update::UserMessage {
            id: "injected-1".into(),
            text: "change direction".into(),
            images: Vec::new(),
            append: false,
        });
        assert!(app.pending_steers.is_empty());
        assert!(
            matches!(app.blocks.last(), Some(Block::User(message)) if message.text == "change direction")
        );
        assert!(app.working());
    }

    #[test]
    fn active_text_is_preserved_when_steering_is_not_advertised() {
        let mut app = app();
        app.apply(Update::State {
            active: true,
            steerable: true,
            cancelled: false,
        });
        app.paste("wait");
        app.last_key = Some(Instant::now() - Duration::from_millis(500));
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert_eq!(app.editor.text(), "wait");
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
    fn switching_sessions_restores_the_initial_view_but_retains_diagnostics() {
        let mut app = app();
        app.blocks
            .push(Block::User("old transcript".to_string().into()));
        app.logs.push("diagnostic".into());
        app.show_logs = true;
        app.usage = Some(super::ContextUsage { used: 1, size: 2 });
        app.start_session("fresh".into());
        assert_eq!(app.session_id.as_deref(), Some("fresh"));
        assert!(app.blocks.is_empty());
        assert!(app.usage.is_none());
        assert!(!app.show_logs);
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
        app.apply(Update::test_text("# Heading\n\tindented  ".into()));
        compose(&mut app, "value = tool({})");
        app.apply(Update::AgentMessage {
            id: "post-tool-agent".into(),
            text: "\n\n- item".into(),
            append: true,
        });

        let action = app.handle_key(modified_press(KeyCode::Char('y'), KeyModifiers::CONTROL));
        let Action::Copy(text) = action else {
            panic!("expected clipboard action");
        };
        assert_eq!(text, "# Heading\n\tindented  \n\n- item");
    }

    #[test]
    fn copies_only_agent_text_after_the_latest_user_message() {
        let mut app = app();
        app.apply(Update::test_text("old".into()));
        app.apply(Update::State {
            active: false,
            steerable: false,
            cancelled: false,
        });
        app.push_user("next".into());
        app.apply(Update::AgentMessage {
            id: "next-agent".into(),
            text: "new".into(),
            append: true,
        });

        assert_eq!(app.latest_agent_text().as_deref(), Some("new"));
    }

    #[test]
    fn streams_agent_text_into_one_block() {
        let mut app = app();
        app.apply(Update::test_text("he".into()));
        app.apply(Update::test_text("llo".into()));
        assert_eq!(app.blocks.len(), 1);
        let Some(Block::Agent(text)) = app.blocks.last() else {
            panic!("expected an agent block");
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn autonomous_text_starts_a_new_block_after_turn_end() {
        let mut app = app();
        app.apply(Update::test_text("Started.".into()));
        app.apply(Update::State {
            active: false,
            steerable: false,
            cancelled: false,
        });
        app.apply(Update::AgentMessage {
            id: "autonomous".into(),
            text: "RAVENS_".into(),
            append: true,
        });
        app.apply(Update::AgentMessage {
            id: "autonomous".into(),
            text: "HARBOR_INEVITABLE".into(),
            append: true,
        });

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
        app.apply(Update::test_text("first completion".into()));
        app.apply(Update::ToolUpdated {
            id: "background".into(),
            status: Some(ToolCallStatus::Completed),
            script: None,
            output: Vec::new(),
            backgrounded: false,
        });
        app.apply(Update::AgentMessage {
            id: "second".into(),
            text: "second completion".into(),
            append: true,
        });
        app.apply(Update::AgentMessage {
            id: "second".into(),
            text: " continued".into(),
            append: true,
        });

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
    fn sessions_command_defers_catalog_work_to_the_event_loop() {
        let mut app = app();
        app.editor.insert_str("/sessions");
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::ListSessions
        ));
        assert!(app.session_dialog.is_none());
    }

    #[test]
    fn session_catalog_update_opens_the_dialog() {
        let mut app = app();
        app.apply(Update::SessionCatalog(Ok(vec![
            crate::session::CatalogEntry {
                id: "saved".into(),
                title: Some("Saved".into()),
                preview: None,
                updated_at: 0,
            },
        ])));
        assert_eq!(app.session_choices[0].id, "saved");
        assert!(app.session_dialog.is_some());
    }

    #[test]
    fn session_dialog_selects_a_catalog_entry_for_existing_resume_flow() {
        let mut app = app();
        app.session_choices = ["newer", "older"]
            .into_iter()
            .map(|id| crate::session::CatalogEntry {
                id: id.into(),
                title: Some(format!("{id} title")),
                preview: None,
                updated_at: 0,
            })
            .collect();
        app.session_dialog = Some(super::SessionDialog { selected: 0 });

        assert!(matches!(app.handle_key(press(KeyCode::Down)), Action::None));
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::Resume(id) if id == "older"
        ));
        assert!(app.session_dialog.is_none());
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
