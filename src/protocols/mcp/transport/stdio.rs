use std::{
    collections::BTreeMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use agentkit_mcp::{McpConnection, McpHandlerConfig, McpServerId};
use rmcp::{
    RoleClient,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};

use crate::{
    capabilities::broker::{
        BrokerInvocation,
        transport_auth::{self, TransportBinding, TransportOperation},
    },
    domain::{
        ids::{PrincipalId, ProjectId},
        lifecycle::{AttemptOwnership, ProcessOwnership},
        secret::SecretLease,
    },
    executor::{
        backends::local_os::{LocalCommand, LocalOsBackend, SandboxPaths},
        process::own::{
            OwnedStdioChild, PreparedCommandToken, ProcessRegistrationContext, ProcessRegistry,
            ProcessRegistryRegistration,
        },
        profile::ExecutorProfile,
    },
    store::sqlite::append::SqliteStore,
    telemetry::redact::{CaptureRedactor, SensitiveDataScanner},
};

use super::{OperationGate, ReadyConnection, TransportError, TransportFailure, TransportLimits};
use crate::protocols::mcp::features::{ConfiguredServerIdentity, RawPayload};

#[derive(Clone, Copy, Debug)]
pub struct OwnedStdioLimits {
    max_frame_bytes: usize,
    io_timeout: Duration,
    close_timeout: Duration,
}

impl OwnedStdioLimits {
    pub const fn max_frame_bytes(self) -> usize {
        self.max_frame_bytes
    }

    pub const fn io_timeout(self) -> Duration {
        self.io_timeout
    }

    pub const fn close_timeout(self) -> Duration {
        self.close_timeout
    }
}

#[derive(Debug)]
pub enum OwnedStdioLaunchError {
    Unavailable,
    Io(io::Error),
}

impl std::fmt::Display for OwnedStdioLaunchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("durable owned-process service unavailable"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for OwnedStdioLaunchError {}

/// Executor-owned process proxy. The executor service retains process-tree
/// custody and recovery ownership; dropping this proxy must never orphan it.
#[async_trait::async_trait]
pub trait OwnedStdioProcess: Send + Sync + 'static {
    async fn send_frame(&self, frame: &[u8]) -> io::Result<()>;
    async fn receive_frame(&self) -> io::Result<Option<Vec<u8>>>;
    async fn close_and_reap(&self) -> io::Result<()>;
}

/// Durable executor boundary for launching an already-issued process token.
/// Implementations must register recovery ownership before releasing the child.
#[async_trait::async_trait]
pub trait OwnedStdioProcessService: Send + Sync + 'static {
    async fn launch(
        &self,
        token: PreparedCommandToken,
        environment: OwnedStdioEnvironment,
        limits: OwnedStdioLimits,
    ) -> Result<OwnedStdioProcessLaunch, OwnedStdioLaunchError>;

    async fn abort_and_reap(&self, process_identity: &str) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnedStdioProfileError {
    Unavailable,
    Invalid,
}

impl std::fmt::Display for OwnedStdioProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "owned stdio process profile is unavailable",
            Self::Invalid => "owned stdio process profile is invalid",
        })
    }
}

impl std::error::Error for OwnedStdioProfileError {}

/// Executor-owned profile resolver. Implementations issue a single-use
/// `PreparedCommandToken` and pair it with the durable process service that
/// returns the process-bound credential scanner in `OwnedStdioProcessLaunch`.
pub trait OwnedStdioProfileProvider: Send + Sync + 'static {
    fn prepare(
        &self,
        profile: &str,
        owner: AttemptOwnership,
        authorized_credentials: &Arc<
            BTreeMap<crate::domain::secret::SecretHandle, Arc<SecretLease>>,
        >,
    ) -> Result<SandboxedStdioLauncher, OwnedStdioProfileError>;
}

pub struct OwnedStdioEnvironment {
    values: BTreeMap<
        String,
        (
            crate::domain::secret::SecretHandle,
            Arc<crate::domain::secret::SecretLease>,
        ),
    >,
}

