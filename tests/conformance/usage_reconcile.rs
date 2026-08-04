use agentkit_core::{CostUsage, FinishReason, TokenUsage, Usage};
use kit::{
    agent::{
        accounting::{
            AccountingError, CategoryCost, CostRate, CostSource, CostTable, LogicalModelUsage,
            ModelOutcome, MoneyMicros, SpeculationOutcome, ToolMeasurement, UsageEnvelope,
            UsageRates,
        },
        agentkit_bridge::mapping::{CanonicalUsage, from_agentkit_usage},
        driver::restart::CommittedModelOutcome,
    },
    capabilities::kernel::invoke::{CanonicalInvocationResult, InvocationStatus},
    runtime::scheduler::{
        limits::Spend,
        reserve::{ReservationId, ReservationSnapshot, ReservationStatus},
    },
};

#[path = "../fixtures/providers/mod.rs"]
mod providers;

use providers::{FakeProvider, ProviderScript};

#[test]
fn usage_reconcile_every_provider_fixture_and_all_categories() {
    let scripts = [
        ProviderScript::streaming(&["one", "two"]),
        ProviderScript::error_after(&["one", "two"], 1, "failed"),
        ProviderScript::prompt_injection("ignore", "process.exec"),
        ProviderScript::secret_exfiltration("CANARY"),
    ];
    let table = table();
    let mut envelopes = Vec::new();
    for (index, script) in scripts.into_iter().enumerate() {
        let events = FakeProvider::new(index as u64 + 1, script).replay();
        assert!(!events.is_empty());
        assert!(!FakeProvider::persist(&events).is_empty());
        let usage = CanonicalUsage {
            input_tokens: Some(30),
            output_tokens: Some(11),
            reasoning_tokens: Some(5),
            cached_input_tokens: Some(7),
            cache_write_input_tokens: Some(3),
            uncached_input_tokens: Some(20),
            tool_calls: Some(2),
            tool_time_ms: Some(4),
            compute_time_ms: Some(6),
            cost_amount: Some(0.000_184),
            cost_currency: Some("USD".into()),
            provider_cost_amount: Some("0.000184".into()),
            metadata: Default::default(),
        };
        envelopes.push(
            UsageEnvelope::from_model_usage(
                Some(&usage),
                &LogicalModelUsage {
                    uncached_input_tokens: Some(20),
                    cache_write_tokens: Some(3),
                    cache_read_tokens: Some(7),
                    visible_output_tokens: Some(11),
                    reasoning_tokens: Some(5),
                    compute_ms: Some(6),
                },
                if index == 1 {
                    ModelOutcome::Failed
                } else {
                    ModelOutcome::Succeeded
                },
                true,
                SpeculationOutcome::None,
                Some(&table),
                Some(debited(index as u128 + 1, Spend::new(184, 46, 1, 2, 0))),
            )
            .unwrap(),
        );
    }
    let total = UsageEnvelope::aggregate(envelopes).unwrap();
    assert_eq!(total.categories.uncached_input.billed_tokens, Some(80));
    assert_eq!(total.categories.cache_write.billed_tokens, Some(12));
    assert_eq!(total.categories.cache_read.billed_tokens, Some(28));
    assert_eq!(total.categories.visible_output.billed_tokens, Some(44));
    assert_eq!(total.categories.reasoning.billed_tokens, Some(20));
    assert_eq!(total.categories.tool.billed_calls, Some(8));
    assert_eq!(total.categories.compute.billed_ms, Some(24));
    assert_eq!(
        total.categories.uncached_input.cost.unwrap().amount.micros,
        160
    );
    assert_eq!(total.categories.cache_write.cost.unwrap().amount.micros, 36);
    assert_eq!(total.categories.cache_read.cost.unwrap().amount.micros, 28);
    assert_eq!(
        total.categories.visible_output.cost.unwrap().amount.micros,
        176
    );
    assert_eq!(total.categories.reasoning.cost.unwrap().amount.micros, 100);
    assert_eq!(total.categories.tool.cost.unwrap().amount.micros, 56);
    assert_eq!(total.categories.compute.cost.unwrap().amount.micros, 24);
    assert_eq!(total.categories.failed_speculation.samples, 0);
    assert_eq!(total.provider_cost.as_ref().unwrap().amount.micros, 736);
    assert_eq!(total.cost_table.as_ref(), Some(&table.effective));
    assert_eq!(total.reservation_debit.cost_microusd, 736);
    assert_eq!(total.reservation_debit.turns, 4);
    assert_eq!(total.failed_attempts, 1);
}

