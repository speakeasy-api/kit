use std::sync::atomic::Ordering;

use crate::{
    api::auth::contract::AuthenticatedPrincipal,
    capabilities::kernel::{
        grant,
        grant_ext::EgressConstraint,
        invoke::{self as kernel_invoke, InvocationEnvelope, InvokeError},
    },
    domain::{
        commands::ExpectedVersion,
        events::{EntityId, EventType, SchemaVersion},
        ids::{ApprovalId, EventId},
        secret::SecretHandle,
    },
    store::sqlite::{
        append::{
            AppendCommand, AppendOutcome, CrashPoint, ExpectedStreamVersion, NewEvent, SqliteStore,
        },
        idempotency::{CanonicalRequestDigest, IdempotencyStatus},
    },
};

use super::{
    AUTH_RECORD_VERSION, AuthBinding, AuthChallenge, AuthChannel, AuthRecord, AuthResolution,
    AuthState, BrokerAuthRequirement, BrokerError, BrokerInvocation, append_auth,
    append_resolution, auth_identity, auth_key, auth_scope, canonical_digest, checked_record,
    digest_hex, ensure_auth_credential, record_bytes,
};

pub const MAX_TRANSPORT_OPERATION_BYTES: usize = 128;

const TRANSPORT_STREAM_KIND: &str = "transport_stream";
const TRANSPORT_CHANNEL: AuthChannel = AuthChannel {
    challenge_command: "capability.broker_transport_auth.challenge",
    resolution_command: "capability.broker_transport_auth.resolve",
    challenge_event: "capability.broker_transport_auth_challenged",
    resolution_event: "capability.broker_transport_auth_resolved",
};
const OPERATION_DISPATCH_COMMAND: &str = "capability.broker_transport.dispatch";
const OPERATION_OUTCOME_COMMAND: &str = "capability.broker_transport.outcome";
const OPERATION_INTENT_EVENT: &str = "capability.broker_transport_intent";
const OPERATION_DISPATCH_EVENT: &str = "capability.broker_transport_dispatched";
const OPERATION_OUTCOME_EVENT: &str = "capability.broker_transport_outcome";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportAuthKind {
    Unauthorized,
    Forbidden,
}

