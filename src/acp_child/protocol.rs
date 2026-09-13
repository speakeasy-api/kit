//! Version-specific wire dispatch for parent-owned children.
//!
//! The initialize message includes both generations' identity/capability keys:
//! the selected protocol then determines response parsing and all session RPCs.
use agent_client_protocol::{
    ConnectionTo, Error, JsonRpcRequest, UntypedMessage,
    schema::{ProtocolVersion, v2},
};
use agentkit_acp as v1;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

// The SDK's stock Client builder pins v1 (and rewrites initialize), while
// Client.v2() rejects v1. These JSON-RPC roles leave protocol selection to this
// module so one subprocess can negotiate either version without a second
// initialization or respawn. Request payloads still use the SDK's typed schemas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Client;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Agent;

macro_rules! peer_role {
    ($role:ident, $peer:ident) => {
        impl agent_client_protocol::Role for $role {
            type Counterpart = $peer;
            fn role_id(&self) -> agent_client_protocol::RoleId {
                agent_client_protocol::RoleId::from_singleton(self)
            }
            fn counterpart(&self) -> $peer {
                $peer
            }
            async fn default_handle_dispatch_from(
                &self,
                message: agent_client_protocol::Dispatch,
                _connection: ConnectionTo<Self>,
            ) -> Result<agent_client_protocol::Handled<agent_client_protocol::Dispatch>, Error>
            {
                Ok(agent_client_protocol::Handled::No {
                    message,
                    retry: false,
                })
            }
        }
        impl agent_client_protocol::role::HasPeer<Self> for $role {
            fn remote_style(&self, _peer: Self) -> agent_client_protocol::role::RemoteStyle {
                agent_client_protocol::role::RemoteStyle::Counterpart
            }
        }
    };
}
peer_role!(Client, Agent);
peer_role!(Agent, Client);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Version {
    V1,
    V2,
}

#[derive(Clone, Debug, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "session/request_permission", response = v1::RequestPermissionResponse)]
pub(super) struct PermissionRequest {
    pub options: Vec<v1::PermissionOption>,
}

fn initialize_params() -> Result<Value, Error> {
    let mut params = serde_json::to_value(
        v1::InitializeRequest::new(ProtocolVersion::V2)
            .client_info(v1::Implementation::new("kit", env!("CARGO_PKG_VERSION")))
            .client_capabilities(
                v1::ClientCapabilities::new().session(
                    v1::ClientSessionCapabilities::new()
                        .compaction(v1::CompactionCapabilities::default()),
                ),
            ),
    )?;
    let v2_params = serde_json::to_value(v2::InitializeRequest::new(
        ProtocolVersion::V2,
        v2::Implementation::new("kit", env!("CARGO_PKG_VERSION")),
    ))?;
    if let (Some(params), Value::Object(v2_params)) = (params.as_object_mut(), v2_params) {
        params.extend(v2_params);
    }
    Ok(params)
}

pub(super) async fn initialize(
    connection: &ConnectionTo<Agent>,
) -> Result<(Version, v1::AgentCapabilities), Error> {
    let response = connection
        .send_request(UntypedMessage::new("initialize", initialize_params()?)?)
        .block_task()
        .await?;
    negotiated(response)
}

fn negotiated(response: Value) -> Result<(Version, v1::AgentCapabilities), Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Selected {
        protocol_version: ProtocolVersion,
    }
    let selected: Selected = serde_json::from_value(response.clone())?;
    match selected.protocol_version {
        ProtocolVersion::V1 => {
            let initialized: v1::InitializeResponse = serde_json::from_value(response)?;
            Ok((Version::V1, initialized.agent_capabilities))
        }
        ProtocolVersion::V2 => {
            let initialized: v2::InitializeResponse = serde_json::from_value(response)?;
            let mut capabilities = v1::AgentCapabilities::default();
            if let Some(session) = initialized.capabilities.session {
                if let Some(prompt) = session.prompt {
                    capabilities.prompt_capabilities.image = prompt.image.is_some();
                    capabilities.prompt_capabilities.audio = prompt.audio.is_some();
                    capabilities.prompt_capabilities.embedded_context =
                        prompt.embedded_context.is_some();
                }
                if session.delete.is_some() {
                    capabilities.session_capabilities.delete = Some(Default::default());
                }
                // close is baseline in v2; fork remains explicitly advertised.
                capabilities.session_capabilities.close = Some(Default::default());
                if session.fork.is_some() {
                    capabilities.session_capabilities.fork = Some(Default::default());
                }
            } else {
                return Err(Error::into_internal_error(std::io::Error::other(
                    "ACP v2 harness does not advertise session support",
                )));
            }
            Ok((Version::V2, capabilities))
        }
        other => Err(Error::into_internal_error(std::io::Error::other(format!(
            "ACP harness selected unsupported protocol version {other}"
        )))),
    }
}

