//! Rate-limit snapshot parsed from Cerebras' `x-ratelimit-*` response headers.

use agentkit_http::HeaderMap;

/// Snapshot of the rate-limit headers returned on the last response.
///
/// Cerebras documents the following keys (all optional, all present only when
/// the corresponding budget is enforced):
///
/// - `x-ratelimit-limit-requests-day`
/// - `x-ratelimit-remaining-requests-day`
/// - `x-ratelimit-reset-requests-day`
/// - `x-ratelimit-limit-tokens-minute`
/// - `x-ratelimit-remaining-tokens-minute`
/// - `x-ratelimit-reset-tokens-minute`
///
/// Any header present in the response that we don't recognise is dropped —
/// the adapter's contract is a structured view, not a raw header map.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RateLimitSnapshot {
    /// Daily request budget.
    pub requests_day_limit: Option<u64>,
    /// Remaining daily requests.
    pub requests_day_remaining: Option<u64>,
    /// Seconds until the daily budget resets.
    pub requests_day_reset: Option<String>,
    /// Per-minute token budget.
    pub tokens_minute_limit: Option<u64>,
    /// Remaining per-minute tokens.
    pub tokens_minute_remaining: Option<u64>,
    /// Seconds until the per-minute budget resets.
    pub tokens_minute_reset: Option<String>,
}

impl RateLimitSnapshot {
    /// Parses a `RateLimitSnapshot` from a `HeaderMap`. Missing fields are
    /// left as `None`; the snapshot is empty if no recognised keys are found.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let get_str = |name: &str| -> Option<String> {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };
        let get_u64 = |name: &str| -> Option<u64> { get_str(name).and_then(|s| s.parse().ok()) };

        Self {
            requests_day_limit: get_u64("x-ratelimit-limit-requests-day"),
            requests_day_remaining: get_u64("x-ratelimit-remaining-requests-day"),
            requests_day_reset: get_str("x-ratelimit-reset-requests-day"),
            tokens_minute_limit: get_u64("x-ratelimit-limit-tokens-minute"),
            tokens_minute_remaining: get_u64("x-ratelimit-remaining-tokens-minute"),
            tokens_minute_reset: get_str("x-ratelimit-reset-tokens-minute"),
        }
    }

    /// Whether any rate-limit header was parsed out.
    pub fn is_empty(&self) -> bool {
        self.requests_day_limit.is_none()
            && self.requests_day_remaining.is_none()
            && self.requests_day_reset.is_none()
            && self.tokens_minute_limit.is_none()
            && self.tokens_minute_remaining.is_none()
            && self.tokens_minute_reset.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_http::{HeaderMap, HeaderName, HeaderValue};

    #[test]
    fn parses_documented_headers() {
        let mut h = HeaderMap::new();
        h.insert(
            HeaderName::from_static("x-ratelimit-limit-requests-day"),
            HeaderValue::from_static("10000"),
        );
        h.insert(
            HeaderName::from_static("x-ratelimit-remaining-requests-day"),
            HeaderValue::from_static("9950"),
        );
        h.insert(
            HeaderName::from_static("x-ratelimit-reset-tokens-minute"),
            HeaderValue::from_static("42s"),
        );
        let s = RateLimitSnapshot::from_headers(&h);
        assert_eq!(s.requests_day_limit, Some(10_000));
        assert_eq!(s.requests_day_remaining, Some(9_950));
        assert_eq!(s.tokens_minute_reset.as_deref(), Some("42s"));
    }

    #[test]
    fn missing_headers_yield_empty_snapshot() {
        let s = RateLimitSnapshot::from_headers(&HeaderMap::new());
        assert!(s.is_empty());
    }
}