impl OwnedStdioEnvironment {
    fn new(
        values: impl IntoIterator<Item = (String, crate::domain::secret::SecretHandle)>,
        authorized: &Arc<BTreeMap<crate::domain::secret::SecretHandle, Arc<SecretLease>>>,
    ) -> Result<Self, OwnedStdioProfileError> {
        let mut resolved = BTreeMap::new();
        for (variable, handle) in values {
            let lease = authorized
                .get(&handle)
                .cloned()
                .ok_or(OwnedStdioProfileError::Invalid)?;
            resolved.insert(variable, (handle, lease));
        }
        Ok(Self { values: resolved })
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &crate::domain::secret::SecretHandle)> {
        self.values
            .iter()
            .map(|(variable, (handle, _))| (variable.as_str(), handle))
    }
}

pub struct OwnedStdioProcessLaunch {
    process: Arc<dyn OwnedStdioProcess>,
    scanners: Vec<SensitiveDataScanner>,
}

#[cfg(not(windows))]
struct ProductionStdioProfile {
    argv: Vec<String>,
    environment: BTreeMap<String, crate::protocols::mcp::config::McpStdioEnvironmentConfig>,
    profile: ExecutorProfile,
    backend: LocalOsBackend,
}

#[cfg(not(windows))]
pub(crate) struct ProductionStdioProfiles {
    profiles: BTreeMap<String, ProductionStdioProfile>,
    paths: SandboxPaths,
    project_root: PathBuf,
    registration: ProcessRegistryRegistration,
    service: Arc<ProductionOwnedStdioService>,
}

#[cfg(not(windows))]
impl ProductionStdioProfiles {
    pub(crate) fn new(
        servers: &[crate::protocols::mcp::config::McpServerConfig],
        project_root: &Path,
        build_root: &Path,
        temp_root: &Path,
        registry: Arc<dyn ProcessRegistry>,
        principal_id: PrincipalId,
        project_id: ProjectId,
    ) -> Result<Option<Arc<Self>>, String> {
        let paths = SandboxPaths::new(project_root, build_root, temp_root)
            .map_err(|error| error.to_string())?;
        let mut profiles = BTreeMap::new();
        for server in servers {
            let crate::protocols::mcp::config::McpTransportConfig::Stdio {
                owned_process_profile,
                argv,
                profile,
                profile_digest,
                environment,
            } = &server.transport
            else {
                continue;
            };
            let profile = ExecutorProfile::new(profile.as_ref().clone())
                .map_err(|error| error.to_string())?;
            if profile.digest().to_string() != *profile_digest {
                return Err(format!(
                    "MCP stdio profile {owned_process_profile:?} digest mismatch"
                ));
            }
            let backend =
                LocalOsBackend::select(&profile, &paths).map_err(|error| error.to_string())?;
            let configured = ProductionStdioProfile {
                argv: argv.clone(),
                environment: environment.clone(),
                profile,
                backend,
            };
            if profiles
                .insert(owned_process_profile.clone(), configured)
                .is_some()
            {
                return Err(format!(
                    "duplicate MCP stdio profile {owned_process_profile:?}"
                ));
            }
        }
        if profiles.is_empty() {
            return Ok(None);
        }
        Ok(Some(Arc::new(Self {
            profiles,
            paths,
            project_root: project_root.to_owned(),
            registration: ProcessRegistryRegistration::new(
                registry,
                ProcessRegistrationContext {
                    principal_id,
                    project_id,
                },
            ),
            service: Arc::new(ProductionOwnedStdioService::default()),
        })))
    }
}

#[cfg(not(windows))]
impl OwnedStdioProfileProvider for ProductionStdioProfiles {
    fn prepare(
        &self,
        profile_name: &str,
        owner: AttemptOwnership,
        authorized_credentials: &Arc<
            BTreeMap<crate::domain::secret::SecretHandle, Arc<SecretLease>>,
        >,
    ) -> Result<SandboxedStdioLauncher, OwnedStdioProfileError> {
        let configured = self
            .profiles
            .get(profile_name)
            .ok_or(OwnedStdioProfileError::Unavailable)?;
        if configured.argv.is_empty()
            || owner.principal_id != self.registration.context.principal_id
        {
            return Err(OwnedStdioProfileError::Invalid);
        }
        let mut command = LocalCommand::new(&configured.argv[0], &self.project_root);
        for argument in &configured.argv[1..] {
            command = command.arg(argument);
        }
        let environment = OwnedStdioEnvironment::new(
            configured
                .environment
                .iter()
                .map(|(variable, credential)| (variable.clone(), credential.handle.clone())),
            authorized_credentials,
        )?;
        let prepared = configured
            .backend
            .prepare(&configured.profile, &self.paths, command)
            .and_then(|prepared| {
                prepared.into_owned_token(
                    ProcessOwnership::Attempt(owner),
                    self.registration.clone(),
                    configured.profile.clone(),
                )
            })
            .map_err(|_| OwnedStdioProfileError::Unavailable)?;
        let service: Arc<dyn OwnedStdioProcessService> = self.service.clone();
        Ok(SandboxedStdioLauncher::with_environment(
            prepared,
            service,
            environment,
        ))
    }
}

