//! Account once per model request, not per cumulative stream snapshot or logical turn.
//! A logical turn can contain several tool rounds; TurnFinished only has the last
//! request's usage. Providers normalize cache/reasoning counts as breakdowns of
//! input/output, so adding those breakdowns again would double count tokens.
use std::sync::Arc;

use agentkit_core::{TokenUsage, TurnCancellation};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, RetryObserver, SessionConfig,
    TurnRequest,
};
use async_trait::async_trait;

use super::activity::SessionActivity;

pub(super) fn accumulate(total: &mut Option<TokenUsage>, next: TokenUsage) {
    let Some(total) = total else {
        *total = Some(next);
        return;
    };
    fn add(left: Option<u64>, right: Option<u64>) -> Option<u64> {
        match (left, right) {
            (Some(left), Some(right)) => Some(left.saturating_add(right)),
            (left, right) => left.or(right),
        }
    }
    total.input_tokens = total.input_tokens.saturating_add(next.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(next.output_tokens);
    total.reasoning_tokens = add(total.reasoning_tokens, next.reasoning_tokens);
    total.cached_input_tokens = add(total.cached_input_tokens, next.cached_input_tokens);
    total.cache_write_input_tokens = add(
        total.cache_write_input_tokens,
        next.cache_write_input_tokens,
    );
}

pub(super) fn v1(tokens: &TokenUsage) -> agent_client_protocol_schema::v1::Usage {
    agent_client_protocol_schema::v1::Usage::new(
        tokens.input_tokens.saturating_add(tokens.output_tokens),
        tokens.input_tokens,
        tokens.output_tokens,
    )
    .thought_tokens(tokens.reasoning_tokens)
    .cached_read_tokens(tokens.cached_input_tokens)
    .cached_write_tokens(tokens.cache_write_input_tokens)
}

pub(super) fn v2(tokens: &TokenUsage) -> agent_client_protocol_schema::v2::Usage {
    agent_client_protocol_schema::v2::Usage::new(
        tokens.input_tokens.saturating_add(tokens.output_tokens),
        tokens.input_tokens,
        tokens.output_tokens,
    )
    .thought_tokens(tokens.reasoning_tokens)
    .cached_read_tokens(tokens.cached_input_tokens)
    .cached_write_tokens(tokens.cache_write_input_tokens)
}

pub(crate) struct Adapter<A> {
    pub(crate) inner: A,
    pub(crate) activity: SessionActivity,
}

pub(crate) struct Session<S> {
    inner: S,
    activity: SessionActivity,
}

pub(crate) struct Turn<T> {
    inner: T,
    activity: SessionActivity,
    tokens: Option<TokenUsage>,
}

#[async_trait]
impl<A: ModelAdapter> ModelAdapter for Adapter<A> {
    type Session = Session<A::Session>;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        Ok(Session {
            inner: self.inner.start_session(config).await?,
            activity: self.activity.clone(),
        })
    }

    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}

#[async_trait]
impl<S: ModelSession> ModelSession for Session<S> {
    type Turn = Turn<S::Turn>;

    fn set_retry_observer(&mut self, observer: Option<Arc<dyn RetryObserver>>) {
        self.inner.set_retry_observer(observer);
    }

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        self.activity.begin_request();
        Ok(Turn {
            inner: self.inner.begin_turn(request, cancellation).await?,
            activity: self.activity.clone(),
            tokens: None,
        })
    }

    fn model_name(&self) -> Option<&str> {
        self.inner.model_name()
    }

    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}

#[async_trait]
impl<T: ModelTurn> ModelTurn for Turn<T> {
    fn on_cancelled(&mut self) {
        self.inner.on_cancelled();
    }

    async fn next_event(
        &mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        let event = self.inner.next_event(cancellation).await?;
        match &event {
            Some(ModelTurnEvent::Usage(usage)) => {
                // Snapshots replace, rather than add to, the current request.
                // A cost-only snapshot must not erase observed token counts.
                if let Some(tokens) = &usage.tokens {
                    self.tokens = Some(tokens.clone());
                }
            }
            Some(ModelTurnEvent::Finished(result)) => {
                if let Some(tokens) = result
                    .usage
                    .as_ref()
                    .and_then(|usage| usage.tokens.as_ref())
                {
                    self.tokens = Some(tokens.clone());
                }
            }
            Some(ModelTurnEvent::ResponseAttemptSuperseded) => self.tokens = None,
            _ => {}
        }
        Ok(event)
    }
}