impl TransportAuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
        }
    }

    fn from_record(value: Option<&str>) -> Result<Self, BrokerError> {
        match value {
            Some("unauthorized") => Ok(Self::Unauthorized),
            Some("forbidden") => Ok(Self::Forbidden),
            _ => Err(BrokerError::InvalidAuthState),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportOperation(String);

impl TransportOperation {
    pub fn parse(operation: &str) -> Result<Self, BrokerError> {
        if operation.is_empty()
            || operation.len() > MAX_TRANSPORT_OPERATION_BYTES
            || operation.bytes().any(|byte| !byte.is_ascii_graphic())
        {
            return Err(BrokerError::InvalidTransportOperation);
        }
        Ok(Self(operation.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransportBinding {
    server_id: String,
    transport: String,
    endpoint: String,
    session_id: Option<String>,
    workspace_id: String,
    workspace_revision: Option<String>,
    principal_id: String,
    project_id: String,
    capability_source: String,
    capability_namespace: String,
    capability_name: String,
    capability_version: String,
    capability_implementation_digest: String,
    discovered_schema_digest: String,
    bound_schema_digest: String,
    credential_id: Option<String>,
    egress: Option<TransportEgressBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct TransportEgressBinding {
    scheme: String,
    host: String,
    port: u16,
    credential_id: String,
}

impl TransportBinding {
    pub(crate) fn new(
        request: &BrokerInvocation<'_>,
        server_id: impl Into<String>,
        transport: impl Into<String>,
        endpoint: impl Into<String>,
        session_id: Option<String>,
    ) -> Self {
        let envelope = &request.envelope;
        let credential = envelope.extension.credential().cloned().or_else(|| {
            envelope
                .extension
                .egress()
                .map(|egress| egress.credential().clone())
        });
        Self {
            server_id: server_id.into(),
            transport: transport.into(),
            endpoint: endpoint.into(),
            session_id,
            workspace_id: envelope.workspace_id.to_string(),
            workspace_revision: envelope.extension.workspace_revision().map(str::to_owned),
            principal_id: envelope.authenticated.principal_id().to_string(),
            project_id: envelope.project_id.to_string(),
            capability_source: envelope.capability.source().as_str().to_owned(),
            capability_namespace: envelope.capability.namespace().as_str().to_owned(),
            capability_name: envelope.capability.name().as_str().to_owned(),
            capability_version: envelope.capability.version().as_str().to_owned(),
            capability_implementation_digest: envelope
                .capability
                .implementation_digest()
                .to_string(),
            discovered_schema_digest: envelope.discovered_schema_digest.to_string(),
            bound_schema_digest: envelope.bound_schema_digest.to_string(),
            credential_id: credential.map(|value| value.identifier().to_owned()),
            egress: envelope
                .extension
                .egress()
                .map(|egress| TransportEgressBinding {
                    scheme: match egress.scheme() {
                        crate::domain::egress::Scheme::Http => "http",
                        crate::domain::egress::Scheme::Https => "https",
                    }
                    .to_owned(),
                    host: egress.host().to_owned(),
                    port: egress.port(),
                    credential_id: egress.credential().identifier().to_owned(),
                }),
        }
    }

    pub(crate) fn with_session(&self, session_id: Option<String>) -> Self {
        let mut binding = self.clone();
        binding.session_id = session_id;
        binding
    }

    pub(crate) fn with_request(&self, request: &BrokerInvocation<'_>) -> Self {
        let envelope = &request.envelope;
        let mut binding = self.clone();
        binding.workspace_id = envelope.workspace_id.to_string();
        binding.workspace_revision = envelope.extension.workspace_revision().map(str::to_owned);
        binding.principal_id = envelope.authenticated.principal_id().to_string();
        binding.project_id = envelope.project_id.to_string();
        binding.capability_source = envelope.capability.source().as_str().to_owned();
        binding.capability_namespace = envelope.capability.namespace().as_str().to_owned();
        binding.capability_name = envelope.capability.name().as_str().to_owned();
        binding.capability_version = envelope.capability.version().as_str().to_owned();
        binding.capability_implementation_digest =
            envelope.capability.implementation_digest().to_string();
        binding.discovered_schema_digest = envelope.discovered_schema_digest.to_string();
        binding.bound_schema_digest = envelope.bound_schema_digest.to_string();
        binding.credential_id = envelope
            .extension
            .credential()
            .cloned()
            .or_else(|| {
                envelope
                    .extension
                    .egress()
                    .map(|egress| egress.credential().clone())
            })
            .map(|credential| credential.identifier().to_owned());
        binding.egress = envelope
            .extension
            .egress()
            .map(|egress| TransportEgressBinding {
                scheme: match egress.scheme() {
                    crate::domain::egress::Scheme::Http => "http",
                    crate::domain::egress::Scheme::Https => "https",
                }
                .to_owned(),
                host: egress.host().to_owned(),
                port: egress.port(),
                credential_id: egress.credential().identifier().to_owned(),
            });
        binding
    }

    fn digest(&self) -> Result<String, BrokerError> {
        let bytes = serde_json::to_vec(self).map_err(|_| BrokerError::InvalidAuthState)?;
        Ok(digest_hex(canonical_digest(&bytes)))
    }

    pub(crate) fn same_connection(&self, other: &Self) -> bool {
        self.server_id == other.server_id
            && self.transport == other.transport
            && self.endpoint == other.endpoint
            && self.workspace_id == other.workspace_id
            && self.workspace_revision == other.workspace_revision
            && self.principal_id == other.principal_id
            && self.project_id == other.project_id
            && self.credential_id == other.credential_id
            && self.egress == other.egress
    }

    pub(crate) fn owned_by(
        &self,
        principal_id: &str,
        project_id: &str,
        workspace_id: &str,
        workspace_revision: Option<&str>,
    ) -> bool {
        self.principal_id == principal_id
            && self.project_id == project_id
            && self.workspace_id == workspace_id
            && self.workspace_revision.as_deref() == workspace_revision
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        server_id: impl Into<String>,
        transport: &'static str,
        endpoint: impl Into<String>,
        session_id: Option<String>,
    ) -> Self {
        Self {
            server_id: server_id.into(),
            transport: transport.to_owned(),
            endpoint: endpoint.into(),
            session_id,
            workspace_id: "test-workspace".to_owned(),
            workspace_revision: Some("test-revision".to_owned()),
            principal_id: "test-principal".to_owned(),
            project_id: "test-project".to_owned(),
            capability_source: "test".to_owned(),
            capability_namespace: "test.mcp".to_owned(),
            capability_name: "invoke".to_owned(),
            capability_version: "1.0.0".to_owned(),
            capability_implementation_digest: "sha256:test-implementation".to_owned(),
            discovered_schema_digest: "sha256:test-schema".to_owned(),
            bound_schema_digest: "sha256:test-schema".to_owned(),
            credential_id: None,
            egress: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportAuthChallenge {
    pub challenge: AuthChallenge,
    pub kind: TransportAuthKind,
    pub operation: TransportOperation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportAuthState {
    Absent,
    Pending(TransportAuthChallenge),
    Granted(TransportAuthChallenge),
    Denied,
    Replayed,
}

pub struct TransportDispatch {
    stream: ApprovalId,
    digest: CanonicalRequestDigest,
    operation: TransportOperation,
    binding: TransportBinding,
    allow_quiescent_driver_claim: bool,
}

#[derive(Clone, Copy)]
pub enum TransportDispatchOutcome {
    Completed,
    AuthInterrupted,
    OutcomeUnknown,
}

impl TransportDispatchOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::AuthInterrupted => "auth_interrupted",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

#[derive(Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct TransportDispatchRecord {
    schema_version: u16,
    operation: String,
    request_digest: String,
    binding_digest: String,
    binding: TransportBinding,
    status: String,
}

/// Opaque proof that the sole capability broker authorized one transport
/// operation for this exact invocation and grant snapshot.
#[derive(Clone)]
pub struct TransportAuthorization {
    principal_id: String,
    project_id: String,
    invocation_id: String,
    decision_digest: String,
    request_digest: String,
    scope: Option<String>,
    credential: Option<SecretHandle>,
    egress: Option<EgressConstraint>,
    credentials: std::collections::BTreeSet<SecretHandle>,
    egresses: std::collections::BTreeSet<EgressConstraint>,
    effect: grant::EffectClass,
    binding: TransportBinding,
    operation: TransportOperation,
    arguments: Vec<u8>,
}

impl TransportAuthorization {
    pub fn principal_id(&self) -> &str {
        &self.principal_id
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }

    pub fn decision_digest(&self) -> &str {
        &self.decision_digest
    }

    pub fn request_digest(&self) -> &str {
        &self.request_digest
    }

    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    pub fn operation(&self) -> &TransportOperation {
        &self.operation
    }

    pub(crate) fn arguments(&self) -> &[u8] {
        &self.arguments
    }

    pub(crate) fn credential(&self) -> Option<&SecretHandle> {
        self.credential.as_ref()
    }

    pub(crate) fn egress(&self) -> Option<&EgressConstraint> {
        self.egress.as_ref()
    }

    pub(crate) fn binding(&self) -> &TransportBinding {
        &self.binding
    }

    pub(crate) fn same_connection(&self, other: &Self) -> bool {
        self.principal_id == other.principal_id
            && self.project_id == other.project_id
            && self.credential == other.credential
            && self.egress == other.egress
            && self.binding.same_connection(&other.binding)
    }

    pub(crate) fn matches_extension_route(
        &self,
        principal_id: &str,
        project_id: &str,
        protocol: &str,
        route_id: &str,
    ) -> bool {
        self.principal_id == principal_id
            && self.project_id == project_id
            && protocol == "mcp"
            && self.binding.server_id == route_id
            && matches!(self.binding.transport.as_str(), "stdio" | "http")
            && self.binding.discovered_schema_digest == self.binding.bound_schema_digest
    }

    pub(crate) fn matches_contract_digests(
        &self,
        schema_digest: &str,
        implementation_digest: &str,
    ) -> bool {
        self.binding.bound_schema_digest == schema_digest
            && self.binding.capability_implementation_digest == implementation_digest
    }

    pub(crate) fn is_brokered_egress_only(&self) -> bool {
        self.effect == grant::EffectClass::NetworkEgress
            && self.binding.transport == "http"
            && self.egress.is_some()
    }

    pub(crate) fn allows_profile(
        &self,
        profile: &crate::executor::profile::ExecutorProfile,
    ) -> bool {
        if self.effect != grant::EffectClass::ProcessSpawn {
            return false;
        }
        let credentials = self
            .credential
            .iter()
            .chain(self.credentials.iter())
            .map(SecretHandle::identifier)
            .collect::<std::collections::BTreeSet<_>>();
        if profile
            .credentials()
            .iter()
            .any(|credential| !credentials.contains(credential.handle.as_str()))
        {
            return false;
        }
        let egresses = self
            .egress
            .iter()
            .chain(self.egresses.iter())
            .map(|egress| (egress.host(), egress.port()))
            .collect::<std::collections::BTreeSet<_>>();
        profile.egress().iter().all(|egress| {
            egress.transport() == crate::executor::profile::EgressTransport::Tcp
                && egresses.contains(&(egress.destination(), egress.port()))
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        operation: TransportOperation,
        credential: Option<SecretHandle>,
        egress: Option<EgressConstraint>,
    ) -> Self {
        Self::for_test_binding(
            operation,
            credential,
            egress,
            TransportBinding::for_test("test-server", "http", "http://127.0.0.1/mcp", None),
        )
    }

    #[cfg(test)]
    pub(crate) fn for_test_binding(
        operation: TransportOperation,
        credential: Option<SecretHandle>,
        egress: Option<EgressConstraint>,
        binding: TransportBinding,
    ) -> Self {
        Self {
            principal_id: "test-principal".to_owned(),
            project_id: "test-project".to_owned(),
            invocation_id: "test-invocation".to_owned(),
            decision_digest: "sha256:test-decision".to_owned(),
            request_digest: "sha256:test-request".to_owned(),
            scope: Some("mcp.connect".to_owned()),
            credential,
            egress,
            credentials: Default::default(),
            egresses: Default::default(),
            effect: grant::EffectClass::ProcessSpawn,
            binding,
            operation,
            arguments: b"{}".to_vec(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_arguments(
        operation: TransportOperation,
        arguments: serde_json::Value,
    ) -> Self {
        Self::for_test_bound_arguments(operation, arguments, None, None)
    }

    #[cfg(test)]
    pub(crate) fn for_test_bound_arguments(
        operation: TransportOperation,
        arguments: serde_json::Value,
        credential: Option<SecretHandle>,
        egress: Option<EgressConstraint>,
    ) -> Self {
        let mut authorization = Self::for_test(operation, credential, egress);
        authorization.arguments = serde_json::to_vec(&arguments).unwrap();
        authorization
    }

    #[cfg(test)]
    pub(crate) fn for_test_bound_arguments_binding(
        operation: TransportOperation,
        arguments: serde_json::Value,
        credential: Option<SecretHandle>,
        egress: Option<EgressConstraint>,
        binding: TransportBinding,
    ) -> Self {
        let mut authorization = Self::for_test_binding(operation, credential, egress, binding);
        authorization.arguments = serde_json::to_vec(&arguments).unwrap();
        authorization
    }

    #[cfg(test)]
    pub(crate) fn for_test_capability_binding(
        operation: TransportOperation,
        capability_name: &str,
        binding: TransportBinding,
    ) -> Self {
        let mut binding = binding;
        binding.capability_name = capability_name.to_owned();
        Self::for_test_binding(operation, None, None, binding)
    }
}

pub(crate) fn authorize(
    request: &BrokerInvocation<'_>,
    operation: &TransportOperation,
    binding: &TransportBinding,
    store: &mut SqliteStore,
) -> Result<TransportAuthorization, BrokerError> {
    authorize_inner(
        request,
        operation,
        binding,
        request.envelope.arguments,
        store,
        false,
    )
}

pub(crate) fn authorize_operation(
    request: &BrokerInvocation<'_>,
    operation: &TransportOperation,
    binding: &TransportBinding,
    arguments: &[u8],
    store: &mut SqliteStore,
) -> Result<TransportAuthorization, BrokerError> {
    authorize_inner(request, operation, binding, arguments, store, false)
}

pub(crate) fn authorize_replay(
    request: &BrokerInvocation<'_>,
    operation: &TransportOperation,
    binding: &TransportBinding,
    store: &mut SqliteStore,
) -> Result<TransportAuthorization, BrokerError> {
    authorize_inner(
        request,
        operation,
        binding,
        request.envelope.arguments,
        store,
        true,
    )
}

pub(crate) fn authorize_operation_replay(
    request: &BrokerInvocation<'_>,
    operation: &TransportOperation,
    binding: &TransportBinding,
    arguments: &[u8],
    store: &mut SqliteStore,
) -> Result<TransportAuthorization, BrokerError> {
    authorize_inner(request, operation, binding, arguments, store, true)
}

fn authorize_inner(
    request: &BrokerInvocation<'_>,
    operation: &TransportOperation,
    binding: &TransportBinding,
    arguments: &[u8],
    store: &mut SqliteStore,
    _allow_replay: bool,
) -> Result<TransportAuthorization, BrokerError> {
    request
        .envelope
        .preflight_authority(store, request.lifecycle_shutdown())
        .map_err(BrokerError::Invoke)?;
    request.preflight_transport()?;
    if request.envelope.cancellation.load(Ordering::Acquire) && !request.lifecycle_shutdown() {
        return Err(BrokerError::TransportAuthCancelled);
    }
    if let Some(requirement) = &request.auth {
        ensure_auth_credential(&request.envelope, requirement)?;
    }
    let decision = grant::decide(request.envelope.grant_request());
    if !decision.is_allowed() {
        return Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(
            decision.reason(),
        )));
    }
    let envelope = &request.envelope;
    Ok(TransportAuthorization {
        principal_id: envelope.authenticated.principal_id().to_string(),
        project_id: envelope.project_id.to_string(),
        invocation_id: envelope.invocation_id.to_string(),
        decision_digest: decision.snapshot_digest().to_string(),
        request_digest: digest_hex(envelope.canonical_request_digest(decision.snapshot_digest())),
        scope: request
            .auth
            .as_ref()
            .map(|requirement| requirement.scope.clone()),
        credential: envelope.extension.credential().cloned().or_else(|| {
            envelope
                .extension
                .egress()
                .map(|egress| egress.credential().clone())
        }),
        egress: envelope.extension.egress().cloned(),
        credentials: envelope.extension.credentials().clone(),
        egresses: envelope.extension.egresses().clone(),
        effect: envelope.effect,
        binding: binding.clone(),
        operation: operation.clone(),
        arguments: arguments.to_vec(),
    })
}

#[cfg(test)]
pub(crate) fn interrupt(
    request: &BrokerInvocation<'_>,
    kind: TransportAuthKind,
    operation: &TransportOperation,
    transport_binding: &TransportBinding,
    challenged_scope: Option<&str>,
    store: &mut SqliteStore,
) -> Result<TransportAuthChallenge, BrokerError> {
    let (binding, challenge) = prepare_challenge(
        request,
        kind,
        operation,
        transport_binding,
        challenged_scope,
        store,
    )?;
    match stored_challenge(store, &request.envelope)? {
        None => {
            super::append_challenge(store, &request.envelope, &binding, &TRANSPORT_CHANNEL)?;
        }
        Some(record) if record == binding.record => {
            if !matches!(
                transport_resolution_state(store, &request.envelope, &binding)?,
                AuthState::Waiting
            ) {
                return Err(BrokerError::RepeatedAuthChallenge);
            }
        }
        Some(_) => return Err(BrokerError::InvalidAuthState),
    }
    Ok(challenge)
}

pub(crate) fn interrupt_dispatch(
    request: &BrokerInvocation<'_>,
    dispatch: TransportDispatch,
    kind: TransportAuthKind,
    operation: &TransportOperation,
    challenged_scope: Option<&str>,
    store: &mut SqliteStore,
) -> Result<TransportAuthChallenge, BrokerError> {
    interrupt_dispatch_inner(
        request,
        dispatch,
        kind,
        operation,
        challenged_scope,
        store,
        |_| false,
    )
}

#[allow(clippy::too_many_arguments)]
fn interrupt_dispatch_inner(
    request: &BrokerInvocation<'_>,
    dispatch: TransportDispatch,
    kind: TransportAuthKind,
    operation: &TransportOperation,
    challenged_scope: Option<&str>,
    store: &mut SqliteStore,
    crash: impl FnMut(CrashPoint) -> bool,
) -> Result<TransportAuthChallenge, BrokerError> {
    if dispatch.operation != *operation {
        return Err(BrokerError::InvalidTransportOperation);
    }
    let (binding, challenge) = prepare_challenge(
        request,
        kind,
        operation,
        &dispatch.binding,
        challenged_scope,
        store,
    )?;
    if stored_challenge(store, &request.envelope)?.is_some() {
        return Err(BrokerError::RepeatedAuthChallenge);
    }
    let challenge_bytes = record_bytes(&binding.record)?;
    if dispatch.binding
        != binding
            .record
            .transport_binding
            .clone()
            .ok_or(BrokerError::InvalidAuthState)?
    {
        return Err(BrokerError::InvalidAuthState);
    }
    let decision = grant::decide(request.envelope.grant_request());
    if !decision.is_allowed() {
        return Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(
            decision.reason(),
        )));
    }
    let outcome_record = dispatch_record(
        operation,
        request
            .envelope
            .canonical_request_digest(decision.snapshot_digest()),
        &dispatch.binding,
        TransportDispatchOutcome::AuthInterrupted.as_str(),
    )?;
    let outcome_bytes =
        serde_json::to_vec(&outcome_record).map_err(|_| BrokerError::InvalidAuthState)?;
    let combined = serde_json::to_vec(&(&binding.record, &outcome_record))
        .map_err(|_| BrokerError::InvalidAuthState)?;
    let envelope = &request.envelope;
    let events = vec![
        transport_event(
            envelope,
            binding.stream,
            TRANSPORT_CHANNEL.challenge_event,
            challenge_bytes,
        ),
        transport_event(
            envelope,
            dispatch.stream,
            OPERATION_OUTCOME_EVENT,
            outcome_bytes,
        ),
    ];
    store
        .append_with_hook(
            AppendCommand {
                idempotency_scope: auth_scope(
                    envelope,
                    binding.stream,
                    "capability.broker_transport_auth.interrupt",
                )?,
                idempotency_key: auth_key(envelope)?,
                request_digest: canonical_digest(&combined),
                claim: None,
                driver_claim: envelope.driver_claim,
                allow_quiescent_driver_claim: false,
                expected_versions: vec![
                    ExpectedStreamVersion {
                        stream: EntityId::Approval(binding.stream),
                        version: ExpectedVersion::new(0),
                    },
                    ExpectedStreamVersion {
                        stream: EntityId::Approval(dispatch.stream),
                        version: ExpectedVersion::new(2),
                    },
                ],
                events,
                response: combined,
            },
            crash,
        )
        .map_err(BrokerError::AuthStore)?;
    Ok(challenge)
}

fn prepare_challenge(
    request: &BrokerInvocation<'_>,
    kind: TransportAuthKind,
    operation: &TransportOperation,
    transport_binding: &TransportBinding,
    challenged_scope: Option<&str>,
    store: &mut SqliteStore,
) -> Result<(AuthBinding, TransportAuthChallenge), BrokerError> {
    request
        .envelope
        .preflight_authority(store, false)
        .map_err(BrokerError::Invoke)?;
    request.preflight_transport()?;
    if request.envelope.cancellation.load(Ordering::Acquire) {
        return Err(BrokerError::TransportAuthCancelled);
    }
    let requirement = auth_requirement(request)?;
    match (kind, challenged_scope) {
        (TransportAuthKind::Forbidden, None) => return Err(BrokerError::AuthScopeMismatch),
        (_, Some(scope)) if !requirement.contains_scope(scope) => {
            return Err(BrokerError::AuthScopeMismatch);
        }
        _ => {}
    }
    let binding = authorized_binding(request, requirement, kind, operation, transport_binding)?;
    let challenge = TransportAuthChallenge {
        challenge: binding.challenge(),
        kind,
        operation: operation.clone(),
    };
    Ok((binding, challenge))
}

fn transport_event(
    envelope: &InvocationEnvelope<'_>,
    stream: ApprovalId,
    event_type: &'static str,
    payload: Vec<u8>,
) -> NewEvent {
    NewEvent {
        id: EventId::from_stable_bytes(
            format!("{}:{stream}", auth_identity(envelope, event_type)).as_bytes(),
        ),
        stream: EntityId::Approval(stream),
        event_type: EventType::parse(event_type).expect("transport event type is valid"),
        schema_version: SchemaVersion::CURRENT,
        occurred_at: envelope.occurred_at.clone(),
        causation_id: envelope.command_id,
        correlation_id: EntityId::Run(envelope.config.run_id()),
        attempt_id: Some(envelope.attempt.attempt_id),
        trace_id: envelope.trace_id.clone(),
        payload,
        artifacts: b"[]".to_vec(),
    }
}

pub(crate) fn state(
    request: &BrokerInvocation<'_>,
    transport_binding: &TransportBinding,
    store: &mut SqliteStore,
) -> Result<TransportAuthState, BrokerError> {
    request
        .envelope
        .preflight_authority(store, true)
        .map_err(BrokerError::Invoke)?;
    request.preflight_transport()?;
    if request.envelope.cancellation.load(Ordering::Acquire) {
        return Err(BrokerError::TransportAuthCancelled);
    }
    let requirement = auth_requirement(request)?;
    let Some(record) = stored_challenge(store, &request.envelope)? else {
        return Ok(TransportAuthState::Absent);
    };
    let kind = TransportAuthKind::from_record(record.transport_kind.as_deref())?;
    let operation = record_operation(&record)?;
    let binding = authorized_binding(request, requirement, kind, &operation, transport_binding)?;
    if record != binding.record {
        return Err(BrokerError::InvalidAuthState);
    }
    let challenge = TransportAuthChallenge {
        challenge: binding.challenge(),
        kind,
        operation,
    };
    match transport_resolution_state(store, &request.envelope, &binding)? {
        AuthState::Missing => unreachable!("transport resolution state never returns missing"),
        AuthState::Waiting => Ok(TransportAuthState::Pending(challenge)),
        AuthState::Granted => {
            if replay_consumed(store, &request.envelope, &binding)? {
                Ok(TransportAuthState::Replayed)
            } else {
                Ok(TransportAuthState::Granted(challenge))
            }
        }
        AuthState::Denied => Ok(TransportAuthState::Denied),
    }
}

pub(crate) fn resume(
    request: &BrokerInvocation<'_>,
    actor: &AuthenticatedPrincipal,
    server_id: &str,
    transport: &str,
    endpoint: &str,
    resolution: AuthResolution,
    store: &mut SqliteStore,
) -> Result<(), BrokerError> {
    resume_expected(
        request, actor, server_id, transport, endpoint, resolution, None, store,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_expected(
    request: &BrokerInvocation<'_>,
    actor: &AuthenticatedPrincipal,
    server_id: &str,
    transport: &str,
    endpoint: &str,
    resolution: AuthResolution,
    expected: Option<(ApprovalId, &str, u64)>,
    store: &mut SqliteStore,
) -> Result<(), BrokerError> {
    request
        .envelope
        .preflight_authority(store, true)
        .map_err(BrokerError::Invoke)?;
    request.preflight_transport()?;
    if request.envelope.cancellation.load(Ordering::Acquire) {
        kernel_invoke::capture_rejected(
            &request.envelope,
            store,
            super::learning_broker_failure(
                &BrokerError::AuthResolutionCancelled,
                request.envelope.retry_safety,
            ),
        )
        .map_err(BrokerError::Invoke)?;
        return Err(BrokerError::AuthResolutionCancelled);
    }
    let requirement = auth_requirement(request)?;
    if actor.principal_id() != request.envelope.authenticated.principal_id()
        || actor.grant_snapshot().project_id() != request.envelope.project_id
    {
        return Err(BrokerError::AuthPrincipalMismatch);
    }
    let decision = grant::decide(request.envelope.grant_request_for(actor));
    if !decision.is_allowed() {
        return Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(
            decision.reason(),
        )));
    }
    let Some(record) = stored_challenge(store, &request.envelope)? else {
        return Err(BrokerError::InvalidAuthState);
    };
    if let Some((challenge_id, kind, generation)) = expected
        && (record.challenge_id != challenge_id.to_string()
            || record.challenge_kind != kind
            || record.generation != generation)
    {
        return Err(BrokerError::InvalidAuthState);
    }
    let kind = TransportAuthKind::from_record(record.transport_kind.as_deref())?;
    let operation = record_operation(&record)?;
    let persisted_binding = record
        .transport_binding
        .clone()
        .ok_or(BrokerError::InvalidAuthState)?;
    let expected_connection = TransportBinding::new(request, server_id, transport, endpoint, None);
    if !persisted_binding.same_connection(&expected_connection) {
        return Err(BrokerError::InvalidAuthState);
    }
    let binding = AuthBinding::build(
        request,
        requirement,
        decision.snapshot_digest(),
        TRANSPORT_STREAM_KIND,
        Some((kind.as_str(), operation.as_str())),
        Some(&persisted_binding),
    )?;
    if record != binding.record {
        return Err(BrokerError::InvalidAuthState);
    }
    match transport_resolution_state(store, &request.envelope, &binding)? {
        AuthState::Missing => unreachable!("transport resolution state never returns missing"),
        AuthState::Waiting | AuthState::Granted | AuthState::Denied => {}
    }
    append_resolution(
        store,
        &request.envelope,
        &binding,
        resolution,
        &TRANSPORT_CHANNEL,
    )?;
    if resolution == AuthResolution::Denied {
        kernel_invoke::capture_rejected(
            &request.envelope,
            store,
            super::learning_broker_failure(&BrokerError::AuthDenied, request.envelope.retry_safety),
        )
        .map_err(BrokerError::Invoke)?;
    }
    Ok(())
}

pub(crate) fn resume_bound_expected(
    request: &BrokerInvocation<'_>,
    actor: &AuthenticatedPrincipal,
    binding: &TransportBinding,
    resolution: AuthResolution,
    expected: (ApprovalId, &str, u64),
    store: &mut SqliteStore,
) -> Result<(), BrokerError> {
    resume_expected(
        request,
        actor,
        &binding.server_id,
        &binding.transport,
        &binding.endpoint,
        resolution,
        Some(expected),
        store,
    )
}

pub(crate) fn begin_dispatch(
    request: &BrokerInvocation<'_>,
    operation: &TransportOperation,
    transport_binding: &TransportBinding,
    replay: bool,
    store: &mut SqliteStore,
) -> Result<TransportDispatch, BrokerError> {
    request
        .envelope
        .preflight_authority(store, request.lifecycle_shutdown())
        .map_err(BrokerError::Invoke)?;
    request.preflight_transport()?;
    if request.envelope.cancellation.load(Ordering::Acquire) && !request.lifecycle_shutdown() {
        return Err(BrokerError::TransportAuthCancelled);
    }
    if replay {
        if request.retry_safety() == crate::capabilities::kernel::invoke::RetrySafety::NonIdempotent
        {
            return Err(BrokerError::TransportOutcomeUnknown);
        }
        let requirement = auth_requirement(request)?;
        let record =
            stored_challenge(store, &request.envelope)?.ok_or(BrokerError::ReplayNotAuthorized)?;
        let kind = TransportAuthKind::from_record(record.transport_kind.as_deref())?;
        let challenged_operation = record_operation(&record)?;
        if challenged_operation != *operation {
            return Err(BrokerError::ReplayNotAuthorized);
        }
        let binding = authorized_binding(request, requirement, kind, operation, transport_binding)?;
        if record != binding.record {
            return Err(BrokerError::InvalidAuthState);
        }
        match transport_resolution_state(store, &request.envelope, &binding)? {
            AuthState::Missing => unreachable!("transport resolution state never returns missing"),
            AuthState::Waiting => return Err(BrokerError::ReplayNotAuthorized),
            AuthState::Denied => return Err(BrokerError::AuthDenied),
            AuthState::Granted => {}
        }
        if dispatch_outcome_for_operation(store, request, operation, transport_binding, false)?
            != Some("auth_interrupted")
        {
            return Err(BrokerError::ReplayNotAuthorized);
        }
        if replay_consumed(store, &request.envelope, &binding)? {
            return Err(BrokerError::ReplayPermitConsumed);
        }
    }
    let decision = grant::decide(request.envelope.grant_request());
    if !decision.is_allowed() {
        return Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(
            decision.reason(),
        )));
    }
    let (stream, digest, request_digest) = dispatch_parts(
        request,
        operation,
        transport_binding,
        replay,
        decision.snapshot_digest(),
    )?;
    let record =
        |status: &str| dispatch_record(operation, request_digest, transport_binding, status);
    let intent_record = record("intent")?;
    let dispatched_record = record("dispatched")?;
    let intent = serde_json::to_vec(&intent_record).map_err(|_| BrokerError::InvalidAuthState)?;
    let dispatched =
        serde_json::to_vec(&dispatched_record).map_err(|_| BrokerError::InvalidAuthState)?;
    let durable_dispatch = serde_json::to_vec(&(&intent_record, &dispatched_record))
        .map_err(|_| BrokerError::InvalidAuthState)?;
    let envelope = &request.envelope;
    let append_outcome = store
        .append(AppendCommand {
            idempotency_scope: auth_scope(envelope, stream, OPERATION_DISPATCH_COMMAND)?,
            idempotency_key: auth_key(envelope)?,
            request_digest: canonical_digest(&durable_dispatch),
            claim: None,
            driver_claim: envelope.driver_claim,
            allow_quiescent_driver_claim: request.lifecycle_shutdown(),
            expected_versions: vec![ExpectedStreamVersion {
                stream: EntityId::Approval(stream),
                version: ExpectedVersion::new(0),
            }],
            events: vec![
                transport_event(envelope, stream, OPERATION_INTENT_EVENT, intent),
                transport_event(envelope, stream, OPERATION_DISPATCH_EVENT, dispatched),
            ],
            response: durable_dispatch,
        })
        .map_err(BrokerError::AuthStore)?;
    if matches!(append_outcome, AppendOutcome::Replayed(_)) {
        match dispatch_outcome(
            store,
            request,
            operation,
            transport_binding,
            replay,
            stream,
            digest,
        )? {
            Some("completed") | Some("auth_interrupted") => {
                return Err(BrokerError::TransportAlreadyCompleted);
            }
            Some("outcome_unknown") | None => return Err(BrokerError::TransportOutcomeUnknown),
            Some(_) => return Err(BrokerError::InvalidAuthState),
        }
    }
    Ok(TransportDispatch {
        stream,
        digest,
        operation: operation.clone(),
        binding: transport_binding.clone(),
        allow_quiescent_driver_claim: request.lifecycle_shutdown(),
    })
}

pub(crate) fn finish_dispatch(
    request: &BrokerInvocation<'_>,
    dispatch: TransportDispatch,
    outcome: TransportDispatchOutcome,
    store: &mut SqliteStore,
) -> Result<(), BrokerError> {
    let decision = grant::decide(request.envelope.grant_request());
    if !decision.is_allowed() {
        return Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(
            decision.reason(),
        )));
    }
    let record = dispatch_record(
        &dispatch.operation,
        request
            .envelope
            .canonical_request_digest(decision.snapshot_digest()),
        &dispatch.binding,
        outcome.as_str(),
    )?;
    let payload = serde_json::to_vec(&record).map_err(|_| BrokerError::InvalidAuthState)?;
    append_auth(
        store,
        &request.envelope,
        dispatch.stream,
        OPERATION_OUTCOME_COMMAND,
        OPERATION_OUTCOME_EVENT,
        2,
        dispatch.digest,
        payload,
        dispatch.allow_quiescent_driver_claim,
    )
    .map(|_| ())
}

fn dispatch_outcome(
    store: &mut SqliteStore,
    request: &BrokerInvocation<'_>,
    operation: &TransportOperation,
    binding: &TransportBinding,
    replay: bool,
    stream: ApprovalId,
    digest: CanonicalRequestDigest,
) -> Result<Option<&'static str>, BrokerError> {
    let decision = grant::decide(request.envelope.grant_request());
    if !decision.is_allowed() {
        return Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(
            decision.reason(),
        )));
    }
    let request_digest = request
        .envelope
        .canonical_request_digest(decision.snapshot_digest());
    let (expected_stream, expected_digest, _) = dispatch_parts(
        request,
        operation,
        binding,
        replay,
        decision.snapshot_digest(),
    )?;
    if stream != expected_stream || digest != expected_digest {
        return Err(BrokerError::InvalidAuthState);
    }
    let intent = dispatch_record(operation, request_digest, binding, "intent")?;
    let dispatched = dispatch_record(operation, request_digest, binding, "dispatched")?;
    let durable_dispatch =
        serde_json::to_vec(&(&intent, &dispatched)).map_err(|_| BrokerError::InvalidAuthState)?;
    let events = store.events().map_err(BrokerError::AuthStore)?;
    let records = |event_type: &str| {
        events
            .iter()
            .filter(|stored| {
                stored.event.stream == EntityId::Approval(stream)
                    && stored.event.event_type.as_str() == event_type
            })
            .collect::<Vec<_>>()
    };
    let intents = records(OPERATION_INTENT_EVENT);
    let dispatched_events = records(OPERATION_DISPATCH_EVENT);
    let outcomes = records(OPERATION_OUTCOME_EVENT);
    if intents.is_empty() && dispatched_events.is_empty() && outcomes.is_empty() {
        let dispatch_status = store
            .idempotency_status(
                &auth_scope(&request.envelope, stream, OPERATION_DISPATCH_COMMAND)?,
                &auth_key(&request.envelope)?,
            )
            .map_err(BrokerError::AuthStore)?;
        let outcome_status = store
            .idempotency_status(
                &auth_scope(&request.envelope, stream, OPERATION_OUTCOME_COMMAND)?,
                &auth_key(&request.envelope)?,
            )
            .map_err(BrokerError::AuthStore)?;
        return if matches!(dispatch_status, IdempotencyStatus::Missing)
            && matches!(outcome_status, IdempotencyStatus::Missing)
        {
            Ok(None)
        } else {
            Err(BrokerError::InvalidAuthState)
        };
    }
    if intents.len() != 1
        || dispatched_events.len() != 1
        || checked_dispatch_record(
            &intents[0].event.payload,
            operation,
            request_digest,
            binding,
            "intent",
        )? != intent
        || checked_dispatch_record(
            &dispatched_events[0].event.payload,
            operation,
            request_digest,
            binding,
            "dispatched",
        )? != dispatched
    {
        return Err(BrokerError::InvalidAuthState);
    }
    match store
        .idempotency_status(
            &auth_scope(&request.envelope, stream, OPERATION_DISPATCH_COMMAND)?,
            &auth_key(&request.envelope)?,
        )
        .map_err(BrokerError::AuthStore)?
    {
        IdempotencyStatus::Terminal {
            request_digest,
            result,
        } if request_digest == canonical_digest(&durable_dispatch)
            && result.commit_positions.len() == 2
            && result.response == durable_dispatch => {}
        _ => return Err(BrokerError::InvalidAuthState),
    }

    if outcomes.len() > 1 {
        return Err(BrokerError::InvalidAuthState);
    }
    let persisted = outcomes
        .first()
        .map(|stored| {
            let record: TransportDispatchRecord = serde_json::from_slice(&stored.event.payload)
                .map_err(|_| BrokerError::InvalidAuthState)?;
            let status = dispatch_status(&record)?;
            checked_dispatch_record(
                &stored.event.payload,
                operation,
                request_digest,
                binding,
                status,
            )?;
            Ok((status, stored.event.payload.as_slice()))
        })
        .transpose()?;
    match store
        .idempotency_status(
            &auth_scope(&request.envelope, stream, OPERATION_OUTCOME_COMMAND)?,
            &auth_key(&request.envelope)?,
        )
        .map_err(BrokerError::AuthStore)?
    {
        IdempotencyStatus::Missing
            if persisted.is_some_and(|(status, _)| status == "auth_interrupted") =>
        {
            Ok(Some("auth_interrupted"))
        }
        IdempotencyStatus::Missing if persisted.is_none() => Ok(None),
        IdempotencyStatus::Missing => Err(BrokerError::InvalidAuthState),
        IdempotencyStatus::Pending { .. } => Err(BrokerError::InvalidAuthState),
        IdempotencyStatus::Terminal {
            request_digest,
            result,
        } => {
            let Some((status, payload)) = persisted else {
                return Err(BrokerError::InvalidAuthState);
            };
            if request_digest != digest
                || result.commit_positions.len() != 1
                || result.response != payload
            {
                return Err(BrokerError::InvalidAuthState);
            }
            Ok(Some(status))
        }
    }
}

fn dispatch_outcome_for_operation(
    store: &mut SqliteStore,
    request: &BrokerInvocation<'_>,
    operation: &TransportOperation,
    binding: &TransportBinding,
    replay: bool,
) -> Result<Option<&'static str>, BrokerError> {
    let decision = grant::decide(request.envelope.grant_request());
    if !decision.is_allowed() {
        return Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(
            decision.reason(),
        )));
    }
    let (stream, digest, _) = dispatch_parts(
        request,
        operation,
        binding,
        replay,
        decision.snapshot_digest(),
    )?;
    dispatch_outcome(store, request, operation, binding, replay, stream, digest)
}

fn dispatch_parts(
    request: &BrokerInvocation<'_>,
    operation: &TransportOperation,
    binding: &TransportBinding,
    replay: bool,
    decision_digest: crate::capabilities::kernel::identity::Digest,
) -> Result<(ApprovalId, CanonicalRequestDigest, CanonicalRequestDigest), BrokerError> {
    let request_digest = request.envelope.canonical_request_digest(decision_digest);
    let identity = format!(
        "{}:{}:{}:{}",
        auth_identity(&request.envelope, "transport_operation"),
        operation.as_str(),
        binding.digest()?,
        if replay { "replay" } else { "initial" }
    );
    let stream = ApprovalId::from_stable_bytes(identity.as_bytes());
    let intent = serde_json::to_vec(&dispatch_record(
        operation,
        request_digest,
        binding,
        "intent",
    )?)
    .map_err(|_| BrokerError::InvalidAuthState)?;
    Ok((stream, canonical_digest(&intent), request_digest))
}

fn dispatch_record(
    operation: &TransportOperation,
    request_digest: CanonicalRequestDigest,
    binding: &TransportBinding,
    status: &str,
) -> Result<TransportDispatchRecord, BrokerError> {
    Ok(TransportDispatchRecord {
        schema_version: AUTH_RECORD_VERSION,
        operation: operation.as_str().to_owned(),
        request_digest: digest_string(request_digest),
        binding_digest: binding.digest()?,
        binding: binding.clone(),
        status: status.to_owned(),
    })
}

fn checked_dispatch_record(
    bytes: &[u8],
    operation: &TransportOperation,
    request_digest: CanonicalRequestDigest,
    binding: &TransportBinding,
    status: &str,
) -> Result<TransportDispatchRecord, BrokerError> {
    let record: TransportDispatchRecord =
        serde_json::from_slice(bytes).map_err(|_| BrokerError::InvalidAuthState)?;
    let expected = dispatch_record(operation, request_digest, binding, status)?;
    if record == expected {
        Ok(record)
    } else {
        Err(BrokerError::InvalidAuthState)
    }
}

fn dispatch_status(record: &TransportDispatchRecord) -> Result<&'static str, BrokerError> {
    match record.status.as_str() {
        "completed" => Ok("completed"),
        "auth_interrupted" => Ok("auth_interrupted"),
        "outcome_unknown" => Ok("outcome_unknown"),
        _ => Err(BrokerError::InvalidAuthState),
    }
}

