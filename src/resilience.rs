use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const MAX_CONFIG_DURATION: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// User-configurable retry and timeout policy for model-provider requests.
///
/// All duration fields are expressed in milliseconds. Optional timeout fields
/// are disabled when omitted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResilienceConfig {
    pub max_retries: usize,
    pub retry_budget_ms: u64,
    pub attempt_timeout_ms: Option<u64>,
    pub stream_idle_timeout_ms: Option<u64>,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl ResilienceConfig {
    pub(crate) fn agentkit_config(&self) -> Result<agentkit_http::ResilienceConfig, String> {
        self.try_into()
    }
}

impl TryFrom<&ResilienceConfig> for agentkit_http::ResilienceConfig {
    type Error = String;

    fn try_from(config: &ResilienceConfig) -> Result<Self, Self::Error> {
        if config.retry_budget_ms == 0 {
            return Err("resilience.retry_budget_ms must be greater than zero".into());
        }
        if config.attempt_timeout_ms == Some(0) {
            return Err("resilience.attempt_timeout_ms must be greater than zero when set".into());
        }
        if config.stream_idle_timeout_ms == Some(0) {
            return Err(
                "resilience.stream_idle_timeout_ms must be greater than zero when set".into(),
            );
        }
        if config.max_backoff_ms < config.initial_backoff_ms {
            return Err(
                "resilience.max_backoff_ms must be greater than or equal to resilience.initial_backoff_ms"
                    .into(),
            );
        }
        let retry_budget = checked_duration("retry_budget_ms", config.retry_budget_ms)?;
        let attempt_timeout = config
            .attempt_timeout_ms
            .map(|value| checked_duration("attempt_timeout_ms", value))
            .transpose()?;
        let stream_idle_timeout = config
            .stream_idle_timeout_ms
            .map(|value| checked_duration("stream_idle_timeout_ms", value))
            .transpose()?;
        let initial_backoff = checked_duration("initial_backoff_ms", config.initial_backoff_ms)?;
        let max_backoff = checked_duration("max_backoff_ms", config.max_backoff_ms)?;
        Ok(Self {
            max_retries: config.max_retries,
            retry_budget,
            attempt_timeout,
            stream_idle_timeout,
            initial_backoff,
            max_backoff,
        })
    }
}

fn checked_duration(field: &str, millis: u64) -> Result<Duration, String> {
    let duration = Duration::from_millis(millis);
    if duration > MAX_CONFIG_DURATION {
        return Err(format!(
            "resilience.{field} exceeds the maximum supported duration of 365 days"
        ));
    }
    Instant::now()
        .checked_add(duration)
        .ok_or_else(|| format!("resilience.{field} is too large for a monotonic deadline"))?;
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::ResilienceConfig;

    fn config() -> ResilienceConfig {
        ResilienceConfig {
            max_retries: 4,
            retry_budget_ms: 12_000,
            attempt_timeout_ms: Some(3_000),
            stream_idle_timeout_ms: None,
            initial_backoff_ms: 125,
            max_backoff_ms: 2_000,
        }
    }

    #[test]
    fn maps_named_millisecond_fields_to_agentkit() {
        let mapped = config().agentkit_config().unwrap();
        assert_eq!(mapped.max_retries, 4);
        assert_eq!(mapped.retry_budget, Duration::from_secs(12));
        assert_eq!(mapped.attempt_timeout, Some(Duration::from_secs(3)));
        assert_eq!(mapped.stream_idle_timeout, None);
        assert_eq!(mapped.initial_backoff, Duration::from_millis(125));
        assert_eq!(mapped.max_backoff, Duration::from_secs(2));
    }

    #[test]
    fn rejects_invalid_duration_relationships() {
        assert!(
            toml::from_str::<ResilienceConfig>(
                "max_retries = 1\nretry_budget_ms = 1000\ninitial_backoff_ms = 1\nmax_backoff_ms = 10\nretry_budget_seconds = 1\n"
            )
            .is_err()
        );

        let mut invalid = config();
        invalid.retry_budget_ms = 0;
        assert!(invalid.agentkit_config().is_err());

        let mut invalid = config();
        invalid.initial_backoff_ms = invalid.max_backoff_ms + 1;
        assert!(invalid.agentkit_config().is_err());
    }

    #[test]
    fn rejects_durations_that_cannot_form_monotonic_deadlines() {
        let mut invalid = config();
        invalid.retry_budget_ms = u64::MAX;
        let error = invalid.agentkit_config().unwrap_err();
        assert!(error.contains("retry_budget_ms"));
        assert!(error.contains("maximum supported duration"));
    }
}