#[cfg(not(windows))]
#[derive(Default)]
struct ProductionOwnedStdioService {
    active: Arc<Mutex<BTreeMap<String, Arc<OwnedStdioChild>>>>,
}

#[cfg(not(windows))]
struct ProductionOwnedStdioProcess {
    identity: String,
    child: Arc<OwnedStdioChild>,
    active: Arc<Mutex<BTreeMap<String, Arc<OwnedStdioChild>>>>,
}

#[cfg(not(windows))]
impl Drop for ProductionOwnedStdioProcess {
    fn drop(&mut self) {
        let child = self
            .active
            .lock()
            .ok()
            .and_then(|mut active| active.remove(&self.identity))
            .unwrap_or_else(|| Arc::clone(&self.child));
        let _ = std::thread::Builder::new()
            .name("kit-mcp-reap".to_owned())
            .spawn(move || {
                let _ = child.close_and_reap();
            });
    }
}

#[cfg(not(windows))]
#[async_trait::async_trait]
impl OwnedStdioProcessService for ProductionOwnedStdioService {
    async fn launch(
        &self,
        token: PreparedCommandToken,
        environment: OwnedStdioEnvironment,
        limits: OwnedStdioLimits,
    ) -> Result<OwnedStdioProcessLaunch, OwnedStdioLaunchError> {
        let identity = token.stdio_identity();
        let max_frame_bytes = limits.max_frame_bytes();
        let child = tokio::task::spawn_blocking(move || {
            let mut leases = Vec::with_capacity(environment.values.len());
            for (variable, (_, lease)) in environment.values {
                leases.push((variable, SecretLease::new(lease.expose().to_vec())));
            }
            let scanner = CaptureRedactor::new(
                &leases
                    .iter()
                    .map(|(_, lease)| SecretLease::new(lease.expose().to_vec()))
                    .collect::<Vec<_>>(),
            )
            .scanner();
            OwnedStdioChild::spawn_with_environment(token, max_frame_bytes, &leases)
                .map(|child| (child, scanner))
        })
        .await
        .map_err(|_| OwnedStdioLaunchError::Unavailable)?
        .map_err(OwnedStdioLaunchError::Io)?;
        let (child, scanner) = child;
        let child = Arc::new(child);
        self.active
            .lock()
            .map_err(|_| OwnedStdioLaunchError::Unavailable)?
            .insert(identity.clone(), Arc::clone(&child));
        Ok(OwnedStdioProcessLaunch {
            process: Arc::new(ProductionOwnedStdioProcess {
                identity,
                child,
                active: Arc::clone(&self.active),
            }),
            scanners: vec![scanner],
        })
    }

    async fn abort_and_reap(&self, process_identity: &str) -> io::Result<()> {
        let child = self
            .active
            .lock()
            .map_err(|_| io::Error::other("MCP stdio process registry poisoned"))?
            .remove(process_identity);
        match child {
            Some(child) => tokio::task::spawn_blocking(move || child.close_and_reap())
                .await
                .map_err(|_| io::Error::other("MCP stdio cleanup task panicked"))?,
            None => Ok(()),
        }
    }
}

#[cfg(not(windows))]
#[async_trait::async_trait]
impl OwnedStdioProcess for ProductionOwnedStdioProcess {
    async fn send_frame(&self, frame: &[u8]) -> io::Result<()> {
        let child = Arc::clone(&self.child);
        let frame = frame.to_vec();
        tokio::task::spawn_blocking(move || child.send_frame(&frame))
            .await
            .map_err(|_| io::Error::other("MCP stdio write task panicked"))?
    }

