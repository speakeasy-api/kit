use std::path::Path;

use crate::{
    api::{
        auth::contract::Authorizer,
        service::{NoopRuntime, Service, SqliteServiceStore},
    },
    domain::config::RunConfigMaterializer,
    runtime::{
        daemon::ControlPlaneAuthority,
        lease::{LeaseError, LocalLeaseRuntime},
    },
    store::{
        backup::{BackupConfig, BackupError, BackupGeneration, BackupManager},
        sqlite::{
            append::{
                AppendCommand, AppendOutcome, CrashPoint, PendingArtifactPublication, SqliteStore,
                StoreError,
            },
            idempotency::{CanonicalRequestDigest, ClaimOutcome, IdempotencyKey, IdempotencyScope},
            projection::{
                ProjectionCrashPoint, ProjectionError, ProjectionSnapshot, ProjectionStore,
                StoreTime,
            },
        },
    },
};
use rusqlite::Transaction;

struct RegisteredProcessBoundary {
    identity: crate::executor::process::tree::BoundaryIdentity,
    quiescent: bool,
}

impl RegisteredProcessBoundary {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            identity: crate::executor::process::tree::BoundaryIdentity::new(
                crate::executor::process::tree::BoundaryKind::Container,
                "test-registered-process",
                "11".repeat(32),
                "test-runtime-boundary",
            )
            .map_err(std::io::Error::other)?,
            quiescent: false,
        })
    }
}

impl crate::executor::process::tree::BoundaryControl for RegisteredProcessBoundary {
    fn identity(&self) -> &crate::executor::process::tree::BoundaryIdentity {
        &self.identity
    }

    fn containment(&self) -> crate::executor::process::tree::Containment {
        crate::executor::process::tree::Containment::Complete
    }

    fn release(&mut self, _deadline: std::time::Instant) -> std::io::Result<()> {
        Ok(())
    }

    fn kill_boundary(&mut self, _deadline: std::time::Instant) -> std::io::Result<()> {
        self.quiescent = true;
        Ok(())
    }

    fn wait_and_reap(&mut self, _deadline: std::time::Instant) -> std::io::Result<()> {
        self.quiescent = true;
        Ok(())
    }

    fn inspect(
        &mut self,
        _deadline: std::time::Instant,
    ) -> std::io::Result<crate::executor::process::tree::Inspection> {
        Ok(crate::executor::process::tree::Inspection {
            identity: self.identity.clone(),
            survivors: self.quiescent.then_some(0),
            quiescent: self.quiescent,
        })
    }
}

pub fn spawn_registered_test_process(
    command: std::process::Command,
    owner: crate::domain::lifecycle::ProcessOwnership,
    registration: crate::executor::process::own::ProcessRegistryRegistration,
    limits: crate::executor::profile::ResourceLimits,
) -> std::io::Result<crate::executor::process::own::OwnedProcess> {
    let token = crate::executor::process::own::PreparedCommandToken::issue_observed_registered(
        command,
        owner,
        RegisteredProcessBoundary::new()?,
        |_: &crate::executor::process::tree::PersistedBoundary| Ok(()),
        |_, _| Ok(()),
        Some(registration),
        std::time::Instant::now() + std::time::Duration::from_secs(5),
        limits,
    )?;
    crate::executor::process::own::spawn_owned(token, limits)
}

#[derive(Clone, Copy, Debug)]
pub enum SyntaxTestAction {
    Pass,
    Fail,
    Stuck,
}

pub fn syntax_executor(
    language: &str,
    version: &str,
    action: SyntaxTestAction,
) -> crate::executor::syntax::SyntaxExecutor {
    use crate::executor::syntax::DebugSyntaxAction as Action;

    crate::executor::syntax::SyntaxExecutor::debug(
        language,
        version,
        match action {
            SyntaxTestAction::Pass => Action::Pass(None),
            SyntaxTestAction::Fail => Action::Fail,
            SyntaxTestAction::Stuck => Action::Stuck,
        },
    )
}

