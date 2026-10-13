use std::sync::Arc;

use agenthooksprotocol::inputs::{
    ModelErrorInput, ModelRequestBeforeInput, ModelResponseAfterInput,
};
use agenthooksprotocol::{
    EventType, ExecutionEventAttempt, ExecutionEventError, ExecutionEventExecution,
    ExecutionEventExecutionExecuted, ExecutionEventModel, Integer, ModelErrorInputExecution,
    ModelErrorInputExecutionExecuted, ModelRequestBeforeInputParams, SessionStartInputTrigger,
};
use agentkit_core::TurnCancellation;
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, RetryObserver, SessionConfig,
    TurnRequest,
};
use async_trait::async_trait;

use super::Hooks;

/// Model adapter that reports session start and model calls to agent hooks.
#[derive(Clone)]
pub(crate) struct HookedAdapter<A> {
    inner: A,
    hooks: Option<Arc<Hooks>>,
    trigger: SessionStartInputTrigger,
}

impl<A> HookedAdapter<A> {
    pub(crate) fn new(
        inner: A,
        hooks: Option<Arc<Hooks>>,
        trigger: SessionStartInputTrigger,
    ) -> Self {
        Self {
            inner,
            hooks,
            trigger,
        }
    }
}

#[async_trait]
impl<A: ModelAdapter> ModelAdapter for HookedAdapter<A> {
    type Session = HookedSession<A::Session>;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        let session_id = config.session_id.to_string();
        let inner = self.inner.start_session(config).await?;
        if let Some(hooks) = &self.hooks {
            hooks.session_start(&session_id, self.trigger.clone()).await;
        }
        Ok(HookedSession {
            inner,
            hooks: self.hooks.clone(),
            attempts: 0,
        })
    }

    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}

pub(crate) struct HookedSession<S> {
    inner: S,
    hooks: Option<Arc<Hooks>>,
    attempts: u64,
}

#[derive(Clone)]
struct Call {
    hooks: Arc<Hooks>,
    attempt: ExecutionEventAttempt,
    model: ExecutionEventModel,
}

impl Call {
    async fn failed(&self, error: &LoopError) {
        if matches!(error, LoopError::Cancelled) {
            return;
        }
        let class = match error {
            LoopError::ProviderFailure(_) | LoopError::Provider(_) => "provider",
            _ => "adapter",
        };
        let input = ModelErrorInput::new(
            Box::new(self.attempt.clone()),
            Box::new(ExecutionEventError::new(class, error.to_string())),
            ModelErrorInputExecution::Executed(ModelErrorInputExecutionExecuted::new()),
            Box::new(self.model.clone()),
        );
        super::settle(
            EventType::ModelError,
            self.hooks.sdk.model_error(input).await,
        );
    }
}

#[async_trait]
impl<S: ModelSession> ModelSession for HookedSession<S> {
    type Turn = HookedTurn<S::Turn>;

    fn set_retry_observer(&mut self, observer: Option<Arc<dyn RetryObserver>>) {
        self.inner.set_retry_observer(observer);
    }

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        let Some(hooks) = &self.hooks else {
            let inner = self.inner.begin_turn(request, cancellation).await?;
            return Ok(HookedTurn { inner, call: None });
        };
        hooks.flush_turn_events().await;
        self.attempts += 1;
        let Some(number) = Integer::new(self.attempts.into()) else {
            let inner = self.inner.begin_turn(request, cancellation).await?;
            return Ok(HookedTurn { inner, call: None });
        };
        let call = Call {
            hooks: Arc::clone(hooks),
            attempt: ExecutionEventAttempt::new(
                format!("{}/{}", request.turn_id, self.attempts),
                number,
            ),
            model: ExecutionEventModel::new(
                self.inner.model_name().unwrap_or("unknown"),
                self.inner.provider_name().unwrap_or("unknown"),
            ),
        };
        let input = ModelRequestBeforeInput::new(
            Box::new(call.attempt.clone()),
            Vec::new(),
            Box::new(call.model.clone()),
            ModelRequestBeforeInputParams::new(),
        )
        .with_sources()
        .with_items(super::messages(&request.transcript));
        let boundary = hooks.sdk.model_request_before(input);
        let result = match &cancellation {
            Some(cancellation) => boundary.cancel_when(cancellation.cancelled()).await,
            None => boundary.await,
        };
        let reason = match super::settle(EventType::ModelRequestBefore, result) {
            Some(outcome) if !outcome.outcome.is_denied() => None,
            Some(outcome) => Some(super::denial(&outcome.outcome)),
            None => Some("agent hook dispatch failed".into()),
        };
        if let Some(reason) = reason {
            return Err(LoopError::InvalidState(format!(
                "model request denied by agent hook: {reason}"
            )));
        }
        match self.inner.begin_turn(request, cancellation).await {
            Ok(inner) => Ok(HookedTurn {
                inner,
                call: Some(call),
            }),
            Err(error) => {
                call.failed(&error).await;
                Err(error)
            }
        }
    }

    fn model_name(&self) -> Option<&str> {
        self.inner.model_name()
    }

    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}

pub(crate) struct HookedTurn<T> {
    inner: T,
    call: Option<Call>,
}

#[async_trait]
impl<T: ModelTurn> ModelTurn for HookedTurn<T> {
    fn on_cancelled(&mut self) {
        self.inner.on_cancelled();
    }

    async fn next_event(
        &mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        let event = self.inner.next_event(cancellation).await;
        let Some(call) = &self.call else {
            return event;
        };
        match &event {
            Ok(Some(ModelTurnEvent::Finished(result))) => {
                let input = ModelResponseAfterInput::new(
                    Box::new(call.attempt.clone()),
                    Box::new(ExecutionEventExecution::Executed(
                        ExecutionEventExecutionExecuted::new(),
                    )),
                    super::finish_reason(&result.finish_reason),
                    Vec::new(),
                    Box::new(call.model.clone()),
                )
                .with_sources()
                .with_items(super::messages(&result.output_items));
                super::settle(
                    EventType::ModelResponseAfter,
                    call.hooks.sdk.model_response_after(input).await,
                );
            }
            Err(error) => call.failed(error).await,
            _ => {}
        }
        event
    }
}