    async fn receive_frame(&self) -> io::Result<Option<Vec<u8>>> {
        let child = Arc::clone(&self.child);
        tokio::task::spawn_blocking(move || child.receive_frame())
            .await
            .map_err(|_| io::Error::other("MCP stdio read task panicked"))?
    }

    async fn close_and_reap(&self) -> io::Result<()> {
        self.active
            .lock()
            .map_err(|_| io::Error::other("MCP stdio process registry poisoned"))?
            .remove(&self.identity);
        let child = Arc::clone(&self.child);
        tokio::task::spawn_blocking(move || child.close_and_reap())
            .await
            .map_err(|_| io::Error::other("MCP stdio cleanup task panicked"))?
    }
}

impl OwnedStdioProcessLaunch {
    pub fn new(
        process: Arc<dyn OwnedStdioProcess>,
        injected_credentials: &CaptureRedactor<'_>,
    ) -> Self {
        Self {
            process,
            scanners: vec![injected_credentials.scanner()],
        }
    }
}

/// Single-use launcher that can only submit an executor-issued process token
/// to the durable owned-process service.
pub struct SandboxedStdioLauncher {
    token: Option<PreparedCommandToken>,
    service: Arc<dyn OwnedStdioProcessService>,
    environment: Option<OwnedStdioEnvironment>,
    scanners: Vec<SensitiveDataScanner>,
}

impl SandboxedStdioLauncher {
    pub fn new(token: PreparedCommandToken, service: Arc<dyn OwnedStdioProcessService>) -> Self {
        Self {
            token: Some(token),
            service,
            environment: Some(OwnedStdioEnvironment {
                values: BTreeMap::new(),
            }),
            scanners: Vec::new(),
        }
    }

    fn with_environment(
        token: PreparedCommandToken,
        service: Arc<dyn OwnedStdioProcessService>,
        environment: OwnedStdioEnvironment,
    ) -> Self {
        Self {
            token: Some(token),
            service,
            environment: Some(environment),
            scanners: Vec::new(),
        }
    }

    pub fn add_scanner(&mut self, scanner: SensitiveDataScanner) {
        self.scanners.push(scanner);
    }

    async fn launch(
        &mut self,
        limits: TransportLimits,
    ) -> Result<OwnedStdioProcessLaunch, TransportError> {
        let token = self
            .token
            .take()
            .ok_or(TransportError::AuthorizationMismatch)?;
        let process_identity = token.stdio_identity();
        let environment = self
            .environment
            .take()
            .ok_or(TransportError::AuthorizationMismatch)?;
        let limits = OwnedStdioLimits {
            max_frame_bytes: limits.max_json_bytes(),
            io_timeout: limits.request_timeout(),
            close_timeout: limits.close_timeout(),
        };
        let launched = self.service.launch(token, environment, limits);
        tokio::pin!(launched);
        let outcome = tokio::select! {
            result = &mut launched => Some(result),
            _ = tokio::time::sleep(limits.io_timeout()) => None,
        };
        match outcome {
            Some(Ok(mut process)) => {
                process.scanners.append(&mut self.scanners);
                Ok(process)
            }
            None => {
                let primary = cleanup_launch_error(
                    Arc::clone(&self.service),
                    &process_identity,
                    limits.close_timeout(),
                    TransportError::Timeout("owned stdio launch"),
                )
                .await;
                match launched.await {
                    Ok(process) => {
                        Err(
                            cleanup_process_error(process.process, limits.close_timeout(), primary)
                                .await,
                        )
                    }
                    Err(_) => Err(primary),
                }
            }
            Some(Err(OwnedStdioLaunchError::Unavailable)) => Err(cleanup_launch_error(
                Arc::clone(&self.service),
                &process_identity,
                limits.close_timeout(),
                TransportError::OwnedProcessUnavailable,
            )
            .await),
            Some(Err(OwnedStdioLaunchError::Io(error))) => Err(cleanup_launch_error(
                Arc::clone(&self.service),
                &process_identity,
                limits.close_timeout(),
                TransportError::Io(error),
            )
            .await),
        }
    }
}