// These requests share JSON payloads; responses also need config identifiers
// normalized below. Keep prompt separate: v2 acceptance is not a v1 stop reason.
pub(super) trait SessionRequest: JsonRpcRequest + Serialize {
    type V2: JsonRpcRequest + DeserializeOwned;
    fn into_v2(self) -> Result<Self::V2, Error> {
        cast(self)
    }
}
macro_rules! session_requests {
    ($($name:ident),* $(,)?) => {$(
        impl SessionRequest for v1::$name { type V2 = v2::$name; }
    )*};
}
session_requests!(NewSessionRequest, ForkSessionRequest, CloseSessionRequest,);

impl SessionRequest for v1::SetSessionConfigOptionRequest {
    type V2 = v2::SetSessionConfigOptionRequest;
    fn into_v2(self) -> Result<Self::V2, Error> {
        let mut params = serde_json::to_value(self)?;
        params["type"] = Value::String(
            if params["value"].is_boolean() {
                "boolean"
            } else {
                "id"
            }
            .into(),
        );
        Ok(serde_json::from_value(params)?)
    }
}

fn cast<T: DeserializeOwned>(value: impl Serialize) -> Result<T, Error> {
    Ok(serde_json::from_value(serde_json::to_value(value)?)?)
}

fn session_response<T: DeserializeOwned>(response: impl Serialize) -> Result<T, Error> {
    let mut response = serde_json::to_value(response)?;
    normalize_config_options(&mut response);
    Ok(serde_json::from_value(response)?)
}

pub(super) fn normalize_config_options(response: &mut Value) {
    if let Some(options) = response
        .get_mut("configOptions")
        .and_then(Value::as_array_mut)
    {
        for option in options {
            if let Some(option) = option.as_object_mut() {
                if let Some(id) = option.remove("configId") {
                    option.insert("id".into(), id);
                }
                if let Some(groups) = option.get_mut("options").and_then(Value::as_array_mut) {
                    for group in groups {
                        if let Some(group) = group.as_object_mut()
                            && let Some(id) = group.remove("groupId")
                        {
                            group.insert("group".into(), id);
                        }
                    }
                }
            }
        }
    }
}

pub(super) async fn request<R>(
    connection: ConnectionTo<Agent>,
    version: Version,
    request: R,
) -> Result<R::Response, Error>
where
    R: SessionRequest,
    R::Response: DeserializeOwned,
    <R::V2 as JsonRpcRequest>::Response: Serialize,
{
    match version {
        Version::V1 => connection.send_request(request).block_task().await,
        Version::V2 => {
            let request = request.into_v2()?;
            session_response(connection.send_request(request).block_task().await?)
        }
    }
}

pub(super) fn fork(
    connection: &ConnectionTo<Agent>,
    version: Version,
    request: v1::ForkSessionRequest,
) -> Result<
    (
        v1::RequestId,
        impl Future<Output = Result<v1::ForkSessionResponse, Error>> + Send + 'static,
    ),
    Error,
> {
    let message = match version {
        Version::V1 => UntypedMessage::new("session/fork", request)?,
        Version::V2 => UntypedMessage::new("session/fork", request.into_v2()?)?,
    };
    let request = connection.send_request(message);
    let id = request.id().clone();
    Ok((id, async move {
        let response = request.block_task().await?;
        match version {
            Version::V1 => Ok(serde_json::from_value(response)?),
            Version::V2 => {
                session_response(serde_json::from_value::<v2::ForkSessionResponse>(response)?)
            }
        }
    }))
}