fn digest_string(digest: CanonicalRequestDigest) -> String {
    digest_hex(digest)
}

fn auth_requirement<'a>(
    request: &'a BrokerInvocation<'_>,
) -> Result<&'a BrokerAuthRequirement, BrokerError> {
    let requirement = request.auth.as_ref().ok_or(BrokerError::AuthNotRequired)?;
    ensure_auth_credential(&request.envelope, requirement)?;
    Ok(requirement)
}

fn authorized_binding(
    request: &BrokerInvocation<'_>,
    requirement: &BrokerAuthRequirement,
    kind: TransportAuthKind,
    operation: &TransportOperation,
    transport_binding: &TransportBinding,
) -> Result<AuthBinding, BrokerError> {
    let decision = grant::decide(request.envelope.grant_request());
    if !decision.is_allowed() {
        return Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(
            decision.reason(),
        )));
    }
    AuthBinding::build(
        request,
        requirement,
        decision.snapshot_digest(),
        TRANSPORT_STREAM_KIND,
        Some((kind.as_str(), operation.as_str())),
        Some(transport_binding),
    )
}

fn record_operation(record: &AuthRecord) -> Result<TransportOperation, BrokerError> {
    TransportOperation::parse(record.transport_operation.as_deref().unwrap_or_default())
        .map_err(|_| BrokerError::InvalidAuthState)
}