pub fn syntax_executor_gate_second(
    language: &str,
    version: &str,
) -> (
    crate::executor::syntax::SyntaxExecutor,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
) {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let executor = crate::executor::syntax::SyntaxExecutor::debug(
        language,
        version,
        crate::executor::syntax::DebugSyntaxAction::GateSecond {
            calls: 0,
            entered: entered_tx,
            release: release_rx,
        },
    );
    (executor, entered_rx, release_tx)
}

pub fn syntax_executor_with_capture(
    language: &str,
    version: &str,
    capture: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) -> crate::executor::syntax::SyntaxExecutor {
    crate::executor::syntax::SyntaxExecutor::debug(
        language,
        version,
        crate::executor::syntax::DebugSyntaxAction::Pass(Some(capture)),
    )
}

fn authority() -> ControlPlaneAuthority {
    ControlPlaneAuthority::for_test()
}

pub fn open_sqlite_store(path: impl AsRef<Path>) -> Result<SqliteStore, StoreError> {
    SqliteStore::open(path, &authority())
}

pub fn open_project_store(
    path: impl AsRef<Path>,
    custody: crate::domain::secret::SecretCustody,
) -> Result<SqliteStore, StoreError> {
    SqliteStore::open(
        path,
        &crate::runtime::daemon::ControlPlaneAuthority::for_test_project(custody),
    )
}

pub fn append(
    store: &mut SqliteStore,
    command: AppendCommand,
) -> Result<AppendOutcome, StoreError> {
    store.append(command)
}

pub fn project_event_export(
    event: &crate::store::sqlite::append::StoredEvent,
    custody: &crate::domain::secret::SecretCustody,
) -> Result<Vec<u8>, String> {
    project_event_export_projection(event, custody).map(|projected| projected.envelope)
}

pub(crate) fn project_event_export_projection(
    event: &crate::store::sqlite::append::StoredEvent,
    custody: &crate::domain::secret::SecretCustody,
) -> Result<crate::api::service::ProjectedEventEnvelope, String> {
    #[derive(serde::Serialize)]
    struct Envelope<'a> {
        id: &'a crate::domain::ids::EventId,
        stream: &'a crate::domain::events::EntityId,
        sequence: crate::domain::events::StreamSequence,
        commit_position: crate::domain::events::CommitPosition,
        #[serde(rename = "type")]
        event_type: &'a crate::domain::events::EventType,
        schema_version: crate::domain::events::SchemaVersion,
        occurred_at: &'a crate::domain::events::UtcDateTime,
        causation_id: &'a crate::domain::ids::CommandId,
        correlation_id: &'a crate::domain::events::EntityId,
        attempt_id: Option<crate::domain::ids::AttemptId>,
        trace_id: &'a crate::domain::events::TraceId,
        payload: &'a serde_json::value::RawValue,
        artifacts: &'a serde_json::value::RawValue,
    }
    let payload = serde_json::from_slice::<&serde_json::value::RawValue>(&event.event.payload)
        .map_err(|error| error.to_string())?;
    let artifacts = serde_json::from_slice::<&serde_json::value::RawValue>(&event.event.artifacts)
        .map_err(|error| error.to_string())?;
    let envelope = serde_json::to_vec(&Envelope {
        id: &event.event.id,
        stream: &event.event.stream,
        sequence: event.sequence,
        commit_position: event.commit_position,
        event_type: &event.event.event_type,
        schema_version: event.event.schema_version,
        occurred_at: &event.event.occurred_at,
        causation_id: &event.event.causation_id,
        correlation_id: &event.event.correlation_id,
        attempt_id: event.event.attempt_id,
        trace_id: &event.event.trace_id,
        payload,
        artifacts,
    })
    .map_err(|error| error.to_string())?;
    crate::api::service::project_event_envelopes(
        custody,
        vec![(envelope, event.event.payload.clone())],
    )
    .map(|mut projected| projected.remove(0))
}

pub fn project_composition_input(
    custody: &crate::domain::secret::SecretCustody,
    input: &crate::agent::prompt::PromptInput,
) -> Result<crate::agent::prompt::PromptInput, String> {
    crate::agent::executor::project_composition_input(custody, input)
        .map_err(|error| error.to_string())
}