pub(super) async fn prompt(
    connection: &ConnectionTo<Agent>,
    version: Version,
    session_id: v1::SessionId,
    content: Vec<v1::ContentBlock>,
) -> Result<v1::PromptResponse, Error> {
    match version {
        Version::V1 => {
            connection
                .send_request(v1::PromptRequest::new(session_id, content))
                .block_task()
                .await
        }
        Version::V2 => {
            connection
                .send_request(v2::PromptRequest::new(
                    session_id.to_string(),
                    cast(content)?,
                ))
                .block_task()
                .await?;
            // The caller still waits for idle before settling the turn. Full v2
            // stop-reason interpretation and reconnect replay belong to #182.
            Ok(v1::PromptResponse::new(v1::StopReason::EndTurn))
        }
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
    fn handshake_identifies_kit_without_advertising_services() {
        let params = initialize_params().unwrap();
        assert_eq!(params["protocolVersion"], 2);
        assert_eq!(params["clientInfo"], params["info"]);
        assert_eq!(params["info"]["name"], "kit");
        assert_eq!(params["info"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(params["capabilities"], json!({}));
        assert_eq!(
            params["clientCapabilities"]["session"]["compaction"],
            json!({})
        );
        let caps: v1::ClientCapabilities =
            serde_json::from_value(params["clientCapabilities"].clone()).unwrap();
        assert!(!caps.fs.read_text_file);
        assert!(!caps.fs.write_text_file);
        assert!(!caps.terminal);
    }

    #[test]
    fn selects_version_before_parsing_capabilities() {
        let (version, caps) = negotiated(json!({"protocolVersion": 1,
            "agentCapabilities": {"sessionCapabilities": {"fork": {}}}
        }))
        .unwrap();
        assert_eq!(version, Version::V1);
        assert!(caps.session_capabilities.fork.is_some());
        assert!(caps.session_capabilities.close.is_none());
        let (version, caps) = negotiated(json!({"protocolVersion": 2,
            "info": {"name": "child", "version": "1"},
            "capabilities": {"session": {"fork": {}}}
        }))
        .unwrap();
        assert_eq!(version, Version::V2);
        assert!(caps.session_capabilities.fork.is_some());
        assert!(caps.session_capabilities.close.is_some());
        let (_, caps) = negotiated(json!({"protocolVersion": 2,
            "info": {"name": "child", "version": "1"},
            "capabilities": {"session": {}}
        }))
        .unwrap();
        assert!(caps.session_capabilities.fork.is_none());
    }

    #[test]
    fn rejects_unsupported_or_malformed_handshakes() {
        for response in [
            json!({"protocolVersion": 99}),
            json!({}),
            json!({"protocolVersion": 2, "capabilities": {"session": {}}}),
            json!({"protocolVersion": 2, "info": {"name": "child", "version": "1"}}),
        ] {
            assert!(negotiated(response).is_err());
        }
    }

    #[test]
    fn foreground_ignores_initial_idle_and_stays_settled() {
        let mut state = Foreground::Waiting;
        assert!(!state.advance(Foreground::Idle));
        assert_eq!(state, Foreground::Waiting);
        assert!(state.advance(Foreground::Running));
        assert!(state.advance(Foreground::Idle));
        assert!(!state.advance(Foreground::Running));
        assert_eq!(state, Foreground::Idle);
    }

    #[test]
    fn recognizes_idle_without_treating_running_as_completion() {
        let update = |state| json!({"sessionId": "child", "update": {"sessionUpdate": "state_update", "state": state}});
        assert_eq!(foreground(&update("idle")).unwrap(), Some(Foreground::Idle));
        assert_eq!(
            foreground(&update("running")).unwrap(),
            Some(Foreground::Running)
        );
        assert!(
            foreground(
                &json!({"sessionId": "child", "update": {"sessionUpdate": "agent_message_chunk"}})
            )
            .unwrap()
            .is_none()
        );
    }
}

// Per-route state is private to one serialized turn. Only notifications write
// Running/Idle; an initial idle is ignored until that turn has started. No lock
// guard is held across request dispatch, wakeup, output recording, or await.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Foreground {
    Waiting,
    Running,
    Idle,
}
impl Foreground {
    pub(super) fn advance(&mut self, next: Self) -> bool {
        if *self == Self::Idle || (*self == Self::Waiting && next == Self::Idle) {
            return false;
        }
        *self = next;
        true
    }
}

pub(super) fn foreground(params: &Value) -> Result<Option<Foreground>, Error> {
    if params["update"]["sessionUpdate"] != "state_update" {
        return Ok(None);
    }
    let notification: v2::UpdateSessionNotification = serde_json::from_value(params.clone())?;
    Ok(match notification.update {
        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(_)) => Some(Foreground::Idle),
        v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => Some(Foreground::Running),
        _ => None,
    })
}

pub(super) fn cancel(
    connection: &ConnectionTo<Agent>,
    version: Version,
    session_id: v1::SessionId,
) -> Result<(), Error> {
    match version {
        Version::V1 => connection.send_notification(v1::CancelNotification::new(session_id)),
        Version::V2 => {
            connection.send_notification(v2::CancelSessionNotification::new(session_id.to_string()))
        }
    }
}
