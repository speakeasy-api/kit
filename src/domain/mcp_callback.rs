use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::{
    events::UtcDateTime,
    ids::{AttemptId, McpCallbackId, PrincipalId, ProjectId, RunId, WorkspaceId},
    lifecycle::FencingToken,
};

pub const MAX_CALLBACK_ARTIFACTS: usize = 4;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct McpCallbackArtifactRef(String);

impl McpCallbackArtifactRef {
    pub fn parse(value: &str) -> Result<Self, McpCallbackError> {
        let valid = value.strip_prefix("artifact-ref:").is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(McpCallbackError::Invalid(
                "invalid callback artifact reference",
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for McpCallbackArtifactRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCallbackKind {
    Elicitation,
    Roots,
    Sampling,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCallbackMode {
    Form,
    RootsResponse,
    SamplingRequest,
    SamplingResponse,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCallbackState {
    Requested,
    AwaitingResolution,
    Resolved,
    ResponsePrepared,
    Delivered,
    DeliveryUnknown,
    Expired,
    Interrupted,
}

impl McpCallbackState {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Delivered | Self::DeliveryUnknown | Self::Expired | Self::Interrupted
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCallbackAction {
    Accept,
    Decline,
    Cancel,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpCallbackProjection {
    pub id: McpCallbackId,
    pub server_id: String,
    pub kind: McpCallbackKind,
    pub mode: McpCallbackMode,
    pub principal_id: PrincipalId,
    pub project_id: ProjectId,
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub fence: FencingToken,
    pub claim_generation: u64,
    pub workspace_id: WorkspaceId,
    pub workspace_revision: String,
    pub request_id: String,
    pub request: serde_json::Value,
    pub schema: serde_json::Value,
    pub request_digest: String,
    pub schema_digest: String,
    pub challenge_generation: u64,
    pub operation_sequence: u64,
    pub expires_at: UtcDateTime,
    pub artifact_expires_at: UtcDateTime,
    pub max_response_bytes: usize,
    pub max_content_bytes: usize,
    #[serde(default = "default_secret_policy_id")]
    pub secret_policy_id: String,
    pub state: McpCallbackState,
    pub version: u64,
    pub resolver_actor: Option<PrincipalId>,
    pub action: Option<McpCallbackAction>,
    pub artifact_refs: Vec<McpCallbackArtifactRef>,
    pub terminal_error: Option<String>,
}

impl McpCallbackProjection {
    pub fn validate(&self) -> Result<(), McpCallbackError> {
        if self.server_id.is_empty()
            || self.server_id.len() > 256
            || self.server_id.chars().any(char::is_control)
            || self.request_id.is_empty()
            || self.request_id.len() > 512
            || self.workspace_revision.is_empty()
            || self.workspace_revision.len() > 256
            || self.claim_generation == 0
            || self.challenge_generation == 0
            || self.operation_sequence == 0
            || self.max_response_bytes == 0
            || self.max_content_bytes == 0
            || self.max_content_bytes > self.max_response_bytes
            || self.artifact_expires_at.unix_micros() <= self.expires_at.unix_micros()
            || self.artifact_refs.len() > MAX_CALLBACK_ARTIFACTS
            || self.secret_policy_id.is_empty()
            || self.secret_policy_id.len() > 128
            || self
                .secret_policy_id
                .bytes()
                .any(|byte| !byte.is_ascii_graphic())
            || !valid_sha256(&self.request_digest)
            || !valid_sha256(&self.schema_digest)
        {
            return Err(McpCallbackError::Invalid("invalid callback binding"));
        }
        validate_resolution_fields(
            self.mode,
            self.state,
            self.resolver_actor,
            self.action,
            &self.artifact_refs,
        )?;
        Ok(())
    }

    pub fn apply(&mut self, event: &McpCallbackEvent) -> Result<(), McpCallbackError> {
        if event.callback_id != self.id {
            return Err(McpCallbackError::Invalid(
                "callback event identity mismatch",
            ));
        }
        if self.state.is_terminal() {
            return Err(McpCallbackError::Terminal(self.state));
        }
        if event.expected_version != self.version {
            return Err(McpCallbackError::VersionConflict {
                expected: event.expected_version,
                actual: self.version,
            });
        }
        let legal = matches!(
            (self.state, event.state),
            (
                McpCallbackState::Requested,
                McpCallbackState::AwaitingResolution
            ) | (McpCallbackState::Requested, McpCallbackState::Interrupted)
                | (
                    McpCallbackState::AwaitingResolution,
                    McpCallbackState::Resolved
                )
                | (
                    McpCallbackState::Resolved,
                    McpCallbackState::ResponsePrepared
                )
                | (
                    McpCallbackState::AwaitingResolution,
                    McpCallbackState::Expired
                )
                | (
                    McpCallbackState::AwaitingResolution,
                    McpCallbackState::Interrupted
                )
                | (McpCallbackState::Resolved, McpCallbackState::Interrupted)
                | (
                    McpCallbackState::ResponsePrepared,
                    McpCallbackState::Delivered
                )
                | (
                    McpCallbackState::ResponsePrepared,
                    McpCallbackState::DeliveryUnknown
                )
        );
        if !legal {
            return Err(McpCallbackError::IllegalTransition {
                from: self.state,
                to: event.state,
            });
        }
        validate_resolution_fields(
            self.mode,
            event.state,
            event.resolver_actor,
            event.action,
            &event.artifact_refs,
        )?;
        self.state = event.state;
        self.version += 1;
        self.resolver_actor = event.resolver_actor;
        self.action = event.action;
        self.artifact_refs = event.artifact_refs.clone();
        self.terminal_error = event.terminal_error.clone();
        Ok(())
    }
}

fn validate_resolution_fields(
    mode: McpCallbackMode,
    state: McpCallbackState,
    resolver: Option<PrincipalId>,
    action: Option<McpCallbackAction>,
    artifacts: &[McpCallbackArtifactRef],
) -> Result<(), McpCallbackError> {
    if artifacts.len() > MAX_CALLBACK_ARTIFACTS {
        return Err(McpCallbackError::Invalid("invalid callback artifacts"));
    }
    let resolved = matches!(
        state,
        McpCallbackState::Resolved
            | McpCallbackState::ResponsePrepared
            | McpCallbackState::Delivered
            | McpCallbackState::DeliveryUnknown
    );
    let interrupted_after_resolution =
        state == McpCallbackState::Interrupted && resolver.is_some() && action.is_some();
    if resolved || interrupted_after_resolution {
        let action = action.ok_or(McpCallbackError::Invalid(
            "resolved callback action is missing",
        ))?;
        if resolver.is_none() {
            return Err(McpCallbackError::Invalid(
                "resolved callback resolver is missing",
            ));
        }
        let expected_artifacts =
            usize::from(mode == McpCallbackMode::Form && action == McpCallbackAction::Accept);
        if artifacts.len() != expected_artifacts {
            return Err(McpCallbackError::Invalid("invalid callback artifacts"));
        }
    } else if resolver.is_some() || action.is_some() || !artifacts.is_empty() {
        return Err(McpCallbackError::Invalid(
            "unresolved callback has resolution fields",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum McpCallbackCommand {
    Request {
        callback: Box<McpCallbackProjection>,
    },
    AwaitResolution {
        callback_id: McpCallbackId,
        expected_version: u64,
    },
    Resolve {
        callback_id: McpCallbackId,
        expected_version: u64,
        challenge_generation: u64,
        schema_digest: String,
        resolver_actor: PrincipalId,
        action: McpCallbackAction,
        artifact_refs: Vec<McpCallbackArtifactRef>,
    },
    PrepareResponse {
        callback_id: McpCallbackId,
        expected_version: u64,
    },
    Deliver {
        callback_id: McpCallbackId,
        expected_version: u64,
    },
    Settle {
        callback_id: McpCallbackId,
        expected_version: u64,
        state: McpCallbackState,
        terminal_error: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpCallbackEvent {
    pub callback_id: McpCallbackId,
    pub expected_version: u64,
    pub state: McpCallbackState,
    pub resolver_actor: Option<PrincipalId>,
    pub action: Option<McpCallbackAction>,
    pub artifact_refs: Vec<McpCallbackArtifactRef>,
    pub terminal_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpCallbackEventEnvelope {
    pub principal_id: PrincipalId,
    pub project_id: ProjectId,
    pub run_id: RunId,
    pub stored_at_unix_micros: i64,
    pub command: McpCallbackCommand,
    pub event: McpCallbackEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpCallbackError {
    NotFound,
    VersionConflict {
        expected: u64,
        actual: u64,
    },
    IllegalTransition {
        from: McpCallbackState,
        to: McpCallbackState,
    },
    Terminal(McpCallbackState),
    IdempotencyConflict,
    Authority,
    Expired,
    Invalid(&'static str),
    Store(String),
}

impl fmt::Display for McpCallbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("MCP callback not found"),
            Self::VersionConflict { expected, actual } => {
                write!(
                    formatter,
                    "MCP callback expected version {expected}, actual {actual}"
                )
            }
            Self::IllegalTransition { from, to } => {
                write!(
                    formatter,
                    "illegal MCP callback transition {from:?} -> {to:?}"
                )
            }
            Self::Terminal(state) => write!(formatter, "MCP callback is terminal ({state:?})"),
            Self::IdempotencyConflict => formatter.write_str("MCP callback idempotency conflict"),
            Self::Authority => formatter.write_str("MCP callback authority is stale"),
            Self::Expired => formatter.write_str("MCP callback expired"),
            Self::Invalid(message) => write!(formatter, "invalid MCP callback: {message}"),
            Self::Store(message) => write!(formatter, "MCP callback store error: {message}"),
        }
    }
}

impl std::error::Error for McpCallbackError {}

fn valid_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn default_secret_policy_id() -> String {
    "authorized-secrets-v1".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_and_illegal_transitions_do_not_mutate_projection() {
        let mut callback = fixture();
        let original = callback.clone();
        let illegal = McpCallbackEvent {
            callback_id: callback.id,
            expected_version: 1,
            state: McpCallbackState::Delivered,
            resolver_actor: None,
            action: None,
            artifact_refs: Vec::new(),
            terminal_error: None,
        };
        assert!(matches!(
            callback.apply(&illegal),
            Err(McpCallbackError::IllegalTransition { .. })
        ));
        assert_eq!(callback, original);

        callback
            .apply(&McpCallbackEvent {
                callback_id: callback.id,
                expected_version: 1,
                state: McpCallbackState::Interrupted,
                resolver_actor: None,
                action: None,
                artifact_refs: Vec::new(),
                terminal_error: Some("outcome_unknown".to_owned()),
            })
            .unwrap();
        let terminal = callback.clone();
        assert!(matches!(
            callback.apply(&illegal),
            Err(McpCallbackError::Terminal(_))
        ));
        assert_eq!(callback, terminal);
    }

    #[test]
    fn sampling_approval_accepts_only_an_exact_digest_without_content_artifacts() {
        let mut callback = fixture();
        callback.kind = McpCallbackKind::Sampling;
        callback.mode = McpCallbackMode::SamplingRequest;
        callback.state = McpCallbackState::AwaitingResolution;
        callback
            .apply(&McpCallbackEvent {
                callback_id: callback.id,
                expected_version: 1,
                state: McpCallbackState::Resolved,
                resolver_actor: Some(callback.principal_id),
                action: Some(McpCallbackAction::Accept),
                artifact_refs: Vec::new(),
                terminal_error: None,
            })
            .unwrap();
        assert_eq!(callback.action, Some(McpCallbackAction::Accept));
        assert!(callback.artifact_refs.is_empty());
    }

    #[test]
    fn every_legal_transition_and_one_hundred_illegal_attempts_are_exact() {
        let legal = [
            (
                McpCallbackState::Requested,
                McpCallbackState::AwaitingResolution,
            ),
            (McpCallbackState::Requested, McpCallbackState::Interrupted),
            (
                McpCallbackState::AwaitingResolution,
                McpCallbackState::Resolved,
            ),
            (
                McpCallbackState::Resolved,
                McpCallbackState::ResponsePrepared,
            ),
            (
                McpCallbackState::AwaitingResolution,
                McpCallbackState::Expired,
            ),
            (
                McpCallbackState::AwaitingResolution,
                McpCallbackState::Interrupted,
            ),
            (McpCallbackState::Resolved, McpCallbackState::Interrupted),
            (
                McpCallbackState::ResponsePrepared,
                McpCallbackState::Delivered,
            ),
            (
                McpCallbackState::ResponsePrepared,
                McpCallbackState::DeliveryUnknown,
            ),
        ];
        for (from, to) in legal {
            let mut callback = fixture();
            callback.state = from;
            let action = if matches!(
                to,
                McpCallbackState::Resolved
                    | McpCallbackState::ResponsePrepared
                    | McpCallbackState::Delivered
                    | McpCallbackState::DeliveryUnknown
            ) || from == McpCallbackState::Resolved
                && to == McpCallbackState::Interrupted
            {
                Some(McpCallbackAction::Accept)
            } else {
                None
            };
            let artifacts = if action == Some(McpCallbackAction::Accept) {
                vec![
                    McpCallbackArtifactRef::parse(&format!("artifact-ref:{}", "0".repeat(64)))
                        .unwrap(),
                ]
            } else {
                Vec::new()
            };
            callback
                .apply(&McpCallbackEvent {
                    callback_id: callback.id,
                    expected_version: 1,
                    state: to,
                    resolver_actor: action.map(|_| callback.principal_id),
                    action,
                    artifact_refs: artifacts,
                    terminal_error: None,
                })
                .unwrap();
            assert_eq!(callback.state, to);
            assert_eq!(callback.version, 2);
        }

        let states = [
            McpCallbackState::Requested,
            McpCallbackState::AwaitingResolution,
            McpCallbackState::Resolved,
            McpCallbackState::ResponsePrepared,
            McpCallbackState::Delivered,
            McpCallbackState::DeliveryUnknown,
            McpCallbackState::Expired,
            McpCallbackState::Interrupted,
        ];
        let mut tested = std::collections::BTreeSet::new();
        for from in states {
            for to in states {
                if legal.contains(&(from, to)) {
                    continue;
                }
                for action in [None, Some(McpCallbackAction::Cancel)] {
                    let mut callback = fixture();
                    callback.state = from;
                    let before = callback.clone();
                    let result = callback.apply(&McpCallbackEvent {
                        callback_id: callback.id,
                        expected_version: callback.version,
                        state: to,
                        resolver_actor: action.map(|_| callback.principal_id),
                        action,
                        artifact_refs: Vec::new(),
                        terminal_error: None,
                    });
                    assert!(matches!(
                        result,
                        Err(McpCallbackError::IllegalTransition { .. }
                            | McpCallbackError::Terminal(_))
                    ));
                    assert_eq!(callback, before);
                    tested.insert((from as u8, to as u8, action.map(|value| value as u8)));
                }
            }
        }
        assert!(
            tested.len() >= 100,
            "only {} distinct attempts",
            tested.len()
        );
    }

    fn fixture() -> McpCallbackProjection {
        McpCallbackProjection {
            id: McpCallbackId::parse("mcp_callback_00000000000000000000000001").unwrap(),
            server_id: "server".to_owned(),
            kind: McpCallbackKind::Elicitation,
            mode: McpCallbackMode::Form,
            principal_id: PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
            project_id: ProjectId::parse("project_00000000000000000000000001").unwrap(),
            run_id: RunId::parse("run_00000000000000000000000001").unwrap(),
            attempt_id: AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
            fence: FencingToken::new(1),
            claim_generation: 1,
            workspace_id: WorkspaceId::parse("workspace_00000000000000000000000001").unwrap(),
            workspace_revision: "revision".to_owned(),
            request_id: "1".to_owned(),
            request: serde_json::json!({"message":"name"}),
            schema: serde_json::json!({"type":"object"}),
            request_digest: format!("sha256:{}", "0".repeat(64)),
            schema_digest: format!("sha256:{}", "1".repeat(64)),
            challenge_generation: 1,
            operation_sequence: 1,
            expires_at: UtcDateTime::parse("2099-01-01T00:00:00Z").unwrap(),
            artifact_expires_at: UtcDateTime::parse("2100-01-01T00:00:00Z").unwrap(),
            max_response_bytes: 1024,
            max_content_bytes: 900,
            secret_policy_id: "authorized-secrets-v1".to_owned(),
            state: McpCallbackState::Requested,
            version: 1,
            resolver_actor: None,
            action: None,
            artifact_refs: Vec::new(),
            terminal_error: None,
        }
    }
}
