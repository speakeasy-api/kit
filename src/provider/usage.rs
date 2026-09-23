//! Read-only provider quota reporting shared by the CLI and terminal UI.
use super::{OpenRouterApiKey, adapter, openai_auth};
use crate::credentials::CredentialStorage;
use serde::Deserialize;
use std::{
    io::Read,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BYTES: u64 = 256 * 1024;
const PROVIDERS: [&str; 2] = ["openai", "openrouter"];

/// Fetch all authenticated supported providers, or one explicitly named provider.
/// Blocking: callers on async runtimes must use a blocking worker.
pub fn fetch_usage(
    provider: Option<&str>,
    storage: &CredentialStorage,
    explicit_key: Option<&OpenRouterApiKey>,
) -> Result<String, String> {
    let providers = match provider {
        None => PROVIDERS.to_vec(),
        Some("openai" | "openai-subscription") => vec!["openai"],
        Some("openrouter") => vec!["openrouter"],
        Some(_) => {
            return Err(
                "Unknown or unsupported usage provider. Supported: openai, openrouter.".into(),
            );
        }
    };
    let results = std::thread::scope(|scope| {
        let workers: Vec<_> = providers
            .iter()
            .map(|name| {
                scope.spawn(move || {
                    let result = match *name {
                        "openai" => openai(storage),
                        _ => openrouter(storage, explicit_key),
                    };
                    (*name, result)
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .unwrap_or(("provider", Err("Usage worker failed.".into())))
            })
            .collect::<Vec<_>>()
    });
    let mut sections = Vec::new();
    let mut successful = false;
    for (name, result) in results {
        match result {
            Ok(Some(text)) => {
                successful = true;
                sections.push(text);
            }
            Ok(None) if provider.is_some() => sections.push(format!(
                "{name}: not authenticated. Run `kit auth login {name}`{}.",
                if name == "openrouter" {
                    " or set OPENROUTER_API_KEY"
                } else {
                    ""
                }
            )),
            Ok(None) => {}
            Err(error) => sections.push(format!("{name}: {error}")),
        }
    }
    if sections.is_empty() {
        return Err("No authenticated supported providers. Run `kit auth login openai` or `kit auth login openrouter`.".into());
    }
    let report = sections.join("\n\n");
    if successful { Ok(report) } else { Err(report) }
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Could not initialize usage client.".into())
}

fn decode<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
) -> Result<T, String> {
    if !response.status().is_success() {
        return Err(format!(
            "Usage request failed (HTTP {}). Try authenticating again if access was denied.",
            response.status().as_u16()
        ));
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read usage response.".to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("Usage response exceeded size limit.".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "Provider returned an invalid usage response.".into())
}

fn send(request: reqwest::blocking::RequestBuilder) -> Result<reqwest::blocking::Response, String> {
    request
        .send()
        .map_err(|_| "Usage request failed or timed out; retry later.".into())
}

fn openai(storage: &CredentialStorage) -> Result<Option<String>, String> {
    if !openai_auth::has_credentials(storage)
        .map_err(|_| "Could not read OpenAI credentials.".to_string())?
    {
        return Ok(None);
    }
    let deadline = Instant::now() + TIMEOUT;
    let token = openai_auth::access_token(storage, deadline).map_err(|_| {
        "Could not refresh OpenAI authentication. Run `kit auth login openai`.".to_string()
    })?;
    let client = client()?;
    let request = |token: &openai_auth::TokenRecord| {
        let mut req = client
            .get("https://chatgpt.com/backend-api/wham/usage")
            .bearer_auth(token.access_token())
            .timeout(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            );
        if let Some(account) = token.account_id() {
            req = req.header("ChatGPT-Account-Id", account);
        }
        req
    };
    let mut response = send(request(&token))?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        let refreshed =
            openai_auth::refresh_after_unauthorized(storage, token.access_token(), deadline)
                .map_err(|_| {
                    "OpenAI authentication expired. Run `kit auth login openai`.".to_string()
                })?;
        response = send(request(&refreshed))?;
    }
    decode::<OpenAiUsage>(response).map(|usage| Some(usage.render()))
}

fn openrouter(
    storage: &CredentialStorage,
    explicit_key: Option<&OpenRouterApiKey>,
) -> Result<Option<String>, String> {
    let key = adapter::resolve_openrouter_key(storage, explicit_key, |name| std::env::var(name))
        .map_err(|_| "Could not resolve OpenRouter credentials.".to_string())?;
    let Some((key, _)) = key else {
        return Ok(None);
    };
    let key = OpenRouterApiKey::new(key);
    let response = send(
        client()?
            .get("https://openrouter.ai/api/v1/key")
            .bearer_auth(key.as_str()),
    )?;
    decode::<OpenRouterResponse>(response).map(|usage| Some(usage.data.render()))
}

// Never print arbitrary provider text or control sequences into the terminal.
fn label(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '/'))
        .take(100)
        .collect()
}

