use std::{cell::RefCell, collections::BTreeSet, fmt, io::Read, sync::atomic::Ordering};

use serde::{Deserialize, Serialize};

use crate::{
    agent::accounting::{AccountingError, SpeculationOutcome, ToolMeasurement, UsageEnvelope},
    capabilities::{
        catalog::CapabilityKind,
        discovery::{BindingId, CapabilityBinding},
        kernel::{
            grant,
            identity::{Digest, DigestAlgorithm},
            invoke::{
                self as kernel_invoke, AuthorizedInvocation, DispatchOutcome, InvocationCrashPoint,
                InvocationEnvelope, InvocationPhaseRuntime, InvocationResult, InvocationRuntime,
                InvokeError, MAX_INVOCATION_ARGUMENT_BYTES, PrepareOutcome,
            },
        },
        native::NativeCatalog,
        registration::InvocationContext,
        result::{
            CallProvenance, CallProvenanceInput, CanonicalResult, DelegationProvenance,
            Presentation, ResultError,
        },
        schema::{NormalizedSchema, SchemaValidation},
    },
    domain::{
        commands::ExpectedVersion,
        events::{EntityId, EventType, SchemaVersion},
        ids::{ApprovalId, EventId, ToolCallId},
        secret::SecretHandle,
    },
    runtime::scheduler::{limits::Spend, reserve::BudgetLedger},
    store::artifacts::{ArtifactClass, ArtifactMetadata, ArtifactRetention, ArtifactStore},
    store::sqlite::{
        append::{
            AppendCommand, AppendOutcome, ExpectedStreamVersion, NewEvent, SqliteStore, StoreError,
        },
        idempotency::{
            CanonicalRequestDigest, IdempotencyKey, IdempotencyScope, IdempotencyStatus,
        },
    },
};

pub mod transport_auth;

const AUTH_RECORD_VERSION: u16 = 2;
const AUTH_CHALLENGE_COMMAND: &str = "capability.broker_auth.challenge";
const AUTH_RESOLUTION_COMMAND: &str = "capability.broker_auth.resolve";
const AUTH_CHALLENGE_EVENT: &str = "capability.broker_auth_challenged";
const AUTH_RESOLUTION_EVENT: &str = "capability.broker_auth_resolved";
const MAX_AUTH_SCOPE_BYTES: usize = 512;
const MAX_AUTH_RECORD_BYTES: usize = 8192;

struct AuthChannel {
    challenge_command: &'static str,
    resolution_command: &'static str,
    challenge_event: &'static str,
    resolution_event: &'static str,
}

const REQUIREMENT_CHANNEL: AuthChannel = AuthChannel {
    challenge_command: AUTH_CHALLENGE_COMMAND,
    resolution_command: AUTH_RESOLUTION_COMMAND,
    challenge_event: AUTH_CHALLENGE_EVENT,
    resolution_event: AUTH_RESOLUTION_EVENT,
};

pub struct BrokerInvocation<'a> {
    envelope: InvocationEnvelope<'a>,
    validation_schema: &'a NormalizedSchema,
    binding: Option<&'a CapabilityBinding>,
    auth: Option<BrokerAuthRequirement>,
    transport_initialize: bool,
    lifecycle: bool,
    lifecycle_shutdown: bool,
}

pub(crate) struct OwnedBrokerInvocation {
    authenticated: crate::api::auth::contract::AuthenticatedPrincipal,
    config: crate::domain::config::RunConfigSnapshot,
    grants: crate::capabilities::kernel::grant::CapabilityGrantSnapshot,
    delegation: Option<crate::capabilities::kernel::grant::DelegationSnapshot>,
    extension: crate::capabilities::kernel::grant_ext::RequestExtension,
    capability: crate::capabilities::kernel::identity::CapabilityIdentity,
    schema: NormalizedSchema,
    argument_constraints: crate::capabilities::kernel::grant::ArgumentConstraints,
    effect: crate::capabilities::kernel::grant::EffectClass,
    arguments: Vec<u8>,
    workspace_id: crate::domain::ids::WorkspaceId,
    project_id: crate::domain::ids::ProjectId,
    invocation_id: ToolCallId,
    idempotency_key: IdempotencyKey,
    reservation: Spend,
    retry_safety: kernel_invoke::RetrySafety,
    approval: kernel_invoke::ApprovalState,
    cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
    attempt: crate::domain::lifecycle::AttemptOwnership,
    driver_claim: Option<crate::api::service::AttemptDriverClaim>,
    current_fence: std::sync::Arc<std::sync::atomic::AtomicU64>,
    command_id: crate::domain::ids::CommandId,
    intent_event_id: EventId,
    outcome_event_id: EventId,
    occurred_at: crate::domain::events::UtcDateTime,
    trace_id: crate::domain::events::TraceId,
    auth: Option<BrokerAuthRequirement>,
}