pub fn append_with_hook(
    store: &mut SqliteStore,
    command: AppendCommand,
    crash: impl FnMut(CrashPoint) -> bool,
) -> Result<AppendOutcome, StoreError> {
    store.append_with_hook(command, crash)
}

pub fn arm_artifact_publication(
    store: &mut SqliteStore,
    publication: PendingArtifactPublication,
) -> Result<(), StoreError> {
    store.arm_artifact_publication(publication)
}

pub fn claim(
    store: &mut SqliteStore,
    scope: IdempotencyScope,
    key: IdempotencyKey,
    digest: CanonicalRequestDigest,
) -> Result<ClaimOutcome, StoreError> {
    store.claim(scope, key, digest)
}

pub fn open_service_store(
    path: impl AsRef<Path>,
) -> Result<SqliteServiceStore, crate::api::service::ServiceError> {
    SqliteServiceStore::open(path, &authority())
}

pub fn open_project_service_store(
    path: impl AsRef<Path>,
    custody: crate::domain::secret::SecretCustody,
) -> Result<SqliteServiceStore, crate::api::service::ServiceError> {
    SqliteServiceStore::open(
        path,
        &crate::runtime::daemon::ControlPlaneAuthority::for_test_project(custody),
    )
}

pub fn service<S, A>(store: S, authorizer: A) -> Service<S, A, NoopRuntime>
where
    A: Authorizer,
{
    Service::new(store, authorizer, &authority())
}

pub fn service_with_runtime<S, A, R>(store: S, authorizer: A, runtime: R) -> Service<S, A, R>
where
    A: Authorizer,
{
    Service::with_runtime(store, authorizer, runtime, &authority())
}

pub fn project_service_with_runtime<S, A, R>(
    store: S,
    authorizer: A,
    runtime: R,
    custody: crate::domain::secret::SecretCustody,
) -> Service<S, A, R>
where
    A: Authorizer,
{
    Service::with_runtime(
        store,
        authorizer,
        runtime,
        &crate::runtime::daemon::ControlPlaneAuthority::for_test_project(custody),
    )
}

pub fn service_with_runtime_and_config<S, A, R, M>(
    store: S,
    authorizer: A,
    runtime: R,
    config_materializer: M,
) -> Service<S, A, R, M>
where
    A: Authorizer,
    M: RunConfigMaterializer,
{
    Service::with_runtime_and_config(
        store,
        authorizer,
        runtime,
        config_materializer,
        &authority(),
    )
}

pub fn service_with_config<S, A, M>(
    store: S,
    authorizer: A,
    config_materializer: M,
) -> Service<S, A, NoopRuntime, M>
where
    A: Authorizer,
    M: RunConfigMaterializer,
{
    Service::with_config(store, authorizer, config_materializer, &authority())
}

