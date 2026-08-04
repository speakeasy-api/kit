use agentkit_core::FinishReason;
use serde::{Deserialize, Serialize};

use crate::{
    agent::{
        agentkit_bridge::mapping::{CanonicalUsage, from_agentkit_usage},
        driver::restart::CommittedModelOutcome,
    },
    capabilities::kernel::invoke::{CanonicalInvocationResult, InvocationStatus},
    runtime::scheduler::{
        limits::Spend,
        reserve::{ReservationSnapshot, ReservationStatus},
    },
};

use super::{
    AccountingError, CostTable, CostTableSnapshot, MoneyMicros, USAGE_ENVELOPE_VERSION,
    UsageCategory,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum CostSource {
    ProviderReported,
    CostTable { version: String, snapshot: String },
    SchedulerReservation,
    Mixed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CategoryCost {
    pub amount: MoneyMicros,
    pub source: CostSource,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TokenUsageCategory {
    pub samples: u64,
    pub logical_tokens: Option<u64>,
    pub billed_tokens: Option<u64>,
    pub cost: Option<CategoryCost>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolUsage {
    pub samples: u64,
    pub logical_calls: Option<u64>,
    pub billed_calls: Option<u64>,
    pub duration_ms: Option<u64>,
    pub cost: Option<CategoryCost>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputeUsage {
    pub samples: u64,
    pub logical_ms: Option<u64>,
    pub billed_ms: Option<u64>,
    pub cost: Option<CategoryCost>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FailedSpeculationUsage {
    pub samples: u64,
    pub logical_attempts: Option<u64>,
    pub billed_attempts: Option<u64>,
    pub cost: Option<CategoryCost>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageCategories {
    pub uncached_input: TokenUsageCategory,
    pub cache_write: TokenUsageCategory,
    pub cache_read: TokenUsageCategory,
    pub visible_output: TokenUsageCategory,
    pub reasoning: TokenUsageCategory,
    pub tool: ToolUsage,
    pub compute: ComputeUsage,
    pub failed_speculation: FailedSpeculationUsage,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LogicalModelUsage {
    pub uncached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub visible_output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub compute_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOutcome {
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeculationOutcome {
    #[default]
    None,
    Used,
    Failed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolMeasurement {
    pub logical_calls: Option<u64>,
    pub duration_ms: Option<u64>,
    pub billed_cost: Option<CategoryCost>,
}

impl ToolMeasurement {
    pub fn one_call() -> Self {
        Self {
            logical_calls: Some(1),
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SchedulerDebit {
    pub cost_microusd: u64,
    pub tokens: u64,
    pub turns: u64,
    pub tools: u64,
    pub processes: u64,
}

impl SchedulerDebit {
    fn from_spend(spend: Spend) -> Self {
        Self {
            cost_microusd: spend.cost_microusd(),
            tokens: spend.tokens(),
            turns: spend.turns(),
            tools: spend.tools(),
            processes: spend.processes(),
        }
    }

    fn checked_add(self, other: Self) -> Result<Self, AccountingError> {
        Ok(Self {
            cost_microusd: self
                .cost_microusd
                .checked_add(other.cost_microusd)
                .ok_or(AccountingError::UsageOverflow)?,
            tokens: self
                .tokens
                .checked_add(other.tokens)
                .ok_or(AccountingError::UsageOverflow)?,
            turns: self
                .turns
                .checked_add(other.turns)
                .ok_or(AccountingError::UsageOverflow)?,
            tools: self
                .tools
                .checked_add(other.tools)
                .ok_or(AccountingError::UsageOverflow)?,
            processes: self
                .processes
                .checked_add(other.processes)
                .ok_or(AccountingError::UsageOverflow)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageEnvelope {
    pub schema_version: u16,
    pub categories: UsageCategories,
    pub provider_cost: Option<CategoryCost>,
    pub provider_cost_samples: u64,
    pub cost_table: Option<CostTableSnapshot>,
    pub attempts: u64,
    pub failed_attempts: u64,
    pub cancelled_attempts: u64,
    pub unknown_attempts: u64,
    pub reservation_debit: SchedulerDebit,
}

impl Default for UsageEnvelope {
    fn default() -> Self {
        Self {
            schema_version: USAGE_ENVELOPE_VERSION,
            categories: UsageCategories::default(),
            provider_cost: None,
            provider_cost_samples: 0,
            cost_table: None,
            attempts: 0,
            failed_attempts: 0,
            cancelled_attempts: 0,
            unknown_attempts: 0,
            reservation_debit: SchedulerDebit::default(),
        }
    }
}

impl UsageEnvelope {
    #[allow(clippy::too_many_arguments)]
    pub fn from_model_usage(
        usage: Option<&CanonicalUsage>,
        logical: &LogicalModelUsage,
        outcome: ModelOutcome,
        charged: bool,
        speculation: SpeculationOutcome,
        table: Option<&CostTable>,
        reservation: Option<ReservationSnapshot>,
    ) -> Result<Self, AccountingError> {
        let mut envelope = Self {
            cost_table: table.map(|table| table.effective.clone()),
            attempts: 1,
            failed_attempts: u64::from(outcome == ModelOutcome::Failed),
            cancelled_attempts: u64::from(outcome == ModelOutcome::Cancelled),
            unknown_attempts: u64::from(outcome == ModelOutcome::OutcomeUnknown),
            ..Self::default()
        };
        let billed = usage.cloned().unwrap_or_else(empty_usage);
        envelope.categories.uncached_input = token_category(
            logical.uncached_input_tokens,
            billed.uncached_input_tokens,
            UsageCategory::UncachedInput,
            table,
        )?;
        envelope.categories.cache_write = token_category(
            logical.cache_write_tokens,
            billed.cache_write_input_tokens,
            UsageCategory::CacheWrite,
            table,
        )?;
        envelope.categories.cache_read = token_category(
            logical.cache_read_tokens,
            billed.cached_input_tokens,
            UsageCategory::CacheRead,
            table,
        )?;
        envelope.categories.visible_output = token_category(
            logical.visible_output_tokens,
            billed.output_tokens,
            UsageCategory::VisibleOutput,
            table,
        )?;
        envelope.categories.reasoning = token_category(
            logical.reasoning_tokens,
            billed.reasoning_tokens,
            UsageCategory::Reasoning,
            table,
        )?;
        envelope.categories.compute =
            compute_category(logical.compute_ms, billed.compute_time_ms, table)?;
        if billed.tool_calls.is_some() || billed.tool_time_ms.is_some() {
            envelope.categories.tool =
                tool_category(None, billed.tool_calls, billed.tool_time_ms, None, table)?;
        }
        if let Some(currency) = billed.cost_currency.as_deref() {
            let amount = if let Some(amount) = billed.provider_cost_amount.as_deref() {
                Some(MoneyMicros::from_decimal(currency, amount)?)
            } else if let Some(amount) = billed.cost_amount {
                if !amount.is_finite() || amount.is_sign_negative() {
                    return Err(AccountingError::InvalidMoney);
                }
                Some(MoneyMicros::from_decimal(currency, &amount.to_string())?)
            } else {
                None
            };
            envelope.provider_cost = amount.map(|amount| CategoryCost {
                amount,
                source: CostSource::ProviderReported,
            });
        }
        envelope.provider_cost_samples = 1;

        if speculation == SpeculationOutcome::Failed {
            let category_cost = match envelope.provider_cost.clone() {
                Some(cost) => Some(cost),
                None => envelope.estimated_category_cost()?,
            };
            clear_category_costs(&mut envelope.categories);
            envelope.categories.failed_speculation = FailedSpeculationUsage {
                samples: 1,
                logical_attempts: Some(1),
                billed_attempts: Some(u64::from(charged)),
                cost: category_cost,
            };
        }
        envelope.apply_reservation(reservation, charged)?;
        Ok(envelope)
    }

    pub fn from_committed_model(
        outcome: &CommittedModelOutcome,
        logical: &LogicalModelUsage,
        speculation: SpeculationOutcome,
        table: Option<&CostTable>,
        reservation: Option<ReservationSnapshot>,
    ) -> Result<Self, AccountingError> {
        let mapped = outcome.usage.as_ref().map(from_agentkit_usage);
        let status = match &outcome.finish_reason {
            FinishReason::Cancelled => ModelOutcome::Cancelled,
            FinishReason::Error | FinishReason::Blocked => ModelOutcome::Failed,
            _ => ModelOutcome::Succeeded,
        };
        Self::from_model_usage(
            mapped.as_ref(),
            logical,
            status,
            true,
            speculation,
            table,
            reservation,
        )
    }

    pub fn from_tool_outcome(
        outcome: &CanonicalInvocationResult,
        measurement: &ToolMeasurement,
        speculation: SpeculationOutcome,
        table: Option<&CostTable>,
        reservation: Option<ReservationSnapshot>,
    ) -> Result<Self, AccountingError> {
        let charged = outcome.charged;
        let status = match outcome.status {
            InvocationStatus::Succeeded => ModelOutcome::Succeeded,
            InvocationStatus::Cancelled => ModelOutcome::Cancelled,
            InvocationStatus::OutcomeUnknown => ModelOutcome::OutcomeUnknown,
            _ => ModelOutcome::Failed,
        };
        let mut envelope = Self {
            cost_table: table.map(|table| table.effective.clone()),
            attempts: 1,
            failed_attempts: u64::from(status == ModelOutcome::Failed),
            cancelled_attempts: u64::from(status == ModelOutcome::Cancelled),
            unknown_attempts: u64::from(status == ModelOutcome::OutcomeUnknown),
            ..Self::default()
        };
        envelope.categories.tool = tool_category(
            measurement.logical_calls,
            Some(u64::from(charged)),
            measurement.duration_ms,
            measurement.billed_cost.clone(),
            table,
        )?;
        if speculation == SpeculationOutcome::Failed {
            let cost = envelope.categories.tool.cost.take();
            envelope.categories.failed_speculation = FailedSpeculationUsage {
                samples: 1,
                logical_attempts: Some(1),
                billed_attempts: Some(u64::from(charged)),
                cost,
            };
        }
        envelope.apply_reservation(reservation, charged)?;
        Ok(envelope)
    }

    pub fn aggregate(envelopes: impl IntoIterator<Item = Self>) -> Result<Self, AccountingError> {
        let mut total = Self::default();
        for envelope in envelopes {
            total.checked_add_assign(envelope)?;
        }
        Ok(total)
    }

    fn apply_reservation(
        &mut self,
        reservation: Option<ReservationSnapshot>,
        charged: bool,
    ) -> Result<(), AccountingError> {
        let Some(reservation) = reservation else {
            return Ok(());
        };
        let debited = matches!(
            reservation.status(),
            ReservationStatus::Debited
                | ReservationStatus::Reconciled
                | ReservationStatus::ActualOverage
        );
        let released = reservation.status() == ReservationStatus::Released;
        if (!debited && !released) || charged != debited {
            return Err(if charged == debited {
                AccountingError::InvalidReservationStatus
            } else {
                AccountingError::ReservationChargeMismatch
            });
        }
        if debited {
            self.reservation_debit = SchedulerDebit::from_spend(reservation.spend());
        }
        Ok(())
    }

    fn estimated_category_cost(&self) -> Result<Option<CategoryCost>, AccountingError> {
        let mut total = None;
        for cost in [
            self.categories.uncached_input.cost.as_ref(),
            self.categories.cache_write.cost.as_ref(),
            self.categories.cache_read.cost.as_ref(),
            self.categories.visible_output.cost.as_ref(),
            self.categories.reasoning.cost.as_ref(),
            self.categories.tool.cost.as_ref(),
            self.categories.compute.cost.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            total = Some(match total {
                None => cost.clone(),
                Some(current) => add_cost(&current, cost)?,
            });
        }
        Ok(total)
    }

    fn checked_add_assign(&mut self, other: Self) -> Result<(), AccountingError> {
        if self.cost_table.is_none() {
            self.cost_table = other.cost_table.clone();
        } else if other.cost_table.is_some() && self.cost_table != other.cost_table {
            return Err(AccountingError::CostTableMismatch);
        }
        merge_token(
            &mut self.categories.uncached_input,
            other.categories.uncached_input,
        )?;
        merge_token(
            &mut self.categories.cache_write,
            other.categories.cache_write,
        )?;
        merge_token(&mut self.categories.cache_read, other.categories.cache_read)?;
        merge_token(
            &mut self.categories.visible_output,
            other.categories.visible_output,
        )?;
        merge_token(&mut self.categories.reasoning, other.categories.reasoning)?;
        merge_tool(&mut self.categories.tool, other.categories.tool)?;
        merge_compute(&mut self.categories.compute, other.categories.compute)?;
        merge_speculation(
            &mut self.categories.failed_speculation,
            other.categories.failed_speculation,
        )?;
        merge_optional_cost(
            &mut self.provider_cost,
            self.provider_cost_samples,
            other.provider_cost,
            other.provider_cost_samples,
        )?;
        self.provider_cost_samples = add(self.provider_cost_samples, other.provider_cost_samples)?;
        self.attempts = add(self.attempts, other.attempts)?;
        self.failed_attempts = add(self.failed_attempts, other.failed_attempts)?;
        self.cancelled_attempts = add(self.cancelled_attempts, other.cancelled_attempts)?;
        self.unknown_attempts = add(self.unknown_attempts, other.unknown_attempts)?;
        self.reservation_debit = self
            .reservation_debit
            .checked_add(other.reservation_debit)?;
        Ok(())
    }
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

fn token_category(
    logical_tokens: Option<u64>,
    billed_tokens: Option<u64>,
    category: UsageCategory,
    table: Option<&CostTable>,
) -> Result<TokenUsageCategory, AccountingError> {
    let cost = estimated_cost(table, category, billed_tokens)?;
    Ok(TokenUsageCategory {
        samples: 1,
        logical_tokens,
        billed_tokens,
        cost,
    })
}

fn tool_category(
    logical_calls: Option<u64>,
    billed_calls: Option<u64>,
    duration_ms: Option<u64>,
    cost: Option<CategoryCost>,
    table: Option<&CostTable>,
) -> Result<ToolUsage, AccountingError> {
    Ok(ToolUsage {
        samples: 1,
        logical_calls,
        billed_calls,
        duration_ms,
        cost: match cost {
            Some(cost) => Some(cost),
            None => estimated_cost(table, UsageCategory::Tool, billed_calls)?,
        },
    })
}

fn compute_category(
    logical_ms: Option<u64>,
    billed_ms: Option<u64>,
    table: Option<&CostTable>,
) -> Result<ComputeUsage, AccountingError> {
    Ok(ComputeUsage {
        samples: 1,
        logical_ms,
        billed_ms,
        cost: estimated_cost(table, UsageCategory::Compute, billed_ms)?,
    })
}

fn estimated_cost(
    table: Option<&CostTable>,
    category: UsageCategory,
    units: Option<u64>,
) -> Result<Option<CategoryCost>, AccountingError> {
    let (Some(table), Some(units)) = (table, units) else {
        return Ok(None);
    };
    Ok(table.estimate(category, units)?.map(|amount| CategoryCost {
        amount,
        source: CostSource::CostTable {
            version: table.effective.version.clone(),
            snapshot: table.effective.snapshot.clone(),
        },
    }))
}

fn clear_category_costs(categories: &mut UsageCategories) {
    categories.uncached_input.cost = None;
    categories.cache_write.cost = None;
    categories.cache_read.cost = None;
    categories.visible_output.cost = None;
    categories.reasoning.cost = None;
    categories.tool.cost = None;
    categories.compute.cost = None;
}

fn merge_token(
    left: &mut TokenUsageCategory,
    right: TokenUsageCategory,
) -> Result<(), AccountingError> {
    let left_samples = left.samples;
    merge_optional(
        &mut left.logical_tokens,
        left_samples,
        right.logical_tokens,
        right.samples,
    )?;
    merge_optional(
        &mut left.billed_tokens,
        left_samples,
        right.billed_tokens,
        right.samples,
    )?;
    merge_optional_cost(&mut left.cost, left_samples, right.cost, right.samples)?;
    left.samples = add(left.samples, right.samples)?;
    Ok(())
}

fn merge_tool(left: &mut ToolUsage, right: ToolUsage) -> Result<(), AccountingError> {
    let left_samples = left.samples;
    merge_optional(
        &mut left.logical_calls,
        left_samples,
        right.logical_calls,
        right.samples,
    )?;
    merge_optional(
        &mut left.billed_calls,
        left_samples,
        right.billed_calls,
        right.samples,
    )?;
    merge_optional(
        &mut left.duration_ms,
        left_samples,
        right.duration_ms,
        right.samples,
    )?;
    merge_optional_cost(&mut left.cost, left_samples, right.cost, right.samples)?;
    left.samples = add(left.samples, right.samples)?;
    Ok(())
}

fn merge_compute(left: &mut ComputeUsage, right: ComputeUsage) -> Result<(), AccountingError> {
    let left_samples = left.samples;
    merge_optional(
        &mut left.logical_ms,
        left_samples,
        right.logical_ms,
        right.samples,
    )?;
    merge_optional(
        &mut left.billed_ms,
        left_samples,
        right.billed_ms,
        right.samples,
    )?;
    merge_optional_cost(&mut left.cost, left_samples, right.cost, right.samples)?;
    left.samples = add(left.samples, right.samples)?;
    Ok(())
}

fn merge_speculation(
    left: &mut FailedSpeculationUsage,
    right: FailedSpeculationUsage,
) -> Result<(), AccountingError> {
    let left_samples = left.samples;
    merge_optional(
        &mut left.logical_attempts,
        left_samples,
        right.logical_attempts,
        right.samples,
    )?;
    merge_optional(
        &mut left.billed_attempts,
        left_samples,
        right.billed_attempts,
        right.samples,
    )?;
    merge_optional_cost(&mut left.cost, left_samples, right.cost, right.samples)?;
    left.samples = add(left.samples, right.samples)?;
    Ok(())
}

fn merge_optional(
    left: &mut Option<u64>,
    left_samples: u64,
    right: Option<u64>,
    right_samples: u64,
) -> Result<(), AccountingError> {
    if right_samples == 0 {
        return Ok(());
    }
    if left_samples == 0 {
        *left = right;
    } else {
        *left = match (*left, right) {
            (Some(left), Some(right)) => Some(add(left, right)?),
            _ => None,
        };
    }
    Ok(())
}

fn merge_optional_cost(
    left: &mut Option<CategoryCost>,
    left_samples: u64,
    right: Option<CategoryCost>,
    right_samples: u64,
) -> Result<(), AccountingError> {
    if right_samples == 0 {
        return Ok(());
    }
    if left_samples == 0 {
        *left = right;
    } else {
        *left = match (left.as_ref(), right.as_ref()) {
            (Some(left), Some(right)) => Some(add_cost(left, right)?),
            _ => None,
        };
    }
    Ok(())
}

fn add_cost(left: &CategoryCost, right: &CategoryCost) -> Result<CategoryCost, AccountingError> {
    Ok(CategoryCost {
        amount: left.amount.checked_add(&right.amount)?,
        source: if left.source == right.source {
            left.source.clone()
        } else {
            CostSource::Mixed
        },
    })
}

fn add(left: u64, right: u64) -> Result<u64, AccountingError> {
    left.checked_add(right)
        .ok_or(AccountingError::UsageOverflow)
}

#[cfg(test)]
mod usage_tests {
    use agentkit_core::{TokenUsage, Usage};

    use super::*;

    #[test]
    fn accounting_consumes_each_normalized_provider_category_once() {
        let provider = Usage::new(
            TokenUsage::new(50, 25)
                .with_cached_input_tokens(30)
                .with_cache_write_input_tokens(20)
                .with_reasoning_tokens(15),
        );
        let canonical = from_agentkit_usage(&provider);
        let envelope = UsageEnvelope::from_model_usage(
            Some(&canonical),
            &LogicalModelUsage::default(),
            ModelOutcome::Succeeded,
            true,
            SpeculationOutcome::None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(envelope.categories.uncached_input.billed_tokens, Some(50));
        assert_eq!(envelope.categories.cache_read.billed_tokens, Some(30));
        assert_eq!(envelope.categories.cache_write.billed_tokens, Some(20));
        assert_eq!(envelope.categories.visible_output.billed_tokens, Some(25));
        assert_eq!(envelope.categories.reasoning.billed_tokens, Some(15));
    }
}
