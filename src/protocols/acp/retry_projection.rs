//! Bounded, payload-free provider retry activity shared by both ACP versions.
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::events::{ProviderRetryProgress, RetryStatus};
use agentkit_acp::{Notice, NoticeSeverity, SessionUpdate};
use agentkit_loop::{AgentEvent, ProviderRetryEvent};

pub(crate) const META_KEY: &str = "kit/providerRetry";
// Per logical provider request; terminal observations reset the budget.
const MAX_SCHEDULED: u8 = 16;

#[derive(Default)]
struct State {
    emitted: u8,
    last_elapsed: Duration,
}

/// The loop serializes observations. Only `observe` writes this private state.
/// No callback, await, formatting, or serialization runs under its guard; a
/// poisoned projection is isolated without touching the model's outcome.
#[derive(Clone, Default)]
pub(crate) struct Projection(Arc<Mutex<State>>);

impl Projection {
    pub(crate) fn observe(&self, event: &AgentEvent) -> Option<SessionUpdate> {
        let progress = {
            let mut state = self.0.lock().ok()?;
            match event {
                AgentEvent::TurnStarted { .. } => {
                    *state = State::default();
                    return None;
                }
                AgentEvent::ProviderRetry(ProviderRetryEvent::Scheduled(retry)) => {
                    if state.emitted >= MAX_SCHEDULED
                        || (state.emitted > 0
                            && retry.accounting.elapsed.saturating_sub(state.last_elapsed)
                                < Duration::from_secs(1))
                    {
                        return None;
                    }
                    state.emitted += 1;
                    state.last_elapsed = retry.accounting.elapsed;
                    ProviderRetryProgress {
                        status: RetryStatus::Scheduled,
                        route: retry.route,
                        attempts: retry.accounting.attempts,
                        category: Some(retry.reason),
                        next_delay_ms: millis(retry.next_delay),
                        completed_backoff_ms: millis(retry.accounting.completed_backoff),
                    }
                }
                AgentEvent::ProviderRetry(ProviderRetryEvent::Succeeded { route, accounting }) => {
                    let active = state.emitted > 0;
                    *state = State::default();
                    if !active {
                        return None;
                    }
                    ProviderRetryProgress {
                        status: RetryStatus::Succeeded,
                        route: *route,
                        attempts: accounting.attempts,
                        category: None,
                        next_delay_ms: 0,
                        completed_backoff_ms: millis(accounting.completed_backoff),
                    }
                }
                AgentEvent::ProviderRetry(ProviderRetryEvent::Stopped(failure)) => {
                    let active = state.emitted > 0;
                    *state = State::default();
                    if !active {
                        return None;
                    }
                    ProviderRetryProgress {
                        status: RetryStatus::Stopped,
                        route: failure.route,
                        attempts: failure.accounting.attempts,
                        category: Some(failure.reason),
                        next_delay_ms: 0,
                        completed_backoff_ms: millis(failure.accounting.completed_backoff),
                    }
                }
                _ => return None,
            }
        };
        let title = match progress.status {
            RetryStatus::Scheduled => format!(
                "Provider retry: {:?}, attempt {}, {:?}; next delay {} ms, completed backoff {} ms",
                progress.route,
                progress.attempts,
                progress.category,
                progress.next_delay_ms,
                progress.completed_backoff_ms
            ),
            RetryStatus::Succeeded => format!(
                "Provider retry finished after {} attempts",
                progress.attempts
            ),
            RetryStatus::Stopped => format!(
                "Provider retry stopped after {} attempts",
                progress.attempts
            ),
        };
        let meta: serde_json::Map<String, serde_json::Value> =
            [(META_KEY.to_owned(), serde_json::to_value(progress).ok()?)]
                .into_iter()
                .collect();
        Some(SessionUpdate::Notice(
            Notice::new(NoticeSeverity::Info, title).meta(meta),
        ))
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::disallowed_methods
)]
mod tests;
