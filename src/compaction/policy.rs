//! Configurable automatic-compaction thresholds.

use serde::{Deserialize, Serialize};

/// Percentages are whole percentage points of the provider's context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CompactionConfig {
    pub threshold_percent: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u64>,
    pub precompute_margin_percent: u64,
    pub precompute_margin_tokens: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            threshold_percent: 80,
            max_context_tokens: None,
            precompute_margin_percent: 10,
            precompute_margin_tokens: 20_000,
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct RawCompactionConfig {
    threshold_percent: u64,
    max_context_tokens: Option<u64>,
    precompute_margin_percent: u64,
    precompute_margin_tokens: u64,
}

impl Default for RawCompactionConfig {
    fn default() -> Self {
        let config = CompactionConfig::default();
        Self {
            threshold_percent: config.threshold_percent,
            max_context_tokens: config.max_context_tokens,
            precompute_margin_percent: config.precompute_margin_percent,
            precompute_margin_tokens: config.precompute_margin_tokens,
        }
    }
}

// Deserialize through a table so empty arrays cannot be accepted as a
// default-filled struct by Serde's sequence representation.
impl<'de> Deserialize<'de> for CompactionConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let table = toml::Table::deserialize(deserializer)?;
        let raw: RawCompactionConfig = table.try_into().map_err(serde::de::Error::custom)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<RawCompactionConfig> for CompactionConfig {
    type Error = String;

    fn try_from(raw: RawCompactionConfig) -> Result<Self, Self::Error> {
        let config = Self {
            threshold_percent: raw.threshold_percent,
            max_context_tokens: raw.max_context_tokens,
            precompute_margin_percent: raw.precompute_margin_percent,
            precompute_margin_tokens: raw.precompute_margin_tokens,
        };
        config.validate()?;
        Ok(config)
    }
}

/// Token thresholds for a known context window. Callers must disable automatic
/// compaction when the context window is unknown or zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedThresholds {
    pub apply_tokens: u64,
    pub prepare_tokens: u64,
}

impl CompactionConfig {
    /// Validate policies constructed directly as well as deserialized policies.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=99).contains(&self.threshold_percent) {
            return Err("compaction.threshold_percent must be between 1 and 99".into());
        }
        if self.max_context_tokens == Some(0) {
            return Err("compaction.max_context_tokens must be positive".into());
        }
        if self.precompute_margin_percent > 100 {
            return Err("compaction.precompute_margin_percent must be between 0 and 100".into());
        }
        Ok(())
    }

    /// Resolve a validated policy with floor rounding and overflow-safe arithmetic.
    pub fn resolve(&self, window: u64) -> ResolvedThresholds {
        let percent = |value: u64| {
            ((u128::from(window) * u128::from(value)) / 100).min(u128::from(u64::MAX)) as u64
        };
        let apply_tokens =
            percent(self.threshold_percent).min(self.max_context_tokens.unwrap_or(u64::MAX));
        let lead = percent(self.precompute_margin_percent)
            .min(self.precompute_margin_tokens)
            .min(apply_tokens / 2);
        ResolvedThresholds {
            apply_tokens,
            prepare_tokens: apply_tokens - lead,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_macros,
    clippy::disallowed_methods
)]
mod tests {
    use super::*;

    #[test]
    fn defaults_rounding_and_extreme_windows() {
        let config = CompactionConfig::default();
        for (window, apply_tokens, prepare_tokens) in [
            (0, 0, 0),
            (1, 0, 0),
            (99, 79, 70),
            (100_000, 80_000, 70_000),
            (1_000_000, 800_000, 780_000),
            (
                u64::MAX,
                14_757_395_258_967_641_292,
                14_757_395_258_967_621_292,
            ),
        ] {
            assert_eq!(
                config.resolve(window),
                ResolvedThresholds {
                    apply_tokens,
                    prepare_tokens
                }
            );
        }
    }

    #[test]
    fn earliest_threshold_and_bounded_lead() {
        let mut config = CompactionConfig {
            threshold_percent: 55,
            max_context_tokens: Some(200_000),
            ..CompactionConfig::default()
        };
        assert_eq!(
            config.resolve(100_000),
            ResolvedThresholds {
                apply_tokens: 55_000,
                prepare_tokens: 45_000
            }
        );
        assert_eq!(
            config.resolve(1_000_000),
            ResolvedThresholds {
                apply_tokens: 200_000,
                prepare_tokens: 180_000
            }
        );
        config.max_context_tokens = Some(11);
        assert_eq!(
            config.resolve(100_000),
            ResolvedThresholds {
                apply_tokens: 11,
                prepare_tokens: 6
            }
        );
        config.precompute_margin_tokens = 0;
        assert_eq!(config.resolve(100_000).prepare_tokens, 11);
        config.precompute_margin_tokens = 20_000;
        config.precompute_margin_percent = 0;
        assert_eq!(config.resolve(100_000).prepare_tokens, 11);
    }

    #[test]
    fn serde_defaults_roundtrip_and_rejection() {
        let default = CompactionConfig::default();
        assert_eq!(toml::from_str::<CompactionConfig>("").unwrap(), default);
        let serialized = toml::to_string(&default).unwrap();
        assert_eq!(
            toml::from_str::<CompactionConfig>(&serialized).unwrap(),
            default
        );
        for invalid in [
            "threshold_percent = 0",
            "threshold_percent = 100",
            "threshold_percent = -1",
            "threshold_percent = 55.5",
            "threshold_percent = '55'",
            "max_context_tokens = 0",
            "max_context_tokens = -1",
            "precompute_margin_percent = 101",
            "precompute_margin_percent = -1",
            "precompute_margin_tokens = -1",
        ] {
            assert!(
                toml::from_str::<CompactionConfig>(invalid).is_err(),
                "{invalid}"
            );
        }
        for valid in [
            "threshold_percent = 1",
            "threshold_percent = 99",
            "precompute_margin_percent = 100",
            "precompute_margin_tokens = 0",
            "future = true",
            "threshold_percent = 55\nfuture = { enabled = true }",
        ] {
            assert!(toml::from_str::<CompactionConfig>(valid).is_ok(), "{valid}");
        }
    }
}