fn stored_challenge(
    store: &mut SqliteStore,
    envelope: &InvocationEnvelope<'_>,
) -> Result<Option<AuthRecord>, BrokerError> {
    let identity = auth_identity(envelope, TRANSPORT_STREAM_KIND);
    let stream = ApprovalId::from_stable_bytes(identity.as_bytes());
    let persisted = store
        .events()
        .map_err(BrokerError::AuthStore)?
        .into_iter()
        .filter(|stored| {
            stored.event.stream == EntityId::Approval(stream)
                && stored.event.event_type.as_str() == TRANSPORT_CHANNEL.challenge_event
        })
        .collect::<Vec<_>>();
    if persisted.len() > 1 {
        return Err(BrokerError::InvalidAuthState);
    }
    if let Some(stored) = persisted.first() {
        return checked_record(&stored.event.payload).map(Some);
    }
    let status = store
        .idempotency_status(
            &auth_scope(envelope, stream, TRANSPORT_CHANNEL.challenge_command)?,
            &auth_key(envelope)?,
        )
        .map_err(BrokerError::AuthStore)?;
    match status {
        IdempotencyStatus::Missing => Ok(None),
        IdempotencyStatus::Pending { .. } => Err(BrokerError::InvalidAuthState),
        IdempotencyStatus::Terminal { result, .. } => {
            if result.commit_positions.len() != 1 {
                return Err(BrokerError::InvalidAuthState);
            }
            Ok(Some(checked_record(&result.response)?))
        }
    }
}

