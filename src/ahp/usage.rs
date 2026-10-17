//! Preserve reported usage without fabricating absent token counts or costs.
use agenthooksprotocol::{Integer, Presence};
use agentkit_core::Usage;

fn tokens(value: u64) -> Presence<Integer> {
    Integer::new(value.into()).map_or(Presence::Missing, Presence::Present)
}

pub(super) fn attempt(usage: &Usage) -> agenthooksprotocol::ExecutionEventAttemptusage {
    use agenthooksprotocol::{
        ExecutionEventAttemptusage, ExecutionEventAttemptusageCompleteness,
        ExecutionEventAttemptusageCost, ExecutionEventAttemptusageCostBasis,
        ExecutionEventAttemptusageProvenance,
    };
    // The runtime does not certify that all usage dimensions were reported.
    let mut result = ExecutionEventAttemptusage::new(
        ExecutionEventAttemptusageCompleteness::Partial,
        ExecutionEventAttemptusageProvenance::Provider,
    );
    if let Some(value) = &usage.tokens {
        result.input_tokens = tokens(value.input_tokens);
        result.output_tokens = tokens(value.output_tokens);
        result.cache_read_tokens = value.cached_input_tokens.map_or(Presence::Missing, tokens);
        result.cache_write_tokens = value
            .cache_write_input_tokens
            .map_or(Presence::Missing, tokens);
    }
    if let Some(cost) = &usage.cost
        && cost.amount >= 0.0
        && cost.currency.len() == 3
        && cost.currency.bytes().all(|byte| byte.is_ascii_uppercase())
        && let Some(amount) = serde_json::Number::from_f64(cost.amount)
    {
        result.cost = Presence::Present(ExecutionEventAttemptusageCost::new(
            amount,
            ExecutionEventAttemptusageCostBasis::Reported,
            &cost.currency,
        ));
    }
    result
}

pub(super) fn turn(usage: &Usage) -> agenthooksprotocol::ExecutionEventTurnusage {
    use agenthooksprotocol::{
        ExecutionEventTurnusage, ExecutionEventTurnusageCompleteness, ExecutionEventTurnusageCost,
        ExecutionEventTurnusageCostBasis, ExecutionEventTurnusageProvenance,
    };
    // The runtime does not certify that all usage dimensions were reported.
    let mut result = ExecutionEventTurnusage::new(
        ExecutionEventTurnusageCompleteness::Partial,
        ExecutionEventTurnusageProvenance::Provider,
    );
    if let Some(value) = &usage.tokens {
        result.input_tokens = tokens(value.input_tokens);
        result.output_tokens = tokens(value.output_tokens);
        result.cache_read_tokens = value.cached_input_tokens.map_or(Presence::Missing, tokens);
        result.cache_write_tokens = value
            .cache_write_input_tokens
            .map_or(Presence::Missing, tokens);
    }
    if let Some(cost) = &usage.cost
        && cost.amount >= 0.0
        && cost.currency.len() == 3
        && cost.currency.bytes().all(|byte| byte.is_ascii_uppercase())
        && let Some(amount) = serde_json::Number::from_f64(cost.amount)
    {
        result.cost = Presence::Present(ExecutionEventTurnusageCost::new(
            amount,
            ExecutionEventTurnusageCostBasis::Reported,
            &cost.currency,
        ));
    }
    result
}

/// Match the loop's merge_usage precedence: final token/cost dimensions win,
/// streamed dimensions fill missing ones. Never sum successive usage snapshots.
pub(super) fn reported(final_usage: Option<&Usage>, streamed: Option<&Usage>) -> Option<Usage> {
    match (final_usage, streamed) {
        (None, streamed) => streamed.cloned(),
        (Some(final_usage), streamed) => {
            let mut usage = final_usage.clone();
            if let Some(streamed) = streamed {
                if usage.tokens.is_none() {
                    usage.tokens = streamed.tokens.clone();
                }
                if usage.cost.is_none() {
                    usage.cost = streamed.cost.clone();
                }
            }
            Some(usage)
        }
    }
}