#[derive(Deserialize)]
struct OpenAiUsage {
    plan_type: String,
    rate_limit: Option<RateLimit>,
    additional_rate_limits: Option<Vec<AdditionalLimit>>,
}
#[derive(Deserialize)]
struct AdditionalLimit {
    limit_name: String,
    metered_feature: String,
    rate_limit: Option<RateLimit>,
}
#[derive(Deserialize)]
struct RateLimit {
    allowed: bool,
    limit_reached: bool,
    primary_window: Option<Window>,
    secondary_window: Option<Window>,
}
#[derive(Deserialize)]
struct Window {
    used_percent: i32,
    limit_window_seconds: i32,
    reset_after_seconds: i32,
    reset_at: i64,
}
impl OpenAiUsage {
    fn render(&self) -> String {
        let mut lines = vec![format!(
            "OpenAI — ChatGPT subscription/Codex (plan: {})",
            label(&self.plan_type)
        )];
        render_limits(&mut lines, "Main", self.rate_limit.as_ref());
        for extra in self.additional_rate_limits.iter().flatten() {
            render_limits(
                &mut lines,
                &format!(
                    "{} ({})",
                    label(&extra.limit_name),
                    label(&extra.metered_feature)
                ),
                extra.rate_limit.as_ref(),
            );
        }
        lines.join("\n")
    }
}
fn render_limits(lines: &mut Vec<String>, name: &str, limit: Option<&RateLimit>) {
    let Some(limit) = limit else {
        lines.push(format!("{name}: quota not reported"));
        return;
    };
    lines.push(format!(
        "{name}: allowed={}, limit reached={}",
        limit.allowed, limit.limit_reached
    ));
    for (name, window) in [
        ("Primary", &limit.primary_window),
        ("Secondary", &limit.secondary_window),
    ] {
        if let Some(w) = window {
            let remaining = if (0..=100).contains(&w.used_percent) {
                format!("{}% remaining", 100 - w.used_percent)
            } else {
                "remaining unknown (invalid reported percentage)".into()
            };
            let countdown = if w.reset_after_seconds >= 0 {
                format!("in {}", duration_label(w.reset_after_seconds))
            } else {
                "countdown unknown".into()
            };
            let reset_at = if (0..=253_402_300_799).contains(&w.reset_at) {
                crate::session::timestamp_rfc3339(w.reset_at as u64 * 1_000)
                    .replace('T', " ")
                    .replace(".000Z", " UTC")
            } else {
                "reset time unknown".into()
            };
            lines.push(format!(
                "  {name} ({}): {remaining}; resets {countdown} ({reset_at})",
                window_label(w.limit_window_seconds),
            ));
        }
    }
    if limit.primary_window.is_none() && limit.secondary_window.is_none() {
        lines.push("  Windows not reported".into());
    }
}
// Keep fixed-duration windows distinct from calendar-based billing periods.
fn window_label(seconds: i32) -> String {
    if seconds <= 0 {
        return "unknown interval".into();
    }
    if seconds == 7 * 86_400 {
        return "weekly".into();
    }
    for (unit, label) in [(86_400, "day"), (3_600, "hour"), (60, "minute")] {
        if seconds % unit == 0 {
            return format!("{}-{label}", seconds / unit);
        }
    }
    duration_label(seconds)
}

fn duration_label(mut seconds: i32) -> String {
    let mut parts = Vec::new();
    for (unit, suffix) in [(86_400, "d"), (3_600, "h"), (60, "m"), (1, "s")] {
        let count = seconds / unit;
        if count > 0 {
            parts.push(format!("{count}{suffix}"));
        }
        seconds %= unit;
    }
    if parts.is_empty() {
        "0s".into()
    } else {
        parts.join(" ")
    }
}