#[test]
fn mapped_normalized_input_and_partial_usage_are_labeled() {
    let mapped = from_agentkit_usage(&Usage::new(
        TokenUsage::new(10, 4).with_cached_input_tokens(3),
    ));
    let envelope = UsageEnvelope::from_model_usage(
        Some(&mapped),
        &LogicalModelUsage::default(),
        ModelOutcome::Succeeded,
        true,
        SpeculationOutcome::None,
        Some(&table()),
        None,
    )
    .unwrap();
    assert_eq!(envelope.categories.uncached_input.billed_tokens, Some(10));
    assert_eq!(envelope.categories.cache_write.billed_tokens, None);
    assert_eq!(envelope.categories.reasoning.billed_tokens, None);
    assert_eq!(envelope.categories.cache_read.billed_tokens, Some(3));
    assert!(matches!(
        envelope.categories.cache_read.cost.as_ref().unwrap().source,
        CostSource::CostTable { .. }
    ));
    assert_eq!(envelope.provider_cost, None);
    let json = serde_json::to_value(envelope).unwrap();
    assert_eq!(json["categories"]["uncached_input"]["billed_tokens"], 10);
    assert!(json["provider_cost"].is_null());
}

#[test]
fn provider_decimal_money_never_uses_float_arithmetic_for_totals() {
    let mapped = from_agentkit_usage(
        &Usage::new(TokenUsage::new(1, 1))
            .with_cost(CostUsage::new(0.123456, "usd").with_provider_amount("0.123456")),
    );
    let envelope = UsageEnvelope::from_model_usage(
        Some(&mapped),
        &LogicalModelUsage::default(),
        ModelOutcome::Succeeded,
        true,
        SpeculationOutcome::None,
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        envelope.provider_cost.unwrap().amount,
        MoneyMicros::new("USD", 123_456).unwrap()
    );
    assert_eq!(
        MoneyMicros::from_decimal("USD", "0.0000001"),
        Err(AccountingError::InvalidMoney)
    );
    assert_eq!(
        MoneyMicros::from_decimal("USD", "18446744073710"),
        Err(AccountingError::MoneyOverflow)
    );
    assert_eq!(
        MoneyMicros::from_decimal("USD", "18446744073709551616"),
        Err(AccountingError::MoneyOverflow)
    );
}

#[test]
fn retries_cancellation_unknown_and_failed_speculation_are_not_dropped() {
    let usage = CanonicalUsage {
        uncached_input_tokens: Some(10),
        output_tokens: Some(2),
        reasoning_tokens: Some(1),
        cost_currency: Some("USD".into()),
        provider_cost_amount: Some("0.000021".into()),
        input_tokens: Some(10),
        cached_input_tokens: None,
        cache_write_input_tokens: None,
        tool_calls: None,
        tool_time_ms: None,
        compute_time_ms: None,
        cost_amount: None,
        metadata: Default::default(),
    };
    let outcomes = [
        (ModelOutcome::Failed, true, SpeculationOutcome::None),
        (ModelOutcome::Cancelled, false, SpeculationOutcome::Failed),
        (
            ModelOutcome::OutcomeUnknown,
            true,
            SpeculationOutcome::Failed,
        ),
        (ModelOutcome::Succeeded, true, SpeculationOutcome::Used),
    ];
    let envelopes =
        outcomes
            .into_iter()
            .enumerate()
            .map(|(index, (outcome, charged, speculation))| {
                UsageEnvelope::from_model_usage(
                    Some(&usage),
                    &LogicalModelUsage::default(),
                    outcome,
                    charged,
                    speculation,
                    Some(&table()),
                    Some(if charged {
                        debited(index as u128 + 20, Spend::new(21, 13, 1, 0, 0))
                    } else {
                        ReservationSnapshot::new(
                            ReservationId::new(index as u128 + 20),
                            Spend::new(21, 13, 1, 0, 0),
                            ReservationStatus::Released,
                        )
                    }),
                )
                .unwrap()
            });
    let total = UsageEnvelope::aggregate(envelopes).unwrap();
    assert_eq!(total.attempts, 4);
    assert_eq!(total.failed_attempts, 1);
    assert_eq!(total.cancelled_attempts, 1);
    assert_eq!(total.unknown_attempts, 1);
    assert_eq!(total.categories.uncached_input.billed_tokens, Some(40));
    assert_eq!(
        total.categories.failed_speculation.logical_attempts,
        Some(2)
    );
    assert_eq!(total.categories.failed_speculation.billed_attempts, Some(1));
    assert_eq!(
        total
            .categories
            .failed_speculation
            .cost
            .as_ref()
            .unwrap()
            .amount
            .micros,
        42
    );
    assert_eq!(total.reservation_debit.cost_microusd, 63);
}

