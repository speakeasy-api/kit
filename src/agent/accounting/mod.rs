use std::fmt;

use serde::{Deserialize, Serialize};

mod cost;
mod envelope;
mod money;

pub use cost::{CostRate, CostTable, CostTableSnapshot, UsageRates};
pub use envelope::{
    CategoryCost, ComputeUsage, CostSource, FailedSpeculationUsage, LogicalModelUsage,
    ModelOutcome, SchedulerDebit, SpeculationOutcome, TokenUsageCategory, ToolMeasurement,
    ToolUsage, UsageCategories, UsageEnvelope,
};
pub use money::MoneyMicros;

pub const USAGE_ENVELOPE_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCategory {
    UncachedInput,
    CacheWrite,
    CacheRead,
    VisibleOutput,
    Reasoning,
    Tool,
    Compute,
    FailedSpeculation,
}

impl UsageCategory {
    pub const ALL: [Self; 8] = [
        Self::UncachedInput,
        Self::CacheWrite,
        Self::CacheRead,
        Self::VisibleOutput,
        Self::Reasoning,
        Self::Tool,
        Self::Compute,
        Self::FailedSpeculation,
    ];
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AccountingError {
    InvalidCurrency,
    InvalidMoney,
    MoneyOverflow,
    CurrencyMismatch { left: String, right: String },
    InvalidCostRate,
    InexactCost { category: UsageCategory, units: u64 },
    CostTableMismatch,
    UsageOverflow,
    InvalidReservationStatus,
    ReservationChargeMismatch,
}

impl fmt::Display for AccountingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCurrency => {
                formatter.write_str("currency must be a three-letter ASCII code")
            }
            Self::InvalidMoney => formatter.write_str(
                "money must be a non-negative decimal with at most six fractional digits",
            ),
            Self::MoneyOverflow => formatter.write_str("money exceeds the currency-micros range"),
            Self::CurrencyMismatch { left, right } => {
                write!(formatter, "cannot add {left} and {right} money")
            }
            Self::InvalidCostRate => formatter.write_str("cost rate denominator must be non-zero"),
            Self::InexactCost { category, units } => {
                write!(
                    formatter,
                    "cost rate for {category:?} cannot represent {units} units exactly in currency micros"
                )
            }
            Self::CostTableMismatch => {
                formatter.write_str("usage envelopes use different effective cost tables")
            }
            Self::UsageOverflow => formatter.write_str("usage total overflowed"),
            Self::InvalidReservationStatus => {
                formatter.write_str("reservation status does not match a terminal settlement")
            }
            Self::ReservationChargeMismatch => {
                formatter.write_str("reservation charge does not match the durable outcome")
            }
        }
    }
}

impl std::error::Error for AccountingError {}
