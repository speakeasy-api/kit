//! Client state: the transcript, live tool activity, and key handling.

use std::{
    cmp::Reverse,
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    ops::Range,
    path::PathBuf,
    time::{Duration, Instant},
};

#[cfg(any(test, target_os = "linux"))]
use std::ffi::OsStr;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Command, Stdio};

#[cfg(test)]
use agent_client_protocol::schema::v2::RunningStateUpdate;
use agent_client_protocol::schema::v2::{
    AuthMethodTerminal, StateUpdate, StopReason, ToolCallStatus, ToolKind,
};
#[cfg(test)]
use agentkit_core::{DataRef, Item, ItemKind, Modality, Part, ToolOutput};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{layout::Rect, text::Line};
use unicode_segmentation::UnicodeSegmentation;

#[cfg(test)]
use crate::compaction::is_compaction_summary;
use crate::events::{GenerationOutcome, RuntimeEvent, SubagentStatus};
use crate::file_search::FileMatch;

const MAX_TOOL_OUTPUT_LINES: usize = 5_000;
const MAX_IMAGE_BASE64_BYTES: usize = 14 * 1024 * 1024;
const MAX_IMAGE_SOURCE_BYTES: usize = 10 * 1024 * 1024;
const MAX_RETAINED_IMAGE_SOURCE_BYTES: usize = 32 * 1024 * 1024;

use super::{
    command::{self, Command as SlashCommand, Parsed, known_token, parse},
    editor::Editor,
    plan::{PlanNode, parse as parse_plan},
    transcript::{Navigation, Navigator, Role},
    wrap::LinkHit,
};

/// Everything the client learns from the agent or its own runtime channel.
#[derive(Debug)]
pub enum Update {
    /// The actual dynamically allocated A2A listen address.
    A2aAddress(String),
    /// Result of listing sessions without blocking the terminal event loop.
    SessionCatalog(Result<Vec<crate::session::CatalogEntry>, String>),
    FileMatches {
        revision: u64,
        result: Result<Vec<FileMatch>, String>,
    },
    /// Result of changing one session's custom display name.
    SessionRenamed {
        session_id: String,
        display_name: Option<String>,
        result: Result<Option<String>, String>,
    },
    /// A steer was accepted but has not been delivered into the transcript yet.
    SteerAccepted {
        id: String,
        text: String,
        editable: bool,
    },
    SteerMutationFinished {
        id: String,
        token: u64,
        result: Result<(), SteerMutationError>,
    },
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
        intent: Option<Option<String>>,
        backgrounded: bool,
    },
    /// Agent-advertised slash commands for one session.
    AvailableCommands {
        session_id: String,
        commands: Vec<SlashCommand>,
    },
    /// Full session configuration snapshot.
    ConfigOptions(Vec<agent_client_protocol::schema::v2::SessionConfigOption>),
    /// Context window accounting.
    Usage { used: u64, size: u64 },
    /// The authoritative ACP v2 foreground lifecycle, preserved from the wire.
    State(StateUpdate),
    /// A nested tool call started or finished inside a compose run.
    Runtime(RuntimeEvent),
    /// Session identity captured from the ordered stderr stream, before queuing.
    RoutedRuntime {
        session_id: String,
        event: RuntimeEvent,
    },
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
    pub rename: Option<SessionRename>,
}

pub enum SessionRename {
    Editing(String),
    ConfirmClear,
    Saving,
}

pub struct FilePickerDialog {
    pub query_range: Range<usize>,
    pub revision: u64,
    pub activation: u64,
    pub selected: usize,
    pub matches: Vec<FileMatch>,
    pub status: FilePickerStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilePickerStatus {
    Loading,
    Ready,
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
    /// False if the accepted injection included any non-text content blocks.
    pub editable: bool,
}

#[derive(Debug)]
pub struct SteerMutationError {
    pub message: String,
    pub unavailable: bool,
}

struct SteerMutation {
    token: u64,
    edit_token: Option<u64>,
    text: Option<String>,
}

struct SteerEdit {
    id: String,
    token: u64,
    draft: Editor,
    attachments: Vec<Attachment>,
    next_attachment: usize,
}

pub(super) const BRANCH_WARNING: &str = "Only conversation context changes. Filesystem changes, running processes, and external effects are not rolled back.";

pub(super) struct BranchChooser {
    pub boundaries: Vec<crate::protocols::acp::prompt_branches::PromptBoundary>,
    pub selected: usize,
    pub pending: bool,
}

struct BranchDraft {
    original: Box<App>,
    checkout_token: String,
    submitting: bool,
    // stderr can reach the UI before the submit response installs the child.
    // Keep its session attribution until canonical ACP replay is applied.
    early_runtime: Vec<(String, RuntimeEvent)>,
}

pub enum Action {
    ListPromptBranches {
        epoch: u64,
    },
    PreparePromptBranch {
        epoch: u64,
        address: String,
    },
    SubmitPromptBranch {
        epoch: u64,
        checkout_token: String,
        text: String,
    },
    None,
    Redraw,
    Submit {
        prompt: SubmittedPrompt,
        inject: bool,
    },
    ReplaceSteer {
        id: String,
        text: String,
    },
    RevokeSteer {
        id: String,
    },
    New(Option<String>),
    ListSessions,
    RenameSession {
        session_id: String,
        display_name: Option<String>,
    },
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
    Login(AuthMethodTerminal),
    Copy(String),
    Cancel,
    DetachCompose(String),
    CancelBackground(String),
    SearchFiles {
        query: String,
        revision: u64,
        activation: u64,
    },
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ComposeView {
    #[default]
    Output,
    Script,
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
    /// User-facing summary supplied by a compose caller.
    pub intent: Option<String>,
    pub expanded: bool,
    /// The selected view for an expanded completed compose call.
    pub compose_view: ComposeView,
    /// The user explicitly chose the call's expanded state or compose view.
    pub expansion_explicit: bool,
    /// The call detached from its originating turn.
    pub backgrounded: bool,
}

impl ToolCall {
    pub fn is_compose(&self) -> bool {
        self.title == agentkit_tool_compose::COMPOSE_TOOL_NAME
    }

    pub fn display_title(&self) -> &str {
        if !self.is_compose() {
            return &self.title;
        }
        self.intent
            .as_deref()
            .map(str::trim)
            .filter(|intent| !intent.is_empty())
            .unwrap_or("Running tools.")
    }

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

    fn finalize_terminal_state(&mut self) {
        if self.is_compose() && !self.expansion_explicit {
            self.expanded = false;
            self.compose_view = ComposeView::Output;
        }
        self.finished = Some(Instant::now());
        self.finish_running_children();
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRow {
    pub id: String,
    pub name: String,
    pub status: SubagentStatus,
    pub outcome: Option<GenerationOutcome>,
    pub generation: u64,
    pub task: String,
    pub parent_id: Option<String>,
    pub parent_name: Option<String>,
    pub harness: String,
    pub model: Option<String>,
    pub created_at_unix_ms: u64,
    pub generation_started_at_unix_ms: u64,
    pub generation_finished_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTreeRow<'a> {
    pub row: &'a AgentRow,
    pub depth: usize,
    pub ancestor_has_next_sibling: Vec<bool>,
    pub has_next_sibling: bool,
    pub missing_parent: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentCounts {
    pub total: usize,
    pub starting: usize,
    pub working: usize,
    pub idle: usize,
}

pub struct App {
    pub(super) branch_epoch: u64,
    pub(super) branch_chooser: Option<BranchChooser>,
    branch_draft: Option<BranchDraft>,
    // AvailableCommands is emitted after canonical branch replay. stderr may
    // outrun that ordered ACP stream, so retain diagnostics until it arrives.
    branch_runtime_replay: Option<Vec<RuntimeEvent>>,
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
    pub file_picker: Option<FilePickerDialog>,
    session_catalog_pending: bool,
    pub auth_methods: Vec<AuthMethodTerminal>,
    pub available_commands: Vec<SlashCommand>,
    pub command_completion_selected: usize,
    command_completion_query: Option<String>,
    command_completion_dismissed: Option<String>,
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
    pub(super) navigation: Navigation,
    pub editor: Editor,
    pub attachments: Vec<Attachment>,
    next_attachment: usize,
    pub phase: Phase,
    pub turn_started: Option<Instant>,
    pub can_steer: bool,
    pub can_replace_steer: bool,
    pub(super) selected_steer: Option<String>,
    pub(super) queue_focused: bool,
    /// Asynchronous selector closure must not redirect stale destructive keys.
    pub(super) queue_handoff: bool,
    steer_edit: Option<SteerEdit>,
    retired_steers: HashSet<String>,
    steer_mutations: HashMap<String, SteerMutation>,
    next_steer_token: u64,
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
    /// Shared child storage outlives individual sessions.
    pub storage_pending: bool,
    pub storage_exhausted: bool,
    pub show_thoughts: bool,
    agents_visible: bool,
    agents: HashMap<String, AgentRow>,
    agent_versions: HashMap<String, (u64, u8)>,
    /// Process-lifetime terminal suppression for IDs removed by subtree cleanup.
    cleaned_agent_ids: HashSet<String>,
    cleaned_agent_ancestors: HashSet<String>,
    agents_scroll: usize,
    agents_viewport: usize,
    agents_area: Rect,
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
    next_file_search_revision: u64,
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

fn agent_status_rank(status: SubagentStatus) -> u8 {
    match status {
        SubagentStatus::Starting => 0,
        SubagentStatus::Working => 1,
        SubagentStatus::Idle => 2,
        SubagentStatus::Removed => 3,
    }
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
            file_picker: None,
            session_catalog_pending: false,
            branch_epoch: 0,
            branch_chooser: None,
            branch_draft: None,
            branch_runtime_replay: None,
            auth_methods: Vec::new(),
            available_commands: Vec::new(),
            command_completion_selected: 0,
            command_completion_query: None,
            command_completion_dismissed: None,
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
            navigation: Navigation::default(),
            editor: Editor::default(),
            attachments: Vec::new(),
            next_attachment: 0,
            phase: Phase::Idle,
            turn_started: None,
            can_steer: false,
            can_replace_steer: false,
            selected_steer: None,
            queue_focused: false,
            queue_handoff: false,
            steer_edit: None,
            retired_steers: HashSet::new(),
            steer_mutations: HashMap::new(),
            next_steer_token: 0,
            pending_steers: VecDeque::new(),
            message_blocks: HashMap::new(),
            agent_stream_sealed: false,
            latest_agent_source: String::new(),
            compacting: false,
            usage: None,
            logs: Vec::new(),
            show_logs: false,
            storage_pending: false,
            storage_exhausted: false,
            show_thoughts: false,
            agents_visible: false,
            agents: HashMap::new(),
            agent_versions: HashMap::new(),
            cleaned_agent_ids: HashSet::new(),
            cleaned_agent_ancestors: HashSet::new(),
            agents_scroll: 0,
            agents_viewport: 0,
            agents_area: Rect::default(),
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
            next_file_search_revision: 0,
        }
    }

    pub fn working(&self) -> bool {
        self.phase != Phase::Idle
    }

    fn command_completion_prefix(&self) -> Option<&str> {
        if self.editing_steer()
            || self.queue_focused
            || self.editing_branch()
            || self.branch_chooser.is_some()
        {
            return None;
        }
        command::completion_prefix(self.editor.text(), self.editor.cursor())
    }

    pub fn command_completions(&self) -> Vec<SlashCommand> {
        let Some(prefix) = self.command_completion_prefix() else {
            return Vec::new();
        };
        if self.command_completion_dismissed.as_deref() == Some(prefix) {
            return Vec::new();
        }
        command::completions(
            self.editor.text(),
            self.editor.cursor(),
            &self.available_commands,
            !self.auth_methods.is_empty(),
        )
    }

    fn sync_command_completion(&mut self) {
        let query = self.command_completion_prefix().map(str::to_string);
        if query != self.command_completion_query {
            self.command_completion_selected = 0;
            self.command_completion_dismissed = None;
            self.command_completion_query = query;
        }
        let count = command::completions(
            self.editor.text(),
            self.editor.cursor(),
            &self.available_commands,
            !self.auth_methods.is_empty(),
        )
        .len();
        self.command_completion_selected = self
            .command_completion_selected
            .min(count.saturating_sub(1));
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
        self.navigation.push();
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
        self.navigation.sync(self.blocks.len());
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
        self.working()
            || !self.transcript_dynamic.is_empty()
            || self.toast.is_some()
            || self.agents.values().any(|row| match row.status {
                SubagentStatus::Starting | SubagentStatus::Working => true,
                SubagentStatus::Removed => row.outcome == Some(GenerationOutcome::Failed),
                SubagentStatus::Idle => {
                    row.outcome == Some(GenerationOutcome::Failed)
                        && row.generation_finished_at_unix_ms.is_some_and(|finished| {
                            crate::events::now_millis().saturating_sub(finished) < 4_000
                        })
                }
            })
    }

    /// Advances animations and removes expired transient state.
    pub fn tick(&mut self) {
        self.tick_at(crate::events::now_millis());
    }

    fn tick_at(&mut self, now_unix_ms: u64) {
        self.tick = self.tick.wrapping_add(1);
        self.agents.retain(|_, row| {
            row.status != SubagentStatus::Removed
                || row.outcome != Some(GenerationOutcome::Failed)
                || row
                    .generation_finished_at_unix_ms
                    .is_some_and(|finished| now_unix_ms.saturating_sub(finished) < 4_000)
        });
        self.clamp_agents_scroll();
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
                                intent: call
                                    .input
                                    .get("intent")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string),
                                expanded: false,
                                compose_view: ComposeView::Output,
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

    pub fn show_agents(&self) -> bool {
        self.agents_visible
    }

    pub fn toggle_agents(&mut self) {
        self.agents_visible = !self.agents_visible;
        self.clamp_agents_scroll();
    }

    #[cfg(test)]
    pub fn agents(&self) -> Vec<&AgentRow> {
        self.agent_tree_rows()
            .into_iter()
            .map(|tree_row| tree_row.row)
            .collect()
    }

    pub fn agent_tree_rows(&self) -> Vec<AgentTreeRow<'_>> {
        fn status_rank(status: SubagentStatus) -> u8 {
            match status {
                SubagentStatus::Starting => 0,
                SubagentStatus::Working => 1,
                SubagentStatus::Idle => 2,
                SubagentStatus::Removed => 3,
            }
        }

        fn compare_rows(left: &&AgentRow, right: &&AgentRow) -> std::cmp::Ordering {
            (
                status_rank(left.status),
                left.created_at_unix_ms,
                left.id.as_str(),
            )
                .cmp(&(
                    status_rank(right.status),
                    right.created_at_unix_ms,
                    right.id.as_str(),
                ))
        }

        fn ancestry_is_acyclic(row: &AgentRow, agents: &HashMap<String, AgentRow>) -> bool {
            let mut seen = HashSet::new();
            let mut current = row;
            while let Some(parent) = current
                .parent_id
                .as_deref()
                .and_then(|parent_id| agents.get(parent_id))
            {
                if !seen.insert(current.id.as_str()) {
                    return false;
                }
                current = parent;
            }
            seen.insert(current.id.as_str())
        }

        fn append_subtree<'a>(
            row: &'a AgentRow,
            children: &HashMap<&str, Vec<&'a AgentRow>>,
            ancestor_has_next_sibling: &[bool],
            has_next_sibling: bool,
            missing_parent: bool,
            rows: &mut Vec<AgentTreeRow<'a>>,
        ) {
            rows.push(AgentTreeRow {
                row,
                depth: ancestor_has_next_sibling.len(),
                ancestor_has_next_sibling: ancestor_has_next_sibling.to_vec(),
                has_next_sibling,
                missing_parent,
            });
            if let Some(descendants) = children.get(row.id.as_str()) {
                let mut child_ancestors = ancestor_has_next_sibling.to_vec();
                child_ancestors.push(has_next_sibling);
                for (index, descendant) in descendants.iter().enumerate() {
                    append_subtree(
                        descendant,
                        children,
                        &child_ancestors,
                        index + 1 < descendants.len(),
                        false,
                        rows,
                    );
                }
            }
        }

        let mut roots = Vec::new();
        let mut children: HashMap<&str, Vec<&AgentRow>> = HashMap::new();
        for row in self.agents.values() {
            if ancestry_is_acyclic(row, &self.agents)
                && let Some(parent_id) = row
                    .parent_id
                    .as_deref()
                    .filter(|parent_id| self.agents.contains_key(*parent_id))
            {
                children.entry(parent_id).or_default().push(row);
            } else {
                roots.push(row);
            }
        }
        roots.sort_by(compare_rows);
        for siblings in children.values_mut() {
            siblings.sort_by(compare_rows);
        }

        let mut rows = Vec::with_capacity(self.agents.len());
        for (index, root) in roots.iter().enumerate() {
            append_subtree(
                root,
                &children,
                &[],
                index + 1 < roots.len(),
                root.parent_id
                    .as_deref()
                    .is_some_and(|parent_id| !self.agents.contains_key(parent_id)),
                &mut rows,
            );
        }
        rows
    }

    pub fn agent_counts(&self) -> AgentCounts {
        self.agents
            .values()
            .fold(AgentCounts::default(), |mut counts, row| {
                match row.status {
                    SubagentStatus::Starting => counts.starting += 1,
                    SubagentStatus::Working => counts.working += 1,
                    SubagentStatus::Idle => counts.idle += 1,
                    SubagentStatus::Removed => return counts,
                }
                counts.total += 1;
                counts
            })
    }

    pub fn agents_scroll(&self) -> usize {
        self.agents_scroll
    }

    pub fn set_agents_viewport(&mut self, area: Rect, visible_rows: usize) {
        self.agents_area = area;
        self.agents_viewport = visible_rows;
        self.clamp_agents_scroll();
    }

    fn clamp_agents_scroll(&mut self) {
        self.agents_scroll = self
            .agents_scroll
            .min(self.agents.len().saturating_sub(self.agents_viewport));
    }

    fn scroll_agents_by(&mut self, rows: isize) {
        let top = self.agents.len().saturating_sub(self.agents_viewport);
        self.agents_scroll = self.agents_scroll.saturating_add_signed(rows).min(top);
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
        self.retired_steers
            .extend(self.pending_steers.drain(..).map(|pending| pending.id));
        self.close_queue_after_update();
        if self.phase == Phase::Idle {
            self.agent_stream_sealed = true;
            return;
        }
        self.close_thought();
        self.agent_stream_sealed = true;
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
                call.finalize_terminal_state();
                finished.push(index);
            }
        }
        for index in finished {
            self.mark_block_dirty(index);
            self.reclassify_dynamic(index);
        }
        if let Some(notice) = notice {
            self.note(notice);
        }
        if let Some(millis) = turn_millis {
            self.push_block(Block::TurnDuration(millis));
        }
    }