pub fn noop_runtime() -> NoopRuntime {
    NoopRuntime
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn trusted_edit_authority(
    principal: crate::domain::ids::PrincipalId,
    project: crate::domain::ids::ProjectId,
) -> crate::workspace::edit::validate::AuthenticatedEditAuthority {
    use crate::{
        api::auth::contract::{AuthenticatedPrincipal, GrantSnapshot},
        domain::config::Grant,
    };

    let grants = GrantSnapshot::new(principal, project, [Grant::WorkspaceWrite]);
    let authenticated = AuthenticatedPrincipal::from_grants(grants.clone());
    crate::workspace::edit::validate::AuthenticatedEditAuthority::from_authenticated(
        &authenticated,
        &grants,
        project,
    )
    .expect("trusted edit fixture is authorized")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn trusted_verification_context(
    principal: crate::domain::ids::PrincipalId,
    project: crate::domain::ids::ProjectId,
) -> (
    crate::api::auth::contract::AuthenticatedPrincipal,
    crate::api::auth::contract::GrantSnapshot,
    crate::domain::config::RunConfigSnapshot,
) {
    use crate::{
        api::auth::contract::{AuthenticatedPrincipal, GrantSnapshot},
        domain::{
            config::{Grant, LayerStack, RunConfigContext},
            ids::RunId,
        },
    };

    let grants = GrantSnapshot::new(
        principal,
        project,
        [
            Grant::WorkspaceRead,
            Grant::WorkspaceWrite,
            Grant::ProcessSpawn,
            Grant::VerificationTargeted,
            Grant::VerificationFull,
        ],
    );
    let authenticated = AuthenticatedPrincipal::from_grants(grants.clone());
    let config = LayerStack::safe_defaults()
        .materialize(
            RunConfigContext {
                principal_id: principal,
                project_id: project,
                run_id: RunId::generate().expect("test run id"),
            },
            grants.grants(),
        )
        .expect("trusted verification config");
    (authenticated, grants, config)
}

pub fn open_lease_runtime(root: impl AsRef<Path>) -> Result<LocalLeaseRuntime, LeaseError> {
    LocalLeaseRuntime::open(root, &authority())
}

pub fn create_backup(
    manager: &mut BackupManager,
    now_unix_micros: i64,
    inventory: &mut SqliteServiceStore,
) -> Result<BackupGeneration, BackupError> {
    manager.create_backup(now_unix_micros, inventory)
}

pub fn open_backup_manager(config: BackupConfig) -> Result<BackupManager, BackupError> {
    BackupManager::open(config)
}

pub fn reconcile_backup_generations(
    manager: &mut BackupManager,
    now_unix_micros: i64,
) -> Result<Vec<BackupGeneration>, BackupError> {
    manager.reconcile_generations(now_unix_micros)
}

pub fn prune_backup_generations(manager: &mut BackupManager) -> Result<usize, BackupError> {
    manager.prune_generations()
}

pub fn open_projection_store(path: impl AsRef<Path>) -> Result<ProjectionStore, ProjectionError> {
    ProjectionStore::open(path)
}

pub fn update_projection(
    store: &mut ProjectionStore,
    name: &str,
    initial: &[u8],
    reducer: impl FnMut(
        &mut Vec<u8>,
        &crate::store::sqlite::append::StoredEvent,
    ) -> Result<(), ProjectionError>,
) -> Result<ProjectionSnapshot, ProjectionError> {
    store.update(name, initial, reducer)
}

pub fn update_domain_projection(
    store: &mut ProjectionStore,
) -> Result<
    (
        crate::domain::projections::DomainReducer,
        ProjectionSnapshot,
    ),
    ProjectionError,
> {
    store.update_domain()
}

pub fn rebuild_domain_projection(
    store: &mut ProjectionStore,
) -> Result<
    (
        crate::domain::projections::DomainReducer,
        ProjectionSnapshot,
    ),
    ProjectionError,
> {
    store.rebuild_domain()
}

pub fn rebuild_projection(
    store: &mut ProjectionStore,
    name: &str,
    initial: &[u8],
    reducer: impl FnMut(
        &mut Vec<u8>,
        &crate::store::sqlite::append::StoredEvent,
    ) -> Result<(), ProjectionError>,
) -> Result<ProjectionSnapshot, ProjectionError> {
    store.rebuild(name, initial, reducer)
}

pub fn update_projection_with_hook(
    store: &mut ProjectionStore,
    name: &str,
    initial: &[u8],
    rebuild: bool,
    reducer: impl FnMut(
        &mut Vec<u8>,
        &crate::store::sqlite::append::StoredEvent,
    ) -> Result<(), ProjectionError>,
    crash: impl FnMut(ProjectionCrashPoint) -> bool,
) -> Result<ProjectionSnapshot, ProjectionError> {
    store.update_with_hook(name, initial, rebuild, reducer, crash)
}

pub fn with_projection_store_time<T>(
    store: &mut ProjectionStore,
    operation: impl FnOnce(&Transaction<'_>, &StoreTime) -> Result<T, ProjectionError>,
) -> Result<T, ProjectionError> {
    store.with_store_time(operation)
}

pub fn projection_store_time(store: &mut ProjectionStore) -> Result<StoreTime, ProjectionError> {
    store.store_time()
}

pub fn projection_migration_versions() -> impl DoubleEndedIterator<Item = i64> {
    crate::store::sqlite::projection::migration_versions()
}

pub fn rollback_latest_projection_migration(
    path: impl AsRef<Path>,
) -> Result<Option<i64>, ProjectionError> {
    crate::store::sqlite::projection::rollback_latest_migration(path)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn authorize_path_read_with_hook<'guard, 'workspace>(
    authorizer: &mut crate::workspace::path_auth::PathAuthorizer<'guard, 'workspace>,
    path: impl AsRef<Path>,
    hook: impl FnMut(&str, &Path),
) -> Result<
    crate::workspace::path_auth::ExistingRead<'guard, 'workspace>,
    crate::workspace::path_auth::PathAuthError,
> {
    authorizer.authorize_read_with_hook(path, hook)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn validate_edit_with_hook<'workspace>(
    workspace: &'workspace crate::workspace::revision::ManagedWorkspace,
    ir: &crate::workspace::edit::ir::EditIr,
    limits: crate::workspace::edit::ir::EditLimits,
    hook: impl FnMut(&str, &Path),
) -> Result<
    crate::workspace::edit::validate::ValidatedTransaction<'workspace>,
    crate::workspace::edit::validate::ValidationError,
> {
    crate::workspace::edit::validate::validate_with_hook(workspace, ir, limits, hook)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn validated_edit_source_identities(
    transaction: &crate::workspace::edit::validate::ValidatedTransaction<'_>,
) -> Vec<crate::workspace::path_auth::FileIdentity> {
    transaction.capability_source_identities()
}

#[cfg(debug_assertions)]
pub fn shadow_adapter_registry_fixture(
    request: &crate::verify::lsp::shadow::ShadowAdapterRequest,
    shadow_safe: bool,
) -> Result<
    crate::verify::lsp::shadow::ShadowAdapterRegistry,
    crate::verify::lsp::shadow::ShadowRegistryError,
> {
    crate::verify::lsp::shadow::ShadowAdapterRegistry::verified_fixture(request, shadow_safe)
}

#[cfg(debug_assertions)]
pub fn receive_current_notification<L, C>(
    manager: &mut crate::verify::lsp::session::LspSessionManager<L, C>,
    service_id: crate::domain::ids::DaemonServiceId,
    deadline_tick: u64,
) -> Result<
    crate::verify::lsp::session::NotificationDisposition,
    crate::verify::lsp::session::SessionError,
>
where
    L: crate::verify::lsp::session::OwnedLspLauncher,
    C: crate::verify::lsp::session::TickClock,
{
    manager
        .receive_current_notification(service_id, deadline_tick)
        .map(|received| received.into_disposition())
}
pub fn mcp_stdio_worker_main(arguments: &[std::ffi::OsString]) -> Option<std::process::ExitCode> {
    if arguments.get(1).and_then(|value| value.to_str()) != Some("--kit-mcp-conformance-worker") {
        return None;
    }
    use std::io::{BufRead as _, Write as _};

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let request: serde_json::Value =
            match line.ok().and_then(|line| serde_json::from_str(&line).ok()) {
                Some(request) => request,
                None => return Some(std::process::ExitCode::FAILURE),
            };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let result = match request.get("method").and_then(serde_json::Value::as_str) {
            Some("initialize") => serde_json::json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{"tools":{}},
                "serverInfo":{"name":"kit-conformance-worker","version":"1"}
            }),
            Some("tools/list") => serde_json::json!({"tools":[{
                "name":"fixture_echo",
                "description":"Echo fixture text.",
                "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}
            }]}),
            Some("tools/call") => serde_json::json!({
                "content":[{"type":"text","text":"fixture result"}],
                "isError":false
            }),
            _ => serde_json::json!({}),
        };
        if serde_json::to_writer(
            &mut stdout,
            &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
        )
        .is_err()
            || stdout.write_all(b"\n").is_err()
            || stdout.flush().is_err()
        {
            return Some(std::process::ExitCode::FAILURE);
        }
    }
    Some(std::process::ExitCode::SUCCESS)
}