impl<T> Drop for Turn<T> {
    fn drop(&mut self) {
        // The loop drops the request before foreground settlement, including
        // cancellation and errors. Preserve real partial usage in those cases.
        // This concrete owner method invokes no client code, awaits or callbacks.
        if let Some(tokens) = self.tokens.take() {
            self.activity.record_usage(tokens);
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::super::activity::ExecutionOrigin;
    use super::*;
    use agentkit_core::{FinishReason, MetadataMap, SessionId, TurnId, Usage};
    use agentkit_loop::{AgentEvent, ModelTurnResult};
    use std::collections::VecDeque;

    struct ProviderTurn(VecDeque<Result<ModelTurnEvent, LoopError>>);

    #[async_trait]
    impl ModelTurn for ProviderTurn {
        async fn next_event(
            &mut self,
            _: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            self.0.pop_front().transpose()
        }
    }

    struct Provider;
    #[async_trait]
    impl ModelAdapter for Provider {
        type Session = Self;
        async fn start_session(&self, _: SessionConfig) -> Result<Self, LoopError> {
            Ok(Self)
        }
    }
    #[async_trait]
    impl ModelSession for Provider {
        type Turn = ProviderTurn;
        async fn begin_turn(
            &mut self,
            _: TurnRequest,
            _: Option<TurnCancellation>,
        ) -> Result<Self::Turn, LoopError> {
            let mut final_event = finished(Some(TokenUsage::new(10, 5)));
            if let ModelTurnEvent::Finished(result) = &mut final_event {
                result.output_items.push(agentkit_core::Item::text(
                    agentkit_core::ItemKind::Assistant,
                    "done",
                ));
            }
            Ok(ProviderTurn(
                vec![
                    Ok(ModelTurnEvent::Usage(Usage::new(TokenUsage::new(10, 2)))),
                    Ok(final_event),
                ]
                .into(),
            ))
        }
    }

    #[tokio::test]
    async fn end_turn_usage_real_loop_drops_requests_before_settlement() {
        let activity = SessionActivity::new(|_| Ok(()));
        let mut driver = agentkit_loop::Agent::builder()
            .model(Adapter {
                inner: Provider,
                activity: activity.clone(),
            })
            .observer(activity.clone())
            .build()
            .unwrap()
            .start(SessionConfig::new(SessionId::new("usage-loop")))
            .await
            .unwrap();
        activity
            .execute(
                ExecutionOrigin::Prompt,
                async {
                    for _ in 0..2 {
                        driver
                            .submit_input(vec![agentkit_core::Item::text(
                                agentkit_core::ItemKind::User,
                                "go",
                            )])
                            .unwrap();
                        assert!(matches!(
                            driver.next().await.unwrap(),
                            agentkit_loop::LoopStep::Finished(_)
                        ));
                    }
                    Ok(())
                },
                |_| None,
            )
            .await
            .unwrap();
        assert_eq!(
            activity.completed_usage().unwrap(),
            Some(TokenUsage::new(20, 10))
        );
    }

    fn finished(tokens: Option<TokenUsage>) -> ModelTurnEvent {
        ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![],
            usage: tokens.map(Usage::new),
            metadata: MetadataMap::new(),
            model: None,
            response_id: None,
        })
    }

    fn start(activity: &SessionActivity) {
        activity.observe(&AgentEvent::TurnStarted {
            session_id: SessionId::new("usage"),
            turn_id: TurnId::new("logical-turn"),
        });
    }

    async fn request(activity: &SessionActivity, events: Vec<Result<ModelTurnEvent, LoopError>>) {
        activity.begin_request();
        let mut turn = Turn {
            inner: ProviderTurn(events.into()),
            activity: activity.clone(),
            tokens: None,
        };
        while let Ok(Some(event)) = turn.next_event(None).await {
            if matches!(event, ModelTurnEvent::Finished(_)) {
                break;
            }
        }
    }

