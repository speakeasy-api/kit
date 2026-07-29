use std::{
    collections::HashSet,
    fmt,
    path::Path,
    time::{Duration, Instant},
};

use url::Url;

use crate::{
    domain::{
        events::ContentDigest,
        ids::{DaemonServiceId, PrincipalId, ProcessId, ProjectId, WorkspaceId},
    },
    verify::lsp::{
        facts::{
            FactLimits, LiveDiagnostic, LspNormalizeError, LspWorkspaceSnapshot, OpenDocument,
            SnapshotFile, extend_bounded_diagnostics, normalize_live_diagnostics,
        },
        session::{
            AcceptedNotification, DiscardReason, DocumentVersion, ExecutionProfileIdentity,
            LspSessionManager, MonotonicClock, NotificationDisposition, OwnedLspLauncher,
            PositionEncoding, RevisionPolicy, ServerIdentity, SessionError, SessionLimits,
            SessionPurpose, SessionScope, TickClock, TransportError,
        },
    },
    workspace::{
        edit::{
            ir::{EditLimits, RootRelativePath},
            stage::{StageError, StageLimit, StagedEdit, StagedOperation},
        },
        revision::RevisionId,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShadowDiagnosticScope {
    Document,
    Workspace,
}

const MAX_SERVER_VERSION_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShadowAdapterCapabilities {
    document_sync: bool,
    publish_diagnostics: bool,
    versioned_diagnostics: bool,
}

impl ShadowAdapterCapabilities {
    pub const fn new(
        document_sync: bool,
        publish_diagnostics: bool,
        versioned_diagnostics: bool,
    ) -> Self {
        Self {
            document_sync,
            publish_diagnostics,
            versioned_diagnostics,
        }
    }

    const fn shadow_document_ready(self) -> bool {
        self.document_sync && self.publish_diagnostics && self.versioned_diagnostics
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShadowAdapterRequest {
    principal_id: PrincipalId,
    project_id: ProjectId,
    workspace_id: WorkspaceId,
    server: ServerIdentity,
    server_version: String,
    position_encoding: PositionEncoding,
    capabilities: ShadowAdapterCapabilities,
    shadow_safe: bool,
    isolation_identity: ContentDigest,
    execution_profile: ExecutionProfileIdentity,
}

impl ShadowAdapterRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        principal_id: PrincipalId,
        project_id: ProjectId,
        workspace_id: WorkspaceId,
        server: ServerIdentity,
        server_version: impl Into<String>,
        position_encoding: PositionEncoding,
        capabilities: ShadowAdapterCapabilities,
        isolation_identity: ContentDigest,
        execution_profile: ExecutionProfileIdentity,
    ) -> Result<Self, ShadowRegistryError> {
        let mut server_version = server_version.into();
        if server_version.is_empty()
            || server_version.len() > MAX_SERVER_VERSION_BYTES
            || server_version.bytes().any(|byte| byte.is_ascii_control())
            || !execution_profile.resources().finite()
        {
            return Err(ShadowRegistryError::InvalidEntry);
        }
        server_version.shrink_to_fit();
        Ok(Self {
            principal_id,
            project_id,
            workspace_id,
            server,
            server_version,
            position_encoding,
            capabilities,
            shadow_safe: false,
            isolation_identity,
            execution_profile,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VerifiedShadowAdapterPin {
    server: ServerIdentity,
    server_version: String,
    position_encoding: PositionEncoding,
    capabilities: ShadowAdapterCapabilities,
    shadow_safe: bool,
    isolation_identity: ContentDigest,
    execution_profile: ExecutionProfileIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShadowRegistryError {
    InvalidEntry,
    DuplicateEntry,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ShadowAdapterDecisionKind {
    Supported(Box<ShadowAdapterRequest>),
    Fallback {
        server: ServerIdentity,
        reason: ShadowFallbackReason,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShadowAdapterDecision(ShadowAdapterDecisionKind);

impl ShadowAdapterDecision {
    fn fallback(&self) -> Option<(ServerIdentity, ShadowFallbackReason)> {
        let ShadowAdapterDecisionKind::Fallback { server, reason } = &self.0 else {
            return None;
        };
        Some((server.clone(), *reason))
    }

    fn server(&self) -> &ServerIdentity {
        match &self.0 {
            ShadowAdapterDecisionKind::Supported(request) => &request.server,
            ShadowAdapterDecisionKind::Fallback { server, .. } => server,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ShadowAdapterRegistry {
    pins: Vec<VerifiedShadowAdapterPin>,
}

impl Default for ShadowAdapterRegistry {
    fn default() -> Self {
        Self::compiled()
    }
}

struct CompiledShadowAdapterPin {
    server_artifact: &'static str,
    configuration: &'static str,
    server_version: &'static str,
    position_encoding: PositionEncoding,
    capabilities: ShadowAdapterCapabilities,
    shadow_safe: bool,
    isolation_identity: &'static str,
    execution_profile: fn() -> ExecutionProfileIdentity,
}

const COMPILED_SHADOW_ADAPTER_PINS: &[CompiledShadowAdapterPin] = &[];

impl ShadowAdapterRegistry {
    pub fn compiled() -> Self {
        Self::from_pins(
            COMPILED_SHADOW_ADAPTER_PINS
                .iter()
                .map(CompiledShadowAdapterPin::verify)
                .collect::<Result<Vec<_>, _>>()
                .expect("compiled shadow adapter pins are valid"),
        )
        .expect("compiled shadow adapter pins are unique")
    }

    pub fn resolve(&self, request: ShadowAdapterRequest) -> ShadowAdapterDecision {
        self.resolve_for_platform(request, cfg!(any(target_os = "linux", target_os = "macos")))
    }

    fn resolve_for_platform(
        &self,
        mut request: ShadowAdapterRequest,
        staged_edit_available: bool,
    ) -> ShadowAdapterDecision {
        if !staged_edit_available {
            return ShadowAdapterDecision(ShadowAdapterDecisionKind::Fallback {
                server: request.server,
                reason: ShadowFallbackReason::PlatformUnavailable,
            });
        }
        let exact = self.pins.iter().find(|pin| pin.matches(&request));
        let reason = match exact {
            Some(_) if !request.capabilities.shadow_document_ready() => {
                Some(ShadowFallbackReason::CapabilityMismatch)
            }
            Some(pin) if !pin.shadow_safe => Some(ShadowFallbackReason::UnsupportedAdapter),
            Some(_) => None,
            None if !self.pins.iter().any(|pin| pin.matches_server(&request)) => {
                Some(ShadowFallbackReason::PinsUnavailable)
            }
            None if !self
                .pins
                .iter()
                .any(|pin| pin.matches_revision_pins(&request)) =>
            {
                Some(ShadowFallbackReason::RevisionPinsMismatch)
            }
            None if !request.capabilities.shadow_document_ready()
                || !self
                    .pins
                    .iter()
                    .any(|pin| pin.matches_capabilities(&request)) =>
            {
                Some(ShadowFallbackReason::CapabilityMismatch)
            }
            None => Some(ShadowFallbackReason::NoIsolatedRoot),
        };
        match reason {
            Some(reason) => ShadowAdapterDecision(ShadowAdapterDecisionKind::Fallback {
                server: request.server,
                reason,
            }),
            None => {
                request.shadow_safe = exact.expect("supported pin was resolved").shadow_safe;
                ShadowAdapterDecision(ShadowAdapterDecisionKind::Supported(Box::new(request)))
            }
        }
    }

    #[cfg(debug_assertions)]
    pub(crate) fn verified_fixture(
        request: &ShadowAdapterRequest,
        shadow_safe: bool,
    ) -> Result<Self, ShadowRegistryError> {
        Self::from_pins(vec![VerifiedShadowAdapterPin::new(
            request.server.clone(),
            request.server_version.clone(),
            request.position_encoding,
            request.capabilities,
            shadow_safe,
            request.isolation_identity.clone(),
            request.execution_profile.clone(),
        )?])
    }

    fn from_pins(pins: Vec<VerifiedShadowAdapterPin>) -> Result<Self, ShadowRegistryError> {
        if pins
            .iter()
            .enumerate()
            .any(|(index, pin)| pins[..index].iter().any(|other| pin.same_key(other)))
        {
            return Err(ShadowRegistryError::DuplicateEntry);
        }
        Ok(Self { pins })
    }
}

impl CompiledShadowAdapterPin {
    fn verify(&self) -> Result<VerifiedShadowAdapterPin, ShadowRegistryError> {
        VerifiedShadowAdapterPin::new(
            ServerIdentity {
                server_artifact: ContentDigest::parse(self.server_artifact)
                    .map_err(|_| ShadowRegistryError::InvalidEntry)?,
                configuration: ContentDigest::parse(self.configuration)
                    .map_err(|_| ShadowRegistryError::InvalidEntry)?,
            },
            self.server_version.to_owned(),
            self.position_encoding,
            self.capabilities,
            self.shadow_safe,
            ContentDigest::parse(self.isolation_identity)
                .map_err(|_| ShadowRegistryError::InvalidEntry)?,
            (self.execution_profile)(),
        )
    }
}

impl VerifiedShadowAdapterPin {
    #[allow(clippy::too_many_arguments)]
    fn new(
        server: ServerIdentity,
        mut server_version: String,
        position_encoding: PositionEncoding,
        capabilities: ShadowAdapterCapabilities,
        shadow_safe: bool,
        isolation_identity: ContentDigest,
        execution_profile: ExecutionProfileIdentity,
    ) -> Result<Self, ShadowRegistryError> {
        if server_version.is_empty()
            || server_version.len() > MAX_SERVER_VERSION_BYTES
            || server_version.bytes().any(|byte| byte.is_ascii_control())
            || !execution_profile.resources().finite()
        {
            return Err(ShadowRegistryError::InvalidEntry);
        }
        server_version.shrink_to_fit();
        Ok(Self {
            server,
            server_version,
            position_encoding,
            capabilities,
            shadow_safe,
            isolation_identity,
            execution_profile,
        })
    }

    fn matches_server(&self, request: &ShadowAdapterRequest) -> bool {
        self.server == request.server
    }

    fn matches_revision_pins(&self, request: &ShadowAdapterRequest) -> bool {
        self.matches_server(request)
            && self.server_version == request.server_version
            && self.position_encoding == request.position_encoding
    }

    fn matches_capabilities(&self, request: &ShadowAdapterRequest) -> bool {
        self.matches_revision_pins(request) && self.capabilities == request.capabilities
    }

    fn matches(&self, request: &ShadowAdapterRequest) -> bool {
        self.matches_capabilities(request)
            && self.isolation_identity == request.isolation_identity
            && self.execution_profile == request.execution_profile
    }

    fn same_key(&self, other: &Self) -> bool {
        self.server == other.server
            && self.server_version == other.server_version
            && self.position_encoding == other.position_encoding
            && self.capabilities == other.capabilities
            && self.isolation_identity == other.isolation_identity
            && self.execution_profile == other.execution_profile
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShadowFallbackReason {
    PinsUnavailable,
    UnsupportedAdapter,
    RevisionPinsMismatch,
    CapabilityMismatch,
    NoIsolatedRoot,
    NoIsolatedWorkspace,
    DeletedEffect,
    MovedEffect,
    NonUtf8StagedFile,
    PlatformUnavailable,
    AffectedFilesLimitExceeded,
    InvalidTrustedRoot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShadowFallbackAction {
    CompilerChecksThenLiveDiagnosticsAfterCommit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShadowFallbackRecord {
    scope: ShadowDiagnosticScope,
    server: ServerIdentity,
    base_revision: RevisionId,
    staged_digest: ContentDigest,
    affected_paths: Vec<String>,
    affected_count: usize,
    reason: ShadowFallbackReason,
    action: ShadowFallbackAction,
}

impl ShadowFallbackRecord {
    pub const fn scope(&self) -> ShadowDiagnosticScope {
        self.scope
    }

    pub const fn reason(&self) -> ShadowFallbackReason {
        self.reason
    }

    pub const fn action(&self) -> ShadowFallbackAction {
        self.action
    }

    pub const fn server(&self) -> &ServerIdentity {
        &self.server
    }

    pub const fn base_revision(&self) -> RevisionId {
        self.base_revision
    }

    pub const fn staged_digest(&self) -> &ContentDigest {
        &self.staged_digest
    }

    pub fn affected_paths(&self) -> &[String] {
        &self.affected_paths
    }

    pub const fn affected_count(&self) -> usize {
        self.affected_count
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ShadowDocument {
    path: RootRelativePath,
    uri: String,
    staged_text: String,
    staged_version: DocumentVersion,
}

#[derive(Debug, Eq, PartialEq)]
struct ShadowRunInput {
    base_revision: RevisionId,
    staged_digest: ContentDigest,
    nonce: [u8; 32],
    documents: Vec<ShadowDocument>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShadowLimits {
    pub max_notifications: usize,
    pub max_notification_bytes: usize,
    pub max_normalized_output_bytes: usize,
    pub cleanup_grace_ticks: u64,
}

impl Default for ShadowLimits {
    fn default() -> Self {
        Self {
            max_notifications: 1_024,
            max_notification_bytes: 16 * 1024 * 1024,
            max_normalized_output_bytes: 32 * 1024 * 1024,
            cleanup_grace_ticks: 5_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShadowDiagnosticReport {
    run_identity: ContentDigest,
    canonical_root_identity: ContentDigest,
    staged_digest: ContentDigest,
    service_id: DaemonServiceId,
    process_id: ProcessId,
    generation: u64,
    accepted_notification_count: usize,
    diagnostics: Vec<LiveDiagnostic>,
}

impl ShadowDiagnosticReport {
    pub const fn run_identity(&self) -> &ContentDigest {
        &self.run_identity
    }

    pub const fn canonical_root_identity(&self) -> &ContentDigest {
        &self.canonical_root_identity
    }

    pub const fn staged_digest(&self) -> &ContentDigest {
        &self.staged_digest
    }

    pub const fn service_id(&self) -> DaemonServiceId {
        self.service_id
    }

    pub const fn process_id(&self) -> ProcessId {
        self.process_id
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn accepted_notification_count(&self) -> usize {
        self.accepted_notification_count
    }

    pub fn diagnostics(&self) -> &[LiveDiagnostic] {
        &self.diagnostics
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ShadowOutcome {
    Completed(ShadowDiagnosticReport),
    Fallback(ShadowFallbackRecord),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShadowError {
    InvalidLimits,
    InvalidInput,
    NonceGenerationFailed,
    DeadlineExceeded,
    SourceUnavailable,
    NotificationLimitExceeded,
    NotificationInputLimitExceeded,
    OutputLimitExceeded,
    Session(SessionError),
    Protocol(SessionError),
    CleanupFailed(SessionError),
    StageReadFailed,
    Normalize(LspNormalizeError),
}

impl fmt::Display for ShadowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "shadow LSP error: {self:?}")
    }
}

impl std::error::Error for ShadowError {}

pub struct ShadowLspRunner<L, C = MonotonicClock>
where
    L: OwnedLspLauncher,
    C: TickClock,
{
    manager: LspSessionManager<L, C>,
    session_limits: SessionLimits,
    edit_limits: EditLimits,
    fact_limits: FactLimits,
    limits: ShadowLimits,
}

impl<L> ShadowLspRunner<L, MonotonicClock>
where
    L: OwnedLspLauncher,
{
    pub fn new(
        launcher: L,
        session_limits: SessionLimits,
        edit_limits: EditLimits,
        fact_limits: FactLimits,
        limits: ShadowLimits,
    ) -> Result<Self, ShadowError> {
        Self::with_clock(
            launcher,
            session_limits,
            edit_limits,
            fact_limits,
            limits,
            MonotonicClock::default(),
        )
    }
}

impl<L, C> ShadowLspRunner<L, C>
where
    L: OwnedLspLauncher,
    C: TickClock,
{
    pub fn with_clock(
        launcher: L,
        session_limits: SessionLimits,
        edit_limits: EditLimits,
        fact_limits: FactLimits,
        limits: ShadowLimits,
        clock: C,
    ) -> Result<Self, ShadowError> {
        if !valid_limits(session_limits, edit_limits, fact_limits, limits) {
            return Err(ShadowError::InvalidLimits);
        }
        Ok(Self {
            manager: LspSessionManager::with_clock(launcher, session_limits, clock)
                .map_err(ShadowError::Session)?,
            session_limits,
            edit_limits,
            fact_limits,
            limits,
        })
    }

    pub fn deadline_after(&self, duration: Duration) -> Result<u64, ShadowError> {
        let ticks = u64::try_from(duration.as_millis())
            .ok()
            .filter(|ticks| *ticks > 0)
            .ok_or(ShadowError::DeadlineExceeded)?;
        self.manager
            .now_tick()
            .checked_add(ticks)
            .filter(|deadline| *deadline < u64::MAX)
            .ok_or(ShadowError::DeadlineExceeded)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_staged(
        &mut self,
        staged: &StagedEdit<'_>,
        trusted_canonical_root: &Path,
        trusted_root_url: &Url,
        scope: ShadowDiagnosticScope,
        decision: ShadowAdapterDecision,
        deadline_tick: u64,
    ) -> Result<ShadowOutcome, ShadowError> {
        if let Some((server, reason)) = decision.fallback() {
            return self.fallback_for(staged, scope, reason, server);
        }
        self.require_time(deadline_tick)?;
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce).map_err(|_| ShadowError::NonceGenerationFailed)?;
        let read_deadline = self.read_deadline(deadline_tick)?;
        self.run_staged_inner(
            staged,
            trusted_canonical_root,
            trusted_root_url,
            scope,
            decision,
            nonce,
            deadline_tick,
            read_deadline,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_staged_inner(
        &mut self,
        staged: &StagedEdit<'_>,
        trusted_canonical_root: &Path,
        trusted_root_url: &Url,
        scope: ShadowDiagnosticScope,
        decision: ShadowAdapterDecision,
        nonce: [u8; 32],
        deadline_tick: u64,
        read_deadline: Instant,
    ) -> Result<ShadowOutcome, ShadowError> {
        self.require_time(deadline_tick)?;
        let staged_digest =
            ContentDigest::parse(staged.state_digest()).map_err(|_| ShadowError::InvalidInput)?;
        let base_revision = staged.revision();
        let affected_count = staged.changes().len();
        let affected_paths = bounded_affected_paths(staged, self.edit_limits, self.session_limits);
        let fallback = |reason, server: ServerIdentity| {
            fallback(
                scope,
                reason,
                server,
                base_revision,
                staged_digest.clone(),
                affected_paths.clone(),
                affected_count,
            )
        };

        if scope == ShadowDiagnosticScope::Workspace {
            return Ok(fallback(
                ShadowFallbackReason::NoIsolatedWorkspace,
                decision.server().clone(),
            ));
        }
        if staged
            .operations()
            .iter()
            .any(|operation| matches!(operation, StagedOperation::Move { .. }))
        {
            return Ok(fallback(
                ShadowFallbackReason::MovedEffect,
                decision.server().clone(),
            ));
        }
        if staged
            .operations()
            .iter()
            .any(|operation| matches!(operation, StagedOperation::Delete(_)))
        {
            return Ok(fallback(
                ShadowFallbackReason::DeletedEffect,
                decision.server().clone(),
            ));
        }
        if staged.changes().is_empty()
            || staged.changes().len() > self.session_limits.max_documents_per_session
            || staged
                .changes()
                .iter()
                .any(|change| change.path().as_str().len() > self.edit_limits.max_path_bytes)
        {
            return Ok(fallback(
                ShadowFallbackReason::AffectedFilesLimitExceeded,
                decision.server().clone(),
            ));
        }
        if !valid_trusted_root(trusted_canonical_root, trusted_root_url) {
            return Ok(fallback(
                ShadowFallbackReason::InvalidTrustedRoot,
                decision.server().clone(),
            ));
        }

        self.require_time(deadline_tick)?;
        let mut documents = Vec::with_capacity(staged.changes().len());
        let mut files = Vec::with_capacity(staged.changes().len());
        let mut open_documents = Vec::with_capacity(staged.changes().len());
        let retained_limit = self
            .session_limits
            .max_total_document_bytes
            .min(self.fact_limits.max_workspace_bytes)
            .min(self.fact_limits.max_open_document_bytes);
        let mut retained_bytes = 0_usize;
        for change in staged.changes() {
            self.require_time(deadline_tick)?;
            if change.after_hash().is_none() {
                return Ok(fallback(
                    ShadowFallbackReason::DeletedEffect,
                    decision.server().clone(),
                ));
            }
            let uri = Url::from_file_path(trusted_canonical_root.join(change.path().as_str()))
                .map_err(|()| ShadowError::InvalidInput)?
                .to_string();
            if uri.len() > self.session_limits.max_uri_bytes {
                return Ok(fallback(
                    ShadowFallbackReason::AffectedFilesLimitExceeded,
                    decision.server().clone(),
                ));
            }
            let remaining = retained_limit
                .checked_sub(retained_bytes)
                .ok_or(ShadowError::InvalidInput)?;
            let read_limit = max_staged_bytes_for_retained_budget(
                remaining,
                change.path().as_str().len(),
                uri.len(),
            )
            .min(self.session_limits.max_document_bytes)
            .min(self.fact_limits.max_document_bytes);
            if read_limit == 0 {
                return Ok(fallback(
                    ShadowFallbackReason::AffectedFilesLimitExceeded,
                    decision.server().clone(),
                ));
            }
            let bytes = staged
                .read_file_before(change.path(), read_limit, read_deadline)
                .map_err(map_stage_read_error)?;
            self.require_time(deadline_tick)?;
            let retained = staged_text_retained_bytes(
                bytes.capacity(),
                change.path().as_str().len(),
                uri.len(),
            )?;
            retained_bytes = retained_bytes
                .checked_add(retained)
                .filter(|total| *total <= retained_limit)
                .ok_or(ShadowError::InvalidInput)?;
            let Ok(text) = String::from_utf8(bytes) else {
                return Ok(fallback(
                    ShadowFallbackReason::NonUtf8StagedFile,
                    decision.server().clone(),
                ));
            };
            files.push(SnapshotFile::new(
                change.path().as_str(),
                text.as_bytes().to_vec(),
                change.after_mode().is_some_and(|mode| mode & 0o111 != 0),
            ));
            open_documents.push(OpenDocument::new(
                uri.clone(),
                DocumentVersion::new(1),
                text.clone(),
            ));
            documents.push(ShadowDocument {
                path: change.path().clone(),
                uri,
                staged_text: text,
                staged_version: DocumentVersion::new(1),
            });
        }

        let input = ShadowRunInput {
            base_revision,
            staged_digest,
            nonce,
            documents,
        };
        let ShadowAdapterDecisionKind::Supported(attestation) = decision.0 else {
            unreachable!("unsupported dispositions returned before staged reads")
        };
        self.require_time(deadline_tick)?;
        let canonical_root_identity = root_identity(trusted_canonical_root, trusted_root_url);
        self.require_time(deadline_tick)?;
        self.run(
            input,
            *attestation,
            canonical_root_identity,
            trusted_canonical_root,
            files,
            open_documents,
            deadline_tick,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        input: ShadowRunInput,
        attestation: ShadowAdapterRequest,
        canonical_root_identity: ContentDigest,
        trusted_canonical_root: &Path,
        files: Vec<SnapshotFile>,
        open_documents: Vec<OpenDocument>,
        deadline_tick: u64,
    ) -> Result<ShadowOutcome, ShadowError> {
        self.require_time(deadline_tick)?;
        let run_identity = run_identity(&input, &attestation);
        self.require_time(deadline_tick)?;
        let session_scope = SessionScope {
            principal_id: attestation.principal_id,
            project_id: attestation.project_id,
            workspace_id: attestation.workspace_id,
            canonical_root_identity: canonical_root_identity.clone(),
            purpose: SessionPurpose::Shadow(run_identity.clone()),
            revision_policy: RevisionPolicy::Pinned(input.base_revision),
            server: attestation.server.clone(),
            position_encoding: attestation.position_encoding,
            execution_profile: attestation.execution_profile.clone(),
        };
        let service_id = self
            .manager
            .open_until(session_scope, input.base_revision, deadline_tick)
            .map_err(map_session_error)?;
        for document in &input.documents {
            if let Err(error) = self.require_time(deadline_tick) {
                return Err(self.cleanup_error(service_id, error, deadline_tick));
            }
            if let Err(error) = self.manager.open_document_until(
                service_id,
                document.uri.clone(),
                document.staged_version,
                document.staged_text.clone(),
                deadline_tick,
            ) {
                return Err(self.cleanup_error(
                    service_id,
                    map_session_error(error),
                    deadline_tick,
                ));
            }
            if let Err(error) = self.require_time(deadline_tick) {
                return Err(self.cleanup_error(service_id, error, deadline_tick));
            }
        }
        if let Err(error) = self.require_time(deadline_tick) {
            return Err(self.cleanup_error(service_id, error, deadline_tick));
        }
        let snapshot = match self.manager.snapshot(service_id) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(self.cleanup_error(
                    service_id,
                    ShadowError::Session(error),
                    deadline_tick,
                ));
            }
        };
        if let Err(error) = self.require_time(deadline_tick) {
            return Err(self.cleanup_error(service_id, error, deadline_tick));
        }
        let Some(process_id) = snapshot.process_id else {
            return Err(self.cleanup_error(
                service_id,
                ShadowError::Session(SessionError::AdmissionClosed),
                deadline_tick,
            ));
        };
        let generation = snapshot.generation;
        let document_epoch = snapshot.document_epoch;
        let mut accepted = Vec::<AcceptedNotification>::with_capacity(input.documents.len());
        let mut accepted_uris = HashSet::with_capacity(input.documents.len());
        let mut notification_bytes = 0_usize;

        for _ in 0..self.limits.max_notifications {
            if accepted_uris.len() == input.documents.len() {
                break;
            }
            let received = match self
                .manager
                .receive_current_notification(service_id, deadline_tick)
            {
                Ok(received) => received,
                Err(error) => {
                    return Err(self.cleanup_error(
                        service_id,
                        map_receive_error(error),
                        deadline_tick,
                    ));
                }
            };
            notification_bytes = match notification_bytes.checked_add(received.frame_bytes()) {
                Some(total) if total <= self.limits.max_notification_bytes => total,
                _ => {
                    return Err(self.cleanup_error(
                        service_id,
                        ShadowError::NotificationInputLimitExceeded,
                        deadline_tick,
                    ));
                }
            };
            if received.service_id() != service_id
                || received.process_id() != process_id
                || received.generation() != generation
            {
                return Err(self.cleanup_error(
                    service_id,
                    ShadowError::Protocol(SessionError::InvalidNotification),
                    deadline_tick,
                ));
            }
            match received.into_disposition() {
                NotificationDisposition::Accepted(notification) => {
                    if accepted_uris.insert(notification.uri().to_owned()) {
                        accepted.push(notification);
                    }
                }
                NotificationDisposition::Discarded(DiscardReason::StaleDocumentVersion) => {}
                NotificationDisposition::Discarded(_) => {
                    return Err(self.cleanup_error(
                        service_id,
                        ShadowError::Protocol(SessionError::InvalidNotification),
                        deadline_tick,
                    ));
                }
            }
        }
        if accepted.is_empty() || accepted_uris.len() != input.documents.len() {
            return Err(self.cleanup_error(
                service_id,
                ShadowError::NotificationLimitExceeded,
                deadline_tick,
            ));
        }

        if let Err(error) = self.require_time(deadline_tick) {
            return Err(self.cleanup_error(service_id, error, deadline_tick));
        }
        let workspace = match LspWorkspaceSnapshot::new(
            trusted_canonical_root.to_path_buf(),
            input.base_revision,
            document_epoch,
            files,
            open_documents,
            attestation.server,
            attestation.position_encoding,
            self.edit_limits,
            self.fact_limits,
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(self.cleanup_error(
                    service_id,
                    ShadowError::Normalize(error),
                    deadline_tick,
                ));
            }
        };
        if let Err(error) = self.require_time(deadline_tick) {
            return Err(self.cleanup_error(service_id, error, deadline_tick));
        }
        let mut diagnostics = Vec::new();
        let mut output_bytes = 0_usize;
        for notification in &accepted {
            if let Err(error) = self.require_time(deadline_tick) {
                return Err(self.cleanup_error(service_id, error, deadline_tick));
            }
            let normalized = match normalize_live_diagnostics(&workspace, notification) {
                Ok(diagnostics) => diagnostics,
                Err(error) => {
                    return Err(self.cleanup_error(
                        service_id,
                        ShadowError::Normalize(error),
                        deadline_tick,
                    ));
                }
            };
            if let Err(error) = self.require_time(deadline_tick) {
                return Err(self.cleanup_error(service_id, error, deadline_tick));
            }
            if extend_bounded_diagnostics(
                &mut diagnostics,
                &mut output_bytes,
                normalized,
                self.fact_limits.max_diagnostics,
                self.limits.max_normalized_output_bytes,
            )
            .is_err()
            {
                return Err(self.cleanup_error(
                    service_id,
                    ShadowError::OutputLimitExceeded,
                    deadline_tick,
                ));
            }
        }
        let accepted_notification_count = accepted.len();
        drop(accepted);
        if let Err(error) = self.require_time(deadline_tick) {
            return Err(self.cleanup_error(service_id, error, deadline_tick));
        }
        self.cleanup_until(service_id).map_err(map_cleanup_error)?;
        self.require_time(deadline_tick)?;
        Ok(ShadowOutcome::Completed(ShadowDiagnosticReport {
            run_identity,
            canonical_root_identity,
            staged_digest: input.staged_digest,
            service_id,
            process_id,
            generation,
            accepted_notification_count,
            diagnostics,
        }))
    }

    fn require_time(&self, deadline_tick: u64) -> Result<(), ShadowError> {
        if deadline_tick <= self.manager.now_tick() {
            Err(ShadowError::DeadlineExceeded)
        } else {
            Ok(())
        }
    }

    fn read_deadline(&self, deadline_tick: u64) -> Result<Instant, ShadowError> {
        self.require_time(deadline_tick)?;
        let remaining = self
            .manager
            .remaining_until(deadline_tick)
            .min(Duration::from_secs(24 * 60 * 60));
        Instant::now()
            .checked_add(remaining)
            .ok_or(ShadowError::DeadlineExceeded)
    }

    fn fallback_for(
        &self,
        staged: &StagedEdit<'_>,
        scope: ShadowDiagnosticScope,
        reason: ShadowFallbackReason,
        server: ServerIdentity,
    ) -> Result<ShadowOutcome, ShadowError> {
        let staged_digest =
            ContentDigest::parse(staged.state_digest()).map_err(|_| ShadowError::InvalidInput)?;
        Ok(fallback(
            scope,
            reason,
            server,
            staged.revision(),
            staged_digest,
            bounded_affected_paths(staged, self.edit_limits, self.session_limits),
            staged.changes().len(),
        ))
    }

    fn cleanup_error(
        &mut self,
        service_id: DaemonServiceId,
        error: ShadowError,
        _deadline_tick: u64,
    ) -> ShadowError {
        self.cleanup_until(service_id)
            .err()
            .map_or(error, map_cleanup_error)
    }

    fn cleanup_until(&mut self, service_id: DaemonServiceId) -> Result<(), SessionError> {
        let deadline_tick = self
            .manager
            .now_tick()
            .checked_add(self.limits.cleanup_grace_ticks)
            .ok_or(SessionError::DeadlineExceeded)?;
        self.manager.close_session_until(service_id, deadline_tick)
    }
}

fn valid_limits(
    session: SessionLimits,
    edits: EditLimits,
    facts: FactLimits,
    shadow: ShadowLimits,
) -> bool {
    session.valid()
        && facts.valid()
        && edits.max_operations > 0
        && edits.max_path_bytes > 0
        && edits.max_content_bytes > 0
        && edits.max_input_bytes > 0
        && shadow.max_notifications > 0
        && shadow.max_notification_bytes >= session.codec.max_frame_bytes
        && shadow.max_normalized_output_bytes >= facts.max_retained_output_bytes
        && shadow.cleanup_grace_ticks > 0
        && shadow.cleanup_grace_ticks < u64::MAX
}

fn map_session_error(error: SessionError) -> ShadowError {
    match error {
        SessionError::DeadlineExceeded
        | SessionError::Transport(TransportError::WriteDeadlineExceeded) => {
            ShadowError::DeadlineExceeded
        }
        SessionError::Transport(
            error @ (TransportError::CloseOrReapFailed
            | TransportError::CloseOrReapDeadlineExceeded),
        ) => ShadowError::CleanupFailed(SessionError::Transport(error)),
        error => ShadowError::Session(error),
    }
}

fn map_cleanup_error(error: SessionError) -> ShadowError {
    ShadowError::CleanupFailed(error)
}

fn map_stage_read_error(error: StageError) -> ShadowError {
    match error {
        StageError::LimitExceeded(StageLimit::Time) => ShadowError::DeadlineExceeded,
        _ => ShadowError::StageReadFailed,
    }
}

// Five retained text payloads plus one worst-case usize line index consume at most 13 bytes per
// input byte. Sixteen leaves room for allocation rounding; the fixed charge covers maps/structs.
fn staged_text_retained_bytes(
    text_bytes: usize,
    path_bytes: usize,
    uri_bytes: usize,
) -> Result<usize, ShadowError> {
    text_bytes
        .checked_mul(16)
        .and_then(|value| value.checked_add(path_bytes.checked_mul(16)?))
        .and_then(|value| value.checked_add(uri_bytes.checked_mul(16)?))
        .and_then(|value| value.checked_add(1_024))
        .ok_or(ShadowError::InvalidInput)
}

fn max_staged_bytes_for_retained_budget(
    budget: usize,
    path_bytes: usize,
    uri_bytes: usize,
) -> usize {
    staged_text_retained_bytes(0, path_bytes, uri_bytes)
        .ok()
        .and_then(|fixed| budget.checked_sub(fixed))
        .map_or(0, |remaining| remaining / 16)
}

fn map_receive_error(error: SessionError) -> ShadowError {
    match error {
        SessionError::DeadlineExceeded
        | SessionError::Transport(TransportError::ReadDeadlineExceeded) => {
            ShadowError::DeadlineExceeded
        }
        SessionError::Transport(TransportError::ReadFailed) => ShadowError::SourceUnavailable,
        SessionError::Transport(
            error @ (TransportError::CloseOrReapFailed
            | TransportError::CloseOrReapDeadlineExceeded),
        ) => ShadowError::CleanupFailed(SessionError::Transport(error)),
        error => ShadowError::Protocol(error),
    }
}

fn bounded_affected_paths(
    staged: &StagedEdit<'_>,
    edit_limits: EditLimits,
    session_limits: SessionLimits,
) -> Vec<String> {
    staged
        .changes()
        .iter()
        .filter(|change| change.path().as_str().len() <= edit_limits.max_path_bytes)
        .take(session_limits.max_documents_per_session)
        .map(|change| change.path().as_str().to_owned())
        .collect()
}

fn valid_trusted_root(root: &Path, root_url: &Url) -> bool {
    root.is_absolute()
        && root.is_dir()
        && root.canonicalize().is_ok_and(|canonical| canonical == root)
        && Url::from_directory_path(root).is_ok_and(|expected| expected == *root_url)
}

#[allow(clippy::too_many_arguments)]
fn fallback(
    scope: ShadowDiagnosticScope,
    reason: ShadowFallbackReason,
    server: ServerIdentity,
    base_revision: RevisionId,
    staged_digest: ContentDigest,
    affected_paths: Vec<String>,
    affected_count: usize,
) -> ShadowOutcome {
    ShadowOutcome::Fallback(ShadowFallbackRecord {
        scope,
        server,
        base_revision,
        staged_digest,
        affected_paths,
        affected_count,
        reason,
        action: ShadowFallbackAction::CompilerChecksThenLiveDiagnosticsAfterCommit,
    })
}

fn root_identity(root: &Path, root_url: &Url) -> ContentDigest {
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, b"kit-shadow-lsp-root-v1");
    hash_field(&mut hasher, root.as_os_str().as_encoded_bytes());
    hash_field(&mut hasher, root_url.as_str().as_bytes());
    digest_from_hasher(hasher)
}

fn run_identity(input: &ShadowRunInput, attestation: &ShadowAdapterRequest) -> ContentDigest {
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, b"kit-shadow-lsp-run-v2");
    hash_field(&mut hasher, b"document-diagnostics");
    hash_field(&mut hasher, input.base_revision.to_string().as_bytes());
    hash_field(&mut hasher, input.staged_digest.as_str().as_bytes());
    hash_field(
        &mut hasher,
        attestation.server.server_artifact.as_str().as_bytes(),
    );
    hash_field(
        &mut hasher,
        attestation.server.configuration.as_str().as_bytes(),
    );
    hash_field(&mut hasher, attestation.server_version.as_bytes());
    hash_field(
        &mut hasher,
        match attestation.position_encoding {
            PositionEncoding::Utf8 => b"utf-8",
            PositionEncoding::Utf16 => b"utf-16",
            PositionEncoding::Utf32 => b"utf-32",
        },
    );
    hash_field(
        &mut hasher,
        &attestation.execution_profile.digest().as_bytes(),
    );
    hash_field(
        &mut hasher,
        attestation.isolation_identity.as_str().as_bytes(),
    );
    hash_field(
        &mut hasher,
        &[
            u8::from(attestation.capabilities.document_sync),
            u8::from(attestation.capabilities.publish_diagnostics),
            u8::from(attestation.capabilities.versioned_diagnostics),
            u8::from(attestation.shadow_safe),
        ],
    );
    let resources = attestation.execution_profile.resources();
    for value in [
        resources.cpu_millis,
        resources.memory_bytes,
        u64::from(resources.pids),
        resources.file_bytes,
        resources.disk_bytes,
        resources.io_bytes,
        resources.output_bytes,
        resources.wall_time_millis,
    ] {
        hash_field(&mut hasher, &value.to_le_bytes());
    }
    for document in &input.documents {
        hash_field(&mut hasher, document.path.as_str().as_bytes());
        hash_field(&mut hasher, document.uri.as_bytes());
        hash_field(&mut hasher, &document.staged_version.get().to_le_bytes());
        hash_field(
            &mut hasher,
            blake3::hash(document.staged_text.as_bytes()).as_bytes(),
        );
    }
    hash_field(&mut hasher, &input.nonce);
    digest_from_hasher(hasher)
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(
        &u64::try_from(bytes.len())
            .expect("usize fits in the canonical u64 length field")
            .to_le_bytes(),
    );
    hasher.update(bytes);
}

fn digest_from_hasher(hasher: blake3::Hasher) -> ContentDigest {
    ContentDigest::parse(&format!("blake3:{}", hasher.finalize().to_hex()))
        .expect("BLAKE3 produces a valid content digest")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use serde_json::json;

    use crate::{
        domain::{
            ids::ProcessId,
            lifecycle::{ProcessClaim, ProcessOwnership},
        },
        executor::profile::{
            Architecture, ExecutorProfile, Platform, ProfileSpec, ResourceLimits, TrustTier,
        },
        verify::lsp::session::{CodecLimits, LaunchRequest, OwnedLspTransport, SendContext},
    };

    use super::*;

    #[derive(Default)]
    struct TransportState {
        fail_reap: AtomicBool,
        reaped: AtomicUsize,
    }

    #[derive(Clone, Default)]
    struct Launcher {
        frames: Arc<Mutex<VecDeque<Vec<u8>>>>,
        state: Arc<TransportState>,
    }

    impl Launcher {
        fn with_frames(frames: VecDeque<Vec<u8>>) -> Self {
            Self {
                frames: Arc::new(Mutex::new(frames)),
                state: Arc::default(),
            }
        }
    }

    struct Transport {
        claim: ProcessClaim,
        frames: VecDeque<Vec<u8>>,
        state: Arc<TransportState>,
    }

    impl OwnedLspLauncher for Launcher {
        type Transport = Transport;

        fn launch(
            &mut self,
            request: LaunchRequest<'_>,
        ) -> Result<Self::Transport, TransportError> {
            Ok(Transport {
                claim: ProcessClaim::new(
                    ProcessId::generate().map_err(|_| TransportError::LaunchFailed)?,
                    ProcessOwnership::DaemonService(request.service.id),
                ),
                frames: std::mem::take(&mut *self.frames.lock().unwrap()),
                state: self.state.clone(),
            })
        }
    }

    impl OwnedLspTransport for Transport {
        fn claim(&self) -> ProcessClaim {
            self.claim
        }

        fn initialize(
            &mut self,
            _: &[u8],
            _: CodecLimits,
            context: SendContext,
        ) -> Result<(), TransportError> {
            if context.remaining().is_zero() {
                Err(TransportError::WriteDeadlineExceeded)
            } else {
                Ok(())
            }
        }

        fn send_frame(&mut self, _: &[u8], context: SendContext) -> Result<(), TransportError> {
            if context.remaining().is_zero() {
                Err(TransportError::WriteDeadlineExceeded)
            } else {
                Ok(())
            }
        }

        fn receive_frame(
            &mut self,
            limits: CodecLimits,
            context: SendContext,
        ) -> Result<Vec<u8>, TransportError> {
            if context.remaining().is_zero() {
                return Err(TransportError::ReadDeadlineExceeded);
            }
            self.frames
                .pop_front()
                .filter(|frame| frame.len() <= limits.max_frame_bytes)
                .ok_or(TransportError::ReadFailed)
        }

        fn close_and_reap(&mut self, context: SendContext) -> Result<(), TransportError> {
            if context.remaining().is_zero() {
                Err(TransportError::CloseOrReapDeadlineExceeded)
            } else if self.state.fail_reap.swap(false, Ordering::SeqCst) {
                Err(TransportError::CloseOrReapFailed)
            } else {
                self.state.reaped.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct Clock;

    impl TickClock for Clock {
        fn now_tick(&self) -> u64 {
            0
        }

        fn remaining_until(&self, deadline_tick: u64) -> Duration {
            Duration::from_millis(deadline_tick)
        }
    }

    #[derive(Clone)]
    struct ExpiringClock {
        checks: Arc<AtomicUsize>,
        expire_on: usize,
    }

    impl ExpiringClock {
        fn new(expire_on: usize) -> Self {
            Self {
                checks: Arc::new(AtomicUsize::new(0)),
                expire_on,
            }
        }

        fn reset(&self) {
            self.checks.store(0, Ordering::SeqCst);
        }

        fn tick(&self) -> u64 {
            if self.checks.load(Ordering::SeqCst) >= self.expire_on {
                100
            } else {
                0
            }
        }
    }

    impl TickClock for ExpiringClock {
        fn now_tick(&self) -> u64 {
            self.checks.fetch_add(1, Ordering::SeqCst);
            self.tick()
        }

        fn remaining_until(&self, deadline_tick: u64) -> Duration {
            Duration::from_millis(deadline_tick.saturating_sub(self.tick()))
        }
    }

    fn digest(byte: u8) -> ContentDigest {
        ContentDigest::parse(&format!("blake3:{}", format!("{byte:02x}").repeat(32))).unwrap()
    }

    fn revision(byte: u8) -> RevisionId {
        RevisionId::parse(&format!("r:{}", format!("{byte:02x}").repeat(32))).unwrap()
    }

    fn profile_with_memory(memory_bytes: u64) -> ExecutionProfileIdentity {
        ExecutionProfileIdentity::from_profile(
            &ExecutorProfile::new(ProfileSpec::isolated(
                TrustTier::TrustedLocal,
                Platform::MacOs,
                Architecture::Aarch64,
                ResourceLimits::new(1, memory_bytes, 1, 1, 1, 1, 1, 1),
            ))
            .unwrap(),
        )
    }

    fn profile() -> ExecutionProfileIdentity {
        profile_with_memory(1)
    }

    fn attestation() -> ShadowAdapterRequest {
        ShadowAdapterRequest::new(
            PrincipalId::generate().unwrap(),
            ProjectId::generate().unwrap(),
            WorkspaceId::generate().unwrap(),
            ServerIdentity {
                server_artifact: digest(1),
                configuration: digest(2),
            },
            "fixture-lsp-1.0.0",
            PositionEncoding::Utf16,
            ShadowAdapterCapabilities::new(true, true, true),
            digest(4),
            profile(),
        )
        .unwrap()
    }

    fn pin(request: &ShadowAdapterRequest, shadow_safe: bool) -> VerifiedShadowAdapterPin {
        VerifiedShadowAdapterPin::new(
            request.server.clone(),
            request.server_version.clone(),
            request.position_encoding,
            request.capabilities,
            shadow_safe,
            request.isolation_identity.clone(),
            request.execution_profile.clone(),
        )
        .unwrap()
    }

    fn input(revision: u8, text: &str) -> ShadowRunInput {
        ShadowRunInput {
            base_revision: self::revision(revision),
            staged_digest: digest(3),
            nonce: [7; 32],
            documents: vec![ShadowDocument {
                path: RootRelativePath::parse("src/main.rs", 64).unwrap(),
                uri: "file:///workspace/src/main.rs".to_owned(),
                staged_text: text.to_owned(),
                staged_version: DocumentVersion::new(1),
            }],
        }
    }

    struct Fixture {
        parent: PathBuf,
        root: PathBuf,
        url: Url,
    }

    impl Fixture {
        fn new() -> Self {
            let mut random = [0_u8; 8];
            getrandom::fill(&mut random).unwrap();
            let parent = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("kit-shadow-unit-{}", u64::from_le_bytes(random)));
            let root = parent.join("workspace");
            fs::create_dir_all(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let url = Url::from_directory_path(&root).unwrap();
            Self { parent, root, url }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    fn notification(uri: &str, version: i32, message: &str, limits: CodecLimits) -> Vec<u8> {
        crate::verify::lsp::session::LspCodec::encode(
            &json!({
                "jsonrpc":"2.0",
                "method":"textDocument/publishDiagnostics",
                "params":{
                    "uri":uri,
                    "version":version,
                    "diagnostics":[{
                        "range":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":1}
                        },
                        "message":message
                    }]
                }
            }),
            limits,
        )
        .unwrap()
    }

    fn run_expiring(
        runner: &mut ShadowLspRunner<Launcher, ExpiringClock>,
        fixture: &Fixture,
        uri: &str,
        nonce: u8,
    ) -> Result<ShadowOutcome, ShadowError> {
        runner.run(
            ShadowRunInput {
                base_revision: revision(1),
                staged_digest: digest(3),
                nonce: [nonce; 32],
                documents: vec![ShadowDocument {
                    path: RootRelativePath::parse("main.rs", 64).unwrap(),
                    uri: uri.to_owned(),
                    staged_text: "x".to_owned(),
                    staged_version: DocumentVersion::new(1),
                }],
            },
            attestation(),
            root_identity(&fixture.root, &fixture.url),
            &fixture.root,
            vec![SnapshotFile::new("main.rs", b"x".to_vec(), false)],
            vec![OpenDocument::new(
                uri.to_owned(),
                DocumentVersion::new(1),
                "x".to_owned(),
            )],
            100,
        )
    }

    #[test]
    fn same_nonce_with_different_revision_document_server_and_profile_has_distinct_run_identity() {
        let attestation = attestation();
        let original = run_identity(&input(1, "one"), &attestation);
        assert_ne!(original, run_identity(&input(2, "one"), &attestation));
        assert_ne!(original, run_identity(&input(1, "two"), &attestation));
        let mut different_server = attestation.clone();
        different_server.server.server_artifact = digest(9);
        assert_ne!(original, run_identity(&input(1, "one"), &different_server));
        let mut different_profile = attestation.clone();
        different_profile.execution_profile = profile_with_memory(2);
        assert_ne!(original, run_identity(&input(1, "one"), &different_profile));
    }

    #[test]
    fn empty_and_verified_registries_resolve_per_server() {
        let request = attestation();
        assert_eq!(
            ShadowAdapterRegistry::compiled()
                .resolve(request.clone())
                .fallback(),
            Some((
                request.server.clone(),
                ShadowFallbackReason::PinsUnavailable
            ))
        );
        let decision = ShadowAdapterRegistry::verified_fixture(&request, true)
            .unwrap()
            .resolve(request.clone());
        assert!(matches!(
            decision.0,
            ShadowAdapterDecisionKind::Supported(_)
        ));

        let decision = ShadowAdapterRegistry::verified_fixture(&request, true)
            .unwrap()
            .resolve_for_platform(request.clone(), false);
        assert_eq!(
            decision.fallback(),
            Some((request.server, ShadowFallbackReason::PlatformUnavailable))
        );
    }

    #[test]
    fn registry_resolves_the_full_pin_key_and_rejects_duplicates() {
        let first = attestation();
        let mut second = first.clone();
        second.server.configuration = digest(9);
        let registry =
            ShadowAdapterRegistry::from_pins(vec![pin(&first, false), pin(&second, true)]).unwrap();
        assert!(matches!(
            registry.resolve(second.clone()).0,
            ShadowAdapterDecisionKind::Supported(_)
        ));
        assert_eq!(
            registry.resolve(first.clone()).fallback(),
            Some((
                first.server.clone(),
                ShadowFallbackReason::UnsupportedAdapter
            ))
        );

        let mut mismatch = second.clone();
        mismatch.server.configuration = digest(8);
        assert_eq!(
            registry.resolve(mismatch.clone()).fallback(),
            Some((mismatch.server, ShadowFallbackReason::PinsUnavailable))
        );

        for mismatch in [
            {
                let mut mismatch = second.clone();
                mismatch.server_version = "fixture-lsp-2.0.0".to_owned();
                mismatch
            },
            {
                let mut mismatch = second.clone();
                mismatch.position_encoding = PositionEncoding::Utf8;
                mismatch
            },
        ] {
            assert_eq!(
                registry.resolve(mismatch.clone()).fallback(),
                Some((mismatch.server, ShadowFallbackReason::RevisionPinsMismatch))
            );
        }
        assert!(matches!(
            ShadowAdapterRegistry::from_pins(vec![pin(&second, true), pin(&second, false)]),
            Err(ShadowRegistryError::DuplicateEntry)
        ));
    }

    #[test]
    fn cleanup_grace_must_be_finite_and_positive() {
        for cleanup_grace_ticks in [0, u64::MAX] {
            assert!(matches!(
                ShadowLspRunner::with_clock(
                    Launcher::default(),
                    SessionLimits::default(),
                    EditLimits::default(),
                    FactLimits::default(),
                    ShadowLimits {
                        cleanup_grace_ticks,
                        ..ShadowLimits::default()
                    },
                    Clock,
                ),
                Err(ShadowError::InvalidLimits)
            ));
        }
    }

    #[test]
    fn fourth_fake_transport_satisfies_the_bounded_receive_runner_contract() {
        let runner = ShadowLspRunner::with_clock(
            Launcher::default(),
            SessionLimits::default(),
            EditLimits::default(),
            FactLimits::default(),
            ShadowLimits::default(),
            Clock,
        );
        assert!(runner.is_ok());
    }

    #[test]
    fn notification_input_limit_is_cumulative_across_frames() {
        let fixture = Fixture::new();
        let uri = Url::from_file_path(fixture.root.join("main.rs"))
            .unwrap()
            .to_string();
        let codec = CodecLimits {
            max_header_bytes: 64,
            max_body_bytes: 960,
            max_frame_bytes: 1_024,
        };
        let frames = VecDeque::from([
            notification(&uri, 0, &"x".repeat(400), codec),
            notification(&uri, 1, &"x".repeat(400), codec),
        ]);
        assert!(frames.iter().map(Vec::len).sum::<usize>() > codec.max_frame_bytes);
        let launcher = Launcher::with_frames(frames);
        let session_limits = SessionLimits {
            codec,
            ..SessionLimits::default()
        };
        let mut runner = ShadowLspRunner::with_clock(
            launcher,
            session_limits,
            EditLimits::default(),
            FactLimits::default(),
            ShadowLimits {
                max_notification_bytes: codec.max_frame_bytes,
                ..ShadowLimits::default()
            },
            Clock,
        )
        .unwrap();
        let input = ShadowRunInput {
            base_revision: revision(1),
            staged_digest: digest(3),
            nonce: [1; 32],
            documents: vec![ShadowDocument {
                path: RootRelativePath::parse("main.rs", 64).unwrap(),
                uri: uri.clone(),
                staged_text: "x".to_owned(),
                staged_version: DocumentVersion::new(1),
            }],
        };
        assert_eq!(
            runner.run(
                input,
                attestation(),
                root_identity(&fixture.root, &fixture.url),
                &fixture.root,
                vec![SnapshotFile::new("main.rs", b"x".to_vec(), false)],
                vec![OpenDocument::new(
                    uri,
                    DocumentVersion::new(1),
                    "x".to_owned(),
                )],
                100,
            ),
            Err(ShadowError::NotificationInputLimitExceeded)
        );
    }

    #[test]
    fn normalized_output_limit_is_cumulative_across_documents() {
        let fixture = Fixture::new();
        let uris = ["a.rs", "b.rs"].map(|path| {
            Url::from_file_path(fixture.root.join(path))
                .unwrap()
                .to_string()
        });
        let codec = SessionLimits::default().codec;
        let frames = VecDeque::from([
            notification(&uris[0], 1, &"a".repeat(1_400), codec),
            notification(&uris[1], 1, &"b".repeat(1_400), codec),
        ]);
        let launcher = Launcher::with_frames(frames);
        let fact_limits = FactLimits {
            max_retained_output_bytes: 2_048,
            ..FactLimits::default()
        };
        let mut runner = ShadowLspRunner::with_clock(
            launcher,
            SessionLimits::default(),
            EditLimits::default(),
            fact_limits,
            ShadowLimits {
                max_normalized_output_bytes: 2_048,
                ..ShadowLimits::default()
            },
            Clock,
        )
        .unwrap();
        let documents = uris
            .iter()
            .enumerate()
            .map(|(index, uri)| ShadowDocument {
                path: RootRelativePath::parse(format!("{}.rs", char::from(b'a' + index as u8)), 64)
                    .unwrap(),
                uri: uri.clone(),
                staged_text: "x".to_owned(),
                staged_version: DocumentVersion::new(1),
            })
            .collect();
        assert_eq!(
            runner.run(
                ShadowRunInput {
                    base_revision: revision(1),
                    staged_digest: digest(3),
                    nonce: [1; 32],
                    documents,
                },
                attestation(),
                root_identity(&fixture.root, &fixture.url),
                &fixture.root,
                vec![
                    SnapshotFile::new("a.rs", b"x".to_vec(), false),
                    SnapshotFile::new("b.rs", b"x".to_vec(), false),
                ],
                vec![
                    OpenDocument::new(uris[0].clone(), DocumentVersion::new(1), "x".to_owned(),),
                    OpenDocument::new(uris[1].clone(), DocumentVersion::new(1), "x".to_owned(),),
                ],
                100,
            ),
            Err(ShadowError::OutputLimitExceeded)
        );
    }

    #[test]
    fn expiration_during_normalization_reaps_with_a_fresh_cleanup_budget() {
        let fixture = Fixture::new();
        let uri = Url::from_file_path(fixture.root.join("main.rs"))
            .unwrap()
            .to_string();
        let launcher = Launcher::with_frames(VecDeque::from([notification(
            &uri,
            1,
            "message",
            SessionLimits::default().codec,
        )]));
        let state = launcher.state.clone();
        let mut runner = ShadowLspRunner::with_clock(
            launcher,
            SessionLimits::default(),
            EditLimits::default(),
            FactLimits::default(),
            ShadowLimits::default(),
            ExpiringClock::new(13),
        )
        .unwrap();

        assert_eq!(
            run_expiring(&mut runner, &fixture, &uri, 1),
            Err(ShadowError::DeadlineExceeded)
        );
        assert_eq!(state.reaped.load(Ordering::SeqCst), 1);
        assert_eq!(runner.manager.usage().sessions, 0);
    }

    #[test]
    fn expiration_reap_failure_is_cleanup_failed_and_retains_the_process() {
        let fixture = Fixture::new();
        let uri = Url::from_file_path(fixture.root.join("main.rs"))
            .unwrap()
            .to_string();
        let launcher = Launcher::with_frames(VecDeque::from([notification(
            &uri,
            1,
            "message",
            SessionLimits::default().codec,
        )]));
        launcher.state.fail_reap.store(true, Ordering::SeqCst);
        let mut runner = ShadowLspRunner::with_clock(
            launcher,
            SessionLimits::default(),
            EditLimits::default(),
            FactLimits::default(),
            ShadowLimits::default(),
            ExpiringClock::new(13),
        )
        .unwrap();

        assert_eq!(
            run_expiring(&mut runner, &fixture, &uri, 1),
            Err(ShadowError::CleanupFailed(SessionError::Transport(
                TransportError::CloseOrReapFailed
            )))
        );
        assert_eq!(runner.manager.usage().sessions, 1);
        assert_eq!(runner.manager.usage().live_transports, 1);
    }

    #[test]
    fn repeated_expirations_release_session_capacity() {
        let fixture = Fixture::new();
        let uri = Url::from_file_path(fixture.root.join("main.rs"))
            .unwrap()
            .to_string();
        let launcher = Launcher::default();
        let state = launcher.state.clone();
        let clock = ExpiringClock::new(13);
        let mut runner = ShadowLspRunner::with_clock(
            launcher.clone(),
            SessionLimits {
                max_sessions: 1,
                ..SessionLimits::default()
            },
            EditLimits::default(),
            FactLimits::default(),
            ShadowLimits::default(),
            clock.clone(),
        )
        .unwrap();

        for nonce in 1..=3 {
            clock.reset();
            launcher.frames.lock().unwrap().push_back(notification(
                &uri,
                1,
                "message",
                SessionLimits::default().codec,
            ));
            assert_eq!(
                run_expiring(&mut runner, &fixture, &uri, nonce),
                Err(ShadowError::DeadlineExceeded)
            );
        }
        assert_eq!(state.reaped.load(Ordering::SeqCst), 3);
        assert_eq!(runner.manager.usage().sessions, 0);
    }
}
