//! A hook sign-in hint as a session's user receives it: the session's own,
//! or one of its child agents' (relayed up the tree). One shape for every
//! level and client: Kit's structured `kit/hooks/auth` notification carries
//! it as is, and a marked notice carries it in its `_meta` under
//! [`super::HOOK_AUTH_META`], beside text for clients that show only text.
use serde::{Deserialize, Serialize};

/// Why a hint no longer has a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClearReason {
    /// Signed in: the backend needs nothing.
    SignedIn,
    /// The link expired, or consent was not given.
    Expired,
    /// Withdrawn: another target of the backend still needs a sign-in, or
    /// a problem no longer applies.
    Withdrawn,
    /// The backend was logged out.
    LoggedOut,
    /// A reason this Kit does not know (from a newer child): a clear all
    /// the same.
    #[serde(other)]
    Other,
}

/// The backend of an overflow hint: more of an agent's backends need a
/// sign-in than Kit held for it.
pub(crate) const MORE_BACKENDS: &str = "*";

/// One hook sign-in hint. Every field is independent: a link can carry a
/// problem beside it, and a clear says why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HookHint {
    /// The backend id (within its hooks file).
    pub(crate) backend: String,
    /// The child agent the link is for, as a path from this session: `a`,
    /// or `a/b` for a's own child b. `None` for the session's own hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    /// Where to approve Kit, while the link works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
    /// When the link stops working, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<u64>,
    /// The link no longer applies.
    #[serde(default)]
    pub(crate) cleared: bool,
    /// Why, once cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<ClearReason>,
    /// What went wrong, as a clause after "Hook backend `<id>` ".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) problem: Option<String>,
}

/// Bounds a hint from a child is held to on arrival.
const MAX_BACKEND: usize = 256;
const MAX_AGENT: usize = 1024;
const MAX_URL: usize = 8 * 1024;
const MAX_PROBLEM: usize = 1024;

/// Text that shows as written: no control characters (ESC, BEL, NUL...) and
/// no bidirectional controls that would reorder it.
fn displayable(text: &str) -> bool {
    !text.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
    })
}

fn plain(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && displayable(text)
}