    #[tokio::test]
    async fn end_turn_usage_aggregates_requests_not_snapshots_or_breakdowns() {
        let activity = SessionActivity::new(|_| Ok(()));
        activity
            .execute(
                ExecutionOrigin::Prompt,
                async {
                    start(&activity);
                    request(
                        &activity,
                        vec![
                            Ok(ModelTurnEvent::Usage(Usage::new(TokenUsage::new(100, 1)))),
                            Ok(ModelTurnEvent::Usage(Usage::new(TokenUsage::new(100, 3)))),
                            Ok(finished(Some(
                                TokenUsage::new(100, 5)
                                    .with_reasoning_tokens(2)
                                    .with_cached_input_tokens(60)
                                    .with_cache_write_input_tokens(10),
                            ))),
                        ],
                    )
                    .await;
                    // Same logical turn, another model request after a tool round.
                    request(
                        &activity,
                        vec![Ok(finished(Some(TokenUsage::new(200, 10))))],
                    )
                    .await;
                    // Another logical turn in the same foreground interval.
                    activity.observe(&AgentEvent::TurnFinished(agentkit_loop::TurnResult {
                        turn_id: TurnId::new("logical-turn"),
                        finish_reason: FinishReason::Completed,
                        items: vec![],
                        usage: Some(Usage::new(TokenUsage::new(200, 10))),
                        metadata: MetadataMap::new(),
                    }));
                    start(&activity);
                    request(&activity, vec![Ok(finished(Some(TokenUsage::new(50, 2))))]).await;
                    Ok(())
                },
                |_| None,
            )
            .await
            .unwrap();
        let tokens = activity.completed_usage().unwrap().unwrap();
        assert_eq!(
            tokens,
            TokenUsage::new(350, 17)
                .with_reasoning_tokens(2)
                .with_cached_input_tokens(60)
                .with_cache_write_input_tokens(10)
        );
        let response = agent_client_protocol_schema::v1::PromptResponse::new(
            agent_client_protocol_schema::v1::StopReason::EndTurn,
        )
        .usage(v1(&tokens));
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["usage"]["totalTokens"], 367);
        assert_eq!(json["usage"]["thoughtTokens"], 2);
        assert_eq!(json["usage"]["cachedReadTokens"], 60);
        assert_eq!(json["usage"]["cachedWriteTokens"], 10);
        assert_eq!(v2(&tokens).total_tokens, 367);
        activity
            .execute(ExecutionOrigin::Prompt, async { Ok(()) }, |_| None)
            .await
            .unwrap();
        assert_eq!(activity.completed_usage().unwrap(), None);
    }

    #[tokio::test]
    async fn end_turn_usage_preserves_partial_cancelled_and_error_requests() {
        for error in [
            LoopError::Cancelled,
            LoopError::Provider("provider failed".into()),
        ] {
            let activity = SessionActivity::new(|_| Ok(()));
            start(&activity);
            request(
                &activity,
                vec![
                    Ok(ModelTurnEvent::Usage(Usage::new(TokenUsage::new(8, 2)))),
                    Err(error),
                ],
            )
            .await;
            activity
                .settle(Some(FinishReason::Cancelled), None)
                .unwrap();
            assert_eq!(
                activity.completed_usage().unwrap(),
                Some(TokenUsage::new(8, 2))
            );
        }
    }

    #[tokio::test]
    async fn end_turn_usage_discards_superseded_attempt_and_omits_unknown_counts() {
        let activity = SessionActivity::new(|_| Ok(()));
        start(&activity);
        request(
            &activity,
            vec![
                Ok(ModelTurnEvent::Usage(Usage::new(TokenUsage::new(900, 900)))),
                Ok(ModelTurnEvent::ResponseAttemptSuperseded),
                Ok(finished(None)),
            ],
        )
        .await;
        activity.settle(None, None).unwrap();
        assert_eq!(activity.completed_usage().unwrap(), None);
        start(&activity);
        request(&activity, vec![Ok(finished(Some(TokenUsage::new(0, 0))))]).await;
        activity.settle(None, None).unwrap();
        let tokens = activity.completed_usage().unwrap().unwrap();
        assert_eq!(tokens, TokenUsage::new(0, 0));
        assert_eq!(v1(&tokens).thought_tokens, None);
        assert_eq!(v2(&tokens).cached_read_tokens, None);
    }

    #[tokio::test]
    async fn end_turn_usage_loop_recovery_supersedes_only_last_request() {
        let activity = SessionActivity::new(|_| Ok(()));
        start(&activity);
        request(&activity, vec![Ok(finished(Some(TokenUsage::new(10, 2))))]).await;
        request(
            &activity,
            vec![
                Ok(ModelTurnEvent::Usage(Usage::new(TokenUsage::new(100, 0)))),
                Err(LoopError::Provider("context length".into())),
            ],
        )
        .await;
        // Loop recovery signals supersession after it drops the rejected request.
        activity.observe(&AgentEvent::ResponseAttemptSuperseded);
        request(&activity, vec![Ok(finished(Some(TokenUsage::new(20, 3))))]).await;
        activity.settle(None, None).unwrap();
        assert_eq!(
            activity.completed_usage().unwrap(),
            Some(TokenUsage::new(30, 5))
        );
    }

    #[tokio::test]
    async fn end_turn_usage_survives_stream_drop_without_terminal_event() {
        let activity = SessionActivity::new(|_| Ok(()));
        start(&activity);
        activity.begin_request();
        let mut turn = Turn {
            inner: ProviderTurn(
                vec![Ok(ModelTurnEvent::Usage(Usage::new(TokenUsage::new(7, 1))))].into(),
            ),
            activity: activity.clone(),
            tokens: None,
        };
        turn.next_event(None).await.unwrap();
        drop(turn);
        activity.settle(None, None).unwrap();
        assert_eq!(
            activity.completed_usage().unwrap(),
            Some(TokenUsage::new(7, 1))
        );
    }
}