pub(crate) async fn connect_stdio_with_handler<F>(
    server_id: McpServerId,
    profile: &str,
    request: &BrokerInvocation<'_>,
    prepare: F,
    store: &mut SqliteStore,
    limits: TransportLimits,
    handler: McpHandlerConfig,
) -> Result<ReadyConnection, TransportError>
where
    F: FnOnce() -> Result<SandboxedStdioLauncher, OwnedStdioProfileError>,
{
    super::validate_initialize_arguments(request)?;
    let operation = TransportOperation::parse("initialize")?;
    let binding = TransportBinding::new(request, server_id.to_string(), "stdio", profile, None);
    let authorization = transport_auth::authorize(request, &operation, &binding, store)?;
    let operations = OperationGate::new();
    operations.set_binding(binding.clone())?;
    let generation = operations.install(authorization)?;
    let dispatch = match transport_auth::begin_dispatch(request, &operation, &binding, false, store)
    {
        Ok(dispatch) => dispatch,
        Err(error) => {
            operations.clear_generation(generation)?;
            return Err(error.into());
        }
    };
    let result = match prepare() {
        Ok(mut launcher) => {
            connect_stdio_authorized(
                server_id,
                request,
                &mut launcher,
                limits,
                Arc::clone(&operations),
                handler,
            )
            .await
        }
        Err(_) => Err(TransportError::OwnedProcessUnavailable),
    };
    let persisted = transport_auth::finish_dispatch(
        request,
        dispatch,
        if result.is_ok() {
            transport_auth::TransportDispatchOutcome::Completed
        } else {
            transport_auth::TransportDispatchOutcome::OutcomeUnknown
        },
        store,
    );
    let failure = result
        .as_ref()
        .err()
        .and_then(|_| operations.take_failure());
    let cleared = operations.clear_generation(generation);
    persisted?;
    cleared?;
    match failure {
        Some(error) => Err(error),
        None => result.map(|connection| connection.with_lifecycle_authority(request)),
    }
}

pub async fn connect_stdio<F>(
    server_id: McpServerId,
    profile: &str,
    request: &BrokerInvocation<'_>,
    prepare: F,
    store: &mut SqliteStore,
    limits: TransportLimits,
) -> Result<ReadyConnection, TransportError>
where
    F: FnOnce() -> Result<SandboxedStdioLauncher, OwnedStdioProfileError>,
{
    connect_stdio_with_handler(
        server_id,
        profile,
        request,
        prepare,
        store,
        limits,
        McpHandlerConfig::new().with_events_capacity(limits.channel_capacity()),
    )
    .await
}

async fn connect_stdio_authorized(
    server_id: McpServerId,
    request: &BrokerInvocation<'_>,
    launcher: &mut SandboxedStdioLauncher,
    limits: TransportLimits,
    operations: Arc<OperationGate>,
    handler: McpHandlerConfig,
) -> Result<ReadyConnection, TransportError> {
    let launch = launcher.launch(limits).await?;
    let cleanup = Arc::clone(&launch.process);
    let result = connect_owned_transport(
        server_id,
        launch.process,
        launch.scanners,
        limits,
        operations,
        handler,
    )
    .await;
    if request.cancelled() {
        return Err(cleanup_process_error(
            cleanup,
            limits.close_timeout(),
            TransportError::Cancelled,
        )
        .await);
    }
    result
}

async fn connect_owned_transport(
    server_id: McpServerId,
    process: Arc<dyn OwnedStdioProcess>,
    scanners: Vec<SensitiveDataScanner>,
    limits: TransportLimits,
    operations: Arc<OperationGate>,
    handler: McpHandlerConfig,
) -> Result<ReadyConnection, TransportError> {
    let cleanup_process = Arc::clone(&process);
    let ready_reaper = Arc::clone(&process);
    let result = async {
        let configured_server = ConfiguredServerIdentity::new(server_id.to_string())?;
        let transport = BoundedStdioTransport::new(
            process,
            scanners,
            limits,
            Arc::clone(&operations),
            server_id.clone(),
        );
        let result =
            McpConnection::connect_kit_authorized_transport(server_id, transport, handler).await;
        let connection = match result {
            Ok(connection) => connection,
            Err(error) => return Err(operations.take_failure().unwrap_or_else(|| error.into())),
        };
        let authorization = operations.current_authorization()?;
        operations.bind_connection(authorization)?;
        ReadyConnection::new(
            connection,
            configured_server,
            limits,
            operations,
            None,
            Some(ready_reaper),
            false,
        )
    }
    .await;
    match result {
        Ok(connection) => Ok(connection),
        Err(primary) => {
            Err(cleanup_process_error(cleanup_process, limits.close_timeout(), primary).await)
        }
    }
}