impl OwnedBrokerInvocation {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_lifecycle(
        server: &str,
        authenticated: &crate::api::auth::contract::AuthenticatedPrincipal,
        config: &crate::domain::config::RunConfigSnapshot,
        workspace_id: crate::domain::ids::WorkspaceId,
        extension: crate::capabilities::kernel::grant_ext::RequestExtension,
        attempt: crate::domain::lifecycle::AttemptOwnership,
        claim: crate::api::service::AttemptDriverClaim,
        current_fence: std::sync::Arc<std::sync::atomic::AtomicU64>,
        cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
        occurred_at: crate::domain::events::UtcDateTime,
    ) -> Result<Self, BrokerError> {
        use crate::{
            capabilities::{
                kernel::{
                    grant::{
                        ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass,
                    },
                    identity::{
                        CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                        CapabilityVersion, Digest, DigestAlgorithm,
                    },
                    invoke::{ApprovalState, RetrySafety},
                },
                schema::{JSON_SCHEMA_2020_12, NormalizedSchema},
            },
            domain::{
                events::TraceId,
                ids::{CommandId, EventId},
            },
        };

        if claim.owner() != attempt
            || claim.run_id != config.run_id()
            || authenticated.principal_id() != config.principal_id()
            || authenticated.grant_snapshot().project_id() != config.project_id()
        {
            return Err(BrokerError::InvalidAuthState);
        }
        let schema = NormalizedSchema::ingest(
            br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}"#,
            JSON_SCHEMA_2020_12,
            b"run-owned MCP lifecycle",
            DigestAlgorithm::Sha256,
        )
        .map_err(|_| BrokerError::InvalidAuthState)?;
        let effect = if extension.egress().is_some() {
            EffectClass::NetworkEgress
        } else {
            EffectClass::ProcessSpawn
        };
        let capability = CapabilityIdentity::new(
            CapabilitySource::new("mcp.lifecycle").map_err(|_| BrokerError::InvalidAuthState)?,
            CapabilityNamespace::new("kit.mcp.lifecycle")
                .map_err(|_| BrokerError::InvalidAuthState)?,
            CapabilityName::new(server).map_err(|_| BrokerError::InvalidAuthState)?,
            CapabilityVersion::new("1").map_err(|_| BrokerError::InvalidAuthState)?,
            Digest::of(DigestAlgorithm::Sha256, server.as_bytes()),
        );
        let constraints =
            ArgumentConstraints::new([format!("mcp.lifecycle={server}").into_bytes()]);
        let grants = CapabilityGrantSnapshot::new(
            config,
            [CapabilityGrant::new(
                config.principal_id(),
                config.project_id(),
                workspace_id,
                capability.clone(),
                schema.source().normalized_digest(),
                effect,
                constraints.clone(),
            )
            .with_extension(
                crate::capabilities::kernel::grant_ext::GrantExtension::new(
                    extension.egress().cloned(),
                    extension
                        .credential()
                        .cloned()
                        .into_iter()
                        .chain(extension.credentials().iter().cloned()),
                    0,
                )
                .map_err(|_| BrokerError::InvalidAuthState)?,
            )],
            DigestAlgorithm::Sha256,
        );
        let invocation_id = ToolCallId::from_stable_bytes(
            format!("mcp-lifecycle:{}:{server}", config.run_id()).as_bytes(),
        );
        let auth = extension
            .credential()
            .cloned()
            .map(|credential| {
                BrokerAuthRequirement::new("mcp.connection")
                    .map(|value| value.with_credential_id(credential))
            })
            .transpose()?;
        Ok(Self {
            authenticated: authenticated.clone(),
            config: config.clone(),
            grants,
            delegation: None,
            extension,
            capability,
            schema,
            argument_constraints: constraints,
            effect,
            arguments: serde_json::to_vec(
                &crate::protocols::mcp::transport::authorized_initialize_arguments(),
            )
            .map_err(|_| BrokerError::InvalidAuthState)?,
            workspace_id,
            project_id: config.project_id(),
            invocation_id,
            idempotency_key: IdempotencyKey::parse(&format!("mcp-lifecycle-{invocation_id}"))
                .map_err(|_| BrokerError::InvalidAuthState)?,
            reservation: Spend::new(0, 0, 0, 1, 0),
            retry_safety: RetrySafety::Idempotent,
            approval: ApprovalState::NotRequired,
            cancellation,
            attempt,
            driver_claim: Some(claim),
            current_fence,
            command_id: CommandId::from_stable_bytes(
                format!("mcp-lifecycle-command:{}:{server}", config.run_id()).as_bytes(),
            ),
            intent_event_id: EventId::from_stable_bytes(
                format!("mcp-lifecycle-intent:{}:{server}", config.run_id()).as_bytes(),
            ),
            outcome_event_id: EventId::from_stable_bytes(
                format!("mcp-lifecycle-outcome:{}:{server}", config.run_id()).as_bytes(),
            ),
            occurred_at,
            trace_id: TraceId::parse("mcp-run-lifecycle")
                .map_err(|_| BrokerError::InvalidAuthState)?,
            auth,
        })
    }

    pub(crate) fn capture(request: &BrokerInvocation<'_>) -> Self {
        let envelope = &request.envelope;
        Self {
            authenticated: envelope.authenticated.clone(),
            config: envelope.config.clone(),
            grants: envelope.grants.clone(),
            delegation: envelope.delegation.cloned(),
            extension: envelope.extension.clone(),
            capability: envelope.capability.clone(),
            schema: request.validation_schema.clone(),
            argument_constraints: envelope.argument_constraints.clone(),
            effect: envelope.effect,
            arguments: envelope.arguments.to_vec(),
            workspace_id: envelope.workspace_id,
            project_id: envelope.project_id,
            invocation_id: envelope.invocation_id,
            idempotency_key: envelope.idempotency_key.clone(),
            reservation: envelope.reservation,
            retry_safety: envelope.retry_safety,
            approval: envelope.approval,
            cancellation: std::sync::Arc::clone(envelope.cancellation),
            attempt: envelope.attempt,
            driver_claim: envelope.driver_claim,
            current_fence: std::sync::Arc::clone(envelope.current_fence),
            command_id: envelope.command_id,
            intent_event_id: envelope.intent_event_id,
            outcome_event_id: envelope.outcome_event_id,
            occurred_at: envelope.occurred_at.clone(),
            trace_id: envelope.trace_id.clone(),
            auth: request.auth.clone(),
        }
    }

    pub(crate) fn mint(&self) -> Result<Self, BrokerError> {
        let invocation_id = ToolCallId::generate().map_err(|_| BrokerError::InvalidAuthState)?;
        Ok(Self {
            authenticated: self.authenticated.clone(),
            config: self.config.clone(),
            grants: self.grants.clone(),
            delegation: self.delegation.clone(),
            extension: self.extension.clone(),
            capability: self.capability.clone(),
            schema: self.schema.clone(),
            argument_constraints: self.argument_constraints.clone(),
            effect: self.effect,
            arguments: self.arguments.clone(),
            workspace_id: self.workspace_id,
            project_id: self.project_id,
            invocation_id,
            idempotency_key: IdempotencyKey::parse(&format!("mcp-lifecycle-{invocation_id}"))
                .map_err(|_| BrokerError::InvalidAuthState)?,
            reservation: self.reservation,
            retry_safety: self.retry_safety,
            approval: self.approval,
            cancellation: std::sync::Arc::clone(&self.cancellation),
            attempt: self.attempt,
            driver_claim: self.driver_claim,
            current_fence: std::sync::Arc::clone(&self.current_fence),
            command_id: crate::domain::ids::CommandId::generate()
                .map_err(|_| BrokerError::InvalidAuthState)?,
            intent_event_id: EventId::generate().map_err(|_| BrokerError::InvalidAuthState)?,
            outcome_event_id: EventId::generate().map_err(|_| BrokerError::InvalidAuthState)?,
            occurred_at: self.occurred_at.clone(),
            trace_id: self.trace_id.clone(),
            auth: self.auth.clone(),
        })
    }

    pub(crate) fn invocation(&self) -> BrokerInvocation<'_> {
        self.invocation_inner(false)
    }

    pub(crate) fn shutdown_invocation(&self) -> BrokerInvocation<'_> {
        self.invocation_inner(true)
    }

    fn invocation_inner(&self, lifecycle_shutdown: bool) -> BrokerInvocation<'_> {
        BrokerInvocation {
            envelope: InvocationEnvelope {
                authenticated: &self.authenticated,
                config: &self.config,
                grants: &self.grants,
                delegation: self.delegation.as_ref(),
                extension: self.extension.clone(),
                capability: &self.capability,
                discovered_schema_digest: self.schema.source().normalized_digest(),
                bound_schema_digest: self.schema.source().normalized_digest(),
                effect: self.effect,
                argument_constraints: &self.argument_constraints,
                arguments: &self.arguments,
                workspace_id: self.workspace_id,
                project_id: self.project_id,
                invocation_id: self.invocation_id,
                idempotency_key: &self.idempotency_key,
                reservation: self.reservation,
                retry_safety: self.retry_safety,
                approval: self.approval,
                cancellation: &self.cancellation,
                attempt: self.attempt,
                driver_claim: self.driver_claim,
                current_fence: &self.current_fence,
                command_id: self.command_id,
                intent_event_id: self.intent_event_id,
                outcome_event_id: self.outcome_event_id,
                occurred_at: &self.occurred_at,
                trace_id: &self.trace_id,
            },
            validation_schema: &self.schema,
            binding: None,
            auth: self.auth.clone(),
            transport_initialize: false,
            lifecycle: true,
            lifecycle_shutdown,
        }
    }
}

impl<'a> BrokerInvocation<'a> {
    pub fn native(envelope: InvocationEnvelope<'a>) -> Result<Self, BrokerError> {
        let descriptor = NativeCatalog::by_identity(envelope.capability)
            .ok_or(BrokerError::NativeCapabilityBinding)?;
        Ok(Self::validated(envelope, descriptor.normalized_schema()))
    }

    pub fn generic(envelope: InvocationEnvelope<'a>, schema: &'a NormalizedSchema) -> Self {
        Self::validated(envelope, schema)
    }

    pub fn external(envelope: InvocationEnvelope<'a>, schema: &'a NormalizedSchema) -> Self {
        Self::validated(envelope, schema)
    }

