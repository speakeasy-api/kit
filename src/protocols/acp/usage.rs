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

// Unknown is distinct from invalid: omission preserves a client's previous value,
// whereas incompatible currencies (or non-finite amounts) must clear it.
#[derive(Clone, Debug, Default)]
pub(super) enum SessionCost {
    #[default]
    Unknown,
    Cleared,
    Known(agentkit_core::CostUsage),
    Invalid,
}

impl SessionCost {
    pub(super) fn add(&mut self, next: Option<agentkit_core::CostUsage>) {
        let Some(next) = next else { return };
        if !next.amount.is_finite() {
            *self = Self::Invalid;
            return;
        }
        match self {
            Self::Unknown | Self::Cleared => *self = Self::Known(next),
            Self::Known(total) if total.currency == next.currency => {
                let amount = total.amount + next.amount;
                if amount.is_finite() {
                    total.amount = amount;
                } else {
                    *self = Self::Invalid;
                }
            }
            _ => *self = Self::Invalid,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct SessionUsage {
    pub context: Option<(u64, u64)>,
    pub cost: SessionCost,
}

impl SessionUsage {
    pub(super) fn update_context(&mut self, usage: &agentkit_core::Usage) {
        let Some(tokens) = &usage.tokens else { return };
        let Some(used) = tokens.input_tokens.checked_add(tokens.output_tokens) else {
            return;
        };
        let size = [
            "context_window",
            "context_window_tokens",
            "model.context_window",
            "model.context_length",
            "openrouter.context_length",
        ]
        .iter()
        .find_map(|key| usage.metadata.get(*key).and_then(|value| value.as_u64()))
        .or(self.context.map(|(_, size)| size));
        if let Some(size) = size {
            self.context = Some((used, size));
        }
    }

    pub(super) fn v2(&self) -> Option<agent_client_protocol_schema::v2::UsageUpdate> {
        use agent_client_protocol_schema::v2::{Cost, UsageUpdate};
        let (used, size) = self.context?;
        let update = UsageUpdate::new(used, size);
        Some(match &self.cost {
            SessionCost::Unknown => update,
            SessionCost::Known(cost) => update.cost(Cost::new(cost.amount, cost.currency.clone())),
            SessionCost::Invalid | SessionCost::Cleared => update.cost(None),
        })
    }
}

#[derive(Clone)]
pub(crate) enum Accounting {
    Main(SessionActivity),
    // Compaction is independent foreground work, not a replaceable main response.
    Compaction(SessionActivity),
    Disabled,
}

#[derive(Clone)]
pub(crate) struct Adapter<A> {
    pub(crate) inner: A,
    pub(crate) accounting: Accounting,
}

pub(crate) struct Session<S> {
    inner: S,
    accounting: Accounting,
}

pub(crate) struct Turn<T> {
    inner: T,
    accounting: Accounting,
    tokens: Option<TokenUsage>,
    cost: Option<agentkit_core::CostUsage>,
}

#[async_trait]
impl<A: ModelAdapter> ModelAdapter for Adapter<A> {
    type Session = Session<A::Session>;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        Ok(Session {
            inner: self.inner.start_session(config).await?,
            accounting: self.accounting.clone(),
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
        if let Accounting::Main(activity) = &self.accounting {
            activity.begin_request();
        }
        Ok(Turn {
            inner: self.inner.begin_turn(request, cancellation).await?,
            accounting: self.accounting.clone(),
            tokens: None,
            cost: None,
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
        let usage = match &event {
            Some(ModelTurnEvent::Usage(usage)) => Some(usage),
            Some(ModelTurnEvent::Finished(result)) => result.usage.as_ref(),
            _ => None,
        };
        if let Some(usage) = usage {
            if let Some(cost) = &usage.cost {
                self.cost = Some(cost.clone());
            }
            if let Accounting::Main(activity) = &self.accounting {
                activity.record_cost(usage);
            }
        }
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
            Some(ModelTurnEvent::ResponseAttemptSuperseded) => {
                self.tokens = None;
                self.cost = None;
            }
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
        if let Accounting::Compaction(activity) = &self.accounting {
            activity.record_compaction_cost(self.cost.take());
        }
        if let Some(tokens) = self.tokens.take() {
            match &self.accounting {
                Accounting::Main(activity) => activity.record_usage(tokens),
                Accounting::Compaction(activity) => activity.record_compaction_usage(tokens),
                Accounting::Disabled => {}
            }
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

    #[derive(Clone)]
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
                accounting: Accounting::Main(activity.clone()),
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
            accounting: Accounting::Main(activity.clone()),
            tokens: None,
            cost: None,
        };
        while let Ok(Some(event)) = turn.next_event(None).await {
            if matches!(event, ModelTurnEvent::Finished(_)) {
                break;
            }
        }
    }

    fn monetary(amount: f64, currency: &str) -> Usage {
        Usage::new(TokenUsage::new(10, 2))
            .with_cost(agentkit_core::CostUsage::new(amount, currency))
            .with_metadata(MetadataMap::from([("context_window".into(), 1000.into())]))
    }

    fn cost_json(activity: &SessionActivity) -> serde_json::Value {
        serde_json::to_value(activity.session_usage().unwrap().v2().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn session_cost_replaces_snapshots_and_survives_foreground_intervals() {
        let activity = SessionActivity::new(|_| Ok(()));
        for expected in [3.0, 6.0] {
            activity
                .execute(
                    ExecutionOrigin::Prompt,
                    async {
                        start(&activity);
                        let mut final_event = finished(None);
                        if let ModelTurnEvent::Finished(result) = &mut final_event {
                            result.usage = Some(monetary(2.0, "USD"));
                        }
                        request(
                            &activity,
                            vec![
                                Ok(ModelTurnEvent::Usage(monetary(0.5, "USD"))),
                                Ok(ModelTurnEvent::Usage(monetary(1.0, "USD"))),
                                Ok(ModelTurnEvent::Usage(Usage::new(TokenUsage::new(11, 2)))),
                                Ok(final_event),
                            ],
                        )
                        .await;
                        request(
                            &activity,
                            vec![Ok(ModelTurnEvent::Usage(monetary(1.0, "USD")))],
                        )
                        .await;
                        assert_eq!(cost_json(&activity)["cost"]["amount"], expected);
                        Ok(())
                    },
                    |_| None,
                )
                .await
                .unwrap();
            assert_eq!(cost_json(&activity)["cost"]["amount"], expected);
        }
    }

    #[tokio::test]
    async fn session_cost_omits_missing_preserves_zero_and_clears_mixed_currencies() {
        let activity = SessionActivity::new(|_| Ok(()));
        activity
            .execute(
                ExecutionOrigin::Prompt,
                async {
                    start(&activity);
                    let mut unknown = monetary(0.0, "USD");
                    unknown.cost = None;
                    request(&activity, vec![Ok(ModelTurnEvent::Usage(unknown))]).await;
                    assert!(cost_json(&activity).get("cost").is_none());
                    request(
                        &activity,
                        vec![Ok(ModelTurnEvent::Usage(monetary(0.0, "USD")))],
                    )
                    .await;
                    assert_eq!(cost_json(&activity)["cost"]["amount"], 0.0);
                    request(
                        &activity,
                        vec![Ok(ModelTurnEvent::Usage(monetary(1.0, "EUR")))],
                    )
                    .await;
                    assert_eq!(cost_json(&activity)["cost"], serde_json::Value::Null);
                    request(
                        &activity,
                        vec![Ok(ModelTurnEvent::Usage(monetary(1.0, "USD")))],
                    )
                    .await;
                    assert_eq!(cost_json(&activity)["cost"], serde_json::Value::Null);
                    Ok(())
                },
                |_| None,
            )
            .await
            .unwrap();
        assert_eq!(cost_json(&activity)["cost"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn session_cost_supersession_clears_provisional_but_keeps_compaction() {
        let activity = SessionActivity::new(|_| Ok(()));
        activity
            .execute(
                ExecutionOrigin::Prompt,
                async {
                    start(&activity);
                    request(
                        &activity,
                        vec![Ok(ModelTurnEvent::Usage(monetary(7.0, "USD")))],
                    )
                    .await;
                    assert_eq!(cost_json(&activity)["cost"]["amount"], 7.0);
                    activity.observe(&AgentEvent::ResponseAttemptSuperseded);
                    assert_eq!(
                        cost_json(&activity).get("cost"),
                        Some(&serde_json::Value::Null)
                    );
                    request(
                        &activity,
                        vec![Ok(ModelTurnEvent::Usage(monetary(4.0, "USD")))],
                    )
                    .await;
                    let mut compaction = Turn {
                        inner: ProviderTurn(
                            vec![Ok(ModelTurnEvent::Usage(monetary(2.0, "USD")))].into(),
                        ),
                        accounting: Accounting::Compaction(activity.clone()),
                        tokens: None,
                        cost: None,
                    };
                    compaction.next_event(None).await.unwrap();
                    drop(compaction);
                    activity.observe(&AgentEvent::ResponseAttemptSuperseded);
                    assert_eq!(cost_json(&activity)["cost"]["amount"], 2.0);
                    request(
                        &activity,
                        vec![Ok(ModelTurnEvent::Usage(monetary(1.0, "USD")))],
                    )
                    .await;
                    Ok(())
                },
                |_| None,
            )
            .await
            .unwrap();
        assert_eq!(cost_json(&activity)["cost"]["amount"], 3.0);
    }

    #[tokio::test]
    async fn session_cost_ignores_background_compaction_and_preserves_partial_failure() {
        let activity = SessionActivity::new(|_| Ok(()));
        activity
            .execute(
                ExecutionOrigin::Prompt,
                async {
                    start(&activity);
                    request(
                        &activity,
                        vec![
                            Ok(ModelTurnEvent::Usage(monetary(2.0, "USD"))),
                            Err(LoopError::Provider("failed".into())),
                        ],
                    )
                    .await;
                    let mut background = Turn {
                        inner: ProviderTurn(
                            vec![Ok(ModelTurnEvent::Usage(monetary(8.0, "EUR")))].into(),
                        ),
                        accounting: Accounting::Disabled,
                        tokens: None,
                        cost: None,
                    };
                    background.next_event(None).await.unwrap();
                    drop(background);
                    Ok(())
                },
                |_| None,
            )
            .await
            .unwrap();
        assert_eq!(cost_json(&activity)["cost"]["amount"], 2.0);
    }

    #[test]
    fn session_cost_rejects_nonfinite_amounts_and_overflow() {
        for amounts in [[f64::NAN, 0.0], [f64::INFINITY, 0.0], [f64::MAX, f64::MAX]] {
            let mut usage = SessionUsage {
                context: Some((1, 100)),
                ..Default::default()
            };
            for amount in amounts {
                usage
                    .cost
                    .add(Some(agentkit_core::CostUsage::new(amount, "USD")));
            }
            let value = serde_json::to_value(usage.v2().unwrap()).unwrap();
            assert_eq!(value.get("cost"), Some(&serde_json::Value::Null));
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
    async fn end_turn_usage_includes_real_compaction_without_committing_superseded_main() {
        use agentkit_compaction::{CompactionReason, Compactor};
        use agentkit_core::{Item, ItemKind};
        for superseded in [false, true] {
            let activity = SessionActivity::new(|_| Ok(()));
            start(&activity);
            request(&activity, vec![Ok(finished(Some(TokenUsage::new(100, 2))))]).await;
            let compactor = crate::compaction::strategy(
                Adapter {
                    inner: Provider,
                    accounting: Accounting::Compaction(activity.clone()),
                },
                agentkit_loop::TelemetryConfig::default(),
                "summary",
                "usage",
                None,
                crate::compaction::policy::CompactionConfig::default(),
            );
            let history = vec![
                Item::text(ItemKind::User, "old question ".repeat(10_000)),
                Item::text(ItemKind::Assistant, "old answer ".repeat(10_000)),
                Item::text(ItemKind::User, "current request"),
            ];
            compactor
                .compact(&history, CompactionReason::Manual, None)
                .await
                .unwrap();
            if superseded {
                activity.observe(&AgentEvent::ResponseAttemptSuperseded);
            }
            request(&activity, vec![Ok(finished(Some(TokenUsage::new(20, 3))))]).await;
            activity.settle(None, None).unwrap();
            let expected = if superseded {
                TokenUsage::new(30, 8)
            } else {
                TokenUsage::new(130, 10)
            };
            assert_eq!(activity.completed_usage().unwrap(), Some(expected));
        }
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
            accounting: Accounting::Main(activity.clone()),
            tokens: None,
            cost: None,
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
