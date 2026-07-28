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
            append::{AppendCommand, AppendOutcome, CrashPoint, SqliteStore, StoreError},
            idempotency::{CanonicalRequestDigest, ClaimOutcome, IdempotencyKey, IdempotencyScope},
            projection::{
                ProjectionCrashPoint, ProjectionError, ProjectionSnapshot, ProjectionStore,
                StoreTime,
            },
        },
    },
};
use rusqlite::Transaction;

#[derive(Clone, Debug)]
pub enum FormatterTestAction {
    Pass,
    Rewrite(String, Vec<u8>),
    Delete(String),
    Chmod(String, u32),
    Symlink(String, String),
    Exit(i32),
    Timeout,
    Output(usize),
    SurvivingProcess,
    ProvenanceMismatch,
    MeasurementAbsent,
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

pub fn formatter_executor(
    action: FormatterTestAction,
) -> crate::executor::formatter::FormatterExecutor {
    use crate::executor::formatter::DebugFormatterAction as Action;

    crate::executor::formatter::FormatterExecutor::debug(match action {
        FormatterTestAction::Pass => Action::Pass,
        FormatterTestAction::Rewrite(path, bytes) => Action::Rewrite(path, bytes),
        FormatterTestAction::Delete(path) => Action::Delete(path),
        FormatterTestAction::Chmod(path, mode) => Action::Chmod(path, mode),
        FormatterTestAction::Symlink(path, target) => Action::Symlink(path, target),
        FormatterTestAction::Exit(code) => Action::Exit(code),
        FormatterTestAction::Timeout => Action::Timeout,
        FormatterTestAction::Output(bytes) => Action::Output(bytes),
        FormatterTestAction::SurvivingProcess => Action::SurvivingProcess,
        FormatterTestAction::ProvenanceMismatch => Action::ProvenanceMismatch,
        FormatterTestAction::MeasurementAbsent => Action::MeasurementAbsent,
    })
}

pub fn formatter_executor_gate() -> (
    crate::executor::formatter::FormatterExecutor,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
) {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    (
        crate::executor::formatter::FormatterExecutor::debug(
            crate::executor::formatter::DebugFormatterAction::Gate {
                entered: entered_tx,
                release: release_rx,
            },
        ),
        entered_rx,
        release_tx,
    )
}

fn authority() -> ControlPlaneAuthority {
    ControlPlaneAuthority::for_test()
}

pub fn open_sqlite_store(path: impl AsRef<Path>) -> Result<SqliteStore, StoreError> {
    SqliteStore::open(path, &authority())
}

pub fn append(
    store: &mut SqliteStore,
    command: AppendCommand,
) -> Result<AppendOutcome, StoreError> {
    store.append(command)
}

pub fn append_with_hook(
    store: &mut SqliteStore,
    command: AppendCommand,
    crash: impl FnMut(CrashPoint) -> bool,
) -> Result<AppendOutcome, StoreError> {
    store.append_with_hook(command, crash)
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