    pub fn bound_external(
        envelope: InvocationEnvelope<'a>,
        context: &InvocationContext<'a>,
    ) -> Result<Self, BrokerError> {
        let binding = context.binding();
        if context.input_bytes() != envelope.arguments {
            return Err(BrokerError::BindingMismatch);
        }
        if binding.pinned_entry().identity() != envelope.capability
            || binding.input_schema_digest() != envelope.bound_schema_digest
            || binding.authorization_snapshot_digest()
                != grant::decide(envelope.grant_request()).snapshot_digest()
            || binding.pinned_entry().side_effects().effect() != envelope.effect
            || binding.pinned_entry().side_effects().retry_safety() != envelope.retry_safety
        {
            return Err(BrokerError::BindingMismatch);
        }
        let authority = binding.pinned_entry().authority();
        let auth = (!authority.auth_scopes().is_empty() || authority.credential().is_some())
            .then(|| {
                if authority.auth_scopes().is_empty() {
                    return BrokerAuthRequirement::new("mcp.connection");
                }
                BrokerAuthRequirement::from_scopes(
                    authority.auth_scopes().iter().map(AsRef::as_ref),
                )
            })
            .transpose()?
            .map(|requirement| match authority.credential() {
                Some(credential) => requirement.with_credential_id(credential.clone()),
                None => requirement,
            });
        Ok(Self {
            validation_schema: binding.pinned_entry().schemas().input().schema(),
            binding: Some(binding),
            envelope,
            auth,
            transport_initialize: false,
            lifecycle: false,
            lifecycle_shutdown: false,
        })
    }

    pub fn nested(
        mut envelope: InvocationEnvelope<'a>,
        schema: &'a NormalizedSchema,
        delegation: &'a grant::DelegationSnapshot,
    ) -> Self {
        envelope.delegation = Some(delegation);
        Self::validated(envelope, schema)
    }

    fn validated(envelope: InvocationEnvelope<'a>, schema: &'a NormalizedSchema) -> Self {
        Self {
            envelope,
            validation_schema: schema,
            binding: None,
            auth: None,
            transport_initialize: false,
            lifecycle: false,
            lifecycle_shutdown: false,
        }
    }

    pub fn with_auth_requirement(mut self, auth: BrokerAuthRequirement) -> Self {
        self.auth = Some(auth);
        self
    }

    pub(crate) fn arguments(&self) -> &[u8] {
        self.envelope.arguments
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.envelope.cancellation.load(Ordering::Acquire)
    }

    pub fn binding_id(&self) -> Option<BindingId> {
        self.binding.map(CapabilityBinding::id)
    }

    pub fn capability_kind(&self) -> Option<CapabilityKind> {
        self.binding.map(|binding| binding.pinned_entry().kind())
    }

    pub fn output_schema(&self) -> Option<&NormalizedSchema> {
        self.binding
            .and_then(|binding| binding.pinned_entry().schemas().output())
            .map(|output| output.schema())
    }

    pub const fn retry_safety(&self) -> kernel_invoke::RetrySafety {
        self.envelope.retry_safety
    }

    fn result_provenance(
        &self,
        remaining_budget: Spend,
        parent_invocation_id: Option<ToolCallId>,
    ) -> Result<CallProvenance, BrokerError> {
        let binding = self.binding.ok_or(BrokerError::BindingMismatch)?;
        let input = CallProvenanceInput {
            invocation_id: self.envelope.invocation_id,
            principal_id: self.envelope.authenticated.principal_id(),
            binding_id: binding.id(),
            capability: self.envelope.capability.clone(),
            schema_digest: self.envelope.bound_schema_digest,
            authorization_snapshot_digest: binding.authorization_snapshot_digest(),
            grant_snapshot_digest: self.envelope.grants.digest(),
            trace_id: self.envelope.trace_id.clone(),
            idempotency_key: self.envelope.idempotency_key.clone(),
            remaining_budget,
        };
        match (self.envelope.delegation, parent_invocation_id) {
            (None, None) => CallProvenance::direct(input),
            (Some(delegation), Some(parent)) => CallProvenance::nested(
                input,
                parent,
                DelegationProvenance::new(
                    delegation.digest(),
                    u16::try_from(delegation.path().len().saturating_sub(1)).map_err(|_| {
                        BrokerError::InvalidResultProvenance(ResultError::InvalidProvenance)
                    })?,
                    delegation.maximum_depth(),
                )
                .map_err(BrokerError::InvalidResultProvenance)?,
            ),
            _ => Err(ResultError::InvalidProvenance),
        }
        .map_err(BrokerError::InvalidResultProvenance)
    }

    pub(crate) const fn binding(&self) -> Option<&CapabilityBinding> {
        self.binding
    }

    pub(crate) fn transport_initialize<'b>(&'b self, arguments: &'b [u8]) -> BrokerInvocation<'b>
    where
        'a: 'b,
    {
        BrokerInvocation {
            envelope: self.envelope.bind_transport_arguments(arguments),
            validation_schema: self.validation_schema,
            binding: None,
            auth: self.auth.clone(),
            transport_initialize: true,
            lifecycle: self.lifecycle,
            lifecycle_shutdown: self.lifecycle_shutdown,
        }
    }

    pub(crate) fn preflight_transport(&self) -> Result<(), BrokerError> {
        if self.transport_initialize {
            if self.envelope.arguments.len() > MAX_INVOCATION_ARGUMENT_BYTES
                || serde_json::from_slice::<serde_json::Value>(self.envelope.arguments).is_err()
            {
                return Err(BrokerError::InvalidArguments);
            }
            Ok(())
        } else {
            preflight(self)
        }
    }