#[derive(Deserialize)]
struct OpenRouterResponse {
    data: OpenRouterUsage,
}
#[derive(Deserialize)]
struct OpenRouterUsage {
    limit: Option<f64>,
    limit_remaining: Option<f64>,
    limit_reset: Option<String>,
    include_byok_in_limit: bool,
    usage: f64,
    usage_daily: f64,
    usage_weekly: f64,
    usage_monthly: f64,
    byok_usage: f64,
    byok_usage_daily: f64,
    byok_usage_weekly: f64,
    byok_usage_monthly: f64,
}
impl OpenRouterUsage {
    fn render(&self) -> String {
        let cap = self
            .limit
            .map(|n| format!("${n:.4}"))
            .unwrap_or_else(|| "No key cap (not unlimited account credits)".into());
        let remaining = self
            .limit_remaining
            .map(|n| format!("${n:.4}"))
            .unwrap_or_else(|| {
                if self.limit.is_none() {
                    "no key cap".into()
                } else {
                    "not reported".into()
                }
            });
        let recurrence = self
            .limit_reset
            .as_deref()
            .map(label)
            .unwrap_or_else(|| "nonrecurring".into());
        format!(
            "OpenRouter — API key usage (USD)\nKey cap: {cap}; remaining: {remaining}\nBudget recurrence: {recurrence}; next reset: not reported\nBYOK included in cap: {}\nSpend: all time ${:.4}; current UTC day ${:.4}; week (Mon–Sun) ${:.4}; calendar month ${:.4}\nBYOK spend: all time ${:.4}; current UTC day ${:.4}; week (Mon–Sun) ${:.4}; calendar month ${:.4}",
            self.include_byok_in_limit,
            self.usage,
            self.usage_daily,
            self.usage_weekly,
            self.usage_monthly,
            self.byok_usage,
            self.byok_usage_daily,
            self.byok_usage_weekly,
            self.byok_usage_monthly
        )
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn subscription_reports_every_window_and_tolerates_future_plans() {
        let window = json!({"used_percent":42,"limit_window_seconds":123,"reset_after_seconds":17,"reset_at":2000000000});
        let usage: OpenAiUsage = serde_json::from_value(json!({
            "plan_type":"future-plan", "unknown":true,
            "rate_limit":{"allowed":true,"limit_reached":false,"primary_window":window,"secondary_window":window},
            "additional_rate_limits":[{"limit_name":"Extra", "metered_feature":"feature", "rate_limit":{"allowed":false,"limit_reached":true,"primary_window":window}}]
        })).unwrap();
        let text = usage.render();
        assert!(text.contains("future-plan"));
        assert_eq!(text.matches("2m 3s").count(), 3);
        assert!(text.contains("Extra (feature): allowed=false"));
        assert!(text.contains("58% remaining; resets in 17s"));
        assert!(text.contains("2033-05-18 03:33:20 UTC"));
    }

    #[test]
    fn subscription_uses_honest_intervals_and_validates_remaining() {
        for (seconds, interval) in [
            (18_000, "5-hour"),
            (604_800, "weekly"),
            (604_801, "7d 1s"),
            (2_592_000, "30-day"),
            (90, "1m 30s"),
            (7_200, "2-hour"),
            (120, "2-minute"),
            (1, "1s"),
            (0, "unknown interval"),
            (-1, "unknown interval"),
        ] {
            for percent in [i32::MIN, -1, 0, 42, 100, 101, i32::MAX] {
                let usage: OpenAiUsage = serde_json::from_value(json!({
                    "plan_type":"test",
                    "rate_limit":{"allowed":true,"limit_reached":false,
                        "primary_window":{"used_percent":percent,
                            "limit_window_seconds":seconds,"reset_after_seconds":90061,
                            "reset_at":1709164800}}
                }))
                .unwrap();
                let text = usage.render();
                assert!(text.contains(&format!("Primary ({interval}):")), "{text}");
                assert!(text.contains("resets in 1d 1h 1m 1s (2024-02-29 00:00:00 UTC)"));
                if (0..=100).contains(&percent) {
                    assert!(text.contains(&format!("{}% remaining", 100 - percent)));
                } else {
                    assert!(text.contains("remaining unknown (invalid reported percentage)"));
                    assert!(!text.contains("% remaining"));
                }
            }
        }
    }

    #[test]
    fn reset_values_are_validated_without_overflow_or_fabricated_dates() {
        for (seconds, timestamp, expected) in [
            (0, 0, "resets in 0s (1970-01-01 00:00:00 UTC)"),
            (-1, -1, "resets countdown unknown (reset time unknown)"),
            (0, i64::MAX, "resets in 0s (reset time unknown)"),
            (0, 253_402_300_799, "resets in 0s (9999-12-31 23:59:59 UTC)"),
        ] {
            let usage: OpenAiUsage = serde_json::from_value(json!({
                "plan_type":"test",
                "rate_limit":{"allowed":true,"limit_reached":false,
                    "primary_window":{"used_percent":0,"limit_window_seconds":18000,
                        "reset_after_seconds":seconds,"reset_at":timestamp}}
            }))
            .unwrap();
            assert!(usage.render().contains(expected));
        }
    }

    #[test]
    fn absent_quota_is_not_unlimited() {
        let usage: OpenAiUsage = serde_json::from_value(
            json!({"plan_type":"unknown","rate_limit":null,"additional_rate_limits":null}),
        )
        .unwrap();
        assert!(usage.render().contains("quota not reported"));
        assert!(!usage.render().contains("unlimited"));
    }

    fn router() -> OpenRouterUsage {
        serde_json::from_value(json!({"limit":10,"limit_remaining":8,"limit_reset":"daily","include_byok_in_limit":false,"usage":100,"usage_daily":2,"usage_weekly":12,"usage_monthly":40,"byok_usage":1,"byok_usage_daily":0,"byok_usage_weekly":0,"byok_usage_monthly":1})).unwrap()
    }

    #[test]
    fn recurring_key_uses_reported_remaining_not_lifetime_subtraction() {
        let text = router().render();
        assert!(text.contains("remaining: $8.0000"));
        assert!(text.contains("all time $100.0000"));
        assert!(text.contains("daily; next reset: not reported"));
    }

    #[test]
    fn uncapped_key_does_not_claim_unlimited_credits() {
        let mut usage = router();
        usage.limit = None;
        usage.limit_remaining = None;
        usage.limit_reset = None;
        let text = usage.render();
        assert!(text.contains("No key cap (not unlimited account credits)"));
        assert!(text.contains("remaining: no key cap"));
        assert!(text.contains("nonrecurring; next reset: not reported"));
    }

    #[test]
    fn rejects_unsupported_without_authentication() {
        let error = fetch_usage(Some("speakeasy"), &CredentialStorage::Memory, None).unwrap_err();
        assert!(error.contains("unsupported"));
    }

    #[test]
    fn explicit_missing_subscription_explains_login() {
        let error = fetch_usage(Some("openai"), &CredentialStorage::Memory, None).unwrap_err();
        assert!(error.contains("kit auth login openai"));
    }

    #[test]
    fn labels_cannot_inject_terminal_controls() {
        assert_eq!(label("hello\n\x1b[31m"), "hello31m");
        assert_eq!(label(&"a".repeat(200)).len(), 100);
    }

    #[test]
    fn http_errors_and_payloads_are_bounded_and_redacted() {
        use std::{io::Write, net::TcpListener};
        for (status, body, expected) in [
            ("401 Unauthorized", "secret".to_owned(), "HTTP 401"),
            ("200 OK", "secret".to_owned(), "invalid usage response"),
            ("200 OK", "x".repeat(MAX_BYTES as usize + 1), "size limit"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                let _ = socket.read(&mut request);
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
            let response = client()
                .unwrap()
                .get(format!("http://{address}"))
                .send()
                .unwrap();
            let error = decode::<OpenAiUsage>(response).err().unwrap();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("secret"));
            server.join().unwrap();
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod credential_tests {
    use super::*;

    #[test]
    fn effective_key_prefers_explicit_then_environment_and_skips_missing() {
        let explicit = OpenRouterApiKey::new("explicit-test");
        let env = |_: &str| Ok("environment-test".to_string());
        let (key, _) =
            adapter::resolve_openrouter_key(&CredentialStorage::Memory, Some(&explicit), env)
                .unwrap()
                .unwrap();
        assert_eq!(key, "explicit-test");
        let (key, _) = adapter::resolve_openrouter_key(&CredentialStorage::Memory, None, env)
            .unwrap()
            .unwrap();
        assert_eq!(key, "environment-test");
        assert!(
            adapter::resolve_openrouter_key(&CredentialStorage::Memory, None, |_| Err(
                std::env::VarError::NotPresent
            ))
            .unwrap()
            .is_none()
        );
    }
}