    pub fn apply(&mut self, update: Update) {
        // The source stays loaded while its provisional replacement is visible.
        if let Some(draft) = &mut self.branch_draft {
            match update {
                Update::RoutedRuntime { session_id, event } => {
                    if draft.original.session_id.as_ref() == Some(&session_id) {
                        draft
                            .original
                            .apply(Update::RoutedRuntime { session_id, event });
                    } else {
                        draft.early_runtime.push((session_id, event));
                    }
                }
                Update::Runtime(RuntimeEvent::StorageStatus { pending, exhausted }) => {
                    // Durability is process-global, not part of the parked view.
                    self.storage_pending = pending;
                    self.storage_exhausted = exhausted;
                    draft
                        .original
                        .apply(Update::Runtime(RuntimeEvent::StorageStatus {
                            pending,
                            exhausted,
                        }));
                }
                Update::Runtime(RuntimeEvent::SessionStarted { session_id }) => {
                    self.runtime_session_id = Some(session_id.clone());
                    if draft.original.session_id.as_ref() == Some(&session_id) {
                        draft.original.activate_runtime_session();
                    }
                }
                Update::Runtime(event) => {
                    if self.runtime_session_id == draft.original.session_id {
                        draft.original.apply(Update::Runtime(event));
                    } else if let Some(session_id) = &self.runtime_session_id {
                        draft.early_runtime.push((session_id.clone(), event));
                    }
                }
                update => {
                    if let Update::ConfigOptions(options) = &update {
                        super::refresh_config_state(&mut draft.original, Some(options));
                    }
                    draft.original.apply(update);
                }
            }
            return;
        }
        if let Some(pending) = &mut self.branch_runtime_replay
            && let Update::Runtime(event) = &update
            && !matches!(
                event,
                RuntimeEvent::StorageStatus { .. } | RuntimeEvent::SessionStarted { .. }
            )
        {
            if self.runtime_session_id == self.session_id {
                pending.push(event.clone());
            }
            return;
        }
        let replay_complete = matches!(&update, Update::AvailableCommands { session_id, .. }
            if self.session_id.as_ref() == Some(session_id));
        match update {
            Update::A2aAddress(address) => self.a2a = address,
            Update::SessionCatalog(result) => {
                self.session_catalog_pending = false;
                match result {
                    Ok(entries) if entries.is_empty() => {
                        self.toast("no sessions found for this workspace");
                    }
                    Ok(entries) => {
                        self.session_choices = entries;
                        self.file_picker = None;
                        self.session_dialog = Some(SessionDialog {
                            selected: 0,
                            rename: None,
                        });
                    }
                    Err(error) => self.toast(format!("could not list sessions: {error}")),
                }
            }
            Update::SessionRenamed {
                session_id,
                display_name,
                result,
            } => match result {
                Ok(title) => {
                    if let Some(dialog) = &mut self.session_dialog {
                        dialog.rename = None;
                    }
                    if let Some(entry) = self
                        .session_choices
                        .iter_mut()
                        .find(|entry| entry.id == session_id)
                    {
                        entry.title = title;
                    }
                }
                Err(error) => {
                    if let Some(dialog) = &mut self.session_dialog {
                        dialog.rename = Some(match display_name {
                            Some(name) => SessionRename::Editing(name),
                            None => SessionRename::ConfirmClear,
                        });
                    }
                    self.toast(format!("could not rename session: {error}"));
                }
            },
            Update::FileMatches { revision, result } => {
                if self
                    .file_picker
                    .as_ref()
                    .is_none_or(|dialog| dialog.revision != revision)
                {
                    return;
                }
                match result {
                    Ok(matches) => {
                        if let Some(dialog) = &mut self.file_picker {
                            dialog.matches = matches;
                            dialog.selected =
                                dialog.selected.min(dialog.matches.len().saturating_sub(1));
                            dialog.status = FilePickerStatus::Ready;
                        }
                    }
                    Err(error) => {
                        self.file_picker = None;
                        self.toast(format!("file search failed: {error}"));
                    }
                }
            }
            Update::AvailableCommands {
                session_id,
                commands,
            } => {
                if self.session_id.as_deref() == Some(session_id.as_str()) {
                    self.available_commands = commands;
                    self.command_completion_dismissed = None;
                    self.sync_command_completion();
                }
            }
            Update::SteerMutationFinished { id, token, result } => {
                self.finish_steer_mutation(&id, token, result);
            }
            Update::SteerAccepted { id, text, editable } => {
                if self.message_blocks.contains_key(&id) || self.retired_steers.contains(&id) {
                    return;
                }
                if let Some(pending) = self
                    .pending_steers
                    .iter_mut()
                    .find(|pending| pending.id == id)
                {
                    pending.text = text;
                    pending.editable = editable;
                } else {
                    if self.queue_focused && self.selected_steer.is_none() {
                        self.selected_steer = Some(id.clone());
                    }
                    self.pending_steers
                        .push_back(PendingSteer { id, text, editable });
                }
            }
            Update::UserMessage {
                id,
                text,
                images,
                append,
            } => {
                self.remove_pending_steer(&id);
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
                    intent: None,
                    expanded,
                    compose_view: ComposeView::Output,
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
                            call.finalize_terminal_state();
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
                intent,
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
                let Some(call) = self.call_mut(&id) else {
                    return;
                };
                if let Some(title) = title {
                    if title == agentkit_tool_compose::COMPOSE_TOOL_NAME
                        && call.title != agentkit_tool_compose::COMPOSE_TOOL_NAME
                        && call.running()
                        && !call.expansion_explicit
                    {
                        call.expanded = true;
                    }
                    call.title = title;
                }
                if let Some(kind) = kind {
                    call.kind = kind;
                }
                if let Some(intent) = intent {
                    call.intent = intent;
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
                        call.finalize_terminal_state();
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
            Update::RoutedRuntime { session_id, event } => {
                if self.session_id.as_ref() == Some(&session_id) {
                    // A queued marker from an older activation cannot change this
                    // event's identity or the route established by ACP activation.
                    self.activate_runtime_session();
                    self.apply(Update::Runtime(event));
                }
            }
            Update::Runtime(event) => self.apply_runtime(event),
            Update::Log(line) => {
                self.logs.push(line);
                if self.logs.len() > 500 {
                    self.logs.drain(..self.logs.len() - 500);
                }
            }
            Update::ConfigOptions(_) => {}
            Update::State(state) => match state {
                StateUpdate::Running(_) | StateUpdate::RequiresAction(_) => {
                    if self.phase == Phase::Idle {
                        self.agent_stream_sealed = true;
                        self.turn_started = Some(Instant::now());
                    }
                    if self.phase != Phase::Cancelling {
                        self.phase = if matches!(state, StateUpdate::Running(_)) {
                            Phase::Working
                        } else {
                            Phase::Blocked
                        };
                    }
                    self.follow = true;
                    self.scroll = usize::MAX;
                }
                StateUpdate::Idle(idle) => self.finish_with_stop_reason(idle.stop_reason),
                _ => {}
            },
            Update::ProcessExited(error) => {
                self.finish_turn_with_outcome(false, None);
                self.retire_active_agents_at(crate::events::now_millis());
                self.push_block(Block::Error(error));
            }
        }
        if replay_complete && let Some(events) = self.branch_runtime_replay.take() {
            // These events were attributed to the active child when queued.
            let route = self
                .runtime_session_id
                .replace(self.session_id.clone().unwrap());
            for event in events {
                self.apply_runtime(event);
            }
            self.runtime_session_id = route;
        }
        if self.follow {
            self.scroll = usize::MAX;
        }
    }

    fn apply_runtime(&mut self, event: RuntimeEvent) {
        if let RuntimeEvent::StorageStatus { pending, exhausted } = event {
            self.storage_pending = pending;
            self.storage_exhausted = exhausted;
            return;
        }
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
            | RuntimeEvent::SubagentDescendantsRemoved { .. } => {
                self.apply_agent_runtime(event);
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
            RuntimeEvent::StorageStatus { .. }
            | RuntimeEvent::SessionStarted { .. }
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

    /// A successful ACP activation establishes the diagnostic route explicitly.
    /// Stderr markers only label ingress events; ACP owns the visible route.
    pub(super) fn activate_runtime_session(&mut self) {
        self.runtime_session_id = self.session_id.clone();
    }

    /// Switches the visible client state to a fresh persisted session. Editor
    /// history and diagnostics remain useful, while transcript-derived state
    /// starts empty.
    pub fn start_session(&mut self, session_id: String) {
        self.abandon_branch();
        self.branch_runtime_replay = None;
        self.branch_epoch = self.branch_epoch.wrapping_add(1);
        self.cancel_steer_edit();
        self.selected_steer = None;
        self.queue_focused = false;
        self.queue_handoff = false;
        self.retired_steers.clear();
        self.steer_mutations.clear();
        self.session_catalog_pending = false;
        self.session_id = Some(session_id);
        self.file_picker = None;
        self.available_commands.clear();
        self.command_completion_selected = 0;
        self.command_completion_query = None;
        self.command_completion_dismissed = None;
        self.blocks.clear();
        self.navigation.reset();
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
        self.agents.clear();
        self.agent_versions.clear();
        self.cleaned_agent_ids.clear();
        self.cleaned_agent_ancestors.clear();
        self.agents_scroll = 0;
        self.agents_viewport = 0;
        self.agents_area = Rect::default();
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
        self.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        self.blocks.len() as u64
    }

    /// Folds a tool call's raw output open or shut. Completed compose calls
    /// cycle through their output and source views before folding closed.
    pub fn toggle_output(&mut self, id: &str) {
        if let Some(call) = self.call_mut(id) {
            if call.is_compose() && !call.running() {
                match (call.expanded, call.compose_view) {
                    (false, _) => {
                        call.expanded = true;
                        call.compose_view = ComposeView::Output;
                    }
                    (true, ComposeView::Output) => call.compose_view = ComposeView::Script,
                    (true, ComposeView::Script) => call.expanded = false,
                }
            } else {
                call.expanded = !call.expanded;
            }
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

    fn apply_agent_runtime(&mut self, event: RuntimeEvent) {
        self.apply_agent_runtime_at(event, crate::events::now_millis());
    }

    fn retire_active_agents_at(&mut self, now_unix_ms: u64) {
        for row in self.agents.values_mut() {
            if matches!(
                row.status,
                SubagentStatus::Starting | SubagentStatus::Working
            ) {
                row.status = SubagentStatus::Removed;
                row.outcome = Some(GenerationOutcome::Failed);
                row.generation_finished_at_unix_ms
                    .get_or_insert(now_unix_ms);
                self.agent_versions.insert(
                    row.id.clone(),
                    (row.generation, agent_status_rank(SubagentStatus::Removed)),
                );
            }
        }
        self.clamp_agents_scroll();
    }

    #[cfg(test)]
    fn apply_runtime_at(&mut self, event: RuntimeEvent, now_unix_ms: u64) {
        match event {
            RuntimeEvent::SubagentStateChanged { .. }
            | RuntimeEvent::SubagentDescendantsRemoved { .. } => {
                self.apply_agent_runtime_at(event, now_unix_ms);
            }
            event => self.apply_runtime(event),
        }
    }

    fn apply_agent_runtime_at(&mut self, event: RuntimeEvent, now_unix_ms: u64) {
        match event {
            RuntimeEvent::SubagentStateChanged {
                id,
                name,
                status,
                outcome,
                generation,
                task,
                parent_id,
                parent_name,
                harness,
                model,
                created_at_unix_ms,
                generation_started_at_unix_ms,
                generation_finished_at_unix_ms,
            } => {
                if self.cleaned_agent_ids.contains(&id) {
                    if status == SubagentStatus::Removed {
                        self.agents.remove(&id);
                    }
                    return;
                }
                if parent_id.as_ref().is_some_and(|parent| {
                    self.cleaned_agent_ancestors.contains(parent)
                        || self.cleaned_agent_ids.contains(parent)
                }) {
                    self.cleaned_agent_ids.insert(id.clone());
                    self.agents.remove(&id);
                    return;
                }
                let incoming_rank = agent_status_rank(status);
                if self
                    .agent_versions
                    .get(&id)
                    .is_some_and(|current| (generation, incoming_rank) <= *current)
                {
                    return;
                }
                self.agent_versions
                    .insert(id.clone(), (generation, incoming_rank));
                if status == SubagentStatus::Removed
                    && (outcome != Some(GenerationOutcome::Failed)
                        || generation_finished_at_unix_ms
                            .is_none_or(|finished| now_unix_ms.saturating_sub(finished) >= 4_000))
                {
                    self.agents.remove(&id);
                } else {
                    self.agents.insert(
                        id.clone(),
                        AgentRow {
                            id,
                            name,
                            status,
                            outcome,
                            generation,
                            task,
                            parent_id,
                            parent_name,
                            harness,
                            model,
                            created_at_unix_ms,
                            generation_started_at_unix_ms,
                            generation_finished_at_unix_ms,
                        },
                    );
                }
            }
            RuntimeEvent::SubagentDescendantsRemoved { ancestor_id } => {
                let mut removed = HashSet::new();
                loop {
                    let before = removed.len();
                    for row in self.agents.values() {
                        if row.id != ancestor_id
                            && row.parent_id.as_deref().is_some_and(|parent| {
                                parent == ancestor_id || removed.contains(parent)
                            })
                        {
                            removed.insert(row.id.clone());
                        }
                    }
                    if removed.len() == before {
                        break;
                    }
                }
                self.agents.retain(|id, _| !removed.contains(id));
                self.cleaned_agent_ancestors.insert(ancestor_id);
                self.cleaned_agent_ids.extend(removed);
            }
            _ => unreachable!("only subagent runtime events reach the roster reducer"),
        }
        self.clamp_agents_scroll();
    }

    pub fn scroll_by(&mut self, lines: isize) {
        self.navigation.anchored = false;
        self.press = None;
        let top = self.total_lines.saturating_sub(self.viewport);
        let current = self.scroll.min(top);
        self.scroll = current.saturating_add_signed(lines).min(top);
        self.follow = self.scroll >= top;
    }

    fn scroll_to_top(&mut self) {
        self.navigation.anchored = false;
        self.press = None;
        self.follow = false;
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.navigation.anchored = false;
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
        if self.branch_draft.is_some() || self.branch_chooser.is_some() {
            self.toast("prompt branch edits are text-only");
            return;
        }
        if self.editing_steer() {
            self.toast("pending-message edits are text-only");
            return;
        }
        self.queue_handoff = false;
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
    pub fn session_rename_active(&self) -> bool {
        self.session_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.rename.is_some())
    }

    pub fn paste(&mut self, text: &str) {
        if self.branch_chooser.is_some() || self.branch_submitting() {
            return;
        }
        if self.branch_draft.is_some() {
            self.last_key = None;
            self.editor.insert_str(text);
            return;
        }
        if let Some(dialog) = &mut self.navigation.dialog {
            dialog.insert(text);
            self.sync_navigation();
            return;
        }
        // An explicit bracketed paste is not part of the unbracketed key-burst heuristic.
        self.last_key = None;
        if let Some(rename) = self
            .session_dialog
            .as_mut()
            .and_then(|dialog| dialog.rename.as_mut())
        {
            if let SessionRename::Editing(input) = rename {
                let remaining = 100_usize.saturating_sub(input.chars().count());
                input.extend(
                    text.chars()
                        .filter(|character| {
                            crate::session::is_safe_display_name_character(*character)
                        })
                        .take(remaining),
                );
            }
            return;
        }
        self.file_picker = None;
        self.queue_handoff = false;
        self.editor.insert_str(text);
        self.sync_command_completion();
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

    fn handle_session_key(&mut self, key: KeyEvent, pasted: bool) -> Action {
        let Some(dialog) = &mut self.session_dialog else {
            return Action::None;
        };

        if let Some(rename) = &mut dialog.rename {
            match rename {
                SessionRename::Saving => {}
                SessionRename::ConfirmClear => match key.code {
                    KeyCode::Enter if pasted => {}
                    KeyCode::Esc => {
                        dialog.rename = Some(SessionRename::Editing(String::new()));
                    }
                    KeyCode::Enter => {
                        let selected = dialog.selected;
                        dialog.rename = Some(SessionRename::Saving);
                        if let Some(entry) = self.session_choices.get(selected) {
                            return Action::RenameSession {
                                session_id: entry.id.clone(),
                                display_name: None,
                            };
                        }
                    }
                    _ => {}
                },
                SessionRename::Editing(input) => match key.code {
                    KeyCode::Enter if pasted => {}
                    KeyCode::Esc => dialog.rename = None,
                    KeyCode::Backspace => {
                        if let Some((index, _)) = input.grapheme_indices(true).next_back() {
                            input.truncate(index);
                        }
                    }
                    KeyCode::Enter if input.trim().is_empty() => {
                        dialog.rename = Some(SessionRename::ConfirmClear);
                    }
                    KeyCode::Enter => {
                        let selected = dialog.selected;
                        let display_name = input.clone();
                        dialog.rename = Some(SessionRename::Saving);
                        if let Some(entry) = self.session_choices.get(selected) {
                            return Action::RenameSession {
                                session_id: entry.id.clone(),
                                display_name: Some(display_name),
                            };
                        }
                    }
                    KeyCode::Char(character)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && crate::session::is_safe_display_name_character(character)
                            && input.chars().count() < 100 =>
                    {
                        input.push(character);
                    }
                    _ => {}
                },
            }
            return Action::None;
        }

        match key.code {
            KeyCode::Esc => self.session_dialog = None,
            KeyCode::Up => dialog.selected = dialog.selected.saturating_sub(1),
            KeyCode::Down => {
                dialog.selected =
                    (dialog.selected + 1).min(self.session_choices.len().saturating_sub(1));
            }
            KeyCode::Char('r' | 'R') => {
                dialog.rename = Some(SessionRename::Editing(String::new()));
            }
            KeyCode::Enter => {
                if let Some(entry) = self.session_choices.get(dialog.selected) {
                    let id = entry.id.clone();
                    self.session_dialog = None;
                    return Action::Resume(id);
                }
            }
            _ => {}
        }
        Action::None
    }

    fn active_file_query_range(&self) -> Option<Range<usize>> {
        let text = self.editor.text();
        let cursor = self.editor.cursor();
        let start = text[..cursor]
            .char_indices()
            .rev()
            .find_map(|(index, character)| {
                character
                    .is_whitespace()
                    .then_some(index + character.len_utf8())
            })
            .unwrap_or(0);
        if cursor < start + 1 || text.as_bytes().get(start) != Some(&b'@') {
            return None;
        }
        let end = text[cursor..]
            .char_indices()
            .find_map(|(index, character)| character.is_whitespace().then_some(cursor + index))
            .unwrap_or(text.len());
        Some(start..end)
    }

    fn next_file_search_revision(&mut self) -> u64 {
        self.next_file_search_revision = self.next_file_search_revision.wrapping_add(1);
        self.next_file_search_revision
    }

    fn start_file_picker(&mut self, query_range: Range<usize>) -> Action {
        let revision = self.next_file_search_revision();
        let query = self.editor.text()[query_range.start + 1..query_range.end].to_string();
        self.file_picker = Some(FilePickerDialog {
            query_range,
            revision,
            activation: revision,
            selected: 0,
            matches: Vec::new(),
            status: FilePickerStatus::Loading,
        });
        Action::SearchFiles {
            query,
            revision,
            activation: revision,
        }
    }

    fn refresh_file_picker(&mut self) -> Action {
        let Some(query_range) = self.active_file_query_range() else {
            self.file_picker = None;
            return Action::None;
        };
        let activation = self
            .file_picker
            .as_ref()
            .expect("active query belongs to a picker")
            .activation;
        let revision = self.next_file_search_revision();
        let query = self.editor.text()[query_range.start + 1..query_range.end].to_string();
        if let Some(dialog) = &mut self.file_picker {
            dialog.query_range = query_range;
            dialog.revision = revision;
            dialog.selected = 0;
            dialog.status = FilePickerStatus::Loading;
        }
        Action::SearchFiles {
            query,
            revision,
            activation,
        }
    }

    fn revalidate_file_picker(&mut self) {
        let Some(query_range) = self.active_file_query_range() else {
            self.file_picker = None;
            return;
        };
        if let Some(dialog) = &mut self.file_picker {
            dialog.query_range = query_range;
        }
    }

    fn handle_file_picker_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => self.file_picker = None,
            KeyCode::Up => {
                if let Some(dialog) = &mut self.file_picker {
                    dialog.selected = dialog.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(dialog) = &mut self.file_picker {
                    dialog.selected =
                        (dialog.selected + 1).min(dialog.matches.len().saturating_sub(1));
                }
            }
            KeyCode::Tab => {
                let selection = self.file_picker.as_ref().and_then(|dialog| {
                    dialog
                        .matches
                        .get(dialog.selected)
                        .map(|item| (dialog.query_range.clone(), item.relative_path.clone()))
                });
                if let Some((range, path)) = selection {
                    self.editor.replace_range(range, &format!("@{path}"));
                    self.file_picker = None;
                }
            }
            KeyCode::Backspace => {
                self.editor.backspace();
                return self.refresh_file_picker();
            }
            KeyCode::Delete => {
                self.editor.delete_forward();
                return self.refresh_file_picker();
            }
            KeyCode::Left => {
                self.editor.move_left();
                self.revalidate_file_picker();
            }
            KeyCode::Right => {
                self.editor.move_right();
                self.revalidate_file_picker();
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::SUPER) =>
            {
                self.editor.insert_char(character);
                return self.refresh_file_picker();
            }
            _ => self.file_picker = None,
        }
        Action::None
    }

    pub fn editing_steer(&self) -> bool {
        self.steer_edit.is_some()
    }

    fn cancel_steer_edit(&mut self) {
        if let Some(edit) = self.steer_edit.take() {
            self.editor = edit.draft;
            self.attachments = edit.attachments;
            self.next_attachment = edit.next_attachment;
            self.file_picker = None;
            self.sync_command_completion();
        }
    }

    fn close_queue_after_update(&mut self) {
        self.queue_handoff |= self.queue_focused;
        self.queue_focused = false;
        self.selected_steer = None;
    }

    fn remove_pending_steer(&mut self, id: &str) {
        self.retired_steers.insert(id.to_owned());
        let index = self
            .pending_steers
            .iter()
            .position(|pending| pending.id == id);
        self.pending_steers.retain(|pending| pending.id != id);
        if self.pending_steers.is_empty() {
            self.close_queue_after_update();
        } else if self.selected_steer.as_deref() == Some(id) {
            self.selected_steer = index.and_then(|index| {
                self.pending_steers
                    .get(index.min(self.pending_steers.len().saturating_sub(1)))
                    .map(|pending| pending.id.clone())
            });
        }
    }

    pub fn begin_steer_mutation(&mut self, id: &str, text: Option<String>) -> Option<u64> {
        if self.steer_mutations.contains_key(id) {
            self.toast("a change to this pending message is still in progress");
            return None;
        }
        if !self.pending_steers.iter().any(|pending| pending.id == id) {
            self.toast("message is no longer pending");
            return None;
        }
        if text.is_some()
            && self
                .pending_steers
                .iter()
                .any(|pending| pending.id == id && !pending.editable)
        {
            self.toast("pending messages with media cannot be edited; removal is still available");
            return None;
        }
        self.next_steer_token += 1;
        let token = self.next_steer_token;
        let edit_token = self
            .steer_edit
            .as_ref()
            .filter(|edit| edit.id == id)
            .map(|edit| edit.token);
        self.steer_mutations.insert(
            id.to_owned(),
            SteerMutation {
                token,
                edit_token,
                text,
            },
        );
        self.toast(if self.steer_mutations[id].text.is_some() {
            "saving pending message…"
        } else {
            "removing pending message…"
        });
        Some(token)
    }

    fn finish_steer_mutation(
        &mut self,
        id: &str,
        token: u64,
        result: Result<(), SteerMutationError>,
    ) {
        if self
            .steer_mutations
            .get(id)
            .is_none_or(|mutation| mutation.token != token)
        {
            return;
        }
        let mutation = self.steer_mutations.remove(id).expect("matched mutation");
        match result {
            Ok(()) => {
                if let Some(text) = mutation.text {
                    // Delivery may have won the race: update only an existing entry.
                    if let Some(pending) = self
                        .pending_steers
                        .iter_mut()
                        .find(|pending| pending.id == id)
                    {
                        pending.text = text.clone();
                    }
                    // Esc/reopen can create another edit for the same ID while this
                    // request waits. Neither that edit nor newly typed text belongs
                    // to this completion.
                    if self
                        .steer_edit
                        .as_ref()
                        .is_some_and(|edit| Some(edit.token) == mutation.edit_token)
                    {
                        if self.editor.text() == text {
                            self.cancel_steer_edit();
                        } else {
                            self.toast("earlier revision saved; current edit is still unsaved");
                        }
                    }
                } else {
                    self.steer_revoked(id);
                }
            }
            Err(error) => self.steer_mutation_failed(id, error.message, error.unavailable),
        }
    }

    pub fn steer_revoked(&mut self, id: &str) {
        self.remove_pending_steer(id);
    }

    pub fn steer_mutation_failed(&mut self, id: &str, error: String, unavailable: bool) {
        if unavailable {
            self.remove_pending_steer(id);
            self.toast(if self.editing_steer() {
                "message is no longer pending; copy your edit or Esc to restore draft"
            } else {
                "message is no longer pending; it cannot be removed"
            });
            return;
        }
        // Keep the replacement composer and its parked draft intact for retry/copy/Esc.
        self.toast(error);
    }

    fn handle_steer_selection(&mut self, key: KeyEvent) -> Action {
        if matches!(key.code, KeyCode::Esc | KeyCode::F(2)) {
            self.selected_steer = None;
            self.queue_focused = false;
            return Action::None;
        }
        let Some(index) = self
            .pending_steers
            .iter()
            .position(|pending| Some(&pending.id) == self.selected_steer.as_ref())
        else {
            self.close_queue_after_update();
            return Action::None;
        };
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                let next = if key.code == KeyCode::Up {
                    index.saturating_sub(1)
                } else {
                    (index + 1).min(self.pending_steers.len() - 1)
                };
                self.selected_steer = Some(self.pending_steers[next].id.clone());
            }
            KeyCode::Backspace | KeyCode::Delete => {
                if self
                    .steer_mutations
                    .contains_key(&self.pending_steers[index].id)
                {
                    self.toast("a change to this pending message is still in progress");
                    return Action::None;
                }
                return Action::RevokeSteer {
                    id: self.pending_steers[index].id.clone(),
                };
            }
            KeyCode::Enter => {
                if !self.can_replace_steer {
                    self.toast("this agent does not support editing pending messages");
                    return Action::None;
                }
                let pending = &self.pending_steers[index];
                if !pending.editable {
                    self.toast(
                        "pending messages with media cannot be edited; removal is still available",
                    );
                    return Action::None;
                }
                let mut editor = Editor::default();
                editor.insert_str(&pending.text);
                self.next_steer_token += 1;
                self.steer_edit = Some(SteerEdit {
                    id: pending.id.clone(),
                    token: self.next_steer_token,
                    draft: std::mem::replace(&mut self.editor, editor),
                    attachments: std::mem::take(&mut self.attachments),
                    next_attachment: self.next_attachment,
                });
                self.next_attachment = 0;
                self.selected_steer = None;
                self.queue_focused = false;
                self.file_picker = None;
                self.last_key = None;
            }
            _ => {}
        }
        Action::None
    }

    /// Open a read-only view without borrowing the parked composer's state.
    pub(super) fn open_navigation(&mut self) {
        self.navigation.sync(self.blocks.len());
        self.navigation.dialog = Some(Navigator {
            selected: self.navigation.revealed,
            ..Navigator::default()
        });
        self.sync_navigation();
    }

    pub(super) fn sync_navigation(&mut self) -> Vec<usize> {
        let matches = self
            .navigation
            .matches(&self.blocks, &self.transcript_revisions);
        self.navigation.reconcile(&matches);
        matches
    }

    fn handle_navigation_key(&mut self, key: KeyEvent, pasted: bool) -> Action {
        if pasted && matches!(key.code, KeyCode::Enter | KeyCode::Tab) {
            // Keep unbracketed paste inside the query, never reveal or cycle
            // roles. Its control whitespace is discarded just like Event::Paste.
            self.paste(if key.code == KeyCode::Enter {
                "\n"
            } else {
                "\t"
            });
            return Action::None;
        }
        let matches = self.sync_navigation();
        let selected = self
            .navigation
            .dialog
            .as_ref()
            .and_then(|dialog| dialog.selected);
        let current = selected.and_then(|id| self.navigation.index(id));
        match key.code {
            KeyCode::Enter
                if key.modifiers.is_empty()
                    && self
                        .navigation
                        .dialog
                        .as_ref()
                        .is_some_and(|dialog| dialog.query == "/branch") =>
            {
                return self.open_branch_chooser();
            }
            KeyCode::Esc | KeyCode::F(3) => self.navigation.dialog = None,
            KeyCode::Enter if key.modifiers.is_empty() => {
                if let Some(index) = current {
                    if let Some(old) = self
                        .navigation
                        .revealed
                        .and_then(|id| self.navigation.index(id))
                    {
                        self.mark_block_dirty(old);
                    }
                    self.navigation.revealed = selected;
                    self.navigation.reveal_pending = true;
                    self.navigation.anchored = true;
                    self.mark_block_dirty(index);
                    self.navigation.dialog = None;
                }
            }
            KeyCode::Up | KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigation.dialog.as_mut().unwrap().role = Role::User;
                let users = self
                    .navigation
                    .matches(&self.blocks, &self.transcript_revisions);
                let target = if key.code == KeyCode::Up {
                    users
                        .iter()
                        .rev()
                        .find(|index| current.is_none_or(|current| **index < current))
                } else {
                    users
                        .iter()
                        .find(|index| current.is_none_or(|current| **index > current))
                }
                .copied();
                // At an edge retain the current matching prompt, otherwise use
                // the nearest matching prompt. The query is never discarded.
                let target = target
                    .or_else(|| current.filter(|index| users.contains(index)))
                    .or_else(|| {
                        if key.code == KeyCode::Up {
                            users.first()
                        } else {
                            users.last()
                        }
                        .copied()
                    });
                self.navigation.dialog.as_mut().unwrap().selected =
                    target.and_then(|index| self.navigation.id(index));
            }
            KeyCode::Up | KeyCode::Down if key.modifiers.is_empty() => {
                let position = matches
                    .iter()
                    .position(|index| Some(*index) == current)
                    .unwrap_or(0);
                let position = if key.code == KeyCode::Up {
                    position.saturating_sub(1)
                } else {
                    (position + 1).min(matches.len().saturating_sub(1))
                };
                self.navigation.dialog.as_mut().unwrap().selected = matches
                    .get(position)
                    .and_then(|index| self.navigation.id(*index));
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let dialog = self.navigation.dialog.as_mut().unwrap();
                dialog.role = dialog.role.cycle(
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
                );
            }
            KeyCode::Backspace if key.modifiers.is_empty() => {
                self.navigation.dialog.as_mut().unwrap().backspace()
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.navigation
                    .dialog
                    .as_mut()
                    .unwrap()
                    .insert(&character.to_string());
            }
            _ => {}
        }
        self.sync_navigation();
        Action::None
    }

    /// Resolve only after the wrapping cache has current-width prefix offsets.
    pub(super) fn apply_navigation_reveal(&mut self, viewport_changed: bool) {
        if !(self.navigation.reveal_pending || viewport_changed && self.navigation.anchored) {
            return;
        }
        if let Some(index) = self
            .navigation
            .revealed
            .and_then(|id| self.navigation.index(id))
            && let Some(prefix) = self.transcript_prefixes.get(index)
        {
            self.follow = false;
            self.scroll = prefix + usize::from(*prefix > 0);
        }
        self.navigation.reveal_pending = false;
    }

    /// Exclude synchronous navigator/checkout work from the inter-key paste gap, without
    /// erasing time actually spent waiting for input. Never wrap an input wait.
    pub(super) fn with_navigation_clock_paused<T>(
        &mut self,
        work: impl FnOnce(&mut Self) -> T,
    ) -> T {
        self.with_navigation_clock_paused_using(work, Instant::now)
    }

    fn with_navigation_clock_paused_using<T>(
        &mut self,
        work: impl FnOnce(&mut Self) -> T,
        mut now: impl FnMut() -> Instant,
    ) -> T {
        let last_key = self.last_key.filter(|_| {
            self.navigation.dialog.is_some()
                || self.branch_chooser.is_some()
                || self.branch_draft.is_some()
        });
        let started = now();
        let result = work(self);
        // Do not overwrite a clock changed by the work.
        if last_key.is_some() && self.last_key == last_key {
            let elapsed = now().saturating_duration_since(started);
            self.last_key = last_key.and_then(|last| last.checked_add(elapsed));
        }
        result
    }

    /// Applies a key press, returning work for the event loop.
    pub(super) fn editing_branch(&self) -> bool {
        self.branch_draft.is_some()
    }

    pub(super) fn branch_submitting(&self) -> bool {
        self.branch_draft
            .as_ref()
            .is_some_and(|draft| draft.submitting)
    }

    fn open_branch_chooser(&mut self) -> Action {
        if self.working() || self.editing_steer() || !self.pending_steers.is_empty() {
            self.toast(
                "prompt checkout is available only while idle and outside pending-message edits",
            );
            return Action::None;
        }
        self.branch_epoch = self.branch_epoch.wrapping_add(1);
        self.branch_chooser = Some(BranchChooser {
            boundaries: Vec::new(),
            selected: 0,
            pending: true,
        });
        Action::ListPromptBranches {
            epoch: self.branch_epoch,
        }
    }

    pub(super) fn branch_listed(
        &mut self,
        epoch: u64,
        result: Result<Vec<crate::protocols::acp::prompt_branches::PromptBoundary>, String>,
    ) {
        if epoch != self.branch_epoch {
            return;
        }
        let Some(chooser) = &mut self.branch_chooser else {
            return;
        };
        match result {
            Ok(boundaries) => {
                chooser.boundaries = boundaries;
                chooser.pending = false;
            }
            Err(error) => {
                self.branch_chooser = None;
                self.toast(format!("could not list prompt checkouts: {error}"));
            }
        }
    }

    pub(super) fn branch_prepared(
        &mut self,
        epoch: u64,
        result: Result<crate::protocols::acp::prompt_branches::PreparePromptBranchResponse, String>,
    ) {
        if epoch != self.branch_epoch || self.branch_chooser.is_none() {
            return;
        }
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                self.branch_chooser.as_mut().unwrap().pending = false;
                self.toast(format!("could not prepare prompt checkout: {error}"));
                return;
            }
        };
        let mut provisional = App::new(
            self.root.clone(),
            self.provider.clone(),
            self.model.clone(),
            self.a2a.clone(),
        );
        provisional.session_id = self.session_id.clone();
        provisional.runtime_session_id = self.runtime_session_id.clone();
        provisional.storage_pending = self.storage_pending;
        provisional.storage_exhausted = self.storage_exhausted;
        provisional.last_key = self.last_key;
        provisional.can_steer = self.can_steer;
        provisional.can_replace_steer = self.can_replace_steer;
        provisional.auth_methods = self.auth_methods.clone();
        provisional.show_thoughts = self.show_thoughts;
        provisional.branch_epoch = epoch;
        for update in response.prefix {
            let (_, updates) = super::translate(
                agent_client_protocol::schema::v2::UpdateSessionNotification::new(
                    self.session_id.clone().unwrap_or_default(),
                    update,
                ),
            );
            for update in updates {
                provisional.apply(update);
            }
        }
        super::refresh_config_state(&mut provisional, Some(&response.config_options));
        provisional.editor.insert_str(&response.original_text);
        self.branch_chooser = None;
        let original = Box::new(std::mem::replace(self, provisional));
        self.branch_draft = Some(BranchDraft {
            original,
            checkout_token: response.checkout_token,
            submitting: false,
            early_runtime: Vec::new(),
        });
    }

    pub(super) fn abandon_branch(&mut self) {
        let epoch = self.branch_epoch.wrapping_add(1);
        if let Some(draft) = self.branch_draft.take() {
            *self = *draft.original;
        }
        self.branch_chooser = None;
        self.branch_epoch = epoch;
    }

    pub(super) fn branch_submit_failed(&mut self, epoch: u64, error: String) {
        if epoch != self.branch_epoch {
            return;
        }
        if let Some(draft) = &mut self.branch_draft {
            draft.submitting = false;
            self.toast(format!("could not submit prompt checkout: {error}"));
        }
    }

    pub(super) fn branch_submitted(&mut self, epoch: u64, session_id: String) -> bool {
        if epoch != self.branch_epoch || !self.branch_submitting() {
            return false;
        }
        // Drop only the parked view, never close the source ACP session. stderr
        // may precede both the response and canonical replay on the ACP stream.
        let draft = self.branch_draft.take().expect("checked submitting draft");
        let early_runtime = draft
            .early_runtime
            .into_iter()
            .filter_map(|(id, event)| (id == session_id).then_some(event))
            .collect();
        self.start_session(session_id);
        self.activate_runtime_session();
        self.branch_runtime_replay = Some(early_runtime);
        self.editor.clear();
        true
    }

    fn handle_branch_key_at(&mut self, key: KeyEvent, arrival: Instant) -> Action {
        let pasted = self
            .last_key
            .is_some_and(|last| arrival.saturating_duration_since(last) < PASTE_GAP);
        self.handle_branch_key(key, pasted)
    }

    fn handle_branch_key(&mut self, key: KeyEvent, pasted: bool) -> Action {
        if self.branch_submitting() {
            self.toast("creating branch — wait for the result");
            return Action::None;
        }
        if key.code == KeyCode::Esc {
            self.abandon_branch();
            return Action::None;
        }
        if let Some(chooser) = &mut self.branch_chooser {
            if chooser.pending {
                return Action::None;
            }
            match key.code {
                KeyCode::Up => chooser.selected = chooser.selected.saturating_sub(1),
                KeyCode::Down => {
                    chooser.selected =
                        (chooser.selected + 1).min(chooser.boundaries.len().saturating_sub(1))
                }
                KeyCode::Enter if key.modifiers.is_empty() && !pasted => {
                    if let Some(boundary) = chooser.boundaries.get(chooser.selected) {
                        let address = boundary.address.clone();
                        chooser.pending = true;
                        self.branch_epoch = self.branch_epoch.wrapping_add(1);
                        return Action::PreparePromptBranch {
                            epoch: self.branch_epoch,
                            address,
                        };
                    }
                }
                _ => {}
            }
            return Action::None;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter if key.modifiers.is_empty() && !pasted => {
                if !self.attachments.is_empty() {
                    self.toast("prompt branch edits are text-only");
                    return Action::None;
                }
                if self.editor.text().trim().is_empty() {
                    return Action::None;
                }
                let draft = self.branch_draft.as_mut().unwrap();
                draft.submitting = true;
                self.branch_epoch = self.branch_epoch.wrapping_add(1);
                return Action::SubmitPromptBranch {
                    epoch: self.branch_epoch,
                    checkout_token: draft.checkout_token.clone(),
                    text: self.editor.text().to_string(),
                };
            }
            KeyCode::Enter => self.editor.insert_char('\n'),
            KeyCode::Char('j') if control => self.editor.insert_char('\n'),
            KeyCode::Char('a') if control => self.editor.move_line_start(),
            KeyCode::Char('e') if control => self.editor.move_line_end(),
            KeyCode::Char('u') if control => self.editor.delete_to_line_start(),
            KeyCode::Char('k') if control => self.editor.delete_to_line_end(),
            KeyCode::Char('w') if control => self.editor.delete_word_back(),
            KeyCode::Backspace if control => self.editor.delete_word_back(),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => self.editor.delete_forward(),
            KeyCode::Left => self.editor.move_left(),
            KeyCode::Right => self.editor.move_right(),
            KeyCode::Home => self.editor.move_line_start(),
            KeyCode::End => self.editor.move_line_end(),
            KeyCode::Up => {
                self.editor.move_row_up(self.prompt_width);
            }
            KeyCode::Down => {
                self.editor.move_row_down(self.prompt_width);
            }
            KeyCode::PageUp => self.scroll_by(-(self.viewport.max(2) as isize - 1)),
            KeyCode::PageDown => self.scroll_by(self.viewport.max(2) as isize - 1),
            KeyCode::Tab => self.editor.insert_str("    "),
            KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::SUPER) => {
                self.editor.insert_char(c)
            }
            _ => {}
        }
        Action::None
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if self.branch_chooser.is_some() || self.branch_draft.is_some() {
            let action = self.handle_branch_key_at(key, Instant::now());
            self.last_key = Some(Instant::now());
            return action;
        }
        if self.navigation.dialog.is_some() {
            let pasted = self.last_key.is_some_and(|last| last.elapsed() < PASTE_GAP);
            let action = self.handle_navigation_key(key, pasted);
            // The search can take longer than PASTE_GAP. Measure the next gap
            // from completion, not from before that synchronous work.
            self.last_key = Some(Instant::now());
            return action;
        }
        if self.session_dialog.is_some() {
            // Terminals without bracketed paste deliver a paste as a key burst, so
            // the arrival gap is the only thing separating it from typing.
            let pasted = self.last_key.is_some_and(|last| last.elapsed() < PASTE_GAP);
            self.last_key = Some(Instant::now());
            return self.handle_session_key(key, pasted);
        }
        if self.model_dialog.is_some() {
            return self.handle_model_key(key);
        }
        if self.effort_dialog.is_some() {
            return self.handle_effort_key(key);
        }
        if key.code == KeyCode::F(3) && key.modifiers.is_empty() {
            self.open_navigation();
            return Action::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.file_picker = None;
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
            return Action::None;
        }
        if key.code == KeyCode::Char('d')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.editor.is_empty()
        {
            self.file_picker = None;
            return Action::Quit;
        }
        // Queue focus owns composer keys, not global task, view, or copy actions.
        // Ctrl+K is global only when it cancels background work; otherwise it
        // must not fall through and delete text from the parked composer.
        let global_key = match key.code {
            KeyCode::Char('b') => key.modifiers == KeyModifiers::SUPER,
            KeyCode::Char('k') => {
                key.modifiers.contains(KeyModifiers::CONTROL)
                    && self
                        .focus_call()
                        .is_some_and(|call| call.backgrounded && call.running())
            }
            KeyCode::Char('y' | 'r' | 'l' | 'o' | 't') | KeyCode::Home | KeyCode::End => {
                key.modifiers.contains(KeyModifiers::CONTROL)
            }
            KeyCode::Up | KeyCode::Down => key.modifiers.contains(KeyModifiers::SHIFT),
            KeyCode::PageUp | KeyCode::PageDown => true,
            _ => false,
        };
        if self.queue_focused && !global_key {
            return self.handle_steer_selection(key);
        }
        if self.queue_handoff && !global_key {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            let command = key.modifiers.contains(KeyModifiers::SUPER);
            let destructive = matches!(
                key.code,
                KeyCode::Enter | KeyCode::Backspace | KeyCode::Delete
            ) || (control && matches!(key.code, KeyCode::Char('w' | 'u' | 'k')))
                || (key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('d'));
            if destructive {
                self.toast("queue closed — type, move cursor or Esc before sending/deleting draft");
                return Action::None;
            }
            if key.code == KeyCode::Esc {
                self.queue_handoff = false;
                self.toast = None;
                return Action::None;
            }
            // Normal composer interaction stays responsive. View/task shortcuts
            // above do not acknowledge the changed focus, nor do repeat deletes.
            if matches!(
                key.code,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Tab
            ) || (matches!(key.code, KeyCode::Char(_)) && !control && !command)
                || (control && matches!(key.code, KeyCode::Char('a' | 'e' | 'j')))
            {
                self.queue_handoff = false;
                self.toast = None;
            }
        }
        if key.code == KeyCode::F(2) && !self.editing_steer() {
            let Some(pending) = self.pending_steers.front() else {
                self.toast("no pending messages");
                return Action::None;
            };
            self.selected_steer = Some(pending.id.clone());
            self.queue_focused = true;
            self.queue_handoff = false;
            self.file_picker = None;
            return Action::None;
        }
        if key.code == KeyCode::Esc && self.editing_steer() {
            self.cancel_steer_edit();
            return Action::None;
        }
        // Terminals without bracketed paste deliver a paste as a key burst, so
        // the arrival gap is the only thing separating it from typing.
        let pasted = self.last_key.is_some_and(|last| last.elapsed() < PASTE_GAP);
        self.last_key = Some(Instant::now());
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let command = key.modifiers.contains(KeyModifiers::SUPER);
        let file_picker_key = match key.code {
            KeyCode::Char(_) => !control && !alt && !command,
            KeyCode::Esc
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right => key.modifiers.is_empty(),
            _ => false,
        };
        let pasted_input =
            pasted && matches!(key.code, KeyCode::Char(_) | KeyCode::Tab | KeyCode::Enter);
        if self.file_picker.is_some() && !pasted_input && file_picker_key {
            return self.handle_file_picker_key(key);
        }
        self.file_picker = None;
        // `cmd` only reaches the client in terminals that speak the Kitty
        // keyboard protocol; the control equivalents cover the rest.
        let word = alt || control;
        let line = command || control;

        self.sync_command_completion();
        let completions = self.command_completions();
        if !completions.is_empty() && key.modifiers.is_empty() {
            match key.code {
                KeyCode::Esc => {
                    self.command_completion_dismissed =
                        self.command_completion_prefix().map(str::to_string);
                    return Action::None;
                }
                KeyCode::Up => {
                    self.command_completion_selected =
                        self.command_completion_selected.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.command_completion_selected = (self.command_completion_selected + 1)
                        .min(completions.len().saturating_sub(1));
                    return Action::None;
                }
                KeyCode::Tab => {
                    let selected = self
                        .command_completion_selected
                        .min(completions.len().saturating_sub(1));
                    let replacement = completions[selected].name.clone();
                    self.editor.replace_command_token(&replacement);
                    self.command_completion_query = Some(replacement.clone());
                    self.command_completion_dismissed = Some(replacement);
                    return Action::None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('b') if key.modifiers == KeyModifiers::SUPER => {
                let Some(call_id) = self.newest_foreground_compose().map(|call| call.id.clone())
                else {
                    self.toast("no foreground compose call is running");
                    return Action::None;
                };
                return Action::DetachCompose(call_id);
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
                // Local and read-only, even while streaming or editing a steer.
                // Do not submit/clear the parked composer or its attachments.
                if matches!(
                    parse(self.editor.text(), !self.auth_methods.is_empty()),
                    Parsed::Transcript
                ) {
                    self.open_navigation();
                    return Action::None;
                }
                if matches!(
                    parse(self.editor.text(), !self.auth_methods.is_empty()),
                    Parsed::Branch
                ) {
                    return self.open_branch_chooser();
                }
                if self.editor.is_empty() {
                    return Action::None;
                }
                if let Some(edit) = &self.steer_edit {
                    if self.steer_mutations.contains_key(&edit.id) {
                        self.toast("a change to this pending message is still in progress");
                        return Action::None;
                    }
                    if !self
                        .pending_steers
                        .iter()
                        .any(|pending| pending.id == edit.id)
                    {
                        self.toast(
                            "message is no longer pending; copy your edit or Esc to restore draft",
                        );
                        return Action::None;
                    }
                    if !self.can_replace_steer {
                        self.toast("this agent does not support editing pending messages");
                        return Action::None;
                    }
                    return Action::ReplaceSteer {
                        id: edit.id.clone(),
                        text: self.editor.text().to_owned(),
                    };
                }
                let inject = self.working();
                if inject {
                    if self.phase != Phase::Working {
                        self.toast("the agent is waiting for required input");
                        return Action::None;
                    }
                    let input = self.editor.text();
                    if !matches!(
                        parse(input, !self.auth_methods.is_empty()),
                        Parsed::Prompt(_)
                    ) || known_token(
                        input,
                        &self.available_commands,
                        !self.auth_methods.is_empty(),
                    )
                    .is_some()
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
                return match parse(&input, !self.auth_methods.is_empty()) {
                    Parsed::New { prompt } => Action::New(prompt.map(str::to_string)),
                    Parsed::Resume {
                        session_id: Some(session_id),
                    } => Action::Resume(session_id.to_string()),
                    Parsed::Resume { session_id: None } => {
                        self.toast("usage: /resume <session-id>");
                        Action::None
                    }
                    Parsed::Sessions => {
                        if self.session_catalog_pending {
                            self.toast("session catalog scan is already in progress");
                            Action::None
                        } else {
                            self.session_catalog_pending = true;
                            Action::ListSessions
                        }
                    }
                    Parsed::Branch => self.open_branch_chooser(),
                    Parsed::Transcript => {
                        self.open_navigation();
                        Action::None
                    }
                    Parsed::Close => Action::Close,
                    Parsed::Agents => {
                        self.toggle_agents();
                        Action::None
                    }
                    Parsed::Login { method_id } => {
                        let method = method_id
                            .and_then(|method_id| {
                                self.auth_methods
                                    .iter()
                                    .find(|method| method.method_id.0.as_ref() == method_id)
                            })
                            .or_else(|| {
                                (method_id.is_none() && self.auth_methods.len() == 1)
                                    .then(|| &self.auth_methods[0])
                            });
                        if let Some(method) = method {
                            Action::Login(method.clone())
                        } else {
                            let ids = self
                                .auth_methods
                                .iter()
                                .map(|method| method.method_id.0.as_ref())
                                .collect::<Vec<_>>()
                                .join("|");
                            self.toast(format!("usage: /login <{ids}>"));
                            Action::None
                        }
                    }
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
            KeyCode::Char('r') if control => self.toggle_agents(),
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
            KeyCode::Char('@') if !control && !command && !pasted => {
                let at = self.editor.cursor();
                let eligible = at == 0
                    || self.editor.text()[..at]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace);
                self.editor.insert_char('@');
                if eligible && !self.editing_steer() {
                    return self.start_file_picker(at..at + 1);
                }
            }
            KeyCode::Char(character) if !control && !command => self.editor.insert_char(character),
            _ => {}
        }
        self.sync_command_completion();
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
        if self.branch_chooser.is_some() || self.editing_branch() {
            return Action::None;
        }
        if self.navigation.dialog.is_some() {
            return Action::None;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp if self.mouse_in_agents(mouse.column, mouse.row) => {
                self.scroll_agents_by(-3);
                return Action::Redraw;
            }
            MouseEventKind::ScrollDown if self.mouse_in_agents(mouse.column, mouse.row) => {
                self.scroll_agents_by(3);
                return Action::Redraw;
            }
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

    fn mouse_in_agents(&self, column: u16, row: u16) -> bool {
        self.agents_area.width > 0
            && self.agents_area.height > 0
            && column >= self.agents_area.x
            && column < self.agents_area.x.saturating_add(self.agents_area.width)
            && row >= self.agents_area.y
            && row < self.agents_area.y.saturating_add(self.agents_area.height)
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

    use agent_client_protocol::schema::v2::{
        AuthMethodTerminal, IdleStateUpdate, RequiresActionStateUpdate, RunningStateUpdate,
        StateUpdate, StopReason, ToolCallStatus, ToolKind,
    };
    use agentkit_core::{DataRef, Item, ItemKind, MediaPart, MetadataMap, Modality, Part};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    use super::{
        Action, App, AttachmentKind, Block, MAX_IMAGE_BASE64_BYTES, MAX_IMAGE_SOURCE_BYTES,
        MAX_RETAINED_IMAGE_SOURCE_BYTES, Phase, Update, UserImage,
    };
    use crate::{events::RuntimeEvent, file_search::FileMatch, tui::wrap::LinkHit};

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

    fn apply_stderr_runtime(app: &mut App, route: &mut Option<String>, event: RuntimeEvent) {
        let line = format!(
            "{}{}",
            crate::events::EVENT_MARKER,
            serde_json::to_string(&event).unwrap()
        );
        if let Some(update) =
            super::super::runtime_diagnostic_update(route, crate::events::parse(&line).unwrap())
        {
            app.apply(update);
        }
    }

    fn prepare_branch(app: &mut App) {
        let Action::ListPromptBranches { epoch } = app.open_branch_chooser() else {
            panic!("expected chooser")
        };
        app.branch_prepared(
            epoch,
            Ok(
                crate::protocols::acp::prompt_branches::PreparePromptBranchResponse {
                    checkout_token: "checkout-one".into(),
                    original_text: "original prompt".into(),
                    prefix: Vec::new(),
                    config_options: Vec::new(),
                },
            ),
        );
    }

    fn ready_branch_chooser(app: &mut App) -> u64 {
        let Action::ListPromptBranches { epoch } = app.open_branch_chooser() else {
            panic!("chooser")
        };
        app.branch_listed(
            epoch,
            Ok(vec![
                crate::protocols::acp::prompt_branches::PromptBoundary {
                    address: "opaque".into(),
                    text: "original".into(),
                    historical: false,
                },
            ]),
        );
        epoch
    }

    #[test]
    fn branch_chooser_and_draft_exclude_slow_processing_but_preserve_real_input_gaps() {
        // Inject a monotonic clock rather than sleeping: rendering/updating takes
        // five paste gaps while the next Enter is already buffered by the terminal.
        for draft in [false, true] {
            for deliberate in [false, true] {
                let mut app = app();
                app.start_session("source".into());
                if draft {
                    prepare_branch(&mut app);
                } else {
                    ready_branch_chooser(&mut app);
                }
                assert!(app.navigation.dialog.is_none()); // direct /branch path
                let before = Instant::now();
                let after = before + super::PASTE_GAP * 5;
                app.last_key = Some(if deliberate {
                    before - super::PASTE_GAP * 2
                } else {
                    before
                });
                let mut clock = [before, after].into_iter();
                app.with_navigation_clock_paused_using(|_| {}, || clock.next().unwrap());
                let action = app.handle_branch_key_at(press(KeyCode::Enter), after);
                if deliberate {
                    assert!(matches!(action, Action::SubmitPromptBranch { .. }) == draft);
                    assert!(matches!(action, Action::PreparePromptBranch { .. }) != draft);
                } else {
                    assert!(matches!(action, Action::None));
                    assert!(!app.branch_submitting());
                    if draft {
                        assert_eq!(app.editor.text(), "original prompt\n");
                    } else {
                        assert!(!app.branch_chooser.as_ref().unwrap().pending);
                    }
                }
            }
        }
    }

    #[test]
    fn branch_prepare_response_preserves_paste_clock_in_the_fresh_provisional_view() {
        let mut app = app();
        app.start_session("source".into());
        let epoch = ready_branch_chooser(&mut app);
        let before = Instant::now();
        let after = before + super::PASTE_GAP * 5;
        app.last_key = Some(before);
        let mut clock = [before, after].into_iter();
        app.with_navigation_clock_paused_using(
            |app| {
                app.branch_prepared(
                    epoch,
                    Ok(
                        crate::protocols::acp::prompt_branches::PreparePromptBranchResponse {
                            checkout_token: "checkout".into(),
                            original_text: "original".into(),
                            prefix: Vec::new(),
                            config_options: Vec::new(),
                        },
                    ),
                );
            },
            || clock.next().unwrap(),
        );
        assert!(app.editing_branch());
        assert_eq!(app.last_key, Some(after));
        assert!(matches!(
            app.handle_branch_key_at(press(KeyCode::Enter), after),
            Action::None
        ));
        assert!(!app.branch_submitting());
        assert_eq!(app.editor.text(), "original\n");
    }

    fn commit_branch_view(app: &mut App, child: &str) {
        let arrival = Instant::now();
        app.last_key = Some(arrival - super::PASTE_GAP * 2);
        let Action::SubmitPromptBranch { epoch, .. } =
            app.handle_branch_key_at(press(KeyCode::Enter), arrival)
        else {
            panic!("submit")
        };
        assert!(app.branch_submitted(epoch, child.into()));
    }

    #[test]
    fn branch_storage_state_is_visible_before_during_and_after_checkout() {
        for initial in [(true, false), (false, true), (true, true)] {
            for activate in [false, true] {
                let mut app = app();
                app.start_session("source".into());
                app.activate_runtime_session();
                app.apply(Update::Runtime(RuntimeEvent::StorageStatus {
                    pending: initial.0,
                    exhausted: initial.1,
                }));
                prepare_branch(&mut app);
                assert_eq!((app.storage_pending, app.storage_exhausted), initial);
                // Even a child diagnostic-route change cannot hide global warnings.
                app.apply(Update::Runtime(RuntimeEvent::SessionStarted {
                    session_id: "child".into(),
                }));
                for state in [(false, false), (true, true), (false, true), (true, false)] {
                    app.apply(Update::Runtime(RuntimeEvent::StorageStatus {
                        pending: state.0,
                        exhausted: state.1,
                    }));
                    assert_eq!((app.storage_pending, app.storage_exhausted), state);
                    let parked = &app.branch_draft.as_ref().unwrap().original;
                    assert_eq!((parked.storage_pending, parked.storage_exhausted), state);
                }
                if activate {
                    commit_branch_view(&mut app, "child");
                } else {
                    app.abandon_branch();
                }
                assert!(app.storage_pending);
                assert!(!app.storage_exhausted);
                app.apply(Update::Runtime(RuntimeEvent::StorageStatus {
                    pending: false,
                    exhausted: false,
                }));
                assert!(!app.storage_pending);
                assert!(!app.storage_exhausted);
            }
        }
    }

    #[test]
    fn branch_early_diagnostics_replay_after_activation_and_route_back_to_loaded_source() {
        let mut app = app();
        let mut ingress = None;
        app.start_session("source".into());
        app.activate_runtime_session();
        prepare_branch(&mut app);
        apply_stderr_runtime(
            &mut app,
            &mut ingress,
            RuntimeEvent::SessionStarted {
                session_id: "child".into(),
            },
        );
        apply_stderr_runtime(
            &mut app,
            &mut ingress,
            RuntimeEvent::CompactionStarted {
                reason: "early".into(),
                at: 0,
            },
        );
        apply_stderr_runtime(
            &mut app,
            &mut ingress,
            agent_event(
                "child-agent",
                "Child agent",
                crate::events::SubagentStatus::Working,
                None,
                1,
                None,
                (1, 1, None),
            ),
        );
        apply_stderr_runtime(&mut app, &mut ingress, child("call-1:compose:1", "shell"));
        let parked = &app.branch_draft.as_ref().unwrap().original;
        assert_eq!(parked.runtime_session_id.as_deref(), Some("source"));
        assert!(!parked.compacting);
        assert!(parked.agents.is_empty());
        assert!(!app.compacting); // provisional context is not the child yet
        commit_branch_view(&mut app, "child");
        assert_eq!(app.runtime_session_id.as_deref(), Some("child"));
        assert_eq!(app.branch_runtime_replay.as_ref().unwrap().len(), 3);
        // A response is not the replay boundary: keep diagnostics even if ACP
        // replay arrives later than stderr from the newly activated child.
        apply_stderr_runtime(
            &mut app,
            &mut ingress,
            child("call-1:compose:extra", "shell"),
        );
        assert!(!app.compacting);
        compose(&mut app, "shell({})");
        app.apply(Update::AvailableCommands {
            session_id: "child".into(),
            commands: Vec::new(),
        });
        assert!(app.branch_runtime_replay.is_none());
        assert!(app.compacting);
        assert!(app.agents.contains_key("child-agent"));
        assert_eq!(app.call_mut("call-1").unwrap().children.len(), 2);

        // Loaded resume emits an ordered stderr source boundary before responding.
        app.start_session("source".into());
        app.activate_runtime_session();
        compose(&mut app, "source shell({})");
        apply_stderr_runtime(
            &mut app,
            &mut ingress,
            RuntimeEvent::SessionStarted {
                session_id: "source".into(),
            },
        );
        apply_stderr_runtime(
            &mut app,
            &mut ingress,
            RuntimeEvent::CompactionStarted {
                reason: "source".into(),
                at: 1,
            },
        );
        apply_stderr_runtime(
            &mut app,
            &mut ingress,
            agent_event(
                "source-agent",
                "Source agent",
                crate::events::SubagentStatus::Working,
                None,
                1,
                None,
                (1, 1, None),
            ),
        );
        apply_stderr_runtime(&mut app, &mut ingress, child("call-1:compose:2", "shell"));
        assert_eq!(app.runtime_session_id.as_deref(), Some("source"));
        assert!(app.compacting);
        assert!(app.agents.contains_key("source-agent"));
        assert!(!app.agents.contains_key("child-agent"));
        assert_eq!(app.call_mut("call-1").unwrap().children.len(), 1);
    }

    #[test]
    fn branch_abandon_does_not_retarget_source_diagnostics_to_an_early_child() {
        let mut app = app();
        let mut ingress = None;
        app.start_session("source".into());
        app.activate_runtime_session();
        prepare_branch(&mut app);
        apply_stderr_runtime(
            &mut app,
            &mut ingress,
            RuntimeEvent::SessionStarted {
                session_id: "child".into(),
            },
        );
        apply_stderr_runtime(
            &mut app,
            &mut ingress,
            RuntimeEvent::CompactionStarted {
                reason: "child".into(),
                at: 0,
            },
        );
        app.abandon_branch();
        assert_eq!(app.runtime_session_id.as_deref(), Some("source"));
        assert!(!app.compacting);
        apply_stderr_runtime(
            &mut app,
            &mut ingress,
            RuntimeEvent::SessionStarted {
                session_id: "source".into(),
            },
        );
        apply_stderr_runtime(
            &mut app,
            &mut ingress,
            RuntimeEvent::CompactionStarted {
                reason: "source".into(),
                at: 1,
            },
        );
        assert!(app.compacting);
    }

    #[test]
    fn prompt_branch_abandon_restores_parked_editor_attachments_transcript_and_config() {
        let mut app = app();
        app.start_session("source".into());
        app.push_block(Block::Agent("original answer".into()));
        app.paste("unsent draft");
        app.attach(
            PathBuf::from("/tmp/parked.png"),
            "image/png",
            AttachmentKind::Image,
            123,
        );
        app.editor.move_left();
        let text = app.editor.text().to_string();
        let cursor = app.editor.cursor();
        let attachments = app.attachments.clone();
        let next_attachment = app.next_attachment;
        app.reasoning_effort = "high".into();
        app.scroll = 7;
        app.follow = false;
        prepare_branch(&mut app);
        assert!(app.editing_branch());
        assert!(app.blocks.is_empty());
        assert!(app.attachments.is_empty());
        assert_eq!(app.editor.text(), "original prompt");
        app.attach(
            PathBuf::from("/tmp/rejected.png"),
            "image/png",
            AttachmentKind::Image,
            1,
        );
        assert!(app.attachments.is_empty());
        app.paste(" edited");
        assert!(matches!(app.handle_key(press(KeyCode::Esc)), Action::None));
        assert!(!app.editing_branch());
        assert_eq!(app.session_id.as_deref(), Some("source"));
        assert!(matches!(&app.blocks[0], Block::Agent(text) if text == "original answer"));
        assert_eq!(app.editor.text(), text);
        assert_eq!(app.editor.cursor(), cursor);
        assert_eq!(app.attachments, attachments);
        assert_eq!(app.next_attachment, next_attachment);
        assert_eq!(app.reasoning_effort, "high");
        assert_eq!(app.scroll, 7);
        assert!(!app.follow);
    }

    #[test]
    fn prompt_branch_stale_list_prepare_and_submit_cannot_replace_newer_draft_or_route() {
        use crate::protocols::acp::prompt_branches::{PreparePromptBranchResponse, PromptBoundary};
        let mut app = app();
        app.start_session("source".into());
        let Action::ListPromptBranches { epoch } = app.open_branch_chooser() else {
            panic!()
        };
        app.abandon_branch();
        app.open_branch_chooser();
        app.branch_listed(
            epoch,
            Ok(vec![PromptBoundary {
                address: "stale".into(),
                text: "stale".into(),
                historical: false,
            }]),
        );
        assert!(app.branch_chooser.as_ref().unwrap().boundaries.is_empty());
        app.branch_prepared(
            epoch,
            Ok(PreparePromptBranchResponse {
                checkout_token: "stale".into(),
                original_text: "stale".into(),
                prefix: Vec::new(),
                config_options: Vec::new(),
            }),
        );
        assert!(!app.editing_branch());
        app.abandon_branch();
        prepare_branch(&mut app);
        let Action::SubmitPromptBranch { epoch, text, .. } =
            app.handle_branch_key(press(KeyCode::Enter), false)
        else {
            panic!()
        };
        assert_eq!(text, "original prompt");
        app.start_session("other".into());
        app.branch_submit_failed(epoch, "stale error".into());
        assert!(!app.branch_submitted(epoch, "stale-child".into()));
        assert_eq!(app.session_id.as_deref(), Some("other"));
        assert!(!app.editing_branch());
    }

    #[test]
    fn prompt_branch_is_discoverable_without_stealing_navigator_search_characters() {
        let mut app = app();
        app.start_session("source".into());
        app.paste("park this draft");
        app.open_navigation();
        assert!(matches!(
            app.handle_navigation_key(press(KeyCode::Char('e')), false),
            Action::None
        ));
        assert_eq!(app.navigation.dialog.as_ref().unwrap().query, "e");
        app.handle_navigation_key(press(KeyCode::Backspace), false);
        app.paste("/branch");
        assert!(matches!(
            app.handle_navigation_key(press(KeyCode::Enter), false),
            Action::ListPromptBranches { .. }
        ));
        assert_eq!(app.editor.text(), "park this draft");
        app.abandon_branch();
        assert!(app.navigation.dialog.is_some());
        assert_eq!(app.editor.text(), "park this draft");
    }

    #[test]
    fn prompt_branch_submit_is_text_only_and_never_an_ordinary_send() {
        let mut app = app();
        app.start_session("source".into());
        prepare_branch(&mut app);
        for code in [KeyCode::F(2), KeyCode::F(3)] {
            assert!(matches!(app.handle_key(press(code)), Action::None));
            assert!(app.navigation.dialog.is_none());
            assert!(!app.queue_focused);
        }
        app.editor.clear();
        app.paste("/model literal edited text");
        assert!(matches!(
            app.handle_branch_key(press(KeyCode::Enter), true),
            Action::None
        ));
        assert!(app.editor.text().ends_with('\n'));
        let Action::SubmitPromptBranch {
            epoch,
            checkout_token,
            text,
        } = app.handle_branch_key(press(KeyCode::Enter), false)
        else {
            panic!("must submit branch, not prompt/config")
        };
        assert_eq!(checkout_token, "checkout-one");
        assert_eq!(text, "/model literal edited text\n");
        assert!(app.branch_submitting());
        app.paste("ignored while committing");
        app.handle_key(press(KeyCode::Esc));
        assert!(app.branch_submitting());
        assert_eq!(app.editor.text(), text);
        app.branch_submit_failed(epoch, "try again".into());
        assert!(app.editing_branch());
        assert!(!app.branch_submitting());
        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.session_id.as_deref(), Some("source"));
    }

    #[test]
    fn transcript_navigation_slow_work_does_not_create_an_input_gap() {
        let mut app = app();
        app.push_block(Block::Agent("matching text".into()));
        app.paste("parked draft");
        app.open_navigation();
        app.handle_key(press(KeyCode::Char('m')));
        let before_work = app.last_key.unwrap();
        let delay = super::PASTE_GAP * 3;
        // Inject synchronous work slower than the paste threshold. Rendering
        // and update processing use this pause; keys record their completion.
        app.with_navigation_clock_paused(|app| {
            app.handle_navigation_key(press(KeyCode::Char('a')), true);
            std::thread::sleep(delay);
        });
        assert!(app.last_key.unwrap().duration_since(before_work) >= delay);
        // No manual timestamp repair between the slow processing and buffered keys.
        for code in [KeyCode::Tab, KeyCode::Enter, KeyCode::Enter] {
            assert!(matches!(app.handle_key(press(code)), Action::None));
            assert!(app.navigation.dialog.is_some());
            assert_eq!(
                app.navigation.dialog.as_ref().unwrap().role,
                super::Role::All
            );
            assert_eq!(app.editor.text(), "parked draft");
        }
        assert!(app.navigation.revealed.is_none());
    }

    #[test]
    fn transcript_navigation_slow_render_preserves_deliberate_input_gaps() {
        let mut app = app();
        app.push_block(Block::Agent("matching text".into()));
        app.open_navigation();
        app.paste("matching");
        app.last_key = Some(Instant::now() - Duration::from_millis(500));
        app.with_navigation_clock_paused(|_| std::thread::sleep(super::PASTE_GAP * 3));
        // Redrawing a streaming navigator must not turn a deliberate Enter into paste.
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert!(app.navigation.dialog.is_none());
        assert!(app.navigation.revealed.is_some());

        let editor_clock = app.last_key;
        app.with_navigation_clock_paused(|_| {});
        assert_eq!(app.last_key, editor_clock); // No change to unrelated editor semantics.
        app.open_navigation();
        app.with_navigation_clock_paused(|app| app.last_key = None);
        assert!(app.last_key.is_none()); // Do not resurrect a reset clock.
    }

    #[test]
    fn transcript_navigation_preserves_streaming_draft_and_attachments() {
        let mut app = app();
        app.push_block(Block::Agent("visible history".into()));
        app.paste("draft e\u{301} 👩‍💻");
        app.attach(
            PathBuf::from("/tmp/image.png"),
            "image/png",
            AttachmentKind::Image,
            12,
        );
        app.editor.move_left();
        let draft = app.editor.text().to_owned();
        let cursor = app.editor.cursor();
        app.phase = Phase::Working;
        app.follow = false;
        app.scroll = 7;
        assert!(matches!(app.handle_key(press(KeyCode::F(3))), Action::None));
        for key in [
            modified_press(KeyCode::Char('c'), KeyModifiers::CONTROL),
            modified_press(KeyCode::Char('b'), KeyModifiers::SUPER),
            modified_press(KeyCode::Char('k'), KeyModifiers::CONTROL),
        ] {
            assert!(matches!(app.handle_key(key), Action::None));
        }
        app.paste("history");
        assert_eq!(app.sync_navigation(), vec![0]);
        assert!(matches!(app.handle_key(press(KeyCode::Esc)), Action::None));
        assert_eq!(app.editor.text(), draft);
        assert_eq!(app.editor.cursor(), cursor);
        assert_eq!(app.attachments.len(), 1);
        assert_eq!(app.scroll, 7);
        assert!(!app.follow);
        assert!(app.working());
        assert!(app.navigation.revealed.is_none());
    }

    #[test]
    fn transcript_command_is_read_only_while_streaming() {
        let mut app = app();
        app.phase = Phase::Working;
        app.paste("/transcript");
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert!(app.navigation.dialog.is_some());
        assert_eq!(app.editor.text(), "/transcript");
        assert!(app.working());
        assert!(app.pending_steers.is_empty());
        assert!(matches!(app.handle_key(press(KeyCode::Esc)), Action::None));
        assert_eq!(app.editor.text(), "/transcript");
    }

    #[test]
    fn transcript_navigation_filters_unicode_and_jumps_between_user_prompts() {
        let mut app = app();
        app.push_block(Block::User("ÉCOLE first".to_string().into()));
        app.push_block(Block::Agent("école answer".into()));
        app.push_block(Block::Thought {
            text: "école thought".into(),
            started: Instant::now(),
            millis: Some(1),
        });
        app.push_block(Block::User("école last".to_string().into()));
        app.push_block(Block::Notice("école not a message".into()));
        app.open_navigation();
        app.paste("École");
        assert_eq!(app.sync_navigation(), vec![0, 1, 2, 3]);
        app.handle_key(press(KeyCode::Down));
        app.handle_key(modified_press(KeyCode::Down, KeyModifiers::CONTROL));
        let dialog = app.navigation.dialog.as_ref().unwrap();
        assert_eq!(dialog.role, super::Role::User);
        assert_eq!(dialog.query, "École");
        assert_eq!(dialog.selected, app.navigation.id(3));
        app.handle_key(modified_press(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(
            app.navigation.dialog.as_ref().unwrap().selected,
            app.navigation.id(0)
        );
        app.last_key = None; // Intentional key, outside a paste burst.
        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.sync_navigation(), vec![1]);
        app.last_key = None; // Intentional key, outside a paste burst.
        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.sync_navigation(), vec![2]);
        assert!(!app.show_thoughts);
        app.last_key = None; // Intentional key, outside a paste burst.
        app.handle_key(press(KeyCode::Tab));
        assert!(app.sync_navigation().is_empty());
        assert!(app.navigation.dialog.as_ref().unwrap().selected.is_none());
        app.last_key = None; // Intentional key, outside a paste burst.
        app.handle_key(press(KeyCode::Enter));
        assert!(app.navigation.dialog.is_some());
        app.handle_key(press(KeyCode::BackTab));
        assert_eq!(app.sync_navigation(), vec![2]);
    }

    #[test]
    fn transcript_navigation_keeps_identity_across_live_and_replay_upserts() {
        let mut app = app();
        app.start_session("session".into());
        app.apply(Update::AgentMessage {
            id: "acp-message".into(),
            text: "original".into(),
            append: false,
        });
        app.open_navigation();
        let selected = app.navigation.dialog.as_ref().unwrap().selected;
        // Replay replacement uses the same ACP ID; it is not a fork address.
        app.apply(Update::AgentMessage {
            id: "acp-message".into(),
            text: "replacement".into(),
            append: false,
        });
        app.apply(Update::AgentMessage {
            id: "acp-message".into(),
            text: " live tail".into(),
            append: true,
        });
        app.apply(Update::AgentMessage {
            id: "different-message".into(),
            text: "later".into(),
            append: false,
        });
        app.sync_navigation();
        assert_eq!(app.navigation.dialog.as_ref().unwrap().selected, selected);
        assert_eq!(app.blocks.len(), 2);
        app.handle_key(press(KeyCode::Enter));
        assert_eq!(app.navigation.revealed, selected);
        app.start_session("session".into()); // same ID, fresh activation
        assert!(app.navigation.dialog.is_none());
        assert!(app.navigation.revealed.is_none());
        app.apply(Update::AgentMessage {
            id: "acp-message".into(),
            text: "replacement".into(),
            append: false,
        });
        app.open_navigation();
        assert_ne!(app.navigation.dialog.as_ref().unwrap().selected, selected);
    }

    #[test]
    fn transcript_navigation_search_updates_and_excludes_pending_and_image_bytes() {
        let mut app = app();
        app.apply(Update::UserMessage {
            id: "user".into(),
            text: "[Image #1]".into(),
            images: vec![UserImage::new("c2VjcmV0".into(), "image/png".into(), 0).unwrap()],
            append: false,
        });
        app.apply(Update::SteerAccepted {
            id: "pending".into(),
            text: "undelivered secret".into(),
            editable: true,
        });
        app.apply(Update::ToolStarted {
            id: "tool".into(),
            title: "shell".into(),
            kind: ToolKind::Other,
            script: Some("return readable_script".into()),
            backgrounded: false,
        });
        let tool_focus = app.transcript_focus_index;
        app.open_navigation();
        app.paste("c2VjcmV0");
        assert!(app.sync_navigation().is_empty());
        app.navigation.dialog.as_mut().unwrap().query = "undelivered".into();
        assert!(app.sync_navigation().is_empty());
        app.navigation.dialog.as_mut().unwrap().query = "READABLE_SCRIPT".into();
        assert_eq!(app.sync_navigation(), vec![1]);
        app.handle_key(press(KeyCode::Enter));
        assert_eq!(app.transcript_focus_index, tool_focus);
        app.open_navigation();
        app.paste("future text");
        assert!(app.sync_navigation().is_empty());
        app.apply(Update::AgentMessage {
            id: "future".into(),
            text: "future text".into(),
            append: true,
        });
        assert_eq!(app.sync_navigation(), vec![2]);
        app.apply(Update::AgentMessage {
            id: "future".into(),
            text: "no longer matches".into(),
            append: false,
        });
        assert!(app.sync_navigation().is_empty());
        assert!(app.navigation.dialog.as_ref().unwrap().selected.is_none());
    }

    #[test]
    fn transcript_navigation_restored_history_and_empty_session_are_safe() {
        let mut app = app();
        app.open_navigation();
        for key in [KeyCode::Up, KeyCode::Down, KeyCode::Enter, KeyCode::BackTab] {
            assert!(matches!(app.handle_key(press(key)), Action::None));
        }
        assert!(app.navigation.dialog.as_ref().unwrap().selected.is_none());
        app.start_session("restored".into());
        app.restore_transcript(
            "restored".into(),
            &[
                Item::text(ItemKind::User, "replayed user"),
                Item::text(ItemKind::Assistant, "replayed assistant"),
            ],
        );
        app.open_navigation();
        assert_eq!(app.sync_navigation(), vec![0, 1]);
        app.handle_key(press(KeyCode::Down));
        app.last_key = None; // Intentional key, outside a paste burst.
        app.handle_key(press(KeyCode::Enter));
        assert_eq!(app.navigation.revealed, app.navigation.id(1));
    }

    #[test]
    fn typed_eligible_at_opens_picker_but_paste_and_email_do_not() {
        let mut typed = app();
        let Action::SearchFiles {
            query,
            revision,
            activation,
        } = typed.handle_key(press(KeyCode::Char('@')))
        else {
            panic!("eligible @ should search");
        };
        assert_eq!(query, "");
        assert_eq!(revision, 1);
        assert_eq!(activation, revision);
        assert_eq!(typed.editor.text(), "@");
        assert!(typed.file_picker.is_some());
        typed.handle_key(press(KeyCode::Esc));
        assert_eq!(typed.editor.text(), "@");
        assert!(typed.file_picker.is_none());

        let mut pasted = app();
        pasted.paste("@src/lib.rs");
        assert!(pasted.file_picker.is_none());
        assert_eq!(pasted.editor.text(), "@src/lib.rs");

        let mut email = app();
        email.paste("name");
        assert!(matches!(
            email.handle_key(press(KeyCode::Char('@'))),
            Action::None
        ));
        assert_eq!(email.editor.text(), "name@");
        assert!(email.file_picker.is_none());
    }

    #[test]
    fn unbracketed_paste_starting_with_at_dismisses_the_picker() {
        let mut app = app();
        assert!(matches!(
            app.handle_key(press(KeyCode::Char('@'))),
            Action::SearchFiles { .. }
        ));
        app.last_key = Some(Instant::now());

        assert!(matches!(
            app.handle_key(press(KeyCode::Char('s'))),
            Action::None
        ));
        assert_eq!(app.editor.text(), "@s");
        assert!(app.file_picker.is_none());
    }

    #[test]
    fn picker_revisions_discard_stale_results_and_follow_edits() {
        let mut app = app();
        let Action::SearchFiles {
            revision: first,
            activation,
            ..
        } = app.handle_key(press(KeyCode::Char('@')))
        else {
            panic!("picker search");
        };
        app.last_key = None;
        let Action::SearchFiles {
            query,
            revision: second,
            activation: refreshed_activation,
        } = app.handle_key(press(KeyCode::Char('s')))
        else {
            panic!("updated picker search");
        };
        assert_eq!(query, "s");
        assert!(second > first);
        assert_eq!(refreshed_activation, activation);

        app.apply(Update::FileMatches {
            revision: first,
            result: Ok(vec![FileMatch {
                relative_path: "stale.rs".into(),
                match_byte_offsets: Vec::new(),
            }]),
        });
        assert!(app.file_picker.as_ref().unwrap().matches.is_empty());

        app.apply(Update::FileMatches {
            revision: second,
            result: Ok(vec![FileMatch {
                relative_path: "src/lib.rs".into(),
                match_byte_offsets: std::iter::once(0..1).collect(),
            }]),
        });
        assert_eq!(
            app.file_picker.as_ref().unwrap().matches[0].relative_path,
            "src/lib.rs"
        );

        app.last_key = None;
        let Action::SearchFiles {
            query,
            activation: backspace_activation,
            ..
        } = app.handle_key(press(KeyCode::Backspace))
        else {
            panic!("backspace picker search");
        };
        assert_eq!(query, "");
        assert_eq!(backspace_activation, activation);
    }

    #[test]
    fn picker_navigation_selects_paths_with_spaces_as_plain_prompt_text() {
        let mut app = app();
        app.paste("open ");
        let Action::SearchFiles { revision, .. } = app.handle_key(press(KeyCode::Char('@'))) else {
            panic!("picker search");
        };
        app.apply(Update::FileMatches {
            revision,
            result: Ok(vec![
                FileMatch {
                    relative_path: "first.rs".into(),
                    match_byte_offsets: Vec::new(),
                },
                FileMatch {
                    relative_path: "docs/file name.md".into(),
                    match_byte_offsets: Vec::new(),
                },
            ]),
        });
        app.last_key = Some(Instant::now());
        app.handle_key(press(KeyCode::Down));
        app.last_key = None;
        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.editor.text(), "open @docs/file name.md");
        assert!(app.file_picker.is_none());

        app.last_key = None;
        let Action::Submit { prompt, .. } = app.handle_key(press(KeyCode::Enter)) else {
            panic!("plain prompt should submit");
        };
        assert_eq!(prompt.text, "open @docs/file name.md");
    }

    #[test]
    fn picker_dismissal_and_failures_preserve_the_query() {
        let mut app = app();
        let Action::SearchFiles { revision, .. } = app.handle_key(press(KeyCode::Char('@'))) else {
            panic!("picker search");
        };
        app.apply(Update::FileMatches {
            revision,
            result: Err("index unavailable".into()),
        });
        assert_eq!(app.editor.text(), "@");
        assert!(app.file_picker.is_none());
        assert_eq!(
            app.toast_text(),
            Some("file search failed: index unavailable")
        );

        app.paste(" ");
        app.last_key = None;
        assert!(matches!(
            app.handle_key(press(KeyCode::Char('@'))),
            Action::SearchFiles { .. }
        ));
        app.handle_key(press(KeyCode::Left));
        assert_eq!(app.editor.text(), "@ @");
        assert!(app.file_picker.is_none());
    }

    #[test]
    fn modified_editor_keys_bypass_and_dismiss_the_picker() {
        let mut editing = app();
        assert!(matches!(
            editing.handle_key(press(KeyCode::Char('@'))),
            Action::SearchFiles { .. }
        ));
        editing.last_key = None;

        assert!(matches!(
            editing.handle_key(modified_press(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Action::None
        ));
        assert_eq!(editing.editor.cursor(), 0);
        assert_eq!(editing.editor.text(), "@");
        assert!(editing.file_picker.is_none());

        let mut working = app();
        assert!(matches!(
            working.handle_key(press(KeyCode::Char('@'))),
            Action::SearchFiles { .. }
        ));
        working.last_key = None;
        working.phase = Phase::Working;
        assert!(matches!(
            working.handle_key(modified_press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Cancel
        ));
        assert!(working.phase == Phase::Cancelling);
        assert!(working.file_picker.is_none());
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
    fn switching_sessions_clears_the_process_local_agent_roster() {
        use crate::events::{GenerationOutcome, SubagentStatus};
        use ratatui::layout::Rect;

        let mut app = app();
        app.apply_runtime_at(
            agent_event(
                "stale",
                "Scout",
                SubagentStatus::Idle,
                Some(GenerationOutcome::Success),
                1,
                None,
                (10, 20, Some(30)),
            ),
            100,
        );
        app.cleaned_agent_ids.insert("cleaned".into());
        app.cleaned_agent_ancestors.insert("ancestor".into());
        app.set_agents_viewport(Rect::new(20, 5, 10, 4), 2);

        assert!(!app.agents.is_empty());
        assert!(!app.agent_versions.is_empty());
        assert!(!app.cleaned_agent_ids.is_empty());
        assert!(!app.cleaned_agent_ancestors.is_empty());

        app.start_session("replacement".into());

        assert!(app.agents.is_empty());
        assert!(app.agent_versions.is_empty());
        assert!(app.cleaned_agent_ids.is_empty());
        assert!(app.cleaned_agent_ancestors.is_empty());
        assert_eq!(app.agents_scroll, 0);
        assert_eq!(app.agents_viewport, 0);
        assert_eq!(app.agents_area, Rect::default());
    }

    #[test]
    fn turn_end_clears_compaction_state() {
        let mut app = app();
        app.compacting = true;
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));
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
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));
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
        assert!(!call.expanded);
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
            app.apply(Update::State(StateUpdate::Running(
                RunningStateUpdate::new(),
            )));
            app.apply(Update::State(StateUpdate::Idle(
                IdleStateUpdate::new().stop_reason(reason),
            )));

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
    fn transcript_and_tool_activity_do_not_drive_foreground_lifecycle() {
        let mut app = app();
        app.apply(Update::UserMessage {
            id: "user".into(),
            text: "hello".into(),
            images: Vec::new(),
            append: false,
        });
        compose(&mut app, "return 1");
        app.apply(Update::AgentMessage {
            id: "agent".into(),
            text: "hello".into(),
            append: false,
        });
        assert!(!app.working());
        assert!(app.turn_started.is_none());

        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        let started = app.turn_started;
        app.apply(Update::ToolUpdated {
            id: "call-1".into(),
            status: Some(ToolCallStatus::Completed),
            script: None,
            output: Vec::new(),
            backgrounded: false,
        });
        assert!(app.working());
        assert_eq!(app.turn_started, started);
        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));
        app.apply(Update::AgentMessage {
            id: "late".into(),
            text: "background result".into(),
            append: false,
        });
        compose(&mut app, "return 2");
        assert!(!app.working());
        assert!(app.turn_started.is_none());
    }

    #[test]
    fn requires_action_preserves_the_running_turn_timer() {
        let mut app = app();
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        let started = app.turn_started;
        app.apply(Update::State(StateUpdate::RequiresAction(
            RequiresActionStateUpdate::new(),
        )));
        assert!(app.phase == Phase::Blocked);
        assert_eq!(app.turn_started, started);
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        assert!(app.phase == Phase::Working);
        assert_eq!(app.turn_started, started);
    }

    #[test]
    fn cancellation_request_does_not_override_the_actual_stop_reason() {
        let mut app = app();
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        let started = app.turn_started;
        assert!(matches!(app.request_cancel(), Action::Cancel));
        assert_eq!(app.turn_started, started);
        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));
        assert!(!app.working());
        assert!(app.turn_started.is_none());
        assert!(
            !app.blocks
                .iter()
                .any(|block| matches!(block, Block::Notice(text) if text == "turn interrupted"))
        );
    }

    #[test]
    fn duplicate_state_updates_are_idempotent() {
        let mut app = app();
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        let started = app.turn_started;
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        assert_eq!(app.turn_started, started);
        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));
        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));
        assert!(!app.working());
        assert!(app.turn_started.is_none());
        assert_eq!(
            app.blocks
                .iter()
                .filter(|block| matches!(block, Block::TurnDuration(_)))
                .count(),
            1
        );
    }

    #[test]
    fn completed_turn_duration_is_recorded_at_the_end() {
        let mut app = app();
        app.push_user("hello".into());
        app.turn_started = Some(Instant::now() - Duration::from_secs(65));

        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));

        assert!(matches!(
            app.blocks.last(),
            Some(Block::TurnDuration(millis)) if *millis >= 65_000
        ));
    }

    #[test]
    fn autonomous_turn_is_visible_and_cancellable() {
        let mut app = app();

        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));

        assert!(app.working());
        assert!(matches!(app.request_cancel(), Action::Cancel));
        assert!(app.phase == Phase::Cancelling);

        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::Cancelled),
        )));

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

        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));

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
        assert_eq!(
            app.available_commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["compact"]
        );

        app.apply(Update::AvailableCommands {
            session_id: "stale".into(),
            commands: vec!["ignored".into()],
        });
        assert_eq!(
            app.available_commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["compact"]
        );

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
    fn login_is_exposed_only_for_advertised_terminal_auth_methods() {
        let mut unavailable = app();
        unavailable.paste("/login");
        unavailable.last_key = Some(Instant::now() - Duration::from_millis(500));
        let Action::Submit { prompt, .. } = unavailable.handle_key(press(KeyCode::Enter)) else {
            panic!("expected an ordinary prompt");
        };
        assert_eq!(prompt.text, "/login");

        let mut available = app();
        available.auth_methods = vec![
            AuthMethodTerminal::new("openai", "ChatGPT").args(vec![
                "auth".into(),
                "login".into(),
                "openai".into(),
            ]),
            AuthMethodTerminal::new("openrouter", "OpenRouter").args(vec![
                "auth".into(),
                "login".into(),
                "openrouter".into(),
            ]),
        ];
        available.paste("/login openrouter");
        available.last_key = Some(Instant::now() - Duration::from_millis(500));
        assert!(matches!(
            available.handle_key(press(KeyCode::Enter)),
            Action::Login(method) if method.method_id.0.as_ref() == "openrouter"
        ));
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
    fn tab_completes_a_slash_command_without_submitting() {
        let mut app = app();
        app.paste("/mo");
        assert_eq!(app.command_completions()[0].name, "/model");

        assert!(matches!(app.handle_key(press(KeyCode::Tab)), Action::None));
        assert_eq!(app.editor.text(), "/model");
        assert!(app.command_completions().is_empty());
    }

    #[test]
    fn completion_keys_select_and_dismiss_before_normal_editor_actions() {
        let mut app = app();
        app.paste("/");
        app.handle_key(press(KeyCode::Down));
        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.editor.text(), "/resume");

        app.editor.clear();
        app.paste("/m");
        app.handle_key(press(KeyCode::Esc));
        assert!(app.command_completions().is_empty());
        app.handle_key(press(KeyCode::Char('o')));
        assert_eq!(app.command_completions()[0].name, "/model");
    }

    #[test]
    fn tab_keeps_inserting_spaces_without_an_active_completion() {
        let mut app = app();
        app.paste("plain");
        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.editor.text(), "plain    ");
    }

    #[test]
    fn completion_is_hidden_after_arguments_but_available_while_working() {
        let mut app = app();
        app.paste("/model sonnet");
        assert!(app.command_completions().is_empty());

        app.editor.clear();
        app.paste("/mo");
        app.phase = Phase::Working;
        assert_eq!(app.command_completions()[0].name, "/model");
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
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
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
            editable: true,
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

    fn queued_app() -> App {
        let mut app = app();
        app.can_steer = true;
        app.can_replace_steer = true;
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
        for id in ["a", "b", "c"] {
            app.apply(Update::SteerAccepted {
                editable: true,
                id: id.into(),
                text: format!("pending {id}"),
            });
        }
        app
    }

    fn begin_steer_edit(app: &mut App) {
        app.handle_key(press(KeyCode::F(2)));
        app.handle_key(press(KeyCode::Enter));
        assert!(app.editing_steer());
    }

    #[test]
    fn queued_focus_preserves_global_task_view_and_copy_shortcuts() {
        for drained in [false, true] {
            let mut app = queued_app();
            app.paste("draft");
            app.latest_agent_source = "answer".into();
            app.handle_key(press(KeyCode::F(2)));
            if drained {
                for id in ["a", "b", "c"] {
                    app.steer_revoked(id);
                }
            }
            // Without background work Ctrl+K is an editor key, so suppress it.
            app.handle_key(modified_press(KeyCode::Char('k'), KeyModifiers::CONTROL));
            assert_eq!(app.editor.text(), "draft");
            app.handle_key(modified_press(KeyCode::Char('l'), KeyModifiers::CONTROL));
            assert!(app.show_logs);
            assert!(matches!(
                app.handle_key(modified_press(KeyCode::Char('y'), KeyModifiers::CONTROL)),
                Action::Copy(text) if text == "answer"
            ));
            app.apply(Update::ToolStarted {
                id: "foreground".into(),
                title: "compose".into(),
                kind: ToolKind::Other,
                script: None,
                backgrounded: false,
            });
            assert!(matches!(
                app.handle_key(modified_press(KeyCode::Char('b'), KeyModifiers::SUPER)),
                Action::DetachCompose(id) if id == "foreground"
            ));
            app.apply(Update::ToolStarted {
                id: "background".into(),
                title: "compose".into(),
                kind: ToolKind::Other,
                script: None,
                backgrounded: true,
            });
            assert!(matches!(
                app.handle_key(modified_press(KeyCode::Char('k'), KeyModifiers::CONTROL)),
                Action::CancelBackground(id) if id == "background"
            ));
            assert_eq!(app.queue_focused, !drained);
            assert_eq!(app.editor.text(), "draft");
        }
    }

    #[test]
    fn queued_selector_navigation_and_revoke_preserve_order_and_draft() {
        let mut app = queued_app();
        app.paste("draft");
        app.handle_key(press(KeyCode::F(2)));
        app.handle_key(press(KeyCode::Up));
        assert_eq!(app.selected_steer.as_deref(), Some("a"));
        app.handle_key(press(KeyCode::Down));
        let Action::RevokeSteer { id } = app.handle_key(press(KeyCode::Delete)) else {
            panic!("expected revoke");
        };
        assert_eq!(id, "b");
        assert_eq!(
            app.pending_steers.len(),
            3,
            "wait for server acknowledgment"
        );
        app.steer_mutation_failed(&id, "temporary failure".into(), false);
        assert_eq!(app.pending_steers.len(), 3);
        assert_eq!(app.selected_steer.as_deref(), Some("b"));
        app.steer_revoked(&id);
        assert_eq!(
            app.pending_steers
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(app.selected_steer.as_deref(), Some("c"));
        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.selected_steer.as_deref(), Some("c"));
        app.handle_key(press(KeyCode::Esc));
        assert!(app.selected_steer.is_none());
        assert!(app.working(), "Esc in selector must not cancel the turn");
        assert_eq!(app.editor.text(), "draft");
        app.apply(Update::SteerAccepted {
            editable: true,
            id,
            text: "late acceptance".into(),
        });
        assert_eq!(app.pending_steers.len(), 2, "revoked IDs stay retired");
    }

    #[test]
    fn queued_edit_cancel_restores_draft_cursor_and_attachments() {
        let mut app = queued_app();
        app.paste("draft");
        app.attach(
            PathBuf::from("image.png"),
            "image/png",
            AttachmentKind::Image,
            3,
        );
        app.editor.move_left();
        let draft = app.editor.text().to_owned();
        let cursor = app.editor.cursor();
        let attachments = app.attachments.clone();
        let sequence = app.next_attachment;
        begin_steer_edit(&mut app);
        assert_eq!(app.editor.text(), "pending a");
        assert!(app.attachments.is_empty());
        app.paste(" revised");
        app.attach(
            PathBuf::from("other.png"),
            "image/png",
            AttachmentKind::Image,
            4,
        );
        assert!(app.attachments.is_empty(), "replacement is text-only");
        app.handle_key(press(KeyCode::Esc));
        assert!(!app.editing_steer());
        assert_eq!(app.editor.text(), draft);
        assert_eq!(app.editor.cursor(), cursor);
        assert_eq!(app.attachments, attachments);
        assert_eq!(app.next_attachment, sequence);
        assert_eq!(app.pending_steers[0].text, "pending a");
        assert!(app.working());
    }

    #[test]
    fn queued_edit_save_keeps_id_order_and_restores_draft() {
        let mut app = queued_app();
        app.paste("next draft");
        app.handle_key(press(KeyCode::F(2)));
        app.handle_key(press(KeyCode::Down));
        app.handle_key(press(KeyCode::Enter));
        app.paste(" revised");
        let Action::ReplaceSteer { id, text } = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected replacement");
        };
        assert_eq!(id, "b");
        assert_eq!(text, "pending b revised");
        assert_eq!(app.pending_steers[1].text, "pending b");
        assert_eq!(app.editor.text(), text, "keep edit until acknowledged");
        let token = app.begin_steer_mutation(&id, Some(text)).unwrap();
        app.apply(Update::SteerMutationFinished {
            id: id.clone(),
            token,
            result: Ok(()),
        });
        assert_eq!(
            app.pending_steers
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(app.pending_steers[1].text, "pending b revised");
        assert_eq!(app.editor.text(), "next draft");
        assert!(!app.editing_steer());
    }

    #[test]
    fn queued_edit_failure_keeps_both_drafts_and_allows_retry() {
        let mut app = queued_app();
        app.paste("original draft");
        begin_steer_edit(&mut app);
        app.paste(" revision");
        app.steer_mutation_failed("a", "temporary failure".into(), false);
        assert_eq!(app.editor.text(), "pending a revision");
        assert!(app.editing_steer());
        assert_eq!(app.pending_steers[0].text, "pending a");
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::ReplaceSteer { .. }
        ));
        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.editor.text(), "original draft");
    }

    #[test]
    fn queued_edit_delivery_races_never_resurrect_messages() {
        for unavailable_error in [false, true] {
            let mut app = queued_app();
            app.paste("original draft");
            begin_steer_edit(&mut app);
            app.paste(" revision");
            let token = app
                .begin_steer_mutation("a", Some(app.editor.text().to_owned()))
                .unwrap();
            if unavailable_error {
                app.steer_mutation_failed("a", "unknown message".into(), true);
            } else {
                app.apply(Update::UserMessage {
                    id: "a".into(),
                    text: "pending a".into(),
                    images: Vec::new(),
                    append: false,
                });
            }
            assert_eq!(app.editor.text(), "pending a revision");
            assert!(matches!(
                app.handle_key(press(KeyCode::Enter)),
                Action::None
            ));
            assert!(!app.pending_steers.iter().any(|pending| pending.id == "a"));
            app.apply(Update::SteerMutationFinished {
                id: "a".into(),
                token,
                result: Ok(()),
            });
            app.apply(Update::SteerAccepted {
                editable: true,
                id: "a".into(),
                text: "late acceptance".into(),
            });
            assert!(!app.pending_steers.iter().any(|pending| pending.id == "a"));
            assert_eq!(app.editor.text(), "original draft");
        }
    }

    #[test]
    fn queued_delivery_reselects_neighbor_and_session_switch_clears_edit() {
        let mut app = queued_app();
        app.paste("draft");
        app.handle_key(press(KeyCode::F(2)));
        app.apply(Update::UserMessage {
            id: "a".into(),
            text: "pending a".into(),
            images: Vec::new(),
            append: false,
        });
        assert_eq!(app.selected_steer.as_deref(), Some("b"));
        app.handle_key(press(KeyCode::Enter));
        app.start_session("other-session".into());
        assert!(!app.editing_steer());
        assert!(app.selected_steer.is_none());
        assert!(app.pending_steers.is_empty());
        assert!(app.retired_steers.is_empty());
        assert_eq!(app.editor.text(), "draft");
    }

    #[test]
    fn queued_edit_capability_is_required_but_revoke_is_always_available() {
        let mut app = queued_app();
        app.can_replace_steer = false;
        app.paste("draft");
        app.handle_key(press(KeyCode::F(2)));
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert!(!app.editing_steer());
        assert_eq!(app.editor.text(), "draft");
        assert!(matches!(
            app.handle_key(press(KeyCode::Delete)),
            Action::RevokeSteer { .. }
        ));
        app.can_replace_steer = true;
        app.handle_key(press(KeyCode::Enter));
        app.can_replace_steer = false;
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert_eq!(app.editor.text(), "pending a");
        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.editor.text(), "draft");
    }

    #[test]
    fn empty_queue_does_not_capture_composer_focus() {
        let mut app = app();
        app.paste("draft");
        app.handle_key(press(KeyCode::F(2)));
        assert!(!app.queue_focused);
        assert!(!app.queue_handoff);
        assert!(app.selected_steer.is_none());
        assert_eq!(app.toast_text(), Some("no pending messages"));
        app.handle_key(press(KeyCode::Char('!')));
        assert_eq!(app.editor.text(), "draft!");
    }

    #[test]
    fn draining_the_queue_returns_keyboard_focus_to_the_composer() {
        for outcome in ["delivery", "removal", "unavailable", "turn_end"] {
            let mut app = queued_app();
            app.paste("draft");
            app.handle_key(press(KeyCode::F(2)));
            if outcome == "turn_end" {
                app.apply(Update::State(StateUpdate::Idle(
                    IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
                )));
            } else {
                for id in ["a", "b", "c"] {
                    match outcome {
                        "delivery" => app.apply(Update::UserMessage {
                            id: id.into(),
                            text: id.into(),
                            images: Vec::new(),
                            append: false,
                        }),
                        "removal" => app.steer_revoked(id),
                        "unavailable" => app.steer_mutation_failed(id, "gone".into(), true),
                        _ => unreachable!(),
                    }
                }
            }
            assert!(app.pending_steers.is_empty(), "{outcome}");
            assert!(!app.queue_focused, "{outcome}");
            assert!(app.selected_steer.is_none(), "{outcome}");
            assert_eq!(app.editor.text(), "draft");
            app.handle_key(press(KeyCode::Char('!')));
            assert_eq!(app.editor.text(), "draft!", "{outcome}");
        }
    }

    #[test]
    fn automatic_queue_closure_guards_stale_destructive_keys_until_acknowledged() {
        for outcome in ["delivery", "removal", "unavailable", "turn_end"] {
            for key in [KeyCode::Enter, KeyCode::Backspace, KeyCode::Delete] {
                let mut app = queued_app();
                app.paste("draft");
                app.attach(
                    PathBuf::from("image.png"),
                    "image/png",
                    AttachmentKind::Image,
                    3,
                );
                app.editor.move_left();
                let draft = app.editor.text().to_owned();
                let attachments = app.attachments.clone();
                app.handle_key(press(KeyCode::F(2)));
                if outcome == "turn_end" {
                    app.apply(Update::State(StateUpdate::Idle(
                        IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
                    )));
                } else {
                    for id in ["a", "b", "c"] {
                        match outcome {
                            "delivery" => app.apply(Update::UserMessage {
                                id: id.into(),
                                text: id.into(),
                                images: Vec::new(),
                                append: false,
                            }),
                            "removal" => {
                                let token = app.begin_steer_mutation(id, None).unwrap();
                                app.apply(Update::SteerMutationFinished {
                                    id: id.into(),
                                    token,
                                    result: Ok(()),
                                });
                            }
                            "unavailable" => app.steer_mutation_failed(id, "gone".into(), true),
                            _ => unreachable!(),
                        }
                    }
                }
                assert!(!app.queue_focused);
                assert!(app.queue_handoff);
                for _ in 0..3 {
                    assert!(matches!(app.handle_key(press(key)), Action::None));
                    assert_eq!(app.editor.text(), draft, "{outcome} {key:?}");
                    assert_eq!(app.attachments, attachments);
                    assert!(app.queue_handoff);
                }
                assert!(app.toast_text().unwrap().contains("queue closed"));
                // Esc acknowledges composer focus rather than cancelling the turn.
                assert!(matches!(app.handle_key(press(KeyCode::Esc)), Action::None));
                assert!(!app.queue_handoff);
                assert!(app.phase != Phase::Cancelling);
                let action = app.handle_key(press(key));
                if key == KeyCode::Enter {
                    assert!(
                        matches!(action, Action::Submit { prompt, .. } if prompt.text == draft && prompt.attachments == attachments)
                    );
                } else {
                    assert_ne!(app.editor.text(), draft);
                }
            }
        }
    }

    #[test]
    fn typing_paste_and_cursor_movement_acknowledge_queue_handoff() {
        for interaction in ["typing", "paste", "cursor"] {
            let mut app = queued_app();
            app.paste("draft");
            app.handle_key(press(KeyCode::F(2)));
            for id in ["a", "b", "c"] {
                app.steer_revoked(id);
            }
            assert!(app.queue_handoff);
            match interaction {
                "typing" => {
                    app.handle_key(press(KeyCode::Char('!')));
                }
                "paste" => app.paste("!"),
                "cursor" => {
                    app.handle_key(press(KeyCode::Left));
                }
                _ => unreachable!(),
            }
            assert!(!app.queue_handoff, "{interaction}");
            assert!(!app.queue_focused);
            app.apply(Update::State(StateUpdate::Idle(
                IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
            )));
            assert!(
                !app.queue_handoff,
                "turn completion must not rearm the handoff"
            );
            app.last_key = None;
            assert!(matches!(
                app.handle_key(press(KeyCode::Enter)),
                Action::Submit { .. }
            ));
        }
    }

    #[test]
    fn explicit_queue_exit_and_session_switch_do_not_require_handoff_acknowledgement() {
        let mut app = queued_app();
        app.paste("draft");
        app.handle_key(press(KeyCode::F(2)));
        app.handle_key(press(KeyCode::Esc));
        assert!(!app.queue_handoff);
        app.handle_key(press(KeyCode::F(2)));
        for id in ["a", "b", "c"] {
            app.steer_revoked(id);
        }
        assert!(app.queue_handoff);
        app.start_session("other-session".into());
        assert!(!app.queue_handoff);
        assert_eq!(app.editor.text(), "draft");
    }

    #[test]
    fn backspace_and_forward_delete_remove_only_after_acknowledgement() {
        for key in [KeyCode::Backspace, KeyCode::Delete] {
            let mut app = queued_app();
            app.paste("draft");
            app.handle_key(press(KeyCode::F(2)));
            for (index, expected) in ["a", "b", "c"].into_iter().enumerate() {
                let Action::RevokeSteer { id } = app.handle_key(press(key)) else {
                    panic!("expected removal from {key:?}");
                };
                assert_eq!(id, expected);
                let token = app.begin_steer_mutation(&id, None).unwrap();
                assert_eq!(app.pending_steers.len(), 3 - index);
                assert!(app.queue_focused);
                assert!(matches!(app.handle_key(press(key)), Action::None));
                assert_eq!(app.editor.text(), "draft");
                app.apply(Update::SteerMutationFinished {
                    id,
                    token,
                    result: Ok(()),
                });
                assert_eq!(app.pending_steers.len(), 2 - index);
                assert_eq!(app.queue_focused, index < 2);
            }
            assert!(app.selected_steer.is_none());
            assert_eq!(app.editor.text(), "draft");
            app.handle_key(press(KeyCode::Char('!')));
            assert_eq!(app.editor.text(), "draft!");
        }
    }

    #[test]
    fn queued_edit_stays_recoverable_when_turn_finishes() {
        let mut app = queued_app();
        app.paste("draft");
        begin_steer_edit(&mut app);
        app.paste(" revised");
        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));
        assert!(app.pending_steers.is_empty());
        assert!(app.editing_steer());
        assert!(!app.queue_handoff);
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert_eq!(app.editor.text(), "pending a revised");
        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.editor.text(), "draft");
    }

    #[test]
    fn queued_ctrl_c_cancels_then_quits_even_after_delivery_empties_selection() {
        for empty in [false, true] {
            let mut app = queued_app();
            app.paste("draft");
            app.handle_key(press(KeyCode::F(2)));
            if empty {
                for id in ["a", "b", "c"] {
                    app.steer_revoked(id);
                }
            }
            assert!(matches!(
                app.handle_key(modified_press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                Action::Cancel
            ));
            assert_eq!(app.editor.text(), "draft");
            assert!(matches!(
                app.handle_key(modified_press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                Action::Quit
            ));
        }
    }

    #[test]
    fn queued_ctrl_c_retains_idle_clear_then_quit_semantics() {
        let mut app = app();
        app.paste("draft");
        app.handle_key(press(KeyCode::F(2)));
        assert!(matches!(
            app.handle_key(modified_press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::None
        ));
        assert!(app.editor.is_empty());
        assert!(matches!(
            app.handle_key(modified_press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        ));
    }

    #[test]
    fn queued_ctrl_d_keeps_empty_composer_exit_semantics() {
        let mut app = queued_app();
        app.handle_key(press(KeyCode::F(2)));
        assert!(matches!(
            app.handle_key(modified_press(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Action::Quit
        ));
        app.paste("draft");
        assert!(matches!(
            app.handle_key(modified_press(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Action::None
        ));
        assert_eq!(app.editor.text(), "draft");
    }

    #[test]
    fn queued_inflight_mutations_reject_duplicates_and_keep_newer_edit_text() {
        let mut app = queued_app();
        app.paste("draft");
        begin_steer_edit(&mut app);
        let text = app.editor.text().to_owned();
        let token = app.begin_steer_mutation("a", Some(text.clone())).unwrap();
        assert!(app.begin_steer_mutation("a", Some(text.clone())).is_none());
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        app.paste(" newer revision");
        app.apply(Update::SteerMutationFinished {
            id: "a".into(),
            token,
            result: Ok(()),
        });
        assert_eq!(app.pending_steers[0].text, text);
        assert_eq!(app.editor.text(), "pending a newer revision");
        assert!(app.editing_steer());
        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.editor.text(), "draft");
        app.handle_key(press(KeyCode::F(2)));
        app.begin_steer_mutation("a", None).unwrap();
        assert!(matches!(
            app.handle_key(press(KeyCode::Delete)),
            Action::None
        ));
    }

    #[test]
    fn queued_old_completion_cannot_close_reopened_same_id_edit_or_new_request() {
        let mut app = queued_app();
        app.paste("draft");
        begin_steer_edit(&mut app);
        let token = app
            .begin_steer_mutation("a", Some(app.editor.text().to_owned()))
            .unwrap();
        app.handle_key(press(KeyCode::Esc));
        begin_steer_edit(&mut app); // Identical text and ID, but a different edit.
        app.apply(Update::SteerMutationFinished {
            id: "a".into(),
            token,
            result: Ok(()),
        });
        assert!(app.editing_steer());
        let next = app
            .begin_steer_mutation("a", Some("replacement".into()))
            .unwrap();
        app.apply(Update::SteerMutationFinished {
            id: "a".into(),
            token,
            result: Ok(()),
        });
        assert_eq!(app.steer_mutations["a"].token, next);
        assert_eq!(app.pending_steers[0].text, "pending a");
        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.editor.text(), "draft");
    }

    #[test]
    fn queued_async_error_unlocks_retry_without_losing_either_draft() {
        let mut app = queued_app();
        app.paste("draft");
        begin_steer_edit(&mut app);
        app.paste(" revision");
        let token = app
            .begin_steer_mutation("a", Some(app.editor.text().to_owned()))
            .unwrap();
        app.apply(Update::SteerMutationFinished {
            id: "a".into(),
            token,
            result: Err(super::SteerMutationError {
                message: "retry".into(),
                unavailable: false,
            }),
        });
        assert!(app.steer_mutations.is_empty());
        assert_eq!(app.editor.text(), "pending a revision");
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::ReplaceSteer { .. }
        ));
        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.editor.text(), "draft");
    }

    #[test]
    fn queued_session_switch_clears_inflight_state_and_rejects_old_completion() {
        let mut app = queued_app();
        app.paste("draft");
        begin_steer_edit(&mut app);
        let token = app
            .begin_steer_mutation("a", Some("old replacement".into()))
            .unwrap();
        app.start_session("new-session".into());
        assert!(app.steer_mutations.is_empty());
        app.apply(Update::SteerAccepted {
            editable: true,
            id: "a".into(),
            text: "new session message".into(),
        });
        begin_steer_edit(&mut app);
        let next = app
            .begin_steer_mutation("a", Some("new replacement".into()))
            .unwrap();
        app.apply(Update::SteerMutationFinished {
            id: "a".into(),
            token,
            result: Ok(()),
        });
        assert_eq!(app.steer_mutations["a"].token, next);
        assert_eq!(app.pending_steers[0].text, "new session message");
        assert_eq!(app.editor.text(), "new session message");
    }

    #[test]
    fn queued_media_steering_submits_but_only_allows_removal() {
        let mut app = queued_app();
        app.attach(
            PathBuf::from("image.png"),
            "image/png",
            AttachmentKind::Image,
            3,
        );
        let Action::Submit { prompt, inject } = app.handle_key(press(KeyCode::Enter)) else {
            panic!("expected media steer submission");
        };
        assert!(inject);
        assert_eq!(prompt.attachments.len(), 1);
        app.apply(Update::SteerAccepted {
            id: "media".into(),
            text: prompt.text,
            editable: false,
        });
        app.selected_steer = Some("media".into());
        app.queue_focused = true;
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert!(!app.editing_steer());
        assert!(
            app.toast_text()
                .unwrap()
                .contains("with media cannot be edited")
        );
        assert!(
            app.begin_steer_mutation("media", Some("text replacement".into()))
                .is_none()
        );
        assert!(
            matches!(app.handle_key(press(KeyCode::Delete)), Action::RevokeSteer { id } if id == "media")
        );
        assert!(app.begin_steer_mutation("media", None).is_some());
    }

    #[test]
    fn active_text_is_preserved_when_steering_is_not_advertised() {
        let mut app = app();
        app.apply(Update::State(StateUpdate::Running(
            RunningStateUpdate::new(),
        )));
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
    fn storage_status_survives_session_switch_and_clears_after_recovery() {
        let mut app = app();
        app.start_session("old".into());
        // Storage events must bypass the session routing guard.
        app.apply(Update::Runtime(RuntimeEvent::StorageStatus {
            pending: true,
            exhausted: false,
        }));
        assert!(app.storage_pending);
        assert!(!app.show_logs);
        app.start_session("new".into());
        assert!(app.storage_pending);
        app.apply(Update::Runtime(RuntimeEvent::StorageStatus {
            pending: false,
            exhausted: false,
        }));
        assert!(!app.storage_pending);
        app.apply(Update::Runtime(RuntimeEvent::StorageStatus {
            pending: false,
            exhausted: true,
        }));
        assert!(app.storage_exhausted);
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
        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));
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
        app.apply(Update::State(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(StopReason::EndTurn),
        )));
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
        app.editor.insert_str("/sessions");
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert!(app.session_catalog_pending);

        app.start_session("replacement".into());
        assert!(!app.session_catalog_pending);
    }

    #[test]
    fn session_catalog_update_opens_the_dialog() {
        let mut app = app();
        assert!(matches!(
            app.handle_key(press(KeyCode::Char('@'))),
            Action::SearchFiles { .. }
        ));
        app.apply(Update::SessionCatalog(Ok(vec![
            crate::session::CatalogEntry {
                id: "saved".into(),
                title: Some("Saved".into()),
                preview: None,
                is_subagent: false,
                updated_at: 0,
            },
        ])));
        assert_eq!(app.session_choices[0].id, "saved");
        assert!(app.session_dialog.is_some());
        assert!(app.file_picker.is_none());
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
                is_subagent: false,
                updated_at: 0,
            })
            .collect();
        app.session_dialog = Some(super::SessionDialog {
            selected: 0,
            rename: None,
        });

        assert!(matches!(app.handle_key(press(KeyCode::Down)), Action::None));
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::Resume(id) if id == "older"
        ));
        assert!(app.session_dialog.is_none());
    }

    #[test]
    fn session_dialog_renames_in_place_and_preserves_selection() {
        let mut app = app();
        app.session_choices = ["newer", "older"]
            .into_iter()
            .map(|id| crate::session::CatalogEntry {
                id: id.into(),
                title: Some(format!("{id} title")),
                preview: None,
                is_subagent: false,
                updated_at: 0,
            })
            .collect();
        app.session_dialog = Some(super::SessionDialog {
            selected: 1,
            rename: None,
        });

        assert!(matches!(
            app.handle_key(press(KeyCode::Char('R'))),
            Action::None
        ));
        app.paste("OAuth bug");
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::RenameSession { session_id, display_name: Some(name) }
                if session_id == "older" && name == "OAuth bug"
        ));
        assert_eq!(app.session_dialog.as_ref().unwrap().selected, 1);
        assert!(matches!(
            app.session_dialog.as_ref().unwrap().rename,
            Some(super::SessionRename::Saving)
        ));
        assert!(matches!(
            app.handle_key(press(KeyCode::Char('r'))),
            Action::None
        ));

        app.apply(Update::SessionRenamed {
            session_id: "older".into(),
            display_name: Some("OAuth bug".into()),
            result: Ok(Some("OAuth bug".into())),
        });
        assert_eq!(app.session_dialog.as_ref().unwrap().selected, 1);
        assert_eq!(app.session_choices[1].title.as_deref(), Some("OAuth bug"));

        app.session_dialog.as_mut().unwrap().rename = Some(super::SessionRename::Saving);
        app.apply(Update::SessionRenamed {
            session_id: "older".into(),
            display_name: Some("Retry me".into()),
            result: Err("disk full".into()),
        });
        assert!(matches!(
            app.session_dialog.as_ref().unwrap().rename.as_ref(),
            Some(super::SessionRename::Editing(input)) if input == "Retry me"
        ));
    }

    #[test]
    fn session_dialog_confirms_before_clearing_a_name() {
        let mut app = app();
        app.session_choices = vec![crate::session::CatalogEntry {
            id: "saved".into(),
            title: Some("Generated".into()),
            preview: None,
            is_subagent: false,
            updated_at: 0,
        }];
        app.session_dialog = Some(super::SessionDialog {
            selected: 0,
            rename: Some(super::SessionRename::Editing(String::new())),
        });

        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert!(matches!(
            app.session_dialog.as_ref().unwrap().rename,
            Some(super::SessionRename::ConfirmClear)
        ));
        app.last_key = None;
        assert!(matches!(app.handle_key(press(KeyCode::Esc)), Action::None));
        assert!(matches!(
            app.session_dialog.as_ref().unwrap().rename,
            Some(super::SessionRename::Editing(_))
        ));
        app.last_key = None;
        app.handle_key(press(KeyCode::Enter));
        app.last_key = None;
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::RenameSession { session_id, display_name: None } if session_id == "saved"
        ));
        assert!(app.session_dialog.is_some());
    }

    #[test]
    fn session_rename_backspace_removes_a_complete_grapheme() {
        let mut app = app();
        app.session_dialog = Some(super::SessionDialog {
            selected: 0,
            rename: Some(super::SessionRename::Editing("e\u{301} 👨‍👩‍👧".into())),
        });

        assert!(matches!(
            app.handle_key(press(KeyCode::Backspace)),
            Action::None
        ));
        assert!(matches!(
            app.session_dialog.as_ref().unwrap().rename.as_ref(),
            Some(super::SessionRename::Editing(input)) if input == "e\u{301} "
        ));
    }

    #[test]
    fn session_rename_clear_confirmation_ignores_enter_from_a_paste_burst() {
        let mut app = app();
        app.session_dialog = Some(super::SessionDialog {
            selected: 0,
            rename: Some(super::SessionRename::ConfirmClear),
        });
        app.last_key = Some(Instant::now());

        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert!(matches!(
            app.session_dialog.as_ref().unwrap().rename,
            Some(super::SessionRename::ConfirmClear)
        ));
    }

    #[test]
    fn session_rename_ignores_enter_from_an_unbracketed_paste_burst() {
        for input in [String::new(), "Pasted name".into()] {
            let mut app = app();
            app.session_dialog = Some(super::SessionDialog {
                selected: 0,
                rename: Some(super::SessionRename::Editing(input.clone())),
            });
            app.last_key = Some(Instant::now());

            assert!(matches!(
                app.handle_key(press(KeyCode::Enter)),
                Action::None
            ));
            assert!(matches!(
                app.session_dialog.as_ref().unwrap().rename.as_ref(),
                Some(super::SessionRename::Editing(actual)) if actual == &input
            ));
        }
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

    fn agent_event(
        id: &str,
        name: &str,
        status: crate::events::SubagentStatus,
        outcome: Option<crate::events::GenerationOutcome>,
        generation: u64,
        parent: Option<(&str, &str)>,
        timing: (u64, u64, Option<u64>),
    ) -> RuntimeEvent {
        let (created, started, finished) = timing;
        RuntimeEvent::SubagentStateChanged {
            id: id.into(),
            name: name.into(),
            status,
            outcome,
            generation,
            task: format!("task for {id}"),
            parent_id: parent.map(|(id, _)| id.into()),
            parent_name: parent.map(|(_, name)| name.into()),
            harness: "acp.kit".into(),
            model: Some("test".into()),
            created_at_unix_ms: created,
            generation_started_at_unix_ms: started,
            generation_finished_at_unix_ms: finished,
        }
    }

    #[test]
    fn agents_reduce_lifecycle_order_counts_and_duplicate_labels_by_id() {
        use crate::events::{GenerationOutcome, SubagentStatus};
        let mut app = app();
        app.apply_runtime_at(
            agent_event(
                "idle",
                "Scout",
                SubagentStatus::Idle,
                Some(GenerationOutcome::Success),
                1,
                None,
                (10, 20, Some(30)),
            ),
            100,
        );
        app.apply_runtime_at(
            agent_event(
                "working",
                "Scout",
                SubagentStatus::Working,
                None,
                2,
                Some(("idle", "Scout")),
                (11, 50, None),
            ),
            100,
        );
        app.apply_runtime_at(
            agent_event(
                "starting",
                "Pip",
                SubagentStatus::Starting,
                None,
                1,
                None,
                (9, 40, None),
            ),
            100,
        );

        assert_eq!(
            app.agents()
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["starting", "idle", "working"]
        );
        assert_eq!(
            app.agents()
                .iter()
                .filter(|row| row.name == "Scout")
                .count(),
            2
        );
        assert_eq!(
            app.agent_counts(),
            super::AgentCounts {
                total: 3,
                starting: 1,
                working: 1,
                idle: 1
            }
        );

        app.apply_runtime_at(
            agent_event(
                "working",
                "Scout",
                SubagentStatus::Starting,
                None,
                1,
                None,
                (11, 20, None),
            ),
            100,
        );
        app.apply_runtime_at(
            agent_event(
                "working",
                "Scout",
                SubagentStatus::Idle,
                Some(GenerationOutcome::Success),
                2,
                Some(("idle", "Scout")),
                (11, 50, Some(90)),
            ),
            100,
        );
        assert_eq!(
            app.agents()
                .iter()
                .find(|row| row.id == "working")
                .unwrap()
                .status,
            SubagentStatus::Idle
        );
    }

    #[test]
    fn process_exit_retires_active_agents_without_overwriting_terminal_rows() {
        use crate::events::{GenerationOutcome, SubagentStatus};

        let mut app = app();
        for event in [
            agent_event(
                "starting",
                "Starting",
                SubagentStatus::Starting,
                None,
                1,
                None,
                (10, 20, None),
            ),
            agent_event(
                "working",
                "Working",
                SubagentStatus::Working,
                None,
                2,
                None,
                (11, 21, None),
            ),
            agent_event(
                "idle-failed",
                "Idle failed",
                SubagentStatus::Idle,
                Some(GenerationOutcome::Failed),
                1,
                None,
                (12, 22, Some(30)),
            ),
            agent_event(
                "removed-failed",
                "Removed failed",
                SubagentStatus::Removed,
                Some(GenerationOutcome::Failed),
                1,
                None,
                (13, 23, Some(90)),
            ),
        ] {
            app.apply_runtime_at(event, 100);
        }

        app.apply(Update::ProcessExited("runtime exited".into()));

        for id in ["starting", "working"] {
            let row = app
                .agents
                .get(id)
                .expect("active row remains as a tombstone");
            assert_eq!(row.status, SubagentStatus::Removed);
            assert_eq!(row.outcome, Some(GenerationOutcome::Failed));
            assert!(row.generation_finished_at_unix_ms.is_some());
        }
        let idle = app.agents.get("idle-failed").unwrap();
        assert_eq!(idle.status, SubagentStatus::Idle);
        assert_eq!(idle.generation_finished_at_unix_ms, Some(30));
        let removed = app.agents.get("removed-failed").unwrap();
        assert_eq!(removed.status, SubagentStatus::Removed);
        assert_eq!(removed.generation_finished_at_unix_ms, Some(90));
        assert_eq!(app.agent_counts().starting, 0);
        assert_eq!(app.agent_counts().working, 0);

        let retired_at = app.agents["working"]
            .generation_finished_at_unix_ms
            .unwrap();
        app.tick_at(retired_at + 4_000);
        assert_eq!(
            app.agents.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["idle-failed"]
        );
        assert!(!app.needs_redraw_tick());
    }

    #[test]
    fn expired_failed_removed_agent_keeps_ticker_enabled_until_cleanup() {
        use crate::events::{GenerationOutcome, SubagentStatus};

        let mut app = app();
        app.apply_runtime_at(
            agent_event(
                "failed",
                "Failed",
                SubagentStatus::Removed,
                Some(GenerationOutcome::Failed),
                1,
                None,
                (10, 20, Some(1_000)),
            ),
            1_000,
        );

        assert!(app.needs_redraw_tick());
        app.tick_at(5_000);
        assert!(!app.agents.contains_key("failed"));
        assert!(!app.needs_redraw_tick());
    }

    #[test]
    fn agents_render_roots_and_arbitrary_depth_in_tree_order() {
        use crate::events::SubagentStatus;
        let mut app = app();
        for event in [
            agent_event(
                "grand",
                "Grand",
                SubagentStatus::Idle,
                None,
                1,
                Some(("child", "Child")),
                (4, 4, Some(5)),
            ),
            agent_event(
                "root",
                "Root",
                SubagentStatus::Idle,
                None,
                1,
                None,
                (1, 1, Some(2)),
            ),
            agent_event(
                "great",
                "Great",
                SubagentStatus::Idle,
                None,
                1,
                Some(("grand", "Grand")),
                (5, 5, Some(6)),
            ),
            agent_event(
                "child",
                "Child",
                SubagentStatus::Idle,
                None,
                1,
                Some(("root", "Root")),
                (3, 3, Some(4)),
            ),
            agent_event(
                "other",
                "Other",
                SubagentStatus::Idle,
                None,
                1,
                None,
                (2, 2, Some(3)),
            ),
        ] {
            app.apply_runtime_at(event, 100);
        }

        assert_eq!(
            app.agents()
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["root", "child", "grand", "great", "other"]
        );
        let tree = app.agent_tree_rows();
        assert_eq!(tree[0].ancestor_has_next_sibling, Vec::<bool>::new());
        assert!(tree[0].has_next_sibling);
        assert_eq!(tree[1].ancestor_has_next_sibling, vec![true]);
        assert_eq!(tree[2].ancestor_has_next_sibling, vec![true, false]);
        assert_eq!(tree[3].ancestor_has_next_sibling, vec![true, false, false]);
        assert!(!tree[4].has_next_sibling);
    }

    #[test]
    fn agents_sort_siblings_but_keep_active_descendants_with_idle_parents() {
        use crate::events::SubagentStatus;
        let mut app = app();
        for event in [
            agent_event(
                "idle-root",
                "Idle",
                SubagentStatus::Idle,
                None,
                1,
                None,
                (1, 1, Some(2)),
            ),
            agent_event(
                "late-child",
                "Late",
                SubagentStatus::Working,
                None,
                1,
                Some(("idle-root", "Idle")),
                (4, 4, None),
            ),
            agent_event(
                "early-child",
                "Early",
                SubagentStatus::Working,
                None,
                1,
                Some(("idle-root", "Idle")),
                (3, 3, None),
            ),
            agent_event(
                "starting-child",
                "Starting",
                SubagentStatus::Starting,
                None,
                1,
                Some(("idle-root", "Idle")),
                (5, 5, None),
            ),
            agent_event(
                "active-root",
                "Active",
                SubagentStatus::Working,
                None,
                1,
                None,
                (2, 2, None),
            ),
        ] {
            app.apply_runtime_at(event, 100);
        }

        assert_eq!(
            app.agents()
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "active-root",
                "idle-root",
                "starting-child",
                "early-child",
                "late-child"
            ]
        );
    }

    #[test]
    fn agents_missing_parent_falls_back_to_root_then_reparents() {
        use crate::events::SubagentStatus;
        let mut app = app();
        app.apply_runtime_at(
            agent_event(
                "child",
                "Child",
                SubagentStatus::Working,
                None,
                1,
                Some(("parent", "Parent")),
                (2, 2, None),
            ),
            100,
        );
        app.apply_runtime_at(
            agent_event(
                "other",
                "Other",
                SubagentStatus::Idle,
                None,
                1,
                None,
                (3, 3, Some(4)),
            ),
            100,
        );
        assert_eq!(
            app.agents()
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["child", "other"]
        );
        assert!(app.agent_tree_rows()[0].missing_parent);

        app.apply_runtime_at(
            agent_event(
                "parent",
                "Parent",
                SubagentStatus::Idle,
                None,
                1,
                None,
                (1, 1, Some(2)),
            ),
            100,
        );
        assert_eq!(
            app.agents()
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["parent", "child", "other"]
        );
        assert!(!app.agent_tree_rows()[1].missing_parent);
    }

    #[test]
    fn malformed_agent_parent_cycles_render_every_row_once_as_roots() {
        use std::collections::HashSet;

        use crate::events::SubagentStatus;
        let mut app = app();
        for event in [
            agent_event(
                "a",
                "A",
                SubagentStatus::Idle,
                None,
                1,
                Some(("b", "B")),
                (1, 1, Some(2)),
            ),
            agent_event(
                "b",
                "B",
                SubagentStatus::Idle,
                None,
                1,
                Some(("a", "A")),
                (2, 2, Some(3)),
            ),
            agent_event(
                "child",
                "Child",
                SubagentStatus::Working,
                None,
                1,
                Some(("a", "A")),
                (3, 3, None),
            ),
            agent_event(
                "self",
                "Self",
                SubagentStatus::Idle,
                None,
                1,
                Some(("self", "Self")),
                (4, 4, Some(5)),
            ),
        ] {
            app.apply_runtime_at(event, 100);
        }

        let tree = app.agent_tree_rows();
        assert_eq!(tree.len(), 4);
        assert!(tree.iter().all(|row| row.depth == 0));
        let ids = tree
            .iter()
            .map(|row| row.row.id.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(ids, HashSet::from(["a", "b", "child", "self"]));
    }

    #[test]
    fn agents_remove_strict_descendants_and_expire_failed_tombstones() {
        use crate::events::{GenerationOutcome, SubagentStatus};
        let mut app = app();
        app.apply_runtime_at(
            agent_event(
                "root",
                "Root",
                SubagentStatus::Idle,
                None,
                1,
                None,
                (1, 1, Some(2)),
            ),
            100,
        );
        app.apply_runtime_at(
            agent_event(
                "child",
                "Child",
                SubagentStatus::Idle,
                None,
                1,
                Some(("root", "Root")),
                (2, 2, Some(3)),
            ),
            100,
        );
        app.apply_runtime_at(
            agent_event(
                "grand",
                "Grand",
                SubagentStatus::Working,
                None,
                1,
                Some(("child", "Child")),
                (3, 3, None),
            ),
            100,
        );
        app.apply_runtime_at(
            RuntimeEvent::SubagentDescendantsRemoved {
                ancestor_id: "root".into(),
            },
            100,
        );
        assert_eq!(
            app.agents()
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["root"]
        );
        app.apply_runtime_at(
            agent_event(
                "child",
                "Child",
                SubagentStatus::Idle,
                None,
                1,
                Some(("root", "Root")),
                (2, 2, Some(3)),
            ),
            100,
        );
        assert!(app.agents().iter().all(|row| row.id != "child"));

        app.apply_runtime_at(
            agent_event(
                "failed",
                "Failed",
                SubagentStatus::Removed,
                Some(GenerationOutcome::Failed),
                1,
                None,
                (4, 10, Some(1_000)),
            ),
            1_000,
        );
        assert_eq!(app.agent_counts().total, 1);
        assert!(app.agents().iter().any(|row| row.id == "failed"));
        app.tick_at(4_999);
        assert!(app.agents().iter().any(|row| row.id == "failed"));
        app.tick_at(5_000);
        assert!(app.agents().iter().all(|row| row.id != "failed"));

        app.apply_runtime_at(
            agent_event(
                "closed",
                "Closed",
                SubagentStatus::Removed,
                Some(GenerationOutcome::Success),
                1,
                None,
                (5, 10, Some(20)),
            ),
            100,
        );
        assert!(app.agents().iter().all(|row| row.id != "closed"));
        app.apply_runtime_at(
            agent_event(
                "closed",
                "Closed",
                SubagentStatus::Working,
                None,
                1,
                None,
                (5, 10, None),
            ),
            100,
        );
        assert!(app.agents().iter().all(|row| row.id != "closed"));
    }

    #[test]
    fn descendant_cleanup_suppresses_late_children_but_not_ancestor_updates() {
        use crate::events::SubagentStatus;
        let mut app = app();
        app.apply_runtime_at(
            agent_event(
                "root",
                "Root",
                SubagentStatus::Idle,
                None,
                1,
                None,
                (1, 1, Some(2)),
            ),
            100,
        );
        app.apply_runtime_at(
            RuntimeEvent::SubagentDescendantsRemoved {
                ancestor_id: "root".into(),
            },
            100,
        );
        app.apply_runtime_at(
            agent_event(
                "root",
                "Root",
                SubagentStatus::Working,
                None,
                2,
                None,
                (1, 3, None),
            ),
            100,
        );
        for event in [
            agent_event(
                "late-child",
                "Late child",
                SubagentStatus::Working,
                None,
                1,
                Some(("root", "Root")),
                (4, 4, None),
            ),
            agent_event(
                "late-grand",
                "Late grandchild",
                SubagentStatus::Working,
                None,
                1,
                Some(("late-child", "Late child")),
                (5, 5, None),
            ),
        ] {
            app.apply_runtime_at(event, 100);
        }

        assert!(app.agents().iter().any(|row| {
            row.id == "root" && row.status == SubagentStatus::Working && row.generation == 2
        }));
        assert!(
            app.agents()
                .iter()
                .all(|row| row.id != "late-child" && row.id != "late-grand")
        );
    }

    #[test]
    fn agents_descendant_cleanup_terminally_suppresses_every_removed_depth() {
        use crate::events::SubagentStatus;
        let mut app = app();
        for event in [
            agent_event(
                "root",
                "Root",
                SubagentStatus::Idle,
                None,
                1,
                None,
                (1, 1, Some(2)),
            ),
            agent_event(
                "child",
                "Child",
                SubagentStatus::Starting,
                None,
                1,
                Some(("root", "Root")),
                (2, 2, None),
            ),
            agent_event(
                "grand",
                "Grand",
                SubagentStatus::Working,
                None,
                2,
                Some(("child", "Child")),
                (3, 3, None),
            ),
            agent_event(
                "great",
                "Great",
                SubagentStatus::Idle,
                None,
                3,
                Some(("grand", "Grand")),
                (4, 4, Some(5)),
            ),
        ] {
            app.apply_runtime_at(event, 100);
        }
        app.apply_runtime_at(
            RuntimeEvent::SubagentDescendantsRemoved {
                ancestor_id: "root".into(),
            },
            100,
        );

        // Delayed events advance rank and/or generation at every removed depth.
        // Cleanup is terminal for immutable IDs, so none may recreate a row.
        for event in [
            agent_event(
                "child",
                "Child",
                SubagentStatus::Working,
                None,
                1,
                Some(("root", "Root")),
                (2, 2, None),
            ),
            agent_event(
                "grand",
                "Grand",
                SubagentStatus::Idle,
                None,
                2,
                Some(("child", "Child")),
                (3, 3, Some(90)),
            ),
            agent_event(
                "great",
                "Great",
                SubagentStatus::Working,
                None,
                4,
                Some(("grand", "Grand")),
                (4, 80, None),
            ),
        ] {
            app.apply_runtime_at(event, 100);
        }
        assert_eq!(
            app.agents()
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["root"]
        );

        app.apply_runtime_at(
            agent_event(
                "unrelated",
                "Unrelated",
                SubagentStatus::Starting,
                None,
                1,
                None,
                (10, 10, None),
            ),
            100,
        );
        assert!(app.agents().iter().any(|row| row.id == "unrelated"));
    }

    #[test]
    fn agents_toggle_without_disturbing_editor() {
        let mut app = app();
        app.editor.insert_str("abc");
        app.handle_key(modified_press(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle_key(press(KeyCode::Char('X')));
        assert_eq!(app.editor.text(), "Xabc");

        assert!(!app.show_agents());
        assert!(matches!(
            app.handle_key(modified_press(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Action::None
        ));
        assert!(app.show_agents());

        app.editor.clear();
        app.editor.insert_str("/agents");
        app.last_key = None;
        assert!(matches!(
            app.handle_key(press(KeyCode::Enter)),
            Action::None
        ));
        assert!(!app.show_agents());
    }

    #[test]
    fn agents_mouse_scroll_is_independent_bounded_and_consumed() {
        use crate::events::SubagentStatus;
        use ratatui::layout::Rect;
        let mut app = app();
        for index in 0..5 {
            app.apply_runtime_at(
                agent_event(
                    &format!("s-{index}"),
                    "Agent",
                    SubagentStatus::Idle,
                    None,
                    1,
                    None,
                    (index, 1, Some(2)),
                ),
                100,
            );
        }
        app.set_agents_viewport(Rect::new(20, 5, 10, 4), 2);
        app.scroll = 7;
        let wheel = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            app.handle_mouse(wheel(MouseEventKind::ScrollDown, 20, 5)),
            Action::Redraw
        ));
        assert_eq!(app.agents_scroll(), 3);
        assert_eq!(app.scroll, 7);
        app.handle_mouse(wheel(MouseEventKind::ScrollUp, 29, 8));
        assert_eq!(app.agents_scroll(), 0);
        app.handle_mouse(wheel(MouseEventKind::ScrollDown, 30, 8));
        assert_eq!(app.agents_scroll(), 0);
    }
}