fn transport_resolution_state(
    store: &mut SqliteStore,
    envelope: &InvocationEnvelope<'_>,
    binding: &AuthBinding,
) -> Result<AuthState, BrokerError> {
    match store
        .idempotency_status(
            &auth_scope(
                envelope,
                binding.stream,
                TRANSPORT_CHANNEL.resolution_command,
            )?,
            &auth_key(envelope)?,
        )
        .map_err(BrokerError::AuthStore)?
    {
        IdempotencyStatus::Missing => Ok(AuthState::Waiting),
        IdempotencyStatus::Pending { .. } => Err(BrokerError::InvalidAuthState),
        IdempotencyStatus::Terminal {
            request_digest,
            result,
        } => {
            if result.commit_positions.len() != 1 {
                return Err(BrokerError::InvalidAuthState);
            }
            let record = checked_record(&result.response)?;
            let resolution = match record.resolution.as_deref() {
                Some("granted") => AuthResolution::Granted,
                Some("denied") => AuthResolution::Denied,
                _ => return Err(BrokerError::InvalidAuthState),
            };
            let (expected, _, digest) = binding.resolution(resolution)?;
            if request_digest != digest || record != expected {
                return Err(BrokerError::InvalidAuthState);
            }
            Ok(match resolution {
                AuthResolution::Granted => AuthState::Granted,
                AuthResolution::Denied => AuthState::Denied,
            })
        }
    }
}