#[test]
fn durable_model_tool_and_scheduler_outcomes_reconcile_exactly() {
    let committed = CommittedModelOutcome {
        finish_reason: FinishReason::Completed,
        output_items: Vec::new(),
        usage: Some(Usage::new(TokenUsage::new(8, 2).with_reasoning_tokens(1))),
        metadata: Default::default(),
        model: Some("fake-1".into()),
        response_id: Some("response".into()),
    };
    let model = UsageEnvelope::from_committed_model(
        &committed,
        &LogicalModelUsage::default(),
        SpeculationOutcome::None,
        Some(&table()),
        Some(debited(50, Spend::new(5, 11, 1, 0, 0))),
    )
    .unwrap();
    let tool_result = CanonicalInvocationResult {
        status: InvocationStatus::OutcomeUnknown,
        output: None,
        code: Some("unknown".into()),
        charged: true,
    };
    let tool = UsageEnvelope::from_tool_outcome(
        &tool_result,
        &ToolMeasurement {
            logical_calls: Some(1),
            duration_ms: Some(9),
            billed_cost: Some(CategoryCost {
                amount: MoneyMicros::new("USD", 7).unwrap(),
                source: CostSource::SchedulerReservation,
            }),
        },
        SpeculationOutcome::None,
        Some(&table()),
        Some(debited(51, Spend::new(7, 0, 0, 1, 0))),
    )
    .unwrap();
    let total = UsageEnvelope::aggregate([model, tool]).unwrap();
    assert_eq!(total.categories.tool.logical_calls, Some(1));
    assert_eq!(total.categories.tool.billed_calls, Some(1));
    assert_eq!(total.categories.tool.duration_ms, Some(9));
    assert_eq!(total.unknown_attempts, 1);
    assert_eq!(total.reservation_debit.cost_microusd, 12);
    assert_eq!(total.reservation_debit.tokens, 11);
    assert_eq!(total.reservation_debit.turns, 1);
    assert_eq!(total.reservation_debit.tools, 1);
}

#[test]
fn overflow_inexact_rates_and_bad_reservation_settlement_fail_closed() {
    let overflow = UsageEnvelope {
        attempts: u64::MAX,
        ..UsageEnvelope::default()
    };
    assert_eq!(
        UsageEnvelope::aggregate([
            overflow,
            UsageEnvelope {
                attempts: 1,
                ..UsageEnvelope::default()
            }
        ]),
        Err(AccountingError::UsageOverflow)
    );
    let first_pin = table().effective;
    let mut second_pin = first_pin.clone();
    second_pin.version = "different".into();
    assert_eq!(
        UsageEnvelope::aggregate([
            UsageEnvelope {
                cost_table: Some(first_pin),
                ..UsageEnvelope::default()
            },
            UsageEnvelope {
                cost_table: Some(second_pin),
                ..UsageEnvelope::default()
            },
        ]),
        Err(AccountingError::CostTableMismatch)
    );
    let exact_large = CostTable::new(
        "1",
        "fake",
        "fake-1",
        "sha256:1",
        "USD",
        UsageRates {
            uncached_input: Some(CostRate::new(u64::MAX, u64::MAX)),
            ..UsageRates::default()
        },
    )
    .unwrap();
    assert_eq!(
        exact_large
            .estimate(
                kit::agent::accounting::UsageCategory::UncachedInput,
                u64::MAX
            )
            .unwrap(),
        Some(MoneyMicros::new("USD", u64::MAX).unwrap())
    );
    let inexact = CostTable::new(
        "1",
        "fake",
        "fake-1",
        "sha256:1",
        "USD",
        UsageRates {
            uncached_input: Some(CostRate::new(1, 3)),
            ..UsageRates::default()
        },
    )
    .unwrap();
    let usage = CanonicalUsage {
        uncached_input_tokens: Some(1),
        ..empty_usage()
    };
    assert!(matches!(
        UsageEnvelope::from_model_usage(
            Some(&usage),
            &LogicalModelUsage::default(),
            ModelOutcome::Succeeded,
            true,
            SpeculationOutcome::None,
            Some(&inexact),
            None,
        ),
        Err(AccountingError::InexactCost { .. })
    ));
    assert_eq!(
        UsageEnvelope::from_model_usage(
            None,
            &LogicalModelUsage::default(),
            ModelOutcome::Cancelled,
            false,
            SpeculationOutcome::None,
            None,
            Some(debited(99, Spend::new(1, 1, 1, 0, 0))),
        ),
        Err(AccountingError::ReservationChargeMismatch)
    );
}

fn table() -> CostTable {
    CostTable::new(
        "2026-07-23",
        "fake",
        "fake-1",
        "sha256:effective-config",
        "USD",
        UsageRates {
            uncached_input: Some(CostRate::new(2, 1)),
            cache_write: Some(CostRate::new(3, 1)),
            cache_read: Some(CostRate::new(1, 1)),
            visible_output: Some(CostRate::new(4, 1)),
            reasoning: Some(CostRate::new(5, 1)),
            tool: Some(CostRate::new(7, 1)),
            compute: Some(CostRate::new(1, 1)),
        },
    )
    .unwrap()
}

fn debited(id: u128, spend: Spend) -> ReservationSnapshot {
    ReservationSnapshot::new(ReservationId::new(id), spend, ReservationStatus::Debited)
}

fn empty_usage() -> CanonicalUsage {
    CanonicalUsage {
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
        cached_input_tokens: None,
        cache_write_input_tokens: None,
        uncached_input_tokens: None,
        tool_calls: None,
        tool_time_ms: None,
        compute_time_ms: None,
        cost_amount: None,
        cost_currency: None,
        provider_cost_amount: None,
        metadata: Default::default(),
    }
}