async fn cleanup_launch_error(
    service: Arc<dyn OwnedStdioProcessService>,
    process_identity: &str,
    timeout: Duration,
    primary: TransportError,
) -> TransportError {
    match tokio::time::timeout(timeout, service.abort_and_reap(process_identity)).await {
        Ok(Ok(())) => primary,
        Ok(Err(cleanup)) => TransportError::Cleanup {
            primary: Box::new(primary),
            cleanup,
        },
        Err(_) => TransportError::Cleanup {
            primary: Box::new(primary),
            cleanup: io::Error::new(io::ErrorKind::TimedOut, "MCP stdio abort timed out"),
        },
    }
}

async fn cleanup_process_error(
    process: Arc<dyn OwnedStdioProcess>,
    timeout: Duration,
    primary: TransportError,
) -> TransportError {
    match tokio::time::timeout(timeout, process.close_and_reap()).await {
        Ok(Ok(())) => primary,
        Ok(Err(cleanup)) => TransportError::Cleanup {
            primary: Box::new(primary),
            cleanup,
        },
        Err(_) => TransportError::Cleanup {
            primary: Box::new(primary),
            cleanup: io::Error::new(io::ErrorKind::TimedOut, "MCP stdio cleanup timed out"),
        },
    }
}

struct BoundedStdioTransport {
    process: Arc<dyn OwnedStdioProcess>,
    max_frame_bytes: usize,
    io_timeout: Duration,
    close_timeout: Duration,
    scanners: Vec<SensitiveDataScanner>,
    operations: Arc<OperationGate>,
    server_id: McpServerId,
}

impl BoundedStdioTransport {
    fn new(
        process: Arc<dyn OwnedStdioProcess>,
        scanners: Vec<SensitiveDataScanner>,
        limits: TransportLimits,
        operations: Arc<OperationGate>,
        server_id: McpServerId,
    ) -> Self {
        Self {
            process,
            max_frame_bytes: limits.max_json_bytes(),
            io_timeout: limits.request_timeout(),
            close_timeout: limits.close_timeout(),
            scanners,
            operations,
            server_id,
        }
    }
}

impl Transport<RoleClient> for BoundedStdioTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let operations = Arc::clone(&self.operations);
        let max = self.max_frame_bytes;
        let timeout = self.io_timeout;
        let process = Arc::clone(&self.process);
        async move {
            operations
                .authorize_message(&item)
                .map(|_| ())
                .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
            let frame = serde_json::to_vec(&item).map_err(invalid_data)?;
            if frame.len() > max {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "outbound MCP stdio frame exceeds bound",
                ));
            }
            tokio::time::timeout(timeout, process.send_frame(&frame))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "MCP stdio write timed out"))?
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let frame = match tokio::time::timeout(self.io_timeout, self.process.receive_frame()).await
        {
            Ok(Ok(Some(frame))) if frame.len() <= self.max_frame_bytes => frame,
            Ok(Ok(Some(_))) => {
                self.operations.fail(TransportFailure::ResponseTooLarge);
                return None;
            }
            Ok(Ok(None)) => return None,
            Ok(Err(error)) => {
                self.operations
                    .fail(TransportFailure::StdioParse(error.to_string()));
                return None;
            }
            Err(_) => {
                self.operations.fail(TransportFailure::StdioTimeout);
                return None;
            }
        };
        for scanner in &mut self.scanners {
            scanner.push(&frame);
        }
        if self.scanners.iter().any(SensitiveDataScanner::found) {
            self.operations.fail(TransportFailure::SensitivePayload);
            return None;
        }
        let payload = match RawPayload::parse(
            &frame,
            crate::protocols::mcp::features::PayloadLimits::with_max_bytes(self.max_frame_bytes),
        ) {
            Ok(payload) => payload,
            Err(error) => {
                self.operations.fail(TransportFailure::Payload(error));
                return None;
            }
        };
        let value = payload.value();
        if value
            .get("result")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|result| {
                result.contains_key("capabilities")
                    && result.contains_key("serverInfo")
                    && !result.contains_key("protocolVersion")
            })
        {
            self.operations
                .fail(TransportFailure::MissingProtocolVersion(
                    self.server_id.clone(),
                ));
            return None;
        }
        if (value.get("result").is_some() || super::is_terminal_url_elicitation(&payload))
            && let Err(error) = self.operations.capture_payload(payload.clone())
        {
            self.operations
                .fail(TransportFailure::StdioParse(error.to_string()));
            return None;
        }
        match serde_json::from_value(value.clone()) {
            Ok(message) => Some(message),
            Err(error) => {
                self.operations
                    .fail(TransportFailure::StdioParse(error.to_string()));
                None
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        tokio::time::timeout(self.close_timeout, self.process.close_and_reap())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "MCP stdio close timed out"))?
    }
}