/// A backend id that can be put in a suggested shell command, or between
/// backticks, as is: not an option (`-…`), no backtick or shell syntax.
fn shell_safe(text: &str) -> bool {
    !text.starts_with('-')
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

/// `text` as code in notice text, or quoted when backticks would break.
fn code(text: &str) -> String {
    if text.contains('`') {
        format!("\"{text}\"")
    } else {
        format!("`{text}`")
    }
}

impl HookHint {
    /// Accepts a hint from a child only if every field is well formed.
    pub(crate) fn validated(self) -> Option<Self> {
        let agent_ok = self.agent.as_deref().is_none_or(|agent| {
            agent.len() <= MAX_AGENT && agent.split('/').all(|segment| plain(segment, MAX_AGENT))
        });
        let url_ok = self.url.as_deref().is_none_or(|url| {
            url.starts_with("https://")
                && url.len() <= MAX_URL * 2
                && !url.contains(char::is_whitespace)
                && displayable(url)
        });
        let consistent = self.cleared || self.reason.is_none();
        (plain(&self.backend, MAX_BACKEND)
            && agent_ok
            && url_ok
            && self
                .problem
                .as_deref()
                .is_none_or(|problem| plain(problem, MAX_PROBLEM))
            && consistent)
            .then_some(self)
    }

    /// The `_meta` of a marked notice carrying this hint.
    pub(crate) fn mark(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::from_iter([(
            super::HOOK_AUTH_META.to_owned(),
            serde_json::to_value(self).unwrap_or_default(),
        )])
    }

    /// Text for a client that shows only text: a link, a problem, or that
    /// the backend is signed in. Other clears are not worth a line (the
    /// mark still carries them). The link is shown whole whenever it fits;
    /// a longer one is named, with what to run instead.
    pub(crate) fn notice_text(&self) -> Option<String> {
        let label = self
            .agent
            .as_ref()
            .map_or_else(String::new, |agent| format!("Agent {agent}: "));
        if self.backend == MORE_BACKENDS {
            return self
                .problem
                .as_ref()
                .map(|problem| format!("{label}More hook backends need you to sign in {problem}"));
        }
        let backend = &self.backend;
        let name = code(backend);
        let link = match &self.url {
            Some(url) if !self.cleared && url.len() <= MAX_URL => Some(format!(
                "Hook backend {name} needs you to sign in. Open {url} to approve Kit."
            )),
            Some(_) if !self.cleared => {
                // The id goes into a command only if it cannot change it.
                let command = if shell_safe(backend) {
                    format!("kit auth login hooks {backend}")
                } else {
                    "kit auth login hooks".to_owned()
                };
                Some(format!(
                    "Hook backend {name} needs you to sign in; its link is too long to show here. Run `{command}` in its workspace."
                ))
            }
            _ => None,
        };
        let problem = self.problem.as_ref().filter(|_| !self.cleared);
        match (link, problem) {
            (Some(link), Some(problem)) => {
                Some(format!("{label}{link} Also, the backend {problem}"))
            }
            (Some(link), None) => Some(format!("{label}{link}")),
            (None, Some(problem)) => Some(format!("{label}Hook backend {name} {problem}")),
            (None, None) if self.reason == Some(ClearReason::SignedIn) => {
                Some(format!("{label}Hook backend {name} is signed in."))
            }
            (None, None) => None,
        }
    }
}

/// An overflow hint: more of a child's backends need a sign-in than Kit
/// held for it before it had an owner. Relaying labels it with the agent.
pub(crate) fn more_backends() -> HookHint {
    HookHint {
        backend: MORE_BACKENDS.to_owned(),
        agent: None,
        url: None,
        expires_at: None,
        cleared: false,
        reason: None,
        problem: Some(
            "than Kit could pass on. Run `kit auth status hooks` in the agent's workspace to see them."
                .to_owned(),
        ),
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

    fn link(url: &str) -> HookHint {
        HookHint {
            backend: "com.example".into(),
            agent: Some("scout".into()),
            url: Some(url.into()),
            expires_at: None,
            cleared: false,
            reason: None,
            problem: None,
        }
    }

    #[test]
    fn a_link_that_fits_is_shown_whole_and_a_longer_one_names_what_to_run() {
        // Long, but within the link's own budget: whole, whatever the text.
        let url = format!("https://auth.example/{}", "a".repeat(4000));
        let text = link(&url).notice_text().unwrap();
        assert!(text.contains(&url), "{text}");
        assert!(text.starts_with("Agent scout: Hook backend `com.example`"));
        // Too long to show: no part of it, and what to run instead.
        let url = format!("https://auth.example/{}", "b".repeat(MAX_URL));
        let text = link(&url).notice_text().unwrap();
        assert!(!text.contains("https://auth.example/b"), "{text}");
        assert!(text.contains("kit auth login hooks com.example"), "{text}");
    }

    #[test]
    fn an_unknown_reason_is_a_clear_and_an_unsafe_id_never_enters_a_command() {
        let hint: HookHint = serde_json::from_value(serde_json::json!({
            "backend": "x", "cleared": true, "reason": "revoked_by_policy"
        }))
        .unwrap();
        assert_eq!(hint.reason, Some(ClearReason::Other));
        assert!(hint.validated().is_some());
        let long = format!("https://auth.example/{}", "b".repeat(MAX_URL));
        let hint = HookHint {
            backend: "x;rm -rf ~".into(),
            ..link(&long)
        };
        let text = hint.notice_text().unwrap();
        assert!(text.contains("Run `kit auth login hooks` in"), "{text}");
        for backend in ["-rf", "x`whoami`"] {
            let hint = HookHint {
                backend: backend.into(),
                ..link(&long)
            };
            let text = hint.notice_text().unwrap();
            assert!(text.contains("Run `kit auth login hooks` in"), "{text}");
            assert!(!text.contains("``"), "{text}");
        }
    }

    #[test]
    fn malformed_hints_are_refused() {
        assert!(link("https://auth.example/a").validated().is_some());
        for bad in [
            HookHint {
                backend: String::new(),
                ..link("https://auth.example/a")
            },
            HookHint {
                agent: Some(String::new()),
                ..link("https://auth.example/a")
            },
            HookHint {
                agent: Some("a//b".into()),
                ..link("https://auth.example/a")
            },
            link("javascript:alert(1)"),
            link("https://auth.example/a b"),
            link("https://auth.example/a\u{1b}[2J"),
            link("https://auth.example/a\u{7}"),
            link("https://auth.example/\u{202e}gnp.exe"),
            HookHint {
                backend: "x\u{2066}y".into(),
                ..link("https://auth.example/a")
            },
            HookHint {
                backend: "x\u{061c}y".into(),
                ..link("https://auth.example/a")
            },
            HookHint {
                problem: Some("line\u{2028}break".into()),
                ..link("https://auth.example/a")
            },
            HookHint {
                agent: Some("a\u{2029}b".into()),
                ..link("https://auth.example/a")
            },
            HookHint {
                problem: Some("line\nbreak".into()),
                ..link("https://auth.example/a")
            },
            HookHint {
                reason: Some(ClearReason::SignedIn),
                ..link("https://auth.example/a")
            },
        ] {
            assert!(bad.clone().validated().is_none(), "{bad:?}");
        }
    }
}
