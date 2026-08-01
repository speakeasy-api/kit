use serde::{Deserialize, Serialize};

use super::AccountingError;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MoneyMicros {
    pub currency: String,
    pub micros: u64,
}

impl MoneyMicros {
    pub fn new(currency: impl Into<String>, micros: u64) -> Result<Self, AccountingError> {
        let currency = currency.into();
        if currency.len() != 3 || !currency.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return Err(AccountingError::InvalidCurrency);
        }
        Ok(Self {
            currency: currency.to_ascii_uppercase(),
            micros,
        })
    }

    pub fn from_decimal(
        currency: impl Into<String>,
        amount: &str,
    ) -> Result<Self, AccountingError> {
        let (whole, fraction) = amount
            .split_once('.')
            .map_or((amount, ""), |(whole, fraction)| (whole, fraction));
        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.len() > 6
        {
            return Err(AccountingError::InvalidMoney);
        }
        let whole = whole
            .parse::<u64>()
            .map_err(|_| AccountingError::MoneyOverflow)?
            .checked_mul(1_000_000)
            .ok_or(AccountingError::MoneyOverflow)?;
        let fraction = if fraction.is_empty() {
            0
        } else {
            fraction
                .parse::<u64>()
                .map_err(|_| AccountingError::InvalidMoney)?
                * 10_u64.pow(u32::try_from(6 - fraction.len()).expect("fraction length is bounded"))
        };
        Self::new(
            currency,
            whole
                .checked_add(fraction)
                .ok_or(AccountingError::MoneyOverflow)?,
        )
    }

    pub fn checked_add(&self, other: &Self) -> Result<Self, AccountingError> {
        if self.currency != other.currency {
            return Err(AccountingError::CurrencyMismatch {
                left: self.currency.clone(),
                right: other.currency.clone(),
            });
        }
        Self::new(
            self.currency.clone(),
            self.micros
                .checked_add(other.micros)
                .ok_or(AccountingError::MoneyOverflow)?,
        )
    }

    pub(crate) fn is_canonical(&self) -> bool {
        self.currency.len() == 3 && self.currency.bytes().all(|byte| byte.is_ascii_uppercase())
    }
}
