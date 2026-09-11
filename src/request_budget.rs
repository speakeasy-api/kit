//! Process-wide provider request budget, resolved once before starting runtimes.
use std::{sync::OnceLock, time::Duration};

/// Total logical-request budget in seconds (1–3600), including streamed bodies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "u64")]
pub struct RequestBudget(u64);

impl TryFrom<u64> for RequestBudget {
    type Error = String;
    fn try_from(seconds: u64) -> Result<Self, Self::Error> {
        if (1..=3600).contains(&seconds) {
            Ok(Self(seconds))
        } else {
            Err("request budget must be between 1 and 3600 seconds".into())
        }
    }
}

impl std::str::FromStr for RequestBudget {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse::<u64>().map_err(|e| e.to_string())?.try_into()
    }
}

impl Default for RequestBudget {
    fn default() -> Self {
        Self(60)
    }
}

static BUDGET: OnceLock<RequestBudget> = OnceLock::new();

impl RequestBudget {
    pub fn seconds(self) -> u64 {
        self.0
    }

    /// Set once at process startup, before creating any provider or child.
    pub fn initialize(self) -> Result<(), String> {
        match BUDGET.set(self) {
            Ok(()) => Ok(()),
            Err(_) if BUDGET.get() == Some(&self) => Ok(()),
            Err(_) => Err("request budget already initialized with a different value".into()),
        }
    }

    pub(crate) fn current() -> Self {
        BUDGET.get().copied().unwrap_or_default()
    }

    pub(crate) fn resilience(self) -> agentkit_http::ResilienceConfig {
        agentkit_http::ResilienceConfig {
            retry_budget: Duration::from_secs(self.0),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_budget_changes_only_total_budget() {
        let default = agentkit_http::ResilienceConfig::default();
        assert_eq!(RequestBudget::default().resilience(), default);
        let mut expected = default;
        expected.retry_budget = Duration::from_secs(300);
        assert_eq!(RequestBudget::try_from(300).unwrap().resilience(), expected);
        for invalid in [0, 3601, u64::MAX] {
            assert!(RequestBudget::try_from(invalid).is_err());
        }
    }
}
