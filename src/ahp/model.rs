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
            hooks
                .session_start(&session_id, self.trigger.clone(), inner.model_name())
                .await;
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
    session: Box<agenthooksprotocol::Session>,
    turn: String,
    started: std::time::Instant,
    missing_model_fields: Vec<&'static str>,
}

impl Call {
    async fn failed(&self, error: &LoopError, usage: Option<&agentkit_core::Usage>) {
        if matches!(error, LoopError::Cancelled) {
            return;
        }
        let class = match error {
            LoopError::ProviderFailure(_) | LoopError::Provider(_) => "provider",
            _ => "adapter",
        };
        let mut input = ModelErrorInput::new(
            Box::new(self.attempt.clone()),
            Box::new(ExecutionEventError::new(class, error.to_string())),
            ModelErrorInputExecution::Executed(ModelErrorInputExecutionExecuted::new()),
            Box::new(self.model.clone()),
        );
        if !self.missing_model_fields.is_empty() {
            input.gaps = Some(
                self.missing_model_fields
                    .iter()
                    .map(|path| {
                        agenthooksprotocol::ModelErrorInputGapsItem::new(
                            *path,
                            "adapter did not expose this required field; unknown is a placeholder",
                        )
                    })
                    .collect(),
            );
        }
        input.session = Some(self.session.clone());
        input.turn = Some(agenthooksprotocol::ModelErrorInputTurn::new(&self.turn));
        if let LoopError::ProviderFailure(failure) = error {
            // ProviderFailure is explicitly sanitized and payload-free in agentkit:
            // retain real transport-attempt accounting, not our synthesized index.
            let mut extensions = agenthooksprotocol::Extensions::new();
            if let Ok(failure) = serde_json::to_value(failure) {
                extensions
                    .additional_properties
                    .insert("dev.kit.retry".into(), failure);
            }
            input.extensions = Some(Box::new(extensions));
            if let Some(status) = failure.upstream.http_status
                && let Some(status) = Integer::from_i64(i64::from(status))
            {
                input.error.status = agenthooksprotocol::Presence::Present(
                    agenthooksprotocol::ExecutionEventErrorStatus::Integer(status),
                );
            }
            if failure.upstream.code != agentkit_loop::UpstreamErrorKind::Unknown
                && let Ok(serde_json::Value::String(code)) =
                    serde_json::to_value(failure.upstream.code)
            {
                input.error.code = agenthooksprotocol::Presence::Present(code);
            }
        }
        input.usage = usage.map(super::usage::attempt).map(Box::new);
        input.latency_ms =
            serde_json::Number::from_f64(self.started.elapsed().as_secs_f64() * 1000.0);
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
            return Ok(HookedTurn {
                inner,
                call: None,
                latest_usage: None,
            });
        };
        hooks
            .drain_turn_events(&request.session_id.to_string())
            .await;
        self.attempts += 1;
        let Some(number) = Integer::new(self.attempts.into()) else {
            let inner = self.inner.begin_turn(request, cancellation).await?;
            return Ok(HookedTurn {
                inner,
                call: None,
                latest_usage: None,
            });
        };
        let mut call = Call {
            hooks: Arc::clone(hooks),
            session: hooks.session(&request.session_id.to_string()),
            turn: request.turn_id.to_string(),
            started: std::time::Instant::now(),
            missing_model_fields: [
                ("model.id", self.inner.model_name()),
                ("model.provider", self.inner.provider_name()),
            ]
            .into_iter()
            .filter_map(|(field, value)| value.is_none().then_some(field))
            .collect(),
            attempt: ExecutionEventAttempt::new(
                format!("{}/{}", request.turn_id, self.attempts),
                number,
            )
            .with_synthesized(true),
            model: ExecutionEventModel::new(
                self.inner.model_name().unwrap_or("unknown"),
                self.inner.provider_name().unwrap_or("unknown"),
            ),
        };
        let mut input = ModelRequestBeforeInput::new(
            Box::new(call.attempt.clone()),
            Vec::new(),
            Box::new(call.model.clone()),
            ModelRequestBeforeInputParams::new(),
        );
        // Host request settings, not an invented provider wire request. Do not
        // forward arbitrary metadata (which can include authentication bindings).
        let tools = request
            .available_tools
            .iter()
            .map(|tool| {
                super::object([
                    ("name", tool.name.to_string().into()),
                    ("description", tool.description.clone().into()),
                    ("inputSchema", tool.input_schema.clone()),
                ])
            })
            .collect();
        input
            .params
            .additional_properties
            .insert("dev.kit.tools".into(), serde_json::Value::Array(tools));
        if let Some(cache) = &request.cache
            && let (Ok(mode), Ok(retention), Ok(strategy)) = (
                serde_json::to_value(cache.mode),
                serde_json::to_value(cache.retention),
                serde_json::to_value(&cache.strategy),
            )
        {
            input.params.additional_properties.insert(
                "dev.kit.cache".into(),
                super::object([
                    ("mode", mode),
                    ("retention", retention),
                    ("strategy", strategy),
                ]),
            );
        }
        if !call.missing_model_fields.is_empty() {
            input.gaps = Some(
                call.missing_model_fields
                    .iter()
                    .map(|path| {
                        agenthooksprotocol::ModelRequestBeforeInputGapsItem::new(
                            *path,
                            "adapter did not expose this required field; unknown is a placeholder",
                        )
                    })
                    .collect(),
            );
        }
        input.session = Some(call.session.clone());
        input.turn = Some(agenthooksprotocol::ModelRequestBeforeInputTurn::new(
            &call.turn,
        ));
        let input = input
            .with_sources()
            .with_items(super::messages(&request.transcript));
        let boundary = hooks.sdk.model_request_before(input);
        let result = match &cancellation {
            Some(cancellation) => boundary.cancel_when(cancellation.cancelled()).await,
            None => boundary.await,
        };
        // SDK cancellation is a dispatch error, not a policy denial. Preserve the
        // loop's cancelled terminal outcome instead of converting it to InvalidState.
        if cancellation
            .as_ref()
            .is_some_and(TurnCancellation::is_cancelled)
        {
            return Err(LoopError::Cancelled);
        }
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
        call.started = std::time::Instant::now();
        match self.inner.begin_turn(request, cancellation).await {
            Ok(inner) => Ok(HookedTurn {
                inner,
                call: Some(call),
                latest_usage: None,
            }),
            Err(error) => {
                call.failed(&error, None).await;
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
    latest_usage: Option<agentkit_core::Usage>,
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
            Ok(Some(ModelTurnEvent::Usage(usage))) => self.latest_usage = Some(usage.clone()),
            Ok(Some(ModelTurnEvent::ResponseAttemptSuperseded)) => self.latest_usage = None,
            Ok(Some(ModelTurnEvent::Finished(result))) => {
                let reported =
                    super::usage::reported(result.usage.as_ref(), self.latest_usage.as_ref());
                let mut input = ModelResponseAfterInput::new(
                    Box::new(call.attempt.clone()),
                    Box::new(ExecutionEventExecution::Executed(
                        ExecutionEventExecutionExecuted::new(),
                    )),
                    super::finish_reason(&result.finish_reason),
                    Vec::new(),
                    Box::new(call.model.clone()),
                );
                let gaps: Vec<_> = call
                    .missing_model_fields
                    .iter()
                    .filter(|path| **path != "model.id" || result.model.is_none())
                    .map(|path| {
                        agenthooksprotocol::ModelResponseAfterInputGapsItem::new(
                            *path,
                            "adapter did not expose this required field; unknown is a placeholder",
                        )
                    })
                    .collect();
                if !gaps.is_empty() {
                    input.gaps = Some(gaps);
                }
                input.session = Some(call.session.clone());
                input.turn = Some(agenthooksprotocol::ModelResponseAfterInputTurn::new(
                    &call.turn,
                ));
                input.usage = reported.as_ref().map(super::usage::attempt).map(Box::new);
                input.latency_ms =
                    serde_json::Number::from_f64(call.started.elapsed().as_secs_f64() * 1000.0);
                let mut extensions = agenthooksprotocol::Extensions::new();
                if let Some(response_id) = &result.response_id {
                    extensions.additional_properties.insert(
                        "dev.kit.response".into(),
                        super::object([("id", response_id.clone().into())]),
                    );
                }
                if let Some(reasoning) = reported
                    .as_ref()
                    .and_then(|usage| usage.tokens.as_ref())
                    .and_then(|tokens| tokens.reasoning_tokens)
                {
                    extensions.additional_properties.insert(
                        "dev.kit.usage".into(),
                        super::object([("reasoningTokens", reasoning.into())]),
                    );
                }
                if !extensions.additional_properties.is_empty() {
                    input.extensions = Some(Box::new(extensions));
                }
                if let Some(model) = &result.model {
                    input.model.id = model.clone();
                }
                let input = input
                    .with_sources()
                    .with_items(super::messages(&result.output_items));
                super::settle(
                    EventType::ModelResponseAfter,
                    call.hooks.sdk.model_response_after(input).await,
                );
            }
            Err(error) => call.failed(error, self.latest_usage.as_ref()).await,
            _ => {}
        }
        event
    }
}