    pub(crate) const fn lifecycle_shutdown(&self) -> bool {
        self.lifecycle_shutdown
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerAuthRequirement {
    scope: String,
    scopes: BTreeSet<String>,
    credential_id: Option<SecretHandle>,
}

impl BrokerAuthRequirement {
    pub fn new(scope: impl Into<String>) -> Result<Self, BrokerError> {
        Self::from_scopes([scope.into()])
    }

    pub fn from_scopes<I, S>(scopes: I) -> Result<Self, BrokerError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let scopes = scopes
            .into_iter()
            .map(|scope| scope.as_ref().to_owned())
            .collect::<BTreeSet<_>>();
        if scopes.is_empty()
            || scopes.len() > crate::capabilities::catalog::MAX_AUTH_SCOPES
            || scopes.iter().any(|scope| {
                scope.is_empty()
                    || scope.len() > MAX_AUTH_SCOPE_BYTES
                    || scope
                        .bytes()
                        .any(|byte| !(byte.is_ascii_graphic() || byte == b' '))
            })
        {
            return Err(BrokerError::InvalidAuthRequirement);
        }
        let scope = scopes.iter().cloned().collect::<Vec<_>>().join(" ");
        if scope.len() > MAX_AUTH_RECORD_BYTES / 2 {
            return Err(BrokerError::InvalidAuthRequirement);
        }
        Ok(Self {
            scope,
            scopes,
            credential_id: None,
        })
    }

    pub fn with_credential_id(mut self, credential_id: SecretHandle) -> Self {
        self.credential_id = Some(credential_id);
        self
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn scopes(&self) -> &BTreeSet<String> {
        &self.scopes
    }

    pub fn contains_scope(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }

    pub fn credential_id(&self) -> Option<&SecretHandle> {
        self.credential_id.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthChallenge {
    pub challenge_id: ApprovalId,
    pub principal_id: String,
    pub project_id: String,
    pub invocation_id: String,
    pub decision_digest: String,
    pub request_digest: String,
    pub capability_source: String,
    pub capability_namespace: String,
    pub capability_name: String,
    pub capability_version: String,
    pub capability_implementation_digest: String,
    pub schema_digest: String,
    pub scope: String,
    pub scopes: Vec<String>,
    pub credential_id: Option<SecretHandle>,
    pub trace_id: String,
    pub kind: AuthChallengeKind,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthChallengeKind {
    Broker,
    Transport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthResolution {
    Granted,
    Denied,
}

pub struct BrokerRuntime<'a> {
    store: &'a mut SqliteStore,
    budget: &'a BudgetLedger,
    backend: &'a mut dyn FnMut(&AuthorizedInvocation) -> DispatchOutcome,
    crash_at: Option<InvocationCrashPoint>,
}

impl<'a> BrokerRuntime<'a> {
    pub fn new(
        store: &'a mut SqliteStore,
        budget: &'a BudgetLedger,
        backend: &'a mut dyn FnMut(&AuthorizedInvocation) -> DispatchOutcome,
    ) -> Self {
        Self {
            store,
            budget,
            backend,
            crash_at: None,
        }
    }

    #[cfg(debug_assertions)]
    pub fn with_crash_at(mut self, crash_at: InvocationCrashPoint) -> Self {
        self.crash_at = Some(crash_at);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerResult {
    pub invocation: InvocationResult,
    pub accounting: UsageEnvelope,
    pub presentation: Option<Presentation>,
}

pub(crate) struct ExternalResultAuthority {
    provenance: CallProvenance,
    metadata: ArtifactMetadata,
}

impl ExternalResultAuthority {
    pub(crate) const fn provenance(&self) -> &CallProvenance {
        &self.provenance
    }

    pub(crate) const fn metadata(&self) -> &ArtifactMetadata {
        &self.metadata
    }
}

pub(crate) struct BrokerAuthorizedInvocation {
    kernel: AuthorizedInvocation,
    result_authority: Option<ExternalResultAuthority>,
}

impl BrokerAuthorizedInvocation {
    pub(crate) const fn kernel(&self) -> &AuthorizedInvocation {
        &self.kernel
    }

    pub(crate) const fn result_authority(&self) -> Option<&ExternalResultAuthority> {
        self.result_authority.as_ref()
    }
}

pub(crate) enum BrokerPrepareOutcome {
    Authorized(Box<BrokerAuthorizedInvocation>),
    Completed(Box<BrokerResult>),
    AuthRequired(Box<AuthChallenge>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum BrokerOutcome {
    Completed(BrokerResult),
    AuthRequired(AuthChallenge),
}

#[derive(Debug)]
pub enum BrokerError {
    NativeCapabilityBinding,
    BindingMismatch,
    InvalidResultProvenance(ResultError),
    SchemaBindingMismatch,
    UnsupportedValidation,
    InvalidArguments,
    InvalidAuthRequirement,
    InvalidTransportOperation,
    AuthCredentialMismatch,
    AuthNotRequired,
    AuthResolutionCancelled,
    TransportAuthCancelled,
    AuthPrincipalMismatch,
    AuthScopeMismatch,
    AuthDenied,
    RepeatedAuthChallenge,
    ReplayNotAuthorized,
    ReplayPermitConsumed,
    TransportAlreadyCompleted,
    TransportOutcomeUnknown,
    InvalidAuthState,
    AuthStore(StoreError),
    Invoke(InvokeError),
    Accounting(AccountingError),
    ToolReservationRequired,
}

impl fmt::Display for BrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NativeCapabilityBinding => {
                formatter.write_str("native capability is not bound to an exact descriptor")
            }
            Self::BindingMismatch => {
                formatter.write_str("external dispatch does not match its immutable binding")
            }
            Self::InvalidResultProvenance(_) => {
                formatter.write_str("external dispatch result provenance is invalid")
            }
            Self::SchemaBindingMismatch => {
                formatter.write_str("validation schema does not match the bound schema digest")
            }
            Self::UnsupportedValidation => {
                formatter.write_str("normalized schema validation is unsupported")
            }
            Self::InvalidArguments => {
                formatter.write_str("arguments do not satisfy the normalized schema")
            }
            Self::InvalidAuthRequirement => formatter.write_str("invalid broker auth requirement"),
            Self::InvalidTransportOperation => {
                formatter.write_str("transport operation must contain 1 to 128 visible ASCII bytes")
            }
            Self::AuthCredentialMismatch => {
                formatter.write_str("broker auth credential does not match the invocation")
            }
            Self::AuthNotRequired => formatter.write_str("broker auth is not required"),
            Self::AuthResolutionCancelled => {
                formatter.write_str("broker auth cannot be resolved for a cancelled invocation")
            }
            Self::TransportAuthCancelled => formatter
                .write_str("broker transport auth cannot proceed for a cancelled invocation"),
            Self::AuthPrincipalMismatch => {
                formatter.write_str("broker auth principal or project does not match")
            }
            Self::AuthScopeMismatch => {
                formatter.write_str("broker transport auth scope does not match")
            }
            Self::AuthDenied => formatter.write_str("broker auth was denied"),
            Self::RepeatedAuthChallenge => {
                formatter.write_str("broker transport auth was challenged again after resolution")
            }
            Self::ReplayNotAuthorized => {
                formatter.write_str("broker transport replay requires a granted auth resolution")
            }
            Self::ReplayPermitConsumed => {
                formatter.write_str("broker transport replay permit was already consumed")
            }
            Self::TransportAlreadyCompleted => {
                formatter.write_str("broker transport operation already completed")
            }
            Self::TransportOutcomeUnknown => {
                formatter.write_str("broker transport operation requires outcome reconciliation")
            }
            Self::InvalidAuthState => formatter.write_str("invalid persisted broker auth state"),
            Self::AuthStore(error) => error.fmt(formatter),
            Self::Invoke(error) => error.fmt(formatter),
            Self::Accounting(error) => error.fmt(formatter),
            Self::ToolReservationRequired => {
                formatter.write_str("broker invocation requires at least one reserved tool")
            }
        }
    }
}

impl std::error::Error for BrokerError {}

pub fn invoke(
    request: BrokerInvocation<'_>,
    runtime: BrokerRuntime<'_>,
) -> Result<BrokerOutcome, BrokerError> {
    if request.binding.is_none() && request.auth.is_none() {
        request
            .envelope
            .preflight_authority(runtime.store, false)
            .map_err(BrokerError::Invoke)?;
        preflight(&request)?;
        if request.envelope.reservation.tools() == 0 {
            return Err(BrokerError::ToolReservationRequired);
        }
        let kernel_runtime = InvocationRuntime::new(runtime.store, runtime.budget, runtime.backend);
        let kernel_runtime = match runtime.crash_at {
            Some(point) => kernel_runtime.with_crash_at(point),
            None => kernel_runtime,
        };
        let invocation =
            kernel_invoke::invoke(request.envelope, kernel_runtime).map_err(BrokerError::Invoke)?;
        return broker_result(invocation).map(BrokerOutcome::Completed);
    }
    let prepared = prepare(&request, runtime.store, runtime.budget, runtime.crash_at)?;
    let authorized = match prepared {
        BrokerPrepareOutcome::Authorized(authorized) => *authorized,
        BrokerPrepareOutcome::Completed(result) => return Ok(BrokerOutcome::Completed(*result)),
        BrokerPrepareOutcome::AuthRequired(challenge) => {
            return Ok(BrokerOutcome::AuthRequired(*challenge));
        }
    };
    let dispatched = (runtime.backend)(authorized.kernel());
    complete(
        &request,
        authorized,
        dispatched,
        runtime.store,
        runtime.budget,
        runtime.crash_at,
    )
    .map(BrokerOutcome::Completed)
}

pub(crate) fn prepare(
    request: &BrokerInvocation<'_>,
    store: &mut SqliteStore,
    budget: &BudgetLedger,
    crash_at: Option<InvocationCrashPoint>,
) -> Result<BrokerPrepareOutcome, BrokerError> {
    prepare_inner(request, store, budget, crash_at, false)
}

pub(crate) fn prepare_resuming_transport(
    request: &BrokerInvocation<'_>,
    store: &mut SqliteStore,
    budget: &BudgetLedger,
    crash_at: Option<InvocationCrashPoint>,
) -> Result<BrokerPrepareOutcome, BrokerError> {
    prepare_inner(request, store, budget, crash_at, true)
}

pub(crate) fn replay(
    request: &BrokerInvocation<'_>,
    store: &mut SqliteStore,
    budget: &BudgetLedger,
    artifacts: &ArtifactStore,
) -> Result<Option<BrokerResult>, BrokerError> {
    request
        .envelope
        .preflight_authority(store, true)
        .map_err(BrokerError::Invoke)?;
    preflight(request)?;
    let replayed = kernel_invoke::replay(
        &request.envelope,
        &mut InvocationPhaseRuntime::new(store, budget, None),
    )
    .map_err(BrokerError::Invoke)?;
    replayed
        .map(|invocation| {
            let presentation = persisted_presentation(
                &invocation,
                artifacts,
                request
                    .envelope
                    .authenticated
                    .principal_id()
                    .to_string()
                    .as_str(),
                &request.envelope.project_id.to_string(),
            )?;
            let mut result = broker_result(invocation)?;
            result.presentation = presentation;
            Ok(result)
        })
        .transpose()
}

fn prepare_inner(
    request: &BrokerInvocation<'_>,
    store: &mut SqliteStore,
    budget: &BudgetLedger,
    crash_at: Option<InvocationCrashPoint>,
    resume_transport: bool,
) -> Result<BrokerPrepareOutcome, BrokerError> {
    request
        .envelope
        .preflight_authority(store, false)
        .map_err(BrokerError::Invoke)?;
    preflight(request)?;
    if request.envelope.reservation.tools() == 0 {
        return Err(BrokerError::ToolReservationRequired);
    }
    if !request.envelope.cancellation.load(Ordering::Acquire)
        && matches!(
            request.envelope.approval,
            crate::capabilities::kernel::invoke::ApprovalState::NotRequired
                | crate::capabilities::kernel::invoke::ApprovalState::Approved
        )
        && let Some(requirement) = &request.auth
    {
        ensure_auth_credential(&request.envelope, requirement)?;
        let decision = grant::decide(request.envelope.grant_request());
        if !decision.is_allowed() {
            return Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(
                decision.reason(),
            )));
        }
        let binding = AuthBinding::new(request, requirement, decision.snapshot_digest())?;
        match auth_state(store, &request.envelope, &binding, &REQUIREMENT_CHANNEL)? {
            AuthState::Missing => {
                append_challenge(store, &request.envelope, &binding, &REQUIREMENT_CHANNEL)?;
                return Ok(BrokerPrepareOutcome::AuthRequired(Box::new(
                    binding.challenge(),
                )));
            }
            AuthState::Waiting => {
                return Ok(BrokerPrepareOutcome::AuthRequired(Box::new(
                    binding.challenge(),
                )));
            }
            AuthState::Denied => return Err(BrokerError::AuthDenied),
            AuthState::Granted => {}
        }
    }

    let mut phase_runtime = InvocationPhaseRuntime::new(store, budget, crash_at);
    let prepared = if resume_transport {
        kernel_invoke::prepare_resuming_dispatch(&request.envelope, &mut phase_runtime)
    } else {
        kernel_invoke::prepare(&request.envelope, &mut phase_runtime)
    }
    .map_err(BrokerError::Invoke)?;
    let authorized = match prepared {
        PrepareOutcome::Completed(invocation) => {
            return broker_result(*invocation)
                .map(Box::new)
                .map(BrokerPrepareOutcome::Completed);
        }
        PrepareOutcome::Authorized(authorized) => *authorized,
    };
    let result_authority = match request
        .binding
        .map(|_| {
            let totals = budget.totals();
            let remaining = budget.budget().remaining(totals.committed, totals.reserved);
            let provenance = request.result_provenance(remaining, None)?;
            let stored_at = request.envelope.occurred_at.unix_micros();
            let retention = i64::from(request.envelope.config.effective().artifact_retention_days)
                .checked_mul(86_400_000_000)
                .and_then(|ttl| stored_at.checked_add(ttl))
                .map(ArtifactRetention::UntilUnixMicros)
                .ok_or(BrokerError::InvalidResultProvenance(
                    ResultError::InvalidArtifact,
                ))?;
            let metadata = ArtifactMetadata::new(
                "application/vnd.kit.mcp-result+json",
                ArtifactClass::Report,
                request.envelope.authenticated.principal_id().to_string(),
                request.envelope.project_id.to_string(),
                retention,
                stored_at,
            )
            .map_err(|_| BrokerError::InvalidResultProvenance(ResultError::InvalidArtifact))?;
            Ok::<_, BrokerError>(ExternalResultAuthority {
                provenance,
                metadata,
            })
        })
        .transpose()
    {
        Ok(authority) => authority,
        Err(_) => {
            let invocation = kernel_invoke::complete(
                &request.envelope,
                &mut InvocationPhaseRuntime::new(store, budget, crash_at),
                authorized,
                DispatchOutcome::Failed {
                    code: "mcp.result_authority_invalid".to_owned(),
                },
            )
            .map_err(BrokerError::Invoke)?;
            return broker_result(invocation)
                .map(Box::new)
                .map(BrokerPrepareOutcome::Completed);
        }
    };
    Ok(BrokerPrepareOutcome::Authorized(Box::new(
        BrokerAuthorizedInvocation {
            kernel: authorized,
            result_authority,
        },
    )))
}

pub(crate) fn complete(
    request: &BrokerInvocation<'_>,
    authorized: BrokerAuthorizedInvocation,
    dispatched: DispatchOutcome,
    store: &mut SqliteStore,
    budget: &BudgetLedger,
    crash_at: Option<InvocationCrashPoint>,
) -> Result<BrokerResult, BrokerError> {
    let invocation = kernel_invoke::complete(
        &request.envelope,
        &mut InvocationPhaseRuntime::new(store, budget, crash_at),
        authorized.kernel,
        dispatched,
    )
    .map_err(BrokerError::Invoke)?;
    broker_result(invocation)
}

pub(crate) fn complete_external(
    request: &BrokerInvocation<'_>,
    authorized: BrokerAuthorizedInvocation,
    external: (DispatchOutcome, Presentation),
    artifacts: &ArtifactStore,
    store: &mut SqliteStore,
    budget: &BudgetLedger,
    crash_at: Option<InvocationCrashPoint>,
) -> Result<BrokerResult, BrokerError> {
    let (dispatched, presentation) = external;
    let artifact = match verify_external_dispatch(&authorized, &dispatched, artifacts) {
        Ok(artifact) => artifact,
        Err(_) => {
            return complete(
                request,
                authorized,
                DispatchOutcome::Failed {
                    code: "mcp.invalid_external_result".to_owned(),
                },
                store,
                budget,
                crash_at,
            );
        }
    };
    let expected_presentation = match presentation_from_dispatch(&dispatched, artifacts) {
        Ok(Some(presentation)) => presentation,
        Ok(None) | Err(_) => {
            return complete(
                request,
                authorized,
                DispatchOutcome::Failed {
                    code: "mcp.presentation_reconstruction_failed".to_owned(),
                },
                store,
                budget,
                crash_at,
            );
        }
    };
    if presentation != expected_presentation {
        return complete(
            request,
            authorized,
            DispatchOutcome::Failed {
                code: "mcp.invalid_external_result".to_owned(),
            },
            store,
            budget,
            crash_at,
        );
    }
    let authorized = RefCell::new(Some(authorized));
    match artifacts.commit_reference(&artifact, |_| {
        complete(
            request,
            authorized
                .borrow_mut()
                .take()
                .expect("artifact reference commits once"),
            dispatched,
            store,
            budget,
            crash_at,
        )
    }) {
        Ok(mut result) => {
            result.presentation = Some(presentation);
            Ok(result)
        }
        Err(crate::store::artifacts::ReferenceError::Artifact(_)) => {
            if let Some(authorized) = authorized.into_inner() {
                return complete(
                    request,
                    authorized,
                    DispatchOutcome::Failed {
                        code: "mcp.artifact_reference_failed".to_owned(),
                    },
                    store,
                    budget,
                    crash_at,
                );
            }
            Err(BrokerError::InvalidResultProvenance(
                ResultError::InvalidArtifact,
            ))
        }
        Err(crate::store::artifacts::ReferenceError::Commit(error)) => Err(error),
    }
}

fn verify_external_dispatch(
    authorized: &BrokerAuthorizedInvocation,
    dispatched: &DispatchOutcome,
    artifacts: &ArtifactStore,
) -> Result<crate::store::artifacts::VerifiedArtifact, BrokerError> {
    let authority = authorized
        .result_authority
        .as_ref()
        .ok_or(BrokerError::BindingMismatch)?;
    let (output, expected_status, expected_code) = match dispatched {
        DispatchOutcome::DurablyCommitted(output) => (
            output,
            crate::capabilities::kernel::invoke::InvocationStatus::Succeeded,
            None,
        ),
        DispatchOutcome::DurablyFailed { code, output } => (
            output,
            crate::capabilities::kernel::invoke::InvocationStatus::Failed,
            Some(code.as_str()),
        ),
        DispatchOutcome::Succeeded(_)
        | DispatchOutcome::Failed { .. }
        | DispatchOutcome::OutcomeUnknown { .. } => {
            return Err(BrokerError::InvalidResultProvenance(
                ResultError::InvalidStatus,
            ));
        }
    };
    if output.media_type != "application/vnd.kit.canonical-result+json" {
        return Err(BrokerError::InvalidResultProvenance(
            ResultError::InvalidJson,
        ));
    }
    let canonical = CanonicalResult::from_canonical_bytes(&output.body)
        .map_err(BrokerError::InvalidResultProvenance)?;
    if canonical.status() != expected_status
        || canonical.error_code() != expected_code
        || canonical.provenance() != &authority.provenance
        || canonical.artifacts().len() != 1
    {
        return Err(BrokerError::InvalidResultProvenance(
            ResultError::InvalidProvenance,
        ));
    }
    let artifact = artifacts
        .open_reference(canonical.artifacts()[0])
        .map_err(|_| BrokerError::InvalidResultProvenance(ResultError::InvalidArtifact))?;
    if output.artifact_digests.as_slice()
        != [
            crate::domain::events::ArtifactRef::parse(&artifact.digest().to_string())
                .map_err(|_| BrokerError::InvalidResultProvenance(ResultError::InvalidArtifact))?,
        ]
    {
        return Err(BrokerError::InvalidResultProvenance(
            ResultError::InvalidArtifact,
        ));
    }
    let manifest = artifact.manifest();
    let expected = &authority.metadata;
    if manifest.media_type != expected.media_type
        || manifest.class != expected.class
        || manifest.principal != expected.principal
        || manifest.project != expected.project
        || manifest.retention != expected.retention
        || manifest.stored_at_unix_micros != expected.stored_at_unix_micros
    {
        return Err(BrokerError::InvalidResultProvenance(
            ResultError::InvalidArtifact,
        ));
    }
    Ok(artifact)
}

fn persisted_presentation(
    invocation: &InvocationResult,
    artifacts: &ArtifactStore,
    principal_id: &str,
    project_id: &str,
) -> Result<Option<Presentation>, BrokerError> {
    let Some(output) = invocation.canonical.output.as_ref() else {
        return Ok(None);
    };
    presentation_from_output_for_owner(output, artifacts, Some((principal_id, project_id)))
}

fn presentation_from_dispatch(
    dispatched: &DispatchOutcome,
    artifacts: &ArtifactStore,
) -> Result<Option<Presentation>, BrokerError> {
    match dispatched {
        DispatchOutcome::DurablyCommitted(output)
        | DispatchOutcome::DurablyFailed { output, .. } => {
            presentation_from_output(output, artifacts)
        }
        _ => Ok(None),
    }
}

fn presentation_from_output(
    output: &kernel_invoke::CanonicalOutput,
    artifacts: &ArtifactStore,
) -> Result<Option<Presentation>, BrokerError> {
    presentation_from_output_for_owner(output, artifacts, None)
}

fn presentation_from_output_for_owner(
    output: &kernel_invoke::CanonicalOutput,
    artifacts: &ArtifactStore,
    owner: Option<(&str, &str)>,
) -> Result<Option<Presentation>, BrokerError> {
    if output.media_type != "application/vnd.kit.canonical-result+json" {
        return Ok(None);
    }
    let canonical = CanonicalResult::from_canonical_bytes(&output.body)
        .map_err(BrokerError::InvalidResultProvenance)?;
    let Some(reference) = canonical.artifacts().first().copied() else {
        return Ok(None);
    };
    let payload = artifacts
        .with_reference_reader(reference, |manifest, reader| {
            if owner.is_some_and(|(principal, project)| {
                manifest.principal != principal || manifest.project != project
            }) {
                return Err(crate::store::artifacts::ArtifactError::AccessDenied);
            }
            let mut bytes = Vec::new();
            reader.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 8 * 1024 * 1024 {
                return Err(crate::store::artifacts::ArtifactError::InvalidManifest(
                    "MCP result payload exceeds presentation replay bound",
                ));
            }
            Ok(bytes)
        })
        .map_err(|_| BrokerError::InvalidResultProvenance(ResultError::InvalidArtifact))?;
    let value: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|_| BrokerError::InvalidResultProvenance(ResultError::InvalidJson))?;
    let presentation = value
        .get("presentation")
        .and_then(serde_json::Value::as_object)
        .ok_or(BrokerError::InvalidResultProvenance(
            ResultError::InvalidPresentation,
        ))?;
    let encoding = presentation
        .get("encoding")
        .and_then(serde_json::Value::as_str)
        .ok_or(BrokerError::InvalidResultProvenance(
            ResultError::InvalidPresentation,
        ))?;
    let spec_version = presentation
        .get("spec_version")
        .and_then(serde_json::Value::as_str)
        .ok_or(BrokerError::InvalidResultProvenance(
            ResultError::InvalidPresentation,
        ))?;
    let body = presentation
        .get("body")
        .and_then(serde_json::Value::as_str)
        .ok_or(BrokerError::InvalidResultProvenance(
            ResultError::InvalidPresentation,
        ))?;
    Presentation::new(&canonical, encoding, spec_version, body)
        .map(Some)
        .map_err(BrokerError::InvalidResultProvenance)
}

fn broker_result(invocation: InvocationResult) -> Result<BrokerResult, BrokerError> {
    let accounting = UsageEnvelope::from_tool_outcome(
        &invocation.canonical,
        &ToolMeasurement::one_call(),
        SpeculationOutcome::None,
        None,
        Some(invocation.reservation),
    )
    .map_err(BrokerError::Accounting)?;
    Ok(BrokerResult {
        invocation,
        accounting,
        presentation: None,
    })
}

pub fn resolve_auth(
    request: &BrokerInvocation<'_>,
    actor: &crate::api::auth::contract::AuthenticatedPrincipal,
    resolution: AuthResolution,
    store: &mut SqliteStore,
) -> Result<(), BrokerError> {
    resolve_auth_inner(request, actor, resolution, None, store)
}

pub(crate) fn resolve_auth_expected(
    request: &BrokerInvocation<'_>,
    actor: &crate::api::auth::contract::AuthenticatedPrincipal,
    resolution: AuthResolution,
    challenge_id: ApprovalId,
    challenge_kind: AuthChallengeKind,
    challenge_generation: u64,
    store: &mut SqliteStore,
) -> Result<(), BrokerError> {
    resolve_auth_inner(
        request,
        actor,
        resolution,
        Some((challenge_id, challenge_kind, challenge_generation)),
        store,
    )
}

fn resolve_auth_inner(
    request: &BrokerInvocation<'_>,
    actor: &crate::api::auth::contract::AuthenticatedPrincipal,
    resolution: AuthResolution,
    expected: Option<(ApprovalId, AuthChallengeKind, u64)>,
    store: &mut SqliteStore,
) -> Result<(), BrokerError> {
    request
        .envelope
        .preflight_authority(store, true)
        .map_err(BrokerError::Invoke)?;
    preflight(request)?;
    if request.envelope.cancellation.load(Ordering::Acquire) {
        return Err(BrokerError::AuthResolutionCancelled);
    }
    let requirement = request.auth.as_ref().ok_or(BrokerError::AuthNotRequired)?;
    ensure_auth_credential(&request.envelope, requirement)?;
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
    let binding = AuthBinding::new(request, requirement, decision.snapshot_digest())?;
    if let Some((challenge_id, challenge_kind, challenge_generation)) = expected
        && (binding.stream != challenge_id
            || challenge_kind != AuthChallengeKind::Broker
            || binding.record.generation != challenge_generation)
    {
        return Err(BrokerError::InvalidAuthState);
    }
    match auth_state(store, &request.envelope, &binding, &REQUIREMENT_CHANNEL)? {
        AuthState::Missing => return Err(BrokerError::InvalidAuthState),
        AuthState::Granted | AuthState::Denied if expected.is_some() => {
            return Err(BrokerError::InvalidAuthState);
        }
        AuthState::Waiting | AuthState::Granted | AuthState::Denied => {}
    }
    append_resolution(
        store,
        &request.envelope,
        &binding,
        resolution,
        &REQUIREMENT_CHANNEL,
    )
}

fn preflight(request: &BrokerInvocation<'_>) -> Result<(), BrokerError> {
    validate(request.validation_schema, &request.envelope)
}

fn ensure_auth_credential(
    envelope: &InvocationEnvelope<'_>,
    requirement: &BrokerAuthRequirement,
) -> Result<(), BrokerError> {
    let Some(required) = requirement.credential_id() else {
        return Ok(());
    };
    let matches_request = envelope.extension.credential() == Some(required)
        || envelope
            .extension
            .egress()
            .is_some_and(|egress| egress.credential() == required);
    if matches_request {
        Ok(())
    } else {
        Err(BrokerError::AuthCredentialMismatch)
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthRecord {
    schema_version: u16,
    kind: String,
    challenge_id: String,
    principal_id: String,
    project_id: String,
    invocation_id: String,
    decision_digest: String,
    request_digest: String,
    capability_source: String,
    capability_namespace: String,
    capability_name: String,
    capability_version: String,
    capability_implementation_digest: String,
    schema_digest: String,
    scope: String,
    scopes: Vec<String>,
    credential_id: Option<String>,
    trace_id: String,
    challenge_kind: String,
    generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transport_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transport_operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transport_binding: Option<transport_auth::TransportBinding>,
    resolution: Option<String>,
}

struct AuthBinding {
    record: AuthRecord,
    stream: ApprovalId,
    digest: CanonicalRequestDigest,
}

impl AuthBinding {
    fn new(
        request: &BrokerInvocation<'_>,
        requirement: &BrokerAuthRequirement,
        decision_digest: Digest,
    ) -> Result<Self, BrokerError> {
        Self::build(request, requirement, decision_digest, "stream", None, None)
    }

    fn build(
        request: &BrokerInvocation<'_>,
        requirement: &BrokerAuthRequirement,
        decision_digest: Digest,
        stream_kind: &str,
        transport: Option<(&str, &str)>,
        transport_binding: Option<&transport_auth::TransportBinding>,
    ) -> Result<Self, BrokerError> {
        let envelope = &request.envelope;
        let identity = auth_identity(envelope, stream_kind);
        let stream = ApprovalId::from_stable_bytes(identity.as_bytes());
        let request_digest = envelope.canonical_request_digest(decision_digest);
        let record = AuthRecord {
            schema_version: AUTH_RECORD_VERSION,
            kind: "challenge".to_owned(),
            challenge_id: stream.to_string(),
            principal_id: envelope.authenticated.principal_id().to_string(),
            project_id: envelope.project_id.to_string(),
            invocation_id: envelope.invocation_id.to_string(),
            decision_digest: decision_digest.to_string(),
            request_digest: digest_hex(request_digest),
            capability_source: envelope.capability.source().as_str().to_owned(),
            capability_namespace: envelope.capability.namespace().as_str().to_owned(),
            capability_name: envelope.capability.name().as_str().to_owned(),
            capability_version: envelope.capability.version().as_str().to_owned(),
            capability_implementation_digest: envelope
                .capability
                .implementation_digest()
                .to_string(),
            schema_digest: envelope.bound_schema_digest.to_string(),
            scope: requirement.scope.clone(),
            scopes: requirement.scopes.iter().cloned().collect(),
            credential_id: requirement
                .credential_id
                .as_ref()
                .map(|credential| credential.identifier().to_owned()),
            trace_id: envelope.trace_id.as_str().to_owned(),
            challenge_kind: if stream_kind == "transport_stream" {
                "transport"
            } else {
                "broker"
            }
            .to_owned(),
            generation: if stream_kind == "transport_stream" {
                2
            } else {
                1
            },
            transport_kind: transport.map(|(kind, _)| kind.to_owned()),
            transport_operation: transport.map(|(_, operation)| operation.to_owned()),
            transport_binding: transport_binding.cloned(),
            resolution: None,
        };
        let bytes = record_bytes(&record)?;
        Ok(Self {
            record,
            stream,
            digest: canonical_digest(&bytes),
        })
    }

    fn challenge(&self) -> AuthChallenge {
        AuthChallenge {
            challenge_id: self.stream,
            principal_id: self.record.principal_id.clone(),
            project_id: self.record.project_id.clone(),
            invocation_id: self.record.invocation_id.clone(),
            decision_digest: self.record.decision_digest.clone(),
            request_digest: self.record.request_digest.clone(),
            capability_source: self.record.capability_source.clone(),
            capability_namespace: self.record.capability_namespace.clone(),
            capability_name: self.record.capability_name.clone(),
            capability_version: self.record.capability_version.clone(),
            capability_implementation_digest: self.record.capability_implementation_digest.clone(),
            schema_digest: self.record.schema_digest.clone(),
            scope: self.record.scope.clone(),
            scopes: self.record.scopes.clone(),
            credential_id: self
                .record
                .credential_id
                .as_deref()
                .map(SecretHandle::parse)
                .transpose()
                .expect("persisted auth credential was validated"),
            trace_id: self.record.trace_id.clone(),
            kind: match self.record.challenge_kind.as_str() {
                "broker" => AuthChallengeKind::Broker,
                "transport" => AuthChallengeKind::Transport,
                _ => unreachable!("persisted challenge kind was constructed locally"),
            },
            generation: self.record.generation,
        }
    }

    fn resolution(
        &self,
        resolution: AuthResolution,
    ) -> Result<(AuthRecord, Vec<u8>, CanonicalRequestDigest), BrokerError> {
        let mut record = self.record.clone();
        record.kind = "resolution".to_owned();
        record.resolution = Some(
            match resolution {
                AuthResolution::Granted => "granted",
                AuthResolution::Denied => "denied",
            }
            .to_owned(),
        );
        let bytes = record_bytes(&record)?;
        let digest = canonical_digest(&bytes);
        Ok((record, bytes, digest))
    }
}

enum AuthState {
    Missing,
    Waiting,
    Granted,
    Denied,
}

fn auth_state(
    store: &mut SqliteStore,
    envelope: &InvocationEnvelope<'_>,
    binding: &AuthBinding,
    channel: &AuthChannel,
) -> Result<AuthState, BrokerError> {
    let key = auth_key(envelope)?;
    let challenge = store
        .idempotency_status(
            &auth_scope(envelope, binding.stream, channel.challenge_command)?,
            &key,
        )
        .map_err(BrokerError::AuthStore)?;
    let resolution = store
        .idempotency_status(
            &auth_scope(envelope, binding.stream, channel.resolution_command)?,
            &key,
        )
        .map_err(BrokerError::AuthStore)?;
    match challenge {
        IdempotencyStatus::Missing => {
            if matches!(resolution, IdempotencyStatus::Missing) {
                Ok(AuthState::Missing)
            } else {
                Err(BrokerError::InvalidAuthState)
            }
        }
        IdempotencyStatus::Pending { .. } => Err(BrokerError::InvalidAuthState),
        IdempotencyStatus::Terminal {
            request_digest,
            result,
        } => {
            if request_digest != binding.digest
                || result.commit_positions.len() != 1
                || checked_record(&result.response)? != binding.record
            {
                return Err(BrokerError::InvalidAuthState);
            }
            match resolution {
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
                    let (expected, _, expected_digest) = binding.resolution(resolution)?;
                    if request_digest != expected_digest || record != expected {
                        return Err(BrokerError::InvalidAuthState);
                    }
                    Ok(match resolution {
                        AuthResolution::Granted => AuthState::Granted,
                        AuthResolution::Denied => AuthState::Denied,
                    })
                }
            }
        }
    }
}

fn append_challenge(
    store: &mut SqliteStore,
    envelope: &InvocationEnvelope<'_>,
    binding: &AuthBinding,
    channel: &AuthChannel,
) -> Result<(), BrokerError> {
    let bytes = record_bytes(&binding.record)?;
    append_auth(
        store,
        envelope,
        binding.stream,
        channel.challenge_command,
        channel.challenge_event,
        0,
        binding.digest,
        bytes,
        false,
    )
    .map(|_| ())
}

fn append_resolution(
    store: &mut SqliteStore,
    envelope: &InvocationEnvelope<'_>,
    binding: &AuthBinding,
    resolution: AuthResolution,
    channel: &AuthChannel,
) -> Result<(), BrokerError> {
    let (_, bytes, digest) = binding.resolution(resolution)?;
    append_auth(
        store,
        envelope,
        binding.stream,
        channel.resolution_command,
        channel.resolution_event,
        1,
        digest,
        bytes,
        true,
    )
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn append_auth(
    store: &mut SqliteStore,
    envelope: &InvocationEnvelope<'_>,
    stream: ApprovalId,
    command: &str,
    event_type: &str,
    version: u64,
    digest: CanonicalRequestDigest,
    payload: Vec<u8>,
    allow_quiescent_driver_claim: bool,
) -> Result<AppendOutcome, BrokerError> {
    let event_id = EventId::from_stable_bytes(
        format!("{}:{stream}", auth_identity(envelope, event_type)).as_bytes(),
    );
    store
        .append(AppendCommand {
            idempotency_scope: auth_scope(envelope, stream, command)?,
            idempotency_key: auth_key(envelope)?,
            request_digest: digest,
            claim: None,
            driver_claim: envelope.driver_claim,
            allow_quiescent_driver_claim,
            expected_versions: vec![ExpectedStreamVersion {
                stream: EntityId::Approval(stream),
                version: ExpectedVersion::new(version),
            }],
            events: vec![NewEvent {
                id: event_id,
                stream: EntityId::Approval(stream),
                event_type: EventType::parse(event_type).expect("broker auth event type is valid"),
                schema_version: SchemaVersion::CURRENT,
                occurred_at: envelope.occurred_at.clone(),
                causation_id: envelope.command_id,
                correlation_id: EntityId::Run(envelope.config.run_id()),
                attempt_id: Some(envelope.attempt.attempt_id),
                trace_id: envelope.trace_id.clone(),
                payload: payload.clone(),
                artifacts: b"[]".to_vec(),
            }],
            response: payload,
        })
        .map_err(BrokerError::AuthStore)
}

fn auth_identity(envelope: &InvocationEnvelope<'_>, kind: &str) -> String {
    format!(
        "broker-auth:{kind}:{}:{}:{}:{}",
        envelope.authenticated.principal_id(),
        envelope.project_id,
        envelope.config.run_id(),
        envelope.invocation_id
    )
}

fn auth_scope(
    envelope: &InvocationEnvelope<'_>,
    stream: ApprovalId,
    command: &str,
) -> Result<IdempotencyScope, BrokerError> {
    IdempotencyScope::new(
        envelope.authenticated.principal_id(),
        command,
        EntityId::Approval(stream),
    )
    .map_err(|_| BrokerError::InvalidAuthState)
}

fn auth_key(envelope: &InvocationEnvelope<'_>) -> Result<IdempotencyKey, BrokerError> {
    IdempotencyKey::parse(&format!("auth-{}", envelope.invocation_id))
        .map_err(|_| BrokerError::InvalidAuthState)
}

fn record_bytes(record: &AuthRecord) -> Result<Vec<u8>, BrokerError> {
    let bytes = serde_json::to_vec(record).map_err(|_| BrokerError::InvalidAuthState)?;
    if bytes.len() > MAX_AUTH_RECORD_BYTES {
        return Err(BrokerError::InvalidAuthRequirement);
    }
    Ok(bytes)
}

fn checked_record(bytes: &[u8]) -> Result<AuthRecord, BrokerError> {
    if bytes.len() > MAX_AUTH_RECORD_BYTES {
        return Err(BrokerError::InvalidAuthState);
    }
    let record: AuthRecord =
        serde_json::from_slice(bytes).map_err(|_| BrokerError::InvalidAuthState)?;
    if record.schema_version != AUTH_RECORD_VERSION {
        return Err(BrokerError::InvalidAuthState);
    }
    Ok(record)
}

fn canonical_digest(bytes: &[u8]) -> CanonicalRequestDigest {
    CanonicalRequestDigest::new(Digest::of(DigestAlgorithm::Sha256, bytes).as_bytes())
}

fn digest_hex(digest: CanonicalRequestDigest) -> String {
    use fmt::Write as _;

    let mut output = String::from("sha256:");
    for byte in digest.as_bytes() {
        write!(output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn validate(
    schema: &NormalizedSchema,
    envelope: &InvocationEnvelope<'_>,
) -> Result<(), BrokerError> {
    if envelope.discovered_schema_digest != envelope.bound_schema_digest {
        return Err(BrokerError::Invoke(InvokeError::SchemaBindingMismatch));
    }
    if envelope.arguments.len() > MAX_INVOCATION_ARGUMENT_BYTES {
        return Err(BrokerError::Invoke(InvokeError::InvalidArguments));
    }
    if schema.source().normalized_digest() != envelope.bound_schema_digest {
        return Err(BrokerError::SchemaBindingMismatch);
    }
    let arguments = serde_json::from_slice(envelope.arguments)
        .map_err(|_| BrokerError::Invoke(InvokeError::InvalidArguments))?;
    let validation = schema.validate(&arguments);
    if validation == SchemaValidation::Unsupported {
        return Err(BrokerError::UnsupportedValidation);
    }
    match validation {
        SchemaValidation::Valid => Ok(()),
        SchemaValidation::Invalid => Err(BrokerError::InvalidArguments),
        SchemaValidation::Unsupported => unreachable!("handled above"),
    }
}
