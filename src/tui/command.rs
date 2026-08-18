//! Slash commands understood by the terminal client.
//!
//! Parsing lives here so submission behavior and prompt highlighting use the
//! same command registry and token-boundary rules.

use std::ops::Range;

#[derive(Clone, Copy)]
enum Kind {
    Compact,
    New,
}

struct Spec {
    token: &'static str,
    kind: Kind,
}

// Adding a command should require one registry entry and one Parsed variant.
const COMMANDS: &[Spec] = &[
    Spec {
        token: "/compact",
        kind: Kind::Compact,
    },
    Spec {
        token: "/new",
        kind: Kind::New,
    },
];

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed<'a> {
    Compact { prompt: Option<&'a str> },
    New { prompt: Option<&'a str> },
    Prompt(&'a str),
}

fn recognized(input: &str) -> Option<(&Spec, usize)> {
    let token_end = input
        .char_indices()
        .find_map(|(index, character)| character.is_whitespace().then_some(index))
        .unwrap_or(input.len());
    let token = &input[..token_end];
    COMMANDS
        .iter()
        .find(|spec| spec.token == token)
        .map(|spec| (spec, token_end))
}

pub fn parse(input: &str) -> Parsed<'_> {
    let Some((spec, token_end)) = recognized(input) else {
        return Parsed::Prompt(input);
    };
    let remainder = input[token_end..].trim_start();
    let prompt = (!remainder.is_empty()).then_some(remainder);
    match spec.kind {
        Kind::Compact => Parsed::Compact { prompt },
        Kind::New => Parsed::New { prompt },
    }
}

/// Byte range of a recognized command token, for editor highlighting.
pub fn known_token(input: &str) -> Option<Range<usize>> {
    recognized(input).map(|(_, token_end)| 0..token_end)
}

#[cfg(test)]
mod tests {
    use super::{Parsed, known_token, parse};

    #[test]
    fn parses_commands_with_or_without_a_following_prompt() {
        assert_eq!(parse("/compact"), Parsed::Compact { prompt: None });
        assert_eq!(
            parse("/compact   continue here"),
            Parsed::Compact {
                prompt: Some("continue here")
            }
        );
        assert_eq!(parse("/new"), Parsed::New { prompt: None });
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
        for input in ["/newer", "/new/path", " /new", "/unknown arg"] {
            assert_eq!(parse(input), Parsed::Prompt(input));
            assert_eq!(known_token(input), None);
        }
        assert_eq!(known_token("/new prompt"), Some(0..4));
        assert_eq!(known_token("/compact prompt"), Some(0..8));
    }
}
