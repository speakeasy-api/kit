//! Scriptable, introspectable mock [`ModelAdapter`].
//!
//! The mock is intentionally thin: tests script a sequence of
//! [`ModelTurnEvent`]s per turn and the adapter replays them verbatim.
//! There is no specialised "tool call" or "text" enum — the existing
//! agentkit event surface is the script alphabet.
//!
//! What the mock *does* know about is the *model's view*: every
//! [`TurnRequest`] handed to the model is recorded as an [`ObservedTurn`]
//! so tests can assert which transcript items and tool specs the loop
//! advertised. Assertions about which tools actually got *invoked* belong
//! to the tools themselves (use [`crate::mock_tool::RecordingTool`] for
//! that).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use agentkit_core::{
    FinishReason, Item, ItemKind, MetadataMap, Part, TextPart, ToolCallPart, TurnCancellation,
};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult,
    SessionConfig, TurnRequest,
};
use async_trait::async_trait;

/// One full scripted model turn — a sequence of [`ModelTurnEvent`]s the
/// mock will yield in order. The script must include exactly one
/// [`ModelTurnEvent::Finished`] at the end (or a convenience constructor
/// that wraps one for you).
#[derive(Clone, Debug)]
pub struct TurnScript {
    pub events: Vec<ModelTurnEvent>,
}

impl TurnScript {
    /// Build a script from any iterable of [`ModelTurnEvent`]s.
    pub fn new(events: impl IntoIterator<Item = ModelTurnEvent>) -> Self {
        Self {
            events: events.into_iter().collect(),
        }
    }

    /// Convenience: a turn that emits a single assistant text item and
    /// finishes naturally with [`FinishReason::Completed`].
    pub fn text(message: impl Into<String>) -> Self {
        let text = message.into();
        let item = Item::new(
            ItemKind::Assistant,
            vec![Part::Text(TextPart::new(text.clone()))],
        );
        Self::new([ModelTurnEvent::Finished(ModelTurnResult {
            model: None,
            response_id: None,
            finish_reason: FinishReason::Completed,
            output_items: vec![item],
            usage: None,
            metadata: MetadataMap::new(),
        })])
    }

    /// Convenience: a turn that asks the loop to invoke a single tool and
    /// finishes with [`FinishReason::ToolCall`].
    pub fn tool_call(call: ToolCallPart) -> Self {
        let assistant = Item::new(ItemKind::Assistant, vec![Part::ToolCall(call.clone())]);
        Self::new([
            ModelTurnEvent::ToolCall(call),
            ModelTurnEvent::Finished(ModelTurnResult {
                model: None,
                response_id: None,
                finish_reason: FinishReason::ToolCall,
                output_items: vec![assistant],
                usage: None,
                metadata: MetadataMap::new(),
            }),
        ])
    }
}

/// Snapshot of one observed call to [`ModelSession::begin_turn`]: the
/// transcript and tool catalog the loop handed the model.
#[derive(Clone, Debug)]
pub struct ObservedTurn {
    pub session_id: String,
    pub transcript: Vec<Item>,
    pub tool_names: Vec<String>,
}

#[derive(Default)]
struct MockState {
    scripts: Mutex<VecDeque<TurnScript>>,
    observed: Mutex<Vec<ObservedTurn>>,
}

/// Mock model adapter. Cheap to clone — clones share state.
#[derive(Clone, Default)]
pub struct MockAdapter {
    state: Arc<MockState>,
}

impl MockAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a script for the next pending turn.
    pub fn enqueue(&self, script: TurnScript) -> &Self {
        self.state.scripts.lock().unwrap().push_back(script);
        self
    }

    /// Convenience: enqueue many scripts in one call.
    pub fn enqueue_many<I: IntoIterator<Item = TurnScript>>(&self, scripts: I) -> &Self {
        let mut queue = self.state.scripts.lock().unwrap();
        for script in scripts {
            queue.push_back(script);
        }
        self
    }

    /// All [`TurnRequest`]s observed so far, in order.
    pub fn observed(&self) -> Vec<ObservedTurn> {
        self.state.observed.lock().unwrap().clone()
    }

    /// Number of unconsumed scripts in the queue.
    pub fn pending_scripts(&self) -> usize {
        self.state.scripts.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelAdapter for MockAdapter {
    type Session = MockSession;

    async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
        Ok(MockSession {
            state: Arc::clone(&self.state),
        })
    }
}

/// Session counterpart to [`MockAdapter`].
pub struct MockSession {
    state: Arc<MockState>,
}

#[async_trait]
impl ModelSession for MockSession {
    type Turn = MockTurn;

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        _cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        self.state.observed.lock().unwrap().push(ObservedTurn {
            session_id: request.session_id.0.clone(),
            transcript: request.transcript.clone(),
            tool_names: request
                .available_tools
                .iter()
                .map(|spec| spec.name.0.clone())
                .collect(),
        });

        let script = self
            .state
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| {
                LoopError::InvalidState(
                    "MockAdapter received begin_turn with no scripted turn enqueued".into(),
                )
            })?;
        Ok(MockTurn::new(script))
    }
}

/// Streaming turn produced by [`MockSession`].
pub struct MockTurn {
    queue: VecDeque<ModelTurnEvent>,
}

impl MockTurn {
    fn new(script: TurnScript) -> Self {
        Self {
            queue: script.events.into(),
        }
    }
}

#[async_trait]
impl ModelTurn for MockTurn {
    async fn next_event(
        &mut self,
        _cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        Ok(self.queue.pop_front())
    }
}
