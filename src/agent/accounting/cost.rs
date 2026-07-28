use serde::{Deserialize, Serialize};

use super::{AccountingError, MoneyMicros, UsageCategory};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CostRate {
    pub currency_micros: u64,
    pub per_units: u64,
}

impl CostRate {
    pub const fn new(currency_micros: u64, per_units: u64) -> Self {
        Self {
            currency_micros,
            per_units,
        }
    }

    fn cost(
        self,
        category: UsageCategory,
        currency: &str,
        units: u64,
    ) -> Result<MoneyMicros, AccountingError> {
        if self.per_units == 0 {
            return Err(AccountingError::InvalidCostRate);
        }
        let numerator = u128::from(self.currency_micros) * u128::from(units);
        if numerator % u128::from(self.per_units) != 0 {
            return Err(AccountingError::InexactCost { category, units });
        }
        MoneyMicros::new(
            currency,
            u64::try_from(numerator / u128::from(self.per_units))
                .map_err(|_| AccountingError::MoneyOverflow)?,
        )
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageRates {
    pub uncached_input: Option<CostRate>,
    pub cache_write: Option<CostRate>,
    pub cache_read: Option<CostRate>,
    pub visible_output: Option<CostRate>,
    pub reasoning: Option<CostRate>,
    pub tool: Option<CostRate>,
    pub compute: Option<CostRate>,
}

impl UsageRates {
    pub const fn get(&self, category: UsageCategory) -> Option<CostRate> {
        match category {
            UsageCategory::UncachedInput => self.uncached_input,
            UsageCategory::CacheWrite => self.cache_write,
            UsageCategory::CacheRead => self.cache_read,
            UsageCategory::VisibleOutput => self.visible_output,
            UsageCategory::Reasoning => self.reasoning,
            UsageCategory::Tool => self.tool,
            UsageCategory::Compute => self.compute,
            UsageCategory::FailedSpeculation => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CostTableSnapshot {
    pub version: String,
    pub provider: String,
    pub model: String,
    pub snapshot: String,
    pub currency: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CostTable {
    pub effective: CostTableSnapshot,
    pub rates: UsageRates,
}

impl CostTable {
    pub fn new(
        version: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        snapshot: impl Into<String>,
        currency: impl Into<String>,
        rates: UsageRates,
    ) -> Result<Self, AccountingError> {
        let currency = MoneyMicros::new(currency, 0)?.currency;
        if rates
            .uncached_input
            .into_iter()
            .chain(rates.cache_write)
            .chain(rates.cache_read)
            .chain(rates.visible_output)
            .chain(rates.reasoning)
            .chain(rates.tool)
            .chain(rates.compute)
            .any(|rate| rate.per_units == 0)
        {
            return Err(AccountingError::InvalidCostRate);
        }
        Ok(Self {
            effective: CostTableSnapshot {
                version: version.into(),
                provider: provider.into(),
                model: model.into(),
                snapshot: snapshot.into(),
                currency,
            },
            rates,
        })
    }

    pub fn estimate(
        &self,
        category: UsageCategory,
        units: u64,
    ) -> Result<Option<MoneyMicros>, AccountingError> {
        self.rates
            .get(category)
            .map(|rate| rate.cost(category, &self.effective.currency, units))
            .transpose()
    }
}
