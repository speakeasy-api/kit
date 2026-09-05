//! Ephemeral, actor-owned confirmation. Never infer a completion from runtime telemetry.
use std::sync::atomic::{AtomicU64, Ordering};

use agent_client_protocol::Error;
use agentkit_core::{FinishReason, Item, ItemKind, MessageId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::provider::{ModelGroup, ModelSelection};

pub(crate) const META: &str = "kit.model_switch";
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Confirmation {
    pub token: u64,
    pub action: Decision,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Decision {
    Continue,
    Compact,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Warning {
    pub token: u64,
    pub guarded_tokens: String,
    pub target_window: u64,
}

struct Pending {
    token: u64,
    current: ModelSelection,
    target: ModelSelection,
    transcript: Vec<Item>,
    cancellation_generation: u64,
}

#[derive(Default)]
pub(super) struct Guard {
    pending: Option<Pending>,
}

/// The margin applies to the latest request occupancy, not lifetime usage. Cached
/// tokens are already part of input_tokens/context_used and must not be added again.
fn guarded_tokens(transcript: &[Item]) -> Option<u128> {
    crate::compaction::latest_context_tokens(transcript)
        .map(|tokens| (u128::from(tokens) * 120).div_ceil(100))
}

pub(super) fn error(message: &str) -> Error {
    agent_client_protocol::util::internal_error(message)
}

impl Guard {
    pub(super) fn check(
        &mut self,
        current: (&ModelSelection, u64),
        target: ModelSelection,
        catalog: &[ModelGroup],
        transcript: Vec<Item>,
        confirmation: Option<&Value>,
    ) -> Result<Decision, Error> {
        let (current, cancellation_generation) = current;
        let pending = self.pending.take();
        if let Some(value) = confirmation {
            let confirmation: Confirmation = serde_json::from_value(value.clone())
                .map_err(|_| error("invalid model-switch confirmation"))?;
            let valid = pending.is_some_and(|pending| {
                pending.token == confirmation.token
                    && pending.cancellation_generation == cancellation_generation
                    && pending.current == *current
                    && pending.target == target
                    && pending.transcript == transcript
            });
            return if valid {
                Ok(confirmation.action)
            } else {
                Err(error(
                    "model-switch confirmation is stale; select the model again",
                ))
            };
        }
        let group = catalog
            .iter()
            .find(|group| group.provider == target.provider && group.models.contains(&target.model))
            .ok_or_else(|| error("model is not in the advertised catalog"))?;
        let window = group
            .context_windows
            .get(&target.model)
            .copied()
            .filter(|size| *size > 0);
        // Unknown occupancy/window is explicitly unchecked, not an invented limit
        // or a mandatory compaction that could prevent selecting a custom model.
        let Some((tokens, window)) = guarded_tokens(&transcript).zip(window) else {
            return Ok(Decision::Continue);
        };
        if current == &target || tokens * 100 < u128::from(window) * 80 {
            return Ok(Decision::Continue);
        }
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        self.pending = Some(Pending {
            token,
            current: current.clone(),
            target,
            transcript,
            cancellation_generation,
        });
        Err(error("target model context is at least 80% occupied after a 20% tokenizer margin; continue explicitly, compact with the current model, or cancel")
            .data(json!({ META: Warning { token, guarded_tokens: tokens.to_string(), target_window: window } })))
    }
}

/// A fresh marker, never an uncorrelated `CompactionFinished` notification.
pub(super) fn compact_marker() -> Item {
    Item::text(ItemKind::User, "/compact").with_id(MessageId::new(format!(
        "kit-model-switch-{}",
        NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
    )))
}

pub(super) fn compaction_completed(
    reason: &FinishReason,
    marker: &Option<MessageId>,
    transcript: &[Item],
    cancelled: bool,
) -> bool {
    !cancelled
        && *reason == FinishReason::Completed
        && marker.is_some()
        && !transcript.iter().any(|item| &item.id == marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderKind;
    use agentkit_core::{MetadataMap, TokenUsage, Usage};

    fn selection(model: &str) -> ModelSelection {
        ModelSelection {
            provider: ProviderKind::OpenRouter,
            model: model.into(),
        }
    }
    fn catalog(window: Option<u64>) -> Vec<ModelGroup> {
        vec![ModelGroup {
            provider: ProviderKind::OpenRouter,
            models: vec!["target".into()],
            context_windows: window
                .map(|window| ("target".into(), window))
                .into_iter()
                .collect(),
        }]
    }
    fn measured(tokens: u64) -> Item {
        Item::text(ItemKind::Assistant, "response")
            .with_usage(Usage::new(TokenUsage::new(tokens, 0)))
    }
    fn check(
        guard: &mut Guard,
        transcript: Vec<Item>,
        window: Option<u64>,
        confirmation: Option<&Value>,
    ) -> Result<Decision, Error> {
        guard.check(
            (&selection("original"), 0),
            selection("target"),
            &catalog(window),
            transcript,
            confirmation,
        )
    }
    fn warn(guard: &mut Guard) -> Warning {
        let error = check(guard, vec![measured(100)], Some(150), None).unwrap_err();
        serde_json::from_value(error.data.unwrap()[META].clone()).unwrap()
    }

    #[test]
    fn model_switch_exact_threshold_and_below_include_rounded_margin() {
        let mut guard = Guard::default();
        assert_eq!(
            check(&mut guard, vec![measured(99)], Some(150), None).unwrap(),
            Decision::Continue
        );
        let warning = warn(&mut guard);
        assert_eq!(warning.guarded_tokens, "120"); // ceil(100 * 1.20) == 0.80 * 150
        assert_eq!(warning.target_window, 150);
        assert_eq!(guarded_tokens(&[measured(101)]), Some(122));
        assert!(check(&mut guard, vec![measured(100)], Some(151), None).is_ok());
        assert!(check(&mut guard, vec![measured(101)], Some(151), None).is_err());
        assert!(check(&mut guard, vec![measured(u64::MAX)], Some(u64::MAX), None).is_err());
    }

    #[test]
    fn model_switch_latest_occupancy_not_lifetime_or_double_cached_tokens() {
        let last = Item::text(ItemKind::Assistant, "response").with_usage(Usage::new(
            TokenUsage::new(80, 20)
                .with_cached_input_tokens(70)
                .with_cache_write_input_tokens(10),
        ));
        assert_eq!(
            guarded_tokens(&[measured(1_000_000), last.clone()]),
            Some(120)
        );
        let mut authoritative = last;
        authoritative.usage.as_mut().unwrap().metadata =
            MetadataMap::from_iter([("context_used".into(), json!(10))]);
        assert_eq!(guarded_tokens(&[authoritative]), Some(12));
        let unknown = Item::text(ItemKind::Assistant, "unmeasured").with_usage(Usage::default());
        assert_eq!(guarded_tokens(&[measured(100), unknown]), None);
    }

    #[test]
    fn model_switch_unknown_values_allow_an_unchecked_switch() {
        for (transcript, window) in [
            (vec![], Some(100)),
            (vec![measured(100)], None),
            (vec![measured(100)], Some(0)),
        ] {
            assert_eq!(
                check(&mut Guard::default(), transcript, window, None).unwrap(),
                Decision::Continue
            );
        }
    }

    #[test]
    fn model_switch_actions_are_one_shot_and_bound_to_state() {
        for action in [Decision::Continue, Decision::Compact] {
            let mut guard = Guard::default();
            let warning = warn(&mut guard);
            let confirmation = serde_json::to_value(Confirmation {
                token: warning.token,
                action,
            })
            .unwrap();
            assert_eq!(
                check(
                    &mut guard,
                    vec![measured(100)],
                    Some(150),
                    Some(&confirmation)
                )
                .unwrap(),
                action
            );
            assert!(
                check(
                    &mut guard,
                    vec![measured(100)],
                    Some(150),
                    Some(&confirmation)
                )
                .is_err()
            );
        }
        for stale in [
            "transcript",
            "model",
            "target",
            "session",
            "new-request",
            "cancel",
        ] {
            let mut guard = Guard::default();
            let warning = warn(&mut guard);
            let confirmation = serde_json::to_value(Confirmation {
                token: warning.token,
                action: Decision::Continue,
            })
            .unwrap();
            if stale == "session" {
                guard = Guard::default();
            }
            if stale == "new-request" {
                let _ = warn(&mut guard);
            }
            assert!(
                guard
                    .check(
                        (
                            &selection(if stale == "model" {
                                "another"
                            } else {
                                "original"
                            }),
                            if stale == "cancel" { 1 } else { 0 }
                        ),
                        selection(if stale == "target" {
                            "another"
                        } else {
                            "target"
                        }),
                        &catalog(Some(150)),
                        vec![measured(if stale == "transcript" { 101 } else { 100 })],
                        Some(&confirmation),
                    )
                    .is_err(),
                "{stale}"
            );
        }
    }

    #[test]
    fn model_switch_compaction_requires_exact_consumed_marker_and_success() {
        let marker = compact_marker();
        let other = compact_marker();
        assert_ne!(marker.id, other.id);
        assert!(!compaction_completed(
            &FinishReason::Completed,
            &marker.id,
            std::slice::from_ref(&marker),
            false
        ));
        assert!(compaction_completed(
            &FinishReason::Completed,
            &marker.id,
            &[other],
            false
        ));
        for reason in [
            FinishReason::Cancelled,
            FinishReason::Error,
            FinishReason::MaxTokens,
            FinishReason::Blocked,
        ] {
            assert!(!compaction_completed(&reason, &marker.id, &[], false));
        }
        assert!(!compaction_completed(
            &FinishReason::Completed,
            &marker.id,
            &[],
            true
        ));
        assert!(!compaction_completed(
            &FinishReason::Completed,
            &None,
            &[],
            false
        ));
    }
}