fn invalid_data(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::broker::transport_auth::{TransportAuthorization, TransportOperation};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn stdio_environment_rejects_a_handle_outside_lifecycle_authority() {
        let allowed = crate::domain::secret::SecretHandle::parse("env:ALLOWED").unwrap();
        let forbidden = crate::domain::secret::SecretHandle::parse("env:FORBIDDEN").unwrap();
        assert!(matches!(
            OwnedStdioEnvironment::new(
                [("TOKEN".to_owned(), forbidden)],
                &Arc::new(BTreeMap::from([(
                    allowed,
                    Arc::new(SecretLease::new(b"allowed".to_vec())),
                )])),
            ),
            Err(OwnedStdioProfileError::Invalid)
        ));
    }

    #[test]
    fn stdio_environment_accepts_multiple_authorized_opaque_handles() {
        let first = crate::domain::secret::SecretHandle::parse("env:FIRST").unwrap();
        let second = crate::domain::secret::SecretHandle::parse("env:SECOND").unwrap();
        let environment = OwnedStdioEnvironment::new(
            [
                ("FIRST_TOKEN".to_owned(), first.clone()),
                ("SECOND_TOKEN".to_owned(), second.clone()),
            ],
            &Arc::new(BTreeMap::from([
                (first, Arc::new(SecretLease::new(b"first-secret".to_vec()))),
                (
                    second,
                    Arc::new(SecretLease::new(b"second-secret".to_vec())),
                ),
            ])),
        )
        .unwrap();
        assert_eq!(environment.values.len(), 2);
    }

    struct MissingVersionProcess {
        response: tokio::sync::Mutex<Option<Vec<u8>>>,
        closes: Arc<AtomicUsize>,
    }

    struct FrameProcess {
        response: tokio::sync::Mutex<Option<Vec<u8>>>,
    }

    struct ConcurrentProcess {
        response: tokio::sync::Mutex<Option<Vec<u8>>>,
        ready: tokio::sync::Notify,
        closed: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl OwnedStdioProcess for ConcurrentProcess {
        async fn send_frame(&self, frame: &[u8]) -> io::Result<()> {
            *self.response.lock().await = Some(frame.to_vec());
            self.ready.notify_waiters();
            Ok(())
        }

        async fn receive_frame(&self) -> io::Result<Option<Vec<u8>>> {
            loop {
                if let Some(response) = self.response.lock().await.take() {
                    return Ok(Some(response));
                }
                if self.closed.load(Ordering::Acquire) != 0 {
                    return Ok(None);
                }
                self.ready.notified().await;
            }
        }

        async fn close_and_reap(&self) -> io::Result<()> {
            self.closed.store(1, Ordering::Release);
            self.ready.notify_waiters();
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl OwnedStdioProcess for FrameProcess {
        async fn send_frame(&self, _frame: &[u8]) -> io::Result<()> {
            Ok(())
        }

        async fn receive_frame(&self) -> io::Result<Option<Vec<u8>>> {
            Ok(self.response.lock().await.take())
        }

        async fn close_and_reap(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl OwnedStdioProcess for MissingVersionProcess {
        async fn send_frame(&self, frame: &[u8]) -> io::Result<()> {
            let request: serde_json::Value = serde_json::from_slice(frame).map_err(invalid_data)?;
            if request.get("method").and_then(serde_json::Value::as_str) == Some("initialize") {
                *self.response.lock().await = Some(
                    serde_json::to_vec(&serde_json::json!({
                        "jsonrpc":"2.0",
                        "id":request["id"],
                        "result":{
                            "capabilities":{},
                            "serverInfo":{"name":"missing-version","version":"1"}
                        }
                    }))
                    .unwrap(),
                );
            }
            Ok(())
        }

        async fn receive_frame(&self) -> io::Result<Option<Vec<u8>>> {
            Ok(self.response.lock().await.take())
        }

        async fn close_and_reap(&self) -> io::Result<()> {
            self.closes.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    #[tokio::test]
    async fn blocked_receive_does_not_block_request_send() {
        let process = Arc::new(ConcurrentProcess {
            response: tokio::sync::Mutex::new(None),
            ready: tokio::sync::Notify::new(),
            closed: AtomicUsize::new(0),
        });
        let receiver = {
            let process = Arc::clone(&process);
            tokio::spawn(async move { process.receive_frame().await })
        };
        tokio::task::yield_now().await;
        process.send_frame(b"request").await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), receiver)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Some(b"request".to_vec())
        );
    }

    #[tokio::test]
    async fn blocked_receive_does_not_block_cancel_close() {
        let process = Arc::new(ConcurrentProcess {
            response: tokio::sync::Mutex::new(None),
            ready: tokio::sync::Notify::new(),
            closed: AtomicUsize::new(0),
        });
        let receiver = {
            let process = Arc::clone(&process);
            tokio::spawn(async move { process.receive_frame().await })
        };
        tokio::task::yield_now().await;
        tokio::time::timeout(Duration::from_secs(1), process.close_and_reap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), receiver)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn production_adapter_preserves_missing_version_as_typed_refusal() {
        let server_id = McpServerId::new("missing-version");
        let operations = OperationGate::new();
        let binding =
            TransportBinding::for_test(server_id.to_string(), "stdio", "owned-process-token", None);
        operations.set_binding(binding.clone()).unwrap();
        operations
            .install(TransportAuthorization::for_test_bound_arguments_binding(
                TransportOperation::parse("initialize").unwrap(),
                agentkit_mcp::kit_authorized_initialize_arguments(),
                None,
                None,
                binding,
            ))
            .unwrap();
        let closes = Arc::new(AtomicUsize::new(0));
        let error = match connect_owned_transport(
            server_id,
            Arc::new(MissingVersionProcess {
                response: tokio::sync::Mutex::new(None),
                closes: Arc::clone(&closes),
            }),
            vec![CaptureRedactor::new(&[]).scanner()],
            TransportLimits::default(),
            operations,
            McpHandlerConfig::new(),
        )
        .await
        {
            Ok(_) => panic!("missing protocol version connected"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            TransportError::Agentkit(inner)
                if matches!(*inner, agentkit_mcp::McpError::UnsupportedProtocolVersion { negotiated: None, .. })
        ));
        assert_eq!(closes.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn stdio_rejects_duplicate_feature_payload_before_typed_message_parsing() {
        let operations = OperationGate::new();
        let mut transport = BoundedStdioTransport::new(
            Arc::new(FrameProcess {
                response: tokio::sync::Mutex::new(Some(
                    br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[],"tools":[]}}"#.to_vec(),
                )),
            }),
            vec![CaptureRedactor::new(&[]).scanner()],
            TransportLimits::default(),
            Arc::clone(&operations),
            McpServerId::new("duplicate-feature-payload"),
        );
        assert!(transport.receive().await.is_none());
        assert!(matches!(
            operations.take_failure(),
            Some(TransportError::Payload(
                crate::protocols::mcp::features::PayloadError::DuplicateKey
            ))
        ));
    }

    #[tokio::test]
    async fn stdio_scans_injected_credential_output_before_json_parsing() {
        let operations = OperationGate::new();
        let leases = [crate::domain::secret::SecretLease::new(
            b"stdio-credential-canary".to_vec(),
        )];
        let mut transport = BoundedStdioTransport::new(
            Arc::new(FrameProcess {
                response: tokio::sync::Mutex::new(Some(
                    br#"{"jsonrpc":"2.0","id":1,"result":{"text":"stdio-credential-canary"}}"#
                        .to_vec(),
                )),
            }),
            vec![CaptureRedactor::new(&leases).scanner()],
            TransportLimits::default(),
            Arc::clone(&operations),
            McpServerId::new("credential-reflection"),
        );
        assert!(transport.receive().await.is_none());
        assert!(matches!(
            operations.take_failure(),
            Some(TransportError::SensitivePayload)
        ));
    }
}
