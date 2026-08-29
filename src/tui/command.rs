//! Slash commands understood by the terminal client.
//!
//! Parsing lives here so submission behavior and prompt highlighting use the
//! same command registry and token-boundary rules.

use std::ops::Range;

#[derive(Clone, Copy)]
enum Kind {
    New,
    Resume,
    Sessions,
    Close,
    Model,
    Effort,
    Agents,
}

struct Spec {
    token: &'static str,
    kind: Kind,
}

// Agent-advertised commands remain ordinary prompts. Only these commands are
// interpreted by the client itself.
const LOCAL_COMMANDS: &[Spec] = &[
    Spec {
        token: "/new",
        kind: Kind::New,
    },
    Spec {
        token: "/resume",
        kind: Kind::Resume,
    },
    Spec {
        token: "/sessions",
        kind: Kind::Sessions,
    },
    Spec {
        token: "/close",
        kind: Kind::Close,
    },
    Spec {
        token: "/model",
        kind: Kind::Model,
    },
    Spec {
        token: "/effort",
        kind: Kind::Effort,
    },
    Spec {
        token: "/agents",
        kind: Kind::Agents,
    },
];

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed<'a> {
    New { prompt: Option<&'a str> },
    Resume { session_id: Option<&'a str> },
    Sessions,
    Close,
    Model { query: Option<&'a str> },
    Effort { value: Option<&'a str> },
    Agents,
    Prompt(&'a str),
}

fn token(input: &str) -> (&str, usize) {
    let token_end = input
        .char_indices()
        .find_map(|(index, character)| character.is_whitespace().then_some(index))
        .unwrap_or(input.len());
    (&input[..token_end], token_end)
}

fn recognized_local(input: &str) -> Option<(&Spec, usize)> {
    let (token, token_end) = token(input);
    LOCAL_COMMANDS
        .iter()
        .find(|spec| spec.token == token)
        .map(|spec| (spec, token_end))
}

pub fn parse(input: &str) -> Parsed<'_> {
    let Some((spec, token_end)) = recognized_local(input) else {
        return Parsed::Prompt(input);
    };
    let remainder = input[token_end..].trim_start();
    let prompt = (!remainder.is_empty()).then_some(remainder);
    match spec.kind {
        Kind::New => Parsed::New { prompt },
        Kind::Resume => Parsed::Resume { session_id: prompt },
        Kind::Sessions => Parsed::Sessions,
        Kind::Close => Parsed::Close,
        Kind::Model => Parsed::Model { query: prompt },
        Kind::Effort => Parsed::Effort { value: prompt },
        Kind::Agents if prompt.is_none() => Parsed::Agents,
        Kind::Agents => Parsed::Prompt(input),
    }
}

/// Byte range of a local or agent-advertised command token.
pub fn known_token(input: &str, advertised: &[String]) -> Option<Range<usize>> {
    if let Some((spec, token_end)) = recognized_local(input) {
        if matches!(spec.kind, Kind::Agents) && !input[token_end..].trim().is_empty() {
            return None;
        }
        return Some(0..token_end);
    }
    let (token, token_end) = token(input);
    let name = token.strip_prefix('/')?;
    advertised
        .iter()
        .any(|command| !command.is_empty() && command == name)
        .then_some(0..token_end)
}

#[cfg(test)]
mod tests {
    use super::{Parsed, known_token, parse};

    #[test]
    fn parses_commands_with_or_without_a_following_prompt() {
        assert_eq!(parse("/new"), Parsed::New { prompt: None });
        assert_eq!(
            parse("/resume session-1"),
            Parsed::Resume {
                session_id: Some("session-1")
            }
        );
        assert_eq!(parse("/sessions"), Parsed::Sessions);
        assert_eq!(parse("/close"), Parsed::Close);
        assert_eq!(parse("/model"), Parsed::Model { query: None });
        assert_eq!(parse("/effort"), Parsed::Effort { value: None });
        assert_eq!(
            parse("/effort high"),
            Parsed::Effort {
                value: Some("high")
            }
        );
        assert_eq!(
            parse("/model   sonnet"),
            Parsed::Model {
                query: Some("sonnet")
            }
        );
        assert_eq!(
            parse("/new   start here"),
            Parsed::New {
                prompt: Some("start here")
            }
        );
        assert_eq!(
            parse("/new\nfirst line\nsecond line"),
            Parsed::New {
                prompt: Some("first line\nsecond line")
            }
        );
    }

    #[test]
    fn requires_an_exact_command_token_and_preserves_unknown_commands() {
        for input in ["/newer", "/new/path", "/models", " /new", "/unknown arg"] {
            assert_eq!(parse(input), Parsed::Prompt(input));
            assert_eq!(known_token(input, &[]), None);
        }
        assert_eq!(known_token("/new prompt", &[]), Some(0..4));
        assert_eq!(known_token("/sessions", &[]), Some(0..9));
        assert_eq!(known_token("/model sonnet", &[]), Some(0..6));
        assert_eq!(known_token("/effort high", &[]), Some(0..7));
    }

    #[test]
    fn agents_is_an_exact_highlighted_local_command() {
        assert_eq!(parse("/agents"), Parsed::Agents);
        assert_eq!(known_token("/agents", &[]), Some(0..7));
        for input in ["/agents now", "/agentsx", " /agents"] {
            assert_eq!(parse(input), Parsed::Prompt(input));
        }
        assert_eq!(known_token("/agents now", &["agents".into()]), None);
    }

    #[test]
    fn advertised_commands_are_discovered_but_not_parsed_locally() {
        let advertised = vec!["compact".to_string(), "new".to_string()];
        assert_eq!(known_token("/compact next", &advertised), Some(0..8));
        assert_eq!(parse("/compact next"), Parsed::Prompt("/compact next"));
        assert_eq!(
            parse("/new next"),
            Parsed::New {
                prompt: Some("next")
            }
        );
    }
}