fn replay_consumed(
    store: &mut SqliteStore,
    envelope: &InvocationEnvelope<'_>,
    binding: &AuthBinding,
) -> Result<bool, BrokerError> {
    let replay_identity = format!(
        "{}:{}:{}:replay",
        auth_identity(envelope, "transport_operation"),
        binding
            .record
            .transport_operation
            .as_deref()
            .ok_or(BrokerError::InvalidAuthState)?,
        binding
            .record
            .transport_binding
            .as_ref()
            .ok_or(BrokerError::InvalidAuthState)?
            .digest()?,
    );
    let replay_stream = ApprovalId::from_stable_bytes(replay_identity.as_bytes());
    if store
        .events()
        .map_err(BrokerError::AuthStore)?
        .iter()
        .any(|stored| {
            stored.event.stream == EntityId::Approval(replay_stream)
                && stored.event.event_type.as_str() == OPERATION_DISPATCH_EVENT
        })
    {
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64},
        },
    };

    use super::*;
    use crate::{
        api::{
            auth::{
                contract::{Authenticator, GrantSnapshot},
                local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
            },
            service::AttemptDriverClaim,
        },
        capabilities::{
            kernel::{
                grant::{
                    ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass,
                },
                grant_ext::{GrantExtension, RequestExtension},
                identity::{
                    CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                    CapabilityVersion, Digest, DigestAlgorithm,
                },
                invoke::{ApprovalState, RetrySafety},
            },
            schema::{JSON_SCHEMA_2020_12, NormalizedSchema},
        },
        domain::{
            config::{
                BudgetLayer, CONFIG_SCHEMA_VERSION, ConcurrencyLayer, ConfigLayer, Executor, Grant,
                LayerStack, Provider, RetentionLayer, RunConfigContext, RunConfigSnapshot,
            },
            events::{TraceId, UtcDateTime},
            ids::{
                AttemptId, CommandId, EventId, PrincipalId, ProjectId, RunId, ToolCallId,
                WorkspaceId,
            },
            lifecycle::{AttemptOwnership, FencingToken},
            secret::SecretHandle,
        },
        runtime::scheduler::limits::Spend,
        store::sqlite::{append::StoreError, idempotency::IdempotencyKey},
        test_support,
    };

    const UID: u32 = 501;
    const SCOPE: &str = "workspace.read:path";
    const OPERATION: &str = "tools/call";
    const ARGUMENTS: &[u8] = br#"{"path":"README.md"}"#;
    const SCHEMA: &[u8] = br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#;

    struct TestDatabase {
        directory: PathBuf,
        path: PathBuf,
    }

    impl TestDatabase {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "kit-broker-transport-auth-{}",
                EventId::generate().unwrap()
            ));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("store.sqlite3");
            Self { directory, path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    struct Inputs {
        authenticated: AuthenticatedPrincipal,
        authority: BTreeSet<Grant>,
        config: RunConfigSnapshot,
        grants: CapabilityGrantSnapshot,
        capability: CapabilityIdentity,
        schema: Digest,
        normalized_schema: NormalizedSchema,
        constraints: ArgumentConstraints,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        invocation_id: ToolCallId,
        key: IdempotencyKey,
        attempt: AttemptOwnership,
        command_id: CommandId,
        intent_event_id: EventId,
        outcome_event_id: EventId,
        occurred_at: UtcDateTime,
        trace_id: TraceId,
        auth_credential: SecretHandle,
        claim: AttemptDriverClaim,
    }

    impl Inputs {
        fn new() -> Self {
            let principal_id = PrincipalId::generate().unwrap();
            let project_id = ProjectId::generate().unwrap();
            let workspace_id = WorkspaceId::generate().unwrap();
            let authority = BTreeSet::from([Grant::WorkspaceRead]);
            let config = config(principal_id, project_id, authority.clone());
            let authenticated = LocalPeerAuthenticator::new(BTreeMap::from([(
                UID,
                GrantSnapshot::new(principal_id, project_id, authority.clone()),
            )]))
            .authenticate(&LocalPeerObservation::from_transport(UID, 42, UID))
            .unwrap();
            let capability = CapabilityIdentity::new(
                CapabilitySource::new("external").unwrap(),
                CapabilityNamespace::new("fixture.files").unwrap(),
                CapabilityName::new("read").unwrap(),
                CapabilityVersion::new("1.0.0").unwrap(),
                Digest::of(DigestAlgorithm::Blake3, b"transport auth implementation"),
            );
            let normalized_schema = NormalizedSchema::ingest(
                SCHEMA,
                JSON_SCHEMA_2020_12,
                b"transport auth schema",
                DigestAlgorithm::Sha256,
            )
            .unwrap();
            let schema = normalized_schema.source().normalized_digest();
            let constraints = ArgumentConstraints::new([b"workspace=root".as_slice()]);
            let auth_credential = SecretHandle::parse("fault-keychain:item").unwrap();
            let grants = CapabilityGrantSnapshot::new(
                &config,
                [CapabilityGrant::new(
                    principal_id,
                    project_id,
                    workspace_id,
                    capability.clone(),
                    schema,
                    EffectClass::WorkspaceRead,
                    constraints.clone(),
                )
                .with_extension(GrantExtension::new([], [auth_credential.clone()], 0).unwrap())],
                DigestAlgorithm::Sha256,
            );
            let attempt = AttemptOwnership::new(
                AttemptId::generate().unwrap(),
                principal_id,
                FencingToken::new(7),
            );
            let claim = AttemptDriverClaim {
                run_id: config.run_id(),
                attempt_id: attempt.attempt_id,
                principal_id,
                fence: attempt.fencing_token,
                lease_version: 1,
                expires_at_unix_micros: 0,
            };
            Self {
                authenticated,
                authority,
                config,
                grants,
                capability,
                schema,
                normalized_schema,
                constraints,
                workspace_id,
                project_id,
                invocation_id: ToolCallId::generate().unwrap(),
                key: IdempotencyKey::parse("broker-transport-auth").unwrap(),
                attempt,
                command_id: CommandId::generate().unwrap(),
                intent_event_id: EventId::generate().unwrap(),
                outcome_event_id: EventId::generate().unwrap(),
                occurred_at: UtcDateTime::parse("2026-08-01T12:00:00Z").unwrap(),
                trace_id: TraceId::parse("trace-broker-transport-auth").unwrap(),
                auth_credential,
                claim,
            }
        }

        fn capability_grant(&self, capability: CapabilityIdentity) -> CapabilityGrant {
            CapabilityGrant::new(
                self.authenticated.principal_id(),
                self.project_id,
                self.workspace_id,
                capability,
                self.schema,
                EffectClass::WorkspaceRead,
                self.constraints.clone(),
            )
            .with_extension(GrantExtension::new([], [self.auth_credential.clone()], 0).unwrap())
        }

        fn mutated_grants(&self) -> CapabilityGrantSnapshot {
            let extra = CapabilityIdentity::new(
                CapabilitySource::new("external").unwrap(),
                CapabilityNamespace::new("fixture.files").unwrap(),
                CapabilityName::new("write").unwrap(),
                CapabilityVersion::new("1.0.0").unwrap(),
                Digest::of(
                    DigestAlgorithm::Blake3,
                    b"transport auth extra implementation",
                ),
            );
            CapabilityGrantSnapshot::new(
                &self.config,
                [
                    self.capability_grant(self.capability.clone()),
                    self.capability_grant(extra),
                ],
                DigestAlgorithm::Sha256,
            )
        }

        fn revoked_grants(&self) -> CapabilityGrantSnapshot {
            CapabilityGrantSnapshot::new(&self.config, std::iter::empty(), DigestAlgorithm::Sha256)
        }

        fn other_actor(&self) -> AuthenticatedPrincipal {
            LocalPeerAuthenticator::new(BTreeMap::from([(
                UID,
                GrantSnapshot::new(
                    PrincipalId::generate().unwrap(),
                    self.project_id,
                    self.authority.clone(),
                ),
            )]))
            .authenticate(&LocalPeerObservation::from_transport(UID, 42, UID))
            .unwrap()
        }

        fn request<'a>(
            &'a self,
            cancellation: &'a Arc<AtomicBool>,
            fence: &'a Arc<AtomicU64>,
        ) -> BrokerInvocation<'a> {
            self.request_scoped(cancellation, fence, &self.grants, SCOPE)
        }

        fn request_scoped<'a>(
            &'a self,
            cancellation: &'a Arc<AtomicBool>,
            fence: &'a Arc<AtomicU64>,
            grants: &'a CapabilityGrantSnapshot,
            scope: &str,
        ) -> BrokerInvocation<'a> {
            BrokerInvocation::generic(
                crate::capabilities::kernel::invoke::InvocationEnvelope {
                    authenticated: &self.authenticated,
                    config: &self.config,
                    grants,
                    delegation: None,
                    extension: RequestExtension::new(None, Some(self.auth_credential.clone())),
                    capability: &self.capability,
                    discovered_schema_digest: self.schema,
                    bound_schema_digest: self.schema,
                    effect: EffectClass::WorkspaceRead,
                    argument_constraints: &self.constraints,
                    arguments: ARGUMENTS,
                    workspace_id: self.workspace_id,
                    project_id: self.project_id,
                    invocation_id: self.invocation_id,
                    idempotency_key: &self.key,
                    reservation: Spend::new(3, 4, 0, 1, 0),
                    retry_safety: RetrySafety::Idempotent,
                    approval: ApprovalState::NotRequired,
                    cancellation,
                    attempt: self.attempt,
                    driver_claim: Some(self.claim),
                    current_fence: fence,
                    command_id: self.command_id,
                    intent_event_id: self.intent_event_id,
                    outcome_event_id: self.outcome_event_id,
                    occurred_at: &self.occurred_at,
                    trace_id: &self.trace_id,
                    learning: None,
                },
                &self.normalized_schema,
            )
            .with_auth_requirement(
                BrokerAuthRequirement::new(scope)
                    .unwrap()
                    .with_credential_id(self.auth_credential.clone()),
            )
        }
    }

    fn config(
        principal_id: PrincipalId,
        project_id: ProjectId,
        authority: BTreeSet<Grant>,
    ) -> RunConfigSnapshot {
        LayerStack {
            built_in: ConfigLayer {
                schema_version: CONFIG_SCHEMA_VERSION,
                budgets: BudgetLayer {
                    max_tokens: Some(100),
                    max_cost_microusd: Some(100),
                    max_turns: Some(100),
                },
                concurrency: ConcurrencyLayer {
                    max_runs: Some(2),
                    max_tools: Some(2),
                },
                retention: RetentionLayer {
                    event_days: Some(7),
                    artifact_days: Some(7),
                },
                provider: Some(Provider::Anthropic),
                executor: Some(Executor::Local),
                grammar_edit: Some(Default::default()),
                grants: Some(authority.clone()),
            },
            user: None,
            project: None,
            run: None,
            experiment: None,
        }
        .materialize(
            RunConfigContext {
                principal_id,
                project_id,
                run_id: RunId::generate().unwrap(),
            },
            &authority,
        )
        .unwrap()
    }

    fn open(inputs: &Inputs, path: &Path) -> SqliteStore {
        let mut store = test_support::open_sqlite_store(path).unwrap();
        store.install_driver_claim_for_test(inputs.claim).unwrap();
        store
    }

    fn event_types(store: &SqliteStore) -> Vec<String> {
        store
            .events()
            .unwrap()
            .into_iter()
            .map(|stored| stored.event.event_type.as_str().to_owned())
            .collect()
    }

    fn operation() -> TransportOperation {
        TransportOperation::parse(OPERATION).unwrap()
    }

    fn binding(request: &BrokerInvocation<'_>) -> TransportBinding {
        TransportBinding::new(
            request,
            "test-server",
            "http",
            "https://example.test/mcp",
            None,
        )
    }

    fn interrupt_with(
        request: &BrokerInvocation<'_>,
        kind: TransportAuthKind,
        store: &mut SqliteStore,
    ) -> Result<TransportAuthChallenge, BrokerError> {
        interrupt(
            request,
            kind,
            &operation(),
            &binding(request),
            Some(SCOPE),
            store,
        )
    }

    fn resume_granted(
        request: &BrokerInvocation<'_>,
        actor: &AuthenticatedPrincipal,
        store: &mut SqliteStore,
    ) -> Result<(), BrokerError> {
        resume(
            request,
            actor,
            "test-server",
            "http",
            "https://example.test/mcp",
            AuthResolution::Granted,
            store,
        )
    }

    fn replace_attempt(inputs: &mut Inputs) -> Arc<AtomicU64> {
        let fence = inputs.attempt.fencing_token.get() + 1;
        inputs.attempt = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            inputs.authenticated.principal_id(),
            FencingToken::new(fence),
        );
        inputs.claim = AttemptDriverClaim {
            run_id: inputs.config.run_id(),
            attempt_id: inputs.attempt.attempt_id,
            principal_id: inputs.authenticated.principal_id(),
            fence: inputs.attempt.fencing_token,
            lease_version: inputs.claim.lease_version + 1,
            expires_at_unix_micros: 0,
        };
        Arc::new(AtomicU64::new(fence))
    }

    #[test]
    fn crash_then_replacement_attempt_consumes_grant_once() {
        let database = TestDatabase::new();
        let mut inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let old_fence = Arc::new(AtomicU64::new(inputs.attempt.fencing_token.get()));
        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &old_fence);
        let transport_binding = binding(&request);
        let dispatch = begin_dispatch(
            &request,
            &operation(),
            &transport_binding,
            false,
            &mut store,
        )
        .unwrap();
        interrupt_dispatch(
            &request,
            dispatch,
            TransportAuthKind::Unauthorized,
            &operation(),
            Some(SCOPE),
            &mut store,
        )
        .unwrap();
        drop(store);

        let new_fence = replace_attempt(&mut inputs);
        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &new_fence);
        resume_granted(&request, &inputs.authenticated, &mut store).unwrap();
        assert!(matches!(
            state(&request, &binding(&request), &mut store),
            Ok(TransportAuthState::Granted(_))
        ));
        let replay =
            begin_dispatch(&request, &operation(), &binding(&request), true, &mut store).unwrap();
        finish_dispatch(
            &request,
            replay,
            TransportDispatchOutcome::Completed,
            &mut store,
        )
        .unwrap();
        assert!(matches!(
            begin_dispatch(&request, &operation(), &binding(&request), true, &mut store),
            Err(BrokerError::ReplayPermitConsumed)
        ));
    }

    #[test]
    fn crash_then_replacement_attempt_consumes_denial_once() {
        let database = TestDatabase::new();
        let mut inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let old_fence = Arc::new(AtomicU64::new(inputs.attempt.fencing_token.get()));
        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &old_fence);
        interrupt_with(&request, TransportAuthKind::Unauthorized, &mut store).unwrap();
        drop(store);

        let new_fence = replace_attempt(&mut inputs);
        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &new_fence);
        resume(
            &request,
            &inputs.authenticated,
            "test-server",
            "http",
            "https://example.test/mcp",
            AuthResolution::Denied,
            &mut store,
        )
        .unwrap();
        assert_eq!(
            state(&request, &binding(&request), &mut store).unwrap(),
            TransportAuthState::Denied
        );
        assert!(matches!(
            begin_dispatch(&request, &operation(), &binding(&request), true, &mut store),
            Err(BrokerError::AuthDenied)
        ));
        resume(
            &request,
            &inputs.authenticated,
            "test-server",
            "http",
            "https://example.test/mcp",
            AuthResolution::Denied,
            &mut store,
        )
        .unwrap();
    }

    #[test]
    fn pending_interruption_survives_reopen_then_resumes_and_replays_once() {
        let database = TestDatabase::new();
        let inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));

        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &fence);
        let (_, replay) = crate::protocols::mcp::transport::authorize_ready_operation(
            &request,
            &operation(),
            &binding(&request),
            request.arguments(),
            &mut store,
        )
        .unwrap();
        assert!(!replay);
        let dispatch = begin_dispatch(
            &request,
            &operation(),
            &binding(&request),
            false,
            &mut store,
        )
        .unwrap();
        let challenge = interrupt_dispatch(
            &request,
            dispatch,
            TransportAuthKind::Unauthorized,
            &operation(),
            Some(SCOPE),
            &mut store,
        )
        .unwrap();
        assert_eq!(challenge.kind, TransportAuthKind::Unauthorized);
        assert_eq!(challenge.operation.as_str(), OPERATION);
        assert_eq!(challenge.challenge.scope, SCOPE);
        assert_eq!(
            challenge.challenge.principal_id,
            inputs.authenticated.principal_id().to_string()
        );
        assert_eq!(
            challenge.challenge.invocation_id,
            inputs.invocation_id.to_string()
        );
        assert_eq!(
            challenge.challenge.credential_id,
            Some(inputs.auth_credential.clone())
        );
        let repeated =
            interrupt_with(&request, TransportAuthKind::Unauthorized, &mut store).unwrap();
        assert_eq!(repeated, challenge);
        assert_eq!(
            event_types(&store),
            [
                "capability.broker_transport_intent",
                "capability.broker_transport_dispatched",
                "capability.broker_transport_auth_challenged",
                "capability.broker_transport_outcome",
            ]
        );
        drop(store);

        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &fence);
        assert_eq!(
            state(&request, &binding(&request), &mut store).unwrap(),
            TransportAuthState::Pending(challenge.clone())
        );
        assert!(matches!(
            resume_expected(
                &request,
                &inputs.authenticated,
                "test-server",
                "http",
                "https://example.test/mcp",
                AuthResolution::Granted,
                Some((
                    challenge.challenge.challenge_id,
                    "transport",
                    challenge.challenge.generation + 1,
                )),
                &mut store,
            ),
            Err(BrokerError::InvalidAuthState)
        ));
        assert_eq!(
            state(&request, &binding(&request), &mut store).unwrap(),
            TransportAuthState::Pending(challenge.clone())
        );
        store.quiesce_driver_claim(inputs.claim).unwrap();
        resume_granted(&request, &inputs.authenticated, &mut store).unwrap();
        resume_granted(&request, &inputs.authenticated, &mut store).unwrap();
        assert_eq!(
            event_types(&store),
            [
                "capability.broker_transport_intent",
                "capability.broker_transport_dispatched",
                "capability.broker_transport_auth_challenged",
                "capability.broker_transport_outcome",
                "capability.broker_transport_auth_resolved",
            ]
        );
        drop(store);

        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &fence);
        assert_eq!(
            state(&request, &binding(&request), &mut store).unwrap(),
            TransportAuthState::Granted(challenge.clone())
        );
        let replay_binding = binding(&request);
        let (_, replay) = crate::protocols::mcp::transport::authorize_ready_operation(
            &request,
            &operation(),
            &replay_binding,
            request.arguments(),
            &mut store,
        )
        .unwrap();
        assert!(replay);
        let replay =
            begin_dispatch(&request, &operation(), &replay_binding, true, &mut store).unwrap();
        assert_eq!(
            event_types(&store),
            [
                "capability.broker_transport_intent",
                "capability.broker_transport_dispatched",
                "capability.broker_transport_auth_challenged",
                "capability.broker_transport_outcome",
                "capability.broker_transport_auth_resolved",
                "capability.broker_transport_intent",
                "capability.broker_transport_dispatched",
            ]
        );
        assert_eq!(
            state(&request, &replay_binding, &mut store).unwrap(),
            TransportAuthState::Replayed
        );
        assert!(matches!(
            begin_dispatch(&request, &operation(), &replay_binding, true, &mut store),
            Err(BrokerError::ReplayPermitConsumed)
        ));
        finish_dispatch(
            &request,
            replay,
            TransportDispatchOutcome::Completed,
            &mut store,
        )
        .unwrap();
        assert!(matches!(
            interrupt_with(&request, TransportAuthKind::Unauthorized, &mut store),
            Err(BrokerError::RepeatedAuthChallenge)
        ));

        let database = TestDatabase::new();
        let inputs = Inputs::new();
        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &fence);
        store.quiesce_driver_claim(inputs.claim).unwrap();
        assert!(matches!(
            crate::protocols::mcp::transport::authorize_ready_operation(
                &request,
                &operation(),
                &binding(&request),
                request.arguments(),
                &mut store,
            ),
            Err(crate::protocols::mcp::transport::TransportError::Broker(
                BrokerError::Invoke(InvokeError::Store(StoreError::StaleDriverClaim))
            ))
        ));
    }

    #[test]
    fn non_idempotent_auth_interruption_requires_reconciliation() {
        let database = TestDatabase::new();
        let inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let mut store = open(&inputs, database.path());
        let mut request = inputs.request(&cancellation, &fence);
        request.envelope.retry_safety = RetrySafety::NonIdempotent;
        let transport_binding = binding(&request);
        let dispatch = begin_dispatch(
            &request,
            &operation(),
            &transport_binding,
            false,
            &mut store,
        )
        .unwrap();
        interrupt_dispatch(
            &request,
            dispatch,
            TransportAuthKind::Unauthorized,
            &operation(),
            Some(SCOPE),
            &mut store,
        )
        .unwrap();
        store.quiesce_driver_claim(inputs.claim).unwrap();
        resume_granted(&request, &inputs.authenticated, &mut store).unwrap();
        store.install_driver_claim_for_test(inputs.claim).unwrap();
        assert!(matches!(
            begin_dispatch(&request, &operation(), &transport_binding, true, &mut store,),
            Err(BrokerError::TransportOutcomeUnknown)
        ));
    }

    #[test]
    fn resume_rejects_wrong_actor() {
        let database = TestDatabase::new();
        let inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &fence);
        interrupt_with(&request, TransportAuthKind::Forbidden, &mut store).unwrap();
        assert!(matches!(
            resume(
                &request,
                &inputs.authenticated,
                "test-server",
                "http",
                "https://other.example/mcp",
                AuthResolution::Granted,
                &mut store,
            ),
            Err(BrokerError::InvalidAuthState)
        ));
        assert!(matches!(
            resume_granted(&request, &inputs.other_actor(), &mut store),
            Err(BrokerError::AuthPrincipalMismatch)
        ));
        assert_eq!(
            event_types(&store),
            ["capability.broker_transport_auth_challenged"]
        );
    }

    #[test]
    fn post_ready_auth_resolution_uses_persisted_session_binding() {
        for kind in [
            TransportAuthKind::Unauthorized,
            TransportAuthKind::Forbidden,
        ] {
            let database = TestDatabase::new();
            let inputs = Inputs::new();
            let cancellation = Arc::new(AtomicBool::new(false));
            let fence = Arc::new(AtomicU64::new(7));
            let mut store = open(&inputs, database.path());
            let request = inputs.request(&cancellation, &fence);
            let session_binding =
                binding(&request).with_session(Some("current-session".to_owned()));
            let dispatch =
                begin_dispatch(&request, &operation(), &session_binding, false, &mut store)
                    .unwrap();
            interrupt_dispatch(
                &request,
                dispatch,
                kind,
                &operation(),
                Some(SCOPE),
                &mut store,
            )
            .unwrap();
            store.quiesce_driver_claim(inputs.claim).unwrap();

            resume_granted(&request, &inputs.authenticated, &mut store).unwrap();
            assert!(matches!(
                state(&request, &session_binding, &mut store),
                Ok(TransportAuthState::Granted(_))
            ));
            assert!(matches!(
                state(&request, &binding(&request), &mut store),
                Err(BrokerError::InvalidAuthState)
            ));
        }
    }

    #[test]
    fn resume_rejects_grant_mutation_revocation_and_scope_change() {
        let database = TestDatabase::new();
        let inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &fence);
        interrupt_with(&request, TransportAuthKind::Unauthorized, &mut store).unwrap();

        let mutated = inputs.mutated_grants();
        let request = inputs.request_scoped(&cancellation, &fence, &mutated, SCOPE);
        assert!(matches!(
            resume_granted(&request, &inputs.authenticated, &mut store),
            Err(BrokerError::InvalidAuthState)
        ));

        let revoked = inputs.revoked_grants();
        let request = inputs.request_scoped(&cancellation, &fence, &revoked, SCOPE);
        assert!(matches!(
            resume_granted(&request, &inputs.authenticated, &mut store),
            Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(_)))
        ));

        let request = inputs.request_scoped(
            &cancellation,
            &fence,
            &inputs.grants,
            "workspace.read:other",
        );
        assert!(matches!(
            resume_granted(&request, &inputs.authenticated, &mut store),
            Err(BrokerError::InvalidAuthState)
        ));

        let request = inputs.request(&cancellation, &fence);
        assert!(matches!(
            begin_dispatch(&request, &operation(), &binding(&request), true, &mut store,),
            Err(BrokerError::ReplayNotAuthorized)
        ));
        assert_eq!(
            event_types(&store),
            ["capability.broker_transport_auth_challenged"]
        );
    }

    #[test]
    fn cancellation_fails_closed() {
        let database = TestDatabase::new();
        let inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let mut store = open(&inputs, database.path());
        let live = inputs.request(&cancellation, &fence);
        interrupt_with(&live, TransportAuthKind::Unauthorized, &mut store).unwrap();

        let cancelled = Arc::new(AtomicBool::new(true));
        let request = inputs.request(&cancelled, &fence);
        assert!(matches!(
            state(&request, &binding(&request), &mut store),
            Err(BrokerError::TransportAuthCancelled)
        ));
        assert!(matches!(
            interrupt_with(&request, TransportAuthKind::Unauthorized, &mut store),
            Err(BrokerError::TransportAuthCancelled)
        ));
        assert!(matches!(
            resume_granted(&request, &inputs.authenticated, &mut store),
            Err(BrokerError::AuthResolutionCancelled)
        ));
        assert!(matches!(
            begin_dispatch(&request, &operation(), &binding(&request), true, &mut store,),
            Err(BrokerError::TransportAuthCancelled)
        ));
        assert_eq!(
            event_types(&store),
            ["capability.broker_transport_auth_challenged"]
        );
    }

    #[test]
    fn denied_resolution_and_conflicting_or_mutated_challenges_fail_closed() {
        let database = TestDatabase::new();
        let inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &fence);

        assert!(matches!(
            resume_granted(&request, &inputs.authenticated, &mut store),
            Err(BrokerError::InvalidAuthState)
        ));
        assert!(matches!(
            begin_dispatch(&request, &operation(), &binding(&request), true, &mut store,),
            Err(BrokerError::ReplayNotAuthorized)
        ));

        interrupt_with(&request, TransportAuthKind::Unauthorized, &mut store).unwrap();
        assert!(matches!(
            interrupt_with(&request, TransportAuthKind::Forbidden, &mut store),
            Err(BrokerError::InvalidAuthState)
        ));
        assert!(matches!(
            interrupt(
                &request,
                TransportAuthKind::Unauthorized,
                &TransportOperation::parse("resources/read").unwrap(),
                &binding(&request),
                Some(SCOPE),
                &mut store
            ),
            Err(BrokerError::InvalidAuthState)
        ));

        resume(
            &request,
            &inputs.authenticated,
            "test-server",
            "http",
            "https://example.test/mcp",
            AuthResolution::Denied,
            &mut store,
        )
        .unwrap();
        assert_eq!(
            state(&request, &binding(&request), &mut store).unwrap(),
            TransportAuthState::Denied
        );
        assert!(matches!(
            begin_dispatch(&request, &operation(), &binding(&request), true, &mut store,),
            Err(BrokerError::AuthDenied)
        ));
        assert!(matches!(
            interrupt_with(&request, TransportAuthKind::Unauthorized, &mut store),
            Err(BrokerError::RepeatedAuthChallenge)
        ));
        assert!(matches!(
            resume_granted(&request, &inputs.authenticated, &mut store),
            Err(BrokerError::AuthStore(StoreError::IdempotencyConflict(_)))
        ));
    }

    #[test]
    fn unknown_kind_and_operation_bounds_fail_closed() {
        assert!(matches!(
            TransportAuthKind::from_record(Some("basic")),
            Err(BrokerError::InvalidAuthState)
        ));
        assert!(matches!(
            TransportAuthKind::from_record(None),
            Err(BrokerError::InvalidAuthState)
        ));
        assert!(matches!(
            TransportOperation::parse(""),
            Err(BrokerError::InvalidTransportOperation)
        ));
        assert!(matches!(
            TransportOperation::parse("tools call"),
            Err(BrokerError::InvalidTransportOperation)
        ));
        assert!(matches!(
            TransportOperation::parse(&"o".repeat(MAX_TRANSPORT_OPERATION_BYTES + 1)),
            Err(BrokerError::InvalidTransportOperation)
        ));
        assert!(TransportOperation::parse(&"o".repeat(MAX_TRANSPORT_OPERATION_BYTES)).is_ok());
    }

    #[test]
    fn bounded_records_persist_opaque_handles_without_secret_bytes() {
        let database = TestDatabase::new();
        let inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let mut store = open(&inputs, database.path());
        let scope = "s".repeat(super::super::MAX_AUTH_SCOPE_BYTES);
        let request = inputs.request_scoped(&cancellation, &fence, &inputs.grants, &scope);
        let bounded_operation =
            TransportOperation::parse(&"o".repeat(MAX_TRANSPORT_OPERATION_BYTES)).unwrap();
        let challenge = interrupt(
            &request,
            TransportAuthKind::Forbidden,
            &bounded_operation,
            &binding(&request),
            Some(&scope),
            &mut store,
        )
        .unwrap();

        let debug = format!("{challenge:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("fault-keychain:item"));

        let events = store.events().unwrap();
        assert_eq!(events.len(), 1);
        let record: AuthRecord = serde_json::from_slice(&events[0].event.payload).unwrap();
        assert_eq!(record.transport_kind.as_deref(), Some("forbidden"));
        assert_eq!(
            record.transport_operation.as_deref(),
            Some(bounded_operation.as_str())
        );
        assert_eq!(record.scope, scope);
        assert_eq!(
            record.credential_id.as_deref(),
            Some(inputs.auth_credential.identifier())
        );
        assert!(events[0].event.payload.len() <= super::super::MAX_AUTH_RECORD_BYTES);
    }

    #[test]
    fn challenged_scope_must_be_known_and_equal() {
        let database = TestDatabase::new();
        let inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &fence);

        for challenged_scope in [None, Some("workspace.write:path")] {
            assert!(matches!(
                interrupt(
                    &request,
                    TransportAuthKind::Forbidden,
                    &operation(),
                    &binding(&request),
                    challenged_scope,
                    &mut store,
                ),
                Err(BrokerError::AuthScopeMismatch)
            ));
        }
        assert!(event_types(&store).is_empty());
    }

    #[test]
    fn dispatch_lifecycle_is_durable_and_requires_reconciliation_after_interruption() {
        let database = TestDatabase::new();
        let inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &fence);

        let completed = begin_dispatch(
            &request,
            &operation(),
            &binding(&request),
            false,
            &mut store,
        )
        .unwrap();
        finish_dispatch(
            &request,
            completed,
            TransportDispatchOutcome::Completed,
            &mut store,
        )
        .unwrap();
        assert!(matches!(
            begin_dispatch(
                &request,
                &operation(),
                &binding(&request),
                false,
                &mut store,
            ),
            Err(BrokerError::TransportAlreadyCompleted)
        ));

        let interrupted = TransportOperation::parse("resources/read").unwrap();
        let _dispatch = begin_dispatch(
            &request,
            &interrupted,
            &binding(&request),
            false,
            &mut store,
        )
        .unwrap();
        assert!(matches!(
            begin_dispatch(
                &request,
                &interrupted,
                &binding(&request),
                false,
                &mut store,
            ),
            Err(BrokerError::TransportOutcomeUnknown)
        ));
    }

    #[test]
    fn every_dispatch_record_field_is_validated_on_read() {
        let inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let request = inputs.request(&cancellation, &fence);
        let operation = operation();
        let binding = binding(&request);
        let decision = grant::decide(request.envelope.grant_request());
        let request_digest = request
            .envelope
            .canonical_request_digest(decision.snapshot_digest());
        let record = dispatch_record(&operation, request_digest, &binding, "intent").unwrap();
        let original = serde_json::to_value(record).unwrap();

        for pointer in [
            "/schema_version",
            "/operation",
            "/request_digest",
            "/binding_digest",
            "/binding/endpoint",
            "/status",
        ] {
            let mut malformed = original.clone();
            *malformed.pointer_mut(pointer).unwrap() = serde_json::json!("malformed");
            let bytes = serde_json::to_vec(&malformed).unwrap();
            assert!(matches!(
                checked_dispatch_record(&bytes, &operation, request_digest, &binding, "intent",),
                Err(BrokerError::InvalidAuthState)
            ));
        }
    }

    #[test]
    fn granted_challenge_without_atomic_interrupted_outcome_cannot_replay() {
        let database = TestDatabase::new();
        let inputs = Inputs::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let mut store = open(&inputs, database.path());
        let request = inputs.request(&cancellation, &fence);

        interrupt_with(&request, TransportAuthKind::Unauthorized, &mut store).unwrap();
        store.quiesce_driver_claim(inputs.claim).unwrap();
        resume_granted(&request, &inputs.authenticated, &mut store).unwrap();
        store.install_driver_claim_for_test(inputs.claim).unwrap();
        assert!(matches!(
            begin_dispatch(&request, &operation(), &binding(&request), true, &mut store,),
            Err(BrokerError::ReplayNotAuthorized)
        ));
    }

    #[test]
    fn auth_interruption_commit_is_atomic_at_every_store_crash_window() {
        use crate::store::sqlite::append::CrashPoint::*;

        for crash_at in [
            AfterTransactionBegin,
            AfterIdempotencyCheck,
            AfterExpectedVersionCheck,
            AfterEventInsert,
            AfterStreamHeadsUpdate,
            AfterWatermarkUpdate,
            BeforeIdempotencyTerminal,
            AfterIdempotencyTerminal,
            BeforeCommit,
            AfterCommit,
        ] {
            let database = TestDatabase::new();
            let inputs = Inputs::new();
            let cancellation = Arc::new(AtomicBool::new(false));
            let fence = Arc::new(AtomicU64::new(7));
            let mut store = open(&inputs, database.path());
            let request = inputs.request(&cancellation, &fence);
            let dispatch = begin_dispatch(
                &request,
                &operation(),
                &binding(&request),
                false,
                &mut store,
            )
            .unwrap();
            let result = interrupt_dispatch_inner(
                &request,
                dispatch,
                TransportAuthKind::Unauthorized,
                &operation(),
                Some(SCOPE),
                &mut store,
                |point| point == crash_at,
            );
            assert!(matches!(
                result,
                Err(BrokerError::AuthStore(StoreError::InjectedCrash(_)))
            ));
            drop(store);

            let mut store = open(&inputs, database.path());
            let event_count = event_types(&store).len();
            assert!(
                matches!(event_count, 2 | 4),
                "partial atomic state at {crash_at:?}"
            );
            if event_count == 4 {
                assert!(matches!(
                    state(
                        &inputs.request(&cancellation, &fence),
                        &binding(&inputs.request(&cancellation, &fence)),
                        &mut store,
                    ),
                    Ok(TransportAuthState::Pending(_))
                ));
            }
        }
    }
}
