use std::{
    fmt,
    io::{self, BufRead, BufReader, Read, Write},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    domain::{
        ids::{PrincipalId, ProcessId, ProjectId},
        lifecycle::{ProcessClaim, ProcessOwnership},
    },
    executor::{
        process::tree::{
            BoundaryControl, BoundaryPersistence, Containment, LifecycleState, Ownership,
            ProcessTree,
        },
        profile::{ExecutorProfile, ResourceLimits},
        secrets::{
            ExecutorSecretBroker, InjectionBinding, PreparedSecrets, SecretSpawnPlan,
            resolve_for_spawn,
        },
        terminal::{OutputRetention, TerminalRequest, TerminalSize, TerminalTransport},
    },
    telemetry::redact::{
        CaptureBoundary, CapturePersistencePolicy, CaptureRedactor, SanitizedCapture,
    },
};

pub const TRUNCATION_MARKER: &[u8] = b"\n[output truncated]\n";
static QUARANTINED_SECRET_CUSTODIES: AtomicU64 = AtomicU64::new(0);

pub fn quarantined_secret_custodies() -> u64 {
    QUARANTINED_SECRET_CUSTODIES.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct BackendBoundaryToken([u8; 32]);

impl BackendBoundaryToken {
    #[cfg(windows)]
    pub(crate) fn from_hex(value: &str) -> io::Result<Self> {
        parse_token(value).map(Self)
    }
}

impl fmt::Debug for BackendBoundaryToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BackendBoundaryToken(REDACTED)")
    }
}

/// A single-use command issued by an executor backend.
///
/// Construction is crate-private so callers cannot pass an arbitrary host
/// `Command` through the owned-process spawn boundary.
pub struct PreparedCommandToken {
    command: Command,
    claim: ProcessClaim,
    boundary_token: BackendBoundaryToken,
    ownership: Ownership,
    #[cfg(not(windows))]
    boundary: Option<ProcessTree<Box<dyn BoundaryControl>>>,
    #[cfg(windows)]
    boundary: Option<WindowsPreparedBoundary>,
    deadline: Instant,
    secrets: Option<SecretSpawnPlan>,
    observer: Option<ProcessRegistryRegistration>,
    terminal: ProcessTerminalConfig,
    profile: Option<ExecutorProfile>,
}

#[cfg(windows)]
struct WindowsPreparedBoundary {
    control: Box<dyn BoundaryControl>,
    persistence: Box<dyn BoundaryPersistence + Send>,
    register: Box<
        dyn FnMut(
                &ProcessClaim,
                &crate::executor::process::tree::PersistedBoundary,
            ) -> io::Result<()>
            + Send,
    >,
}

impl PreparedCommandToken {
    #[cfg(test)]
    pub(crate) fn issue(
        command: Command,
        owner: ProcessOwnership,
        control: impl BoundaryControl + 'static,
        persistence: impl BoundaryPersistence + Send + 'static,
        deadline: Instant,
        limits: ResourceLimits,
    ) -> io::Result<Self> {
        Self::issue_registered(
            command,
            owner,
            control,
            persistence,
            |_, _| Ok(()),
            deadline,
            limits,
        )
    }

    #[cfg(test)]
    pub(crate) fn issue_registered(
        command: Command,
        owner: ProcessOwnership,
        control: impl BoundaryControl + 'static,
        persistence: impl BoundaryPersistence + Send + 'static,
        register: impl FnMut(
            &ProcessClaim,
            &crate::executor::process::tree::PersistedBoundary,
        ) -> io::Result<()>
        + Send
        + 'static,
        deadline: Instant,
        limits: ResourceLimits,
    ) -> io::Result<Self> {
        Self::issue_observed_registered(
            command,
            owner,
            control,
            persistence,
            register,
            None,
            deadline,
            limits,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn issue_observed_registered(
        command: Command,
        owner: ProcessOwnership,
        control: impl BoundaryControl + 'static,
        mut persistence: impl BoundaryPersistence + Send + 'static,
        mut register: impl FnMut(
            &ProcessClaim,
            &crate::executor::process::tree::PersistedBoundary,
        ) -> io::Result<()>
        + Send
        + 'static,
        observer: Option<ProcessRegistryRegistration>,
        deadline: Instant,
        limits: ResourceLimits,
    ) -> io::Result<Self> {
        validate_spawn_limits(limits, deadline)?;
        if control.containment() != Containment::Complete {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "owned execution requires a complete process boundary",
            ));
        }
        let process_id = ProcessId::generate()
            .map_err(|error| io::Error::other(format!("process ID generation failed: {error}")))?;
        let claim = ProcessClaim::new(process_id, owner);
        let ownership = Ownership::new(
            serde_json::to_string(&owner).map_err(|error| {
                io::Error::other(format!("process owner encoding failed: {error}"))
            })?,
            process_id.to_string(),
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
        let boundary_token =
            BackendBoundaryToken(parse_token(control.identity().ownership_token())?);
        let terminal = observer
            .as_ref()
            .map_or_else(ProcessTerminalConfig::default, |observer| observer.terminal);
        #[cfg(not(windows))]
        let boundary = ProcessTree::start(
            ownership.clone(),
            Box::new(control) as Box<dyn BoundaryControl>,
            |boundary: &crate::executor::process::tree::PersistedBoundary| {
                persistence.persist(boundary)?;
                register(&claim, boundary)?;
                if let Some(observer) = &observer {
                    observer.registry.prepared(
                        observer.context,
                        claim,
                        boundary,
                        observer.terminal,
                    )?;
                }
                Ok(())
            },
            deadline,
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
        #[cfg(windows)]
        let boundary = WindowsPreparedBoundary {
            control: Box::new(control),
            persistence: Box::new(persistence),
            register: Box::new(register),
        };
        Ok(Self {
            command,
            claim,
            boundary_token,
            ownership,
            boundary: Some(boundary),
            deadline,
            secrets: None,
            observer,
            terminal,
            profile: None,
        })
    }

    pub const fn process_id(&self) -> ProcessId {
        self.claim.process_id
    }

    pub const fn owner(&self) -> ProcessOwnership {
        self.claim.owner
    }

    pub const fn boundary_token(&self) -> BackendBoundaryToken {
        self.boundary_token
    }

    pub(crate) fn bind_secrets(mut self, secrets: SecretSpawnPlan) -> Self {
        self.secrets = Some(secrets);
        self
    }

    pub(crate) fn bind_profile(mut self, profile: ExecutorProfile) -> Self {
        self.profile = Some(profile);
        self
    }

    pub(crate) fn stdio_identity(&self) -> String {
        self.claim.process_id.to_string()
    }
}

#[cfg(not(windows))]
pub(crate) struct OwnedStdioChild {
    reader: Mutex<OwnedStdioReader>,
    writer: Arc<Mutex<Option<std::process::ChildStdin>>>,
    lifecycle: Mutex<OwnedStdioLifecycle>,
}

#[cfg(not(windows))]
struct OwnedStdioReader {
    stdout: BufReader<std::process::ChildStdout>,
    max_frame_bytes: usize,
}

#[cfg(not(windows))]
struct OwnedStdioLifecycle {
    child: Child,
    stderr: Option<JoinHandle<()>>,
    record: ProcessRecord,
    ownership: Ownership,
    boundary: ProcessTree<Box<dyn BoundaryControl>>,
    observer: Option<ProcessRegistryRegistration>,
    registry_terminal: bool,
    closed: bool,
}

#[cfg(not(windows))]
struct ChildReapGuard(Option<Child>);

#[cfg(not(windows))]
impl Drop for ChildReapGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(windows)]
pub(crate) struct OwnedStdioChild;

#[cfg(not(windows))]
impl OwnedStdioChild {
    pub(crate) fn spawn_with_environment(
        mut prepared: PreparedCommandToken,
        max_frame_bytes: usize,
        environment: &[(String, crate::domain::secret::SecretLease)],
    ) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            for (variable, lease) in environment {
                prepared
                    .command
                    .env(variable, std::ffi::OsStr::from_bytes(lease.expose()));
            }
        }
        #[cfg(windows)]
        for (variable, lease) in environment {
            let value = std::str::from_utf8(lease.expose()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid MCP stdio environment value",
                )
            })?;
            prepared.command.env(variable, value);
        }
        prepared
            .command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ChildReapGuard(Some(prepared.command.spawn()?));
        prepared.command.env_clear();
        let stdin = child
            .0
            .as_mut()
            .expect("spawned child exists")
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("MCP stdio stdin unavailable"))?;
        let stdout = child
            .0
            .as_mut()
            .expect("spawned child exists")
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("MCP stdio stdout unavailable"))?;
        let mut stderr = child
            .0
            .as_mut()
            .expect("spawned child exists")
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("MCP stdio stderr unavailable"))?;
        let stderr = thread::Builder::new()
            .name("kit-mcp-stderr".to_owned())
            .spawn(move || {
                let mut buffer = [0_u8; 4096];
                while stderr.read(&mut buffer).is_ok_and(|read| read != 0) {}
                buffer.fill(0);
            })?;
        let record = ProcessRecord {
            claim: prepared.claim,
            execution_id: child.0.as_ref().expect("spawned child exists").id(),
            boundary_token: prepared.boundary_token,
            state: ProcessState::Started,
        };
        if let Some(observer) = &prepared.observer {
            observer.registry.started(observer.context, &record)?;
        }
        let child = child.0.take().expect("spawned child exists");
        Ok(Self {
            reader: Mutex::new(OwnedStdioReader {
                stdout: BufReader::new(stdout),
                max_frame_bytes,
            }),
            writer: Arc::new(Mutex::new(Some(stdin))),
            lifecycle: Mutex::new(OwnedStdioLifecycle {
                child,
                stderr: Some(stderr),
                record,
                ownership: prepared.ownership.clone(),
                boundary: prepared.boundary.take().expect("prepared boundary exists"),
                observer: prepared.observer.take(),
                registry_terminal: false,
                closed: false,
            }),
        })
    }

    pub(crate) fn send_frame(&self, frame: &[u8]) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("MCP stdio writer lock poisoned"))?;
        let stdin = writer.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "MCP stdio process is closed")
        })?;
        stdin.write_all(frame)?;
        stdin.write_all(b"\n")?;
        stdin.flush()
    }

    pub(crate) fn receive_frame(&self) -> io::Result<Option<Vec<u8>>> {
        let mut reader = self
            .reader
            .lock()
            .map_err(|_| io::Error::other("MCP stdio reader lock poisoned"))?;
        let mut frame = Vec::new();
        let max_frame_bytes = reader.max_frame_bytes;
        let read = reader
            .stdout
            .by_ref()
            .take(
                u64::try_from(max_frame_bytes)
                    .unwrap_or(u64::MAX)
                    .saturating_add(2),
            )
            .read_until(b'\n', &mut frame)?;
        if read == 0 {
            return Ok(None);
        }
        if frame.last() == Some(&b'\n') {
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
        }
        if frame.len() > max_frame_bytes {
            frame.fill(0);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP stdio frame exceeds bound",
            ));
        }
        Ok(Some(frame))
    }

    pub(crate) fn close_and_reap(&self) -> io::Result<()> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| io::Error::other("MCP stdio lifecycle lock poisoned"))?;
        if lifecycle.closed {
            return Ok(());
        }
        self.writer
            .lock()
            .map_err(|_| io::Error::other("MCP stdio writer lock poisoned"))?
            .take();
        let deadline = cleanup_deadline();
        let ownership = lifecycle.ownership.clone();
        let mut error = lifecycle
            .boundary
            .cancel(&ownership, deadline)
            .err()
            .map(|error| io::Error::other(error.to_string()));
        if let Err(kill) = lifecycle.child.kill()
            && kill.kind() != io::ErrorKind::InvalidInput
        {
            error.get_or_insert(kill);
        }
        match lifecycle.child.wait() {
            Ok(status) => lifecycle.record.state = exit_state(status),
            Err(wait) => {
                error.get_or_insert(wait);
            }
        }
        if let Some(stderr) = lifecycle.stderr.take()
            && stderr.join().is_err()
        {
            error.get_or_insert_with(|| io::Error::other("MCP stderr drain panicked"));
        }
        if !lifecycle.registry_terminal
            && let Some(observer) = &lifecycle.observer
        {
            let result = if error.is_none() {
                observer
                    .registry
                    .exited(observer.context, &lifecycle.record)
            } else {
                observer
                    .registry
                    .outcome_unknown(observer.context, lifecycle.record.process_id())
            };
            lifecycle.registry_terminal = result.is_ok();
            if let Err(registry) = result {
                error.get_or_insert(registry);
            }
        }
        if error.is_none() {
            lifecycle.closed = true;
        }
        error.map_or(Ok(()), Err)
    }
}

#[cfg(not(windows))]
impl Drop for OwnedStdioChild {
    fn drop(&mut self) {
        let _ = self.close_and_reap();
    }
}

impl fmt::Debug for PreparedCommandToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedCommandToken")
            .field("claim", &self.claim)
            .field("boundary_token", &self.boundary_token)
            .field("deadline", &self.deadline)
            .field("secrets", &self.secrets.as_ref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}

impl Drop for PreparedCommandToken {
    fn drop(&mut self) {
        #[cfg(not(windows))]
        if let Some(boundary) = self.boundary.as_mut() {
            let _ = boundary.cancel(&self.ownership, cleanup_deadline());
            if let Some(observer) = &self.observer {
                let _ = observer
                    .registry
                    .outcome_unknown(observer.context, self.claim.process_id);
            }
        }
        #[cfg(windows)]
        if let Some(boundary) = self.boundary.as_mut() {
            let deadline = cleanup_deadline();
            let _ = boundary.control.kill_boundary(deadline);
            let _ = boundary.control.wait_and_reap(deadline);
            let _ = boundary.control.inspect(deadline);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessState {
    Started,
    Exited {
        success: bool,
        code: Option<i32>,
        signal: Option<i32>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessRecord {
    claim: ProcessClaim,
    execution_id: u32,
    boundary_token: BackendBoundaryToken,
    state: ProcessState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessRegistrationContext {
    pub project_id: ProjectId,
    pub principal_id: PrincipalId,
}

#[derive(Clone)]
pub struct ProcessRegistryRegistration {
    pub registry: Arc<dyn ProcessRegistry>,
    pub context: ProcessRegistrationContext,
    terminal: ProcessTerminalConfig,
    custody: crate::domain::secret::SecretCustody,
}

impl ProcessRegistryRegistration {
    pub fn new(registry: Arc<dyn ProcessRegistry>, context: ProcessRegistrationContext) -> Self {
        Self {
            registry,
            context,
            terminal: ProcessTerminalConfig::default(),
            custody: crate::domain::secret::SecretCustody::default(),
        }
    }

    pub fn with_custody(mut self, custody: crate::domain::secret::SecretCustody) -> Self {
        self.custody = custody;
        self
    }

    pub fn with_pty(
        mut self,
        size: TerminalSize,
        retention: OutputRetention,
        capture_policy: CapturePersistencePolicy,
    ) -> Self {
        self.terminal = ProcessTerminalConfig {
            request: TerminalRequest::pty(capture_policy),
            size,
            retention,
        };
        self
    }

    pub const fn terminal_transport(&self) -> TerminalTransport {
        self.terminal.request.transport
    }

    #[cfg(windows)]
    pub(crate) const fn terminal_config(&self) -> ProcessTerminalConfig {
        self.terminal
    }

    #[cfg(windows)]
    pub(crate) fn prepare_conpty(
        &self,
        claim: ProcessClaim,
        boundary_id: &str,
    ) -> io::Result<crate::executor::terminal::ConPtyBinding> {
        self.registry
            .prepare_conpty(self.context, claim, boundary_id, self.terminal)
    }

    #[cfg(windows)]
    pub(crate) fn abort_conpty(&self, process_id: ProcessId) {
        let _ = self.registry.abort_conpty(self.context, process_id);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessTerminalConfig {
    pub request: TerminalRequest,
    pub size: TerminalSize,
    pub retention: OutputRetention,
}

impl Default for ProcessTerminalConfig {
    fn default() -> Self {
        Self {
            request: TerminalRequest::default(),
            size: TerminalSize {
                columns: 80,
                rows: 24,
            },
            retention: OutputRetention::new(1024 * 1024, 24 * 60 * 60 * 1_000),
        }
    }
}

/// Observer for the owned-process lifecycle. `prepared` runs after durable boundary
/// persistence and before the child boundary can be released.
pub trait ProcessRegistry: Send + Sync + 'static {
    fn prepared(
        &self,
        context: ProcessRegistrationContext,
        claim: ProcessClaim,
        boundary: &crate::executor::process::tree::PersistedBoundary,
        terminal: ProcessTerminalConfig,
    ) -> io::Result<()>;
    fn bind_terminal(
        &self,
        _context: ProcessRegistrationContext,
        _process_id: ProcessId,
        _command: &mut Command,
    ) -> io::Result<Box<dyn Read + Send>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "registered process does not provide a native PTY binding",
        ))
    }
    #[cfg(windows)]
    fn prepare_conpty(
        &self,
        _context: ProcessRegistrationContext,
        _claim: ProcessClaim,
        _boundary_id: &str,
        _terminal: ProcessTerminalConfig,
    ) -> io::Result<crate::executor::terminal::ConPtyBinding> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "registered process does not provide a ConPTY binding",
        ))
    }
    #[cfg(windows)]
    fn abort_conpty(
        &self,
        _context: ProcessRegistrationContext,
        _process_id: ProcessId,
    ) -> io::Result<()> {
        Ok(())
    }
    fn append_terminal_output(
        &self,
        _context: ProcessRegistrationContext,
        _process_id: ProcessId,
        _capture: &SanitizedCapture,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "registered process does not provide terminal output",
        ))
    }
    fn set_terminal_capture_policy(
        &self,
        _context: ProcessRegistrationContext,
        _process_id: ProcessId,
        _capture_policy: CapturePersistencePolicy,
    ) -> io::Result<()> {
        Ok(())
    }
    fn close_terminal(
        &self,
        _context: ProcessRegistrationContext,
        _process_id: ProcessId,
    ) -> io::Result<()> {
        Ok(())
    }
    fn started(
        &self,
        context: ProcessRegistrationContext,
        record: &ProcessRecord,
    ) -> io::Result<()>;
    fn exited(&self, context: ProcessRegistrationContext, record: &ProcessRecord)
    -> io::Result<()>;
    fn outcome_unknown(
        &self,
        context: ProcessRegistrationContext,
        process_id: ProcessId,
    ) -> io::Result<()>;
}

impl ProcessRecord {
    #[cfg(windows)]
    pub(crate) const fn started(
        claim: ProcessClaim,
        execution_id: u32,
        boundary_token: BackendBoundaryToken,
    ) -> Self {
        Self {
            claim,
            execution_id,
            boundary_token,
            state: ProcessState::Started,
        }
    }

    #[cfg(windows)]
    pub(crate) fn exited_on_windows(&mut self, code: u32) {
        self.state = ProcessState::Exited {
            success: code == 0,
            code: i32::try_from(code).ok(),
            signal: None,
        };
    }

    pub const fn process_id(&self) -> ProcessId {
        self.claim.process_id
    }

    pub const fn owner(&self) -> ProcessOwnership {
        self.claim.owner
    }

    pub const fn execution_id(&self) -> u32 {
        self.execution_id
    }

    pub const fn boundary_token(&self) -> BackendBoundaryToken {
        self.boundary_token
    }

    pub const fn state(&self) -> ProcessState {
        self.state
    }
}

#[derive(Eq, PartialEq)]
pub struct CapturedStream {
    bytes: Vec<u8>,
    original_bytes: u64,
    truncated_bytes: u64,
}

impl CapturedStream {
    pub(crate) fn raw_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn sanitize(
        &self,
        redactor: &CaptureRedactor<'_>,
        boundary: CaptureBoundary,
    ) -> SanitizedCapture {
        redactor.sanitize(boundary, &self.bytes)
    }

    pub const fn original_bytes(&self) -> u64 {
        self.original_bytes
    }

    pub const fn truncated_bytes(&self) -> u64 {
        self.truncated_bytes
    }

    pub const fn was_truncated(&self) -> bool {
        self.truncated_bytes != 0
    }
}

impl fmt::Debug for CapturedStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedStream")
            .field("retained_bytes", &self.bytes.len())
            .field("original_bytes", &self.original_bytes)
            .field("truncated_bytes", &self.truncated_bytes)
            .finish()
    }
}

impl Drop for CapturedStream {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

#[derive(Eq, PartialEq)]
pub struct ProcessOutput {
    pub stdout: CapturedStream,
    pub stderr: CapturedStream,
}

impl fmt::Debug for ProcessOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessOutput")
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .finish()
    }
}

impl ProcessOutput {
    pub fn retained_bytes(&self) -> usize {
        self.stdout.bytes.len() + self.stderr.bytes.len()
    }

    pub const fn original_bytes(&self) -> u64 {
        self.stdout.original_bytes + self.stderr.original_bytes
    }

    pub const fn truncated_bytes(&self) -> u64 {
        self.stdout.truncated_bytes + self.stderr.truncated_bytes
    }
}

#[cfg(not(windows))]
pub struct OwnedProcess {
    child: Child,
    record: ProcessRecord,
    capture: Option<CaptureThreads>,
    ownership: Ownership,
    boundary: ProcessTree<Box<dyn BoundaryControl>>,
    deadline: Instant,
    custody: Option<PreparedSecrets>,
    observer: Option<ProcessRegistryRegistration>,
    registry_terminal: bool,
}

#[cfg(not(windows))]
impl OwnedProcess {
    pub const fn record(&self) -> &ProcessRecord {
        &self.record
    }

    pub fn secret_bindings(&self) -> &[InjectionBinding] {
        self.custody.as_ref().map_or(&[], PreparedSecrets::bindings)
    }

    pub fn sanitize_capture(&self, boundary: CaptureBoundary, value: &[u8]) -> SanitizedCapture {
        self.custody.as_ref().map_or_else(
            || CaptureRedactor::new(&[]).sanitize(boundary, value),
            |custody| custody.sanitize(boundary, value),
        )
    }

    pub fn start_sanitized_capture(&self, boundary: CaptureBoundary) -> SanitizedCapture {
        self.custody.as_ref().map_or_else(
            || CaptureRedactor::new(&[]).start(boundary),
            |custody| custody.start_capture(boundary),
        )
    }

    pub fn capture_persistence_policy(&self) -> CapturePersistencePolicy {
        self.custody.as_ref().map_or_else(
            CapturePersistencePolicy::no_secrets,
            PreparedSecrets::capture_policy,
        )
    }

    pub fn wait(&mut self) -> io::Result<ProcessOutput> {
        if matches!(self.record.state, ProcessState::Exited { .. }) {
            return Err(io::Error::other("process was already waited"));
        }
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            if Instant::now() >= self.deadline {
                let cleanup = self.terminate(cleanup_deadline());
                return Err(cleanup.err().unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "process exceeded its wall-time limit",
                    )
                }));
            }
            thread::sleep(Duration::from_millis(2));
        };
        self.record.state = exit_state(status);
        if let Err(error) = self.boundary.finish(&self.ownership, self.deadline) {
            let _ = self.terminate(cleanup_deadline());
            return Err(io::Error::other(error.to_string()));
        }
        let output = self
            .capture
            .take()
            .ok_or_else(|| io::Error::other("process output was already collected"))?
            .finish_by(self.deadline);
        self.finalize_registry(true)?;
        output
    }

    fn terminate(&mut self, deadline: Instant) -> io::Result<()> {
        let mut cleanup_error = self
            .boundary
            .cancel(&self.ownership, deadline)
            .err()
            .map(|error| io::Error::other(error.to_string()));
        if !matches!(self.record.state, ProcessState::Exited { .. }) {
            match self.child.kill() {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
                Err(error) => {
                    cleanup_error.get_or_insert(error);
                }
            }
            loop {
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        self.record.state = exit_state(status);
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        cleanup_error.get_or_insert(error);
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    cleanup_error.get_or_insert_with(|| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            "direct child could not be reaped before cleanup deadline",
                        )
                    });
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
        let registry = self.finalize_registry(cleanup_error.is_none());
        match (cleanup_error, registry) {
            (Some(error), _) => Err(error),
            (None, result) => result,
        }
    }

    fn finalize_registry(&mut self, outcome_known: bool) -> io::Result<()> {
        if self.registry_terminal {
            return Ok(());
        }
        let Some(observer) = &self.observer else {
            self.registry_terminal = true;
            return Ok(());
        };
        let process_id = self.record.process_id();
        let result = if outcome_known && matches!(self.record.state, ProcessState::Exited { .. }) {
            observer.registry.exited(observer.context, &self.record)
        } else {
            observer
                .registry
                .outcome_unknown(observer.context, process_id)
        };
        if let Err(error) = result {
            if outcome_known {
                let unknown = observer
                    .registry
                    .outcome_unknown(observer.context, process_id);
                self.registry_terminal = unknown.is_ok();
                return Err(error);
            }
            return Err(error);
        }
        self.registry_terminal = true;
        Ok(())
    }
}

#[cfg(not(windows))]
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if !matches!(self.boundary.state(), LifecycleState::Quiescent)
            || !matches!(self.record.state, ProcessState::Exited { .. })
        {
            let _ = self.terminate(cleanup_deadline());
        }
        if let Some(capture) = self.capture.take() {
            capture.cancel();
        }
        settle_custody(&mut self.custody, self.boundary.state());
        let _ = self.finalize_registry(
            matches!(self.boundary.state(), LifecycleState::Quiescent)
                && matches!(self.record.state, ProcessState::Exited { .. }),
        );
    }
}

#[cfg(windows)]
pub struct OwnedProcess {
    job: crate::executor::backends::windows_job::Job,
    process: crate::executor::backends::windows_job::JobProcess,
    secondary: Box<dyn BoundaryControl>,
    record: ProcessRecord,
    capture: Option<CaptureThreads>,
    deadline: Instant,
    custody: Option<PreparedSecrets>,
    observer: Option<ProcessRegistryRegistration>,
    registry_terminal: bool,
    quiescent: bool,
    job_retired: bool,
}

#[cfg(windows)]
impl OwnedProcess {
    pub const fn record(&self) -> &ProcessRecord {
        &self.record
    }

    pub fn secret_bindings(&self) -> &[InjectionBinding] {
        self.custody.as_ref().map_or(&[], PreparedSecrets::bindings)
    }

    pub fn sanitize_capture(&self, boundary: CaptureBoundary, value: &[u8]) -> SanitizedCapture {
        self.custody.as_ref().map_or_else(
            || CaptureRedactor::new(&[]).sanitize(boundary, value),
            |custody| custody.sanitize(boundary, value),
        )
    }

    pub fn start_sanitized_capture(&self, boundary: CaptureBoundary) -> SanitizedCapture {
        self.custody.as_ref().map_or_else(
            || CaptureRedactor::new(&[]).start(boundary),
            |custody| custody.start_capture(boundary),
        )
    }

    pub fn capture_persistence_policy(&self) -> CapturePersistencePolicy {
        self.custody.as_ref().map_or_else(
            CapturePersistencePolicy::no_secrets,
            PreparedSecrets::capture_policy,
        )
    }

    pub fn wait(&mut self) -> io::Result<ProcessOutput> {
        if matches!(self.record.state, ProcessState::Exited { .. }) {
            return Err(io::Error::other("process was already waited"));
        }
        let status = match self.job.wait(&self.process, self.deadline) {
            Ok(code) => code,
            Err(crate::executor::backends::windows_job::JobError::Io(error))
                if error.kind() == io::ErrorKind::TimedOut =>
            {
                self.terminate(cleanup_deadline())?;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process exceeded its wall-time limit",
                ));
            }
            Err(error) => {
                let cleanup = self.terminate(cleanup_deadline());
                return Err(cleanup
                    .err()
                    .unwrap_or_else(|| io::Error::other(error.to_string())));
            }
        };
        self.record.exited_on_windows(status);
        if let Err(error) = self.finish_boundaries(self.deadline) {
            let cleanup = self.terminate(cleanup_deadline());
            return Err(cleanup.err().unwrap_or(error));
        }
        self.process.close_conpty();
        let output = match self
            .capture
            .take()
            .ok_or_else(|| io::Error::other("process output was already collected"))?
            .finish_by(self.deadline)
        {
            Ok(output) => output,
            Err(error) => {
                let cleanup = self.terminate(cleanup_deadline());
                return Err(cleanup.err().unwrap_or(error));
            }
        };
        self.finalize_registry(true)?;
        Ok(output)
    }

    fn finish_boundaries(&mut self, deadline: Instant) -> io::Result<()> {
        self.job.wait_and_reap(deadline)?;
        self.secondary.wait_and_reap(deadline)?;
        let inspection = self.secondary.inspect(deadline)?;
        if inspection.survivors != Some(0) || !inspection.quiescent {
            return Err(io::Error::other(
                "secondary Windows execution boundary did not prove quiescence",
            ));
        }
        self.quiescent = true;
        Ok(())
    }

    fn terminate(&mut self, deadline: Instant) -> io::Result<()> {
        let mut error = if self.job_retired {
            Some(io::Error::other(
                "Windows Job was retired after unconfirmed cleanup",
            ))
        } else {
            self.job.terminate().err()
        };
        if let Err(secondary) = self.secondary.kill_boundary(deadline) {
            error.get_or_insert(secondary);
        }
        if !self.job_retired
            && let Err(wait) = self.job.wait_and_reap(deadline)
        {
            error.get_or_insert(wait);
        }
        if let Err(wait) = self.secondary.wait_and_reap(deadline) {
            error.get_or_insert(wait);
        }
        match self.process.wait(deadline) {
            Ok(code) => self.record.exited_on_windows(code),
            Err(wait) => {
                error.get_or_insert(wait);
            }
        }
        self.process.close_conpty();
        match self.secondary.inspect(deadline) {
            Ok(inspection) if inspection.survivors == Some(0) && inspection.quiescent => {
                self.quiescent = error.is_none();
            }
            Ok(_) => {
                error.get_or_insert_with(|| {
                    io::Error::other("secondary Windows execution boundary is not quiescent")
                });
            }
            Err(inspect) => {
                error.get_or_insert(inspect);
            }
        }
        if error.is_some() && !self.job_retired {
            self.job.retire();
            self.job_retired = true;
        }
        let registry = self.finalize_registry(error.is_none());
        match (error, registry) {
            (Some(error), _) => Err(error),
            (None, result) => result,
        }
    }

    fn finalize_registry(&mut self, outcome_known: bool) -> io::Result<()> {
        if self.registry_terminal {
            return Ok(());
        }
        let Some(observer) = &self.observer else {
            self.registry_terminal = true;
            return Ok(());
        };
        let result = if outcome_known && matches!(self.record.state, ProcessState::Exited { .. }) {
            observer.registry.exited(observer.context, &self.record)
        } else {
            observer
                .registry
                .outcome_unknown(observer.context, self.record.process_id())
        };
        self.registry_terminal = result.is_ok();
        result
    }
}

#[cfg(windows)]
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if !self.quiescent || !matches!(self.record.state, ProcessState::Exited { .. }) {
            let _ = self.terminate(cleanup_deadline());
        }
        if let Some(capture) = self.capture.take() {
            capture.cancel();
        }
        settle_custody(
            &mut self.custody,
            if self.quiescent {
                LifecycleState::Quiescent
            } else {
                LifecycleState::OutcomeUnknown
            },
        );
        let _ = self.finalize_registry(
            self.quiescent && matches!(self.record.state, ProcessState::Exited { .. }),
        );
    }
}

/// Spawns only a backend-issued token and captures output within the profile bound.
pub fn spawn_owned(
    prepared: PreparedCommandToken,
    limits: ResourceLimits,
) -> io::Result<OwnedProcess> {
    spawn_owned_inner(prepared, limits, None)
}

pub(crate) fn spawn_owned_with_broker(
    prepared: PreparedCommandToken,
    limits: ResourceLimits,
    broker: &dyn ExecutorSecretBroker,
) -> io::Result<OwnedProcess> {
    spawn_owned_inner(prepared, limits, Some(broker))
}

fn spawn_owned_inner(
    prepared: PreparedCommandToken,
    limits: ResourceLimits,
    broker: Option<&dyn ExecutorSecretBroker>,
) -> io::Result<OwnedProcess> {
    #[cfg(windows)]
    {
        spawn_owned_inner_windows(prepared, limits, broker)
    }
    #[cfg(not(windows))]
    {
        spawn_owned_inner_portable(prepared, limits, broker)
    }
}

#[cfg(windows)]
fn spawn_owned_inner_windows(
    mut prepared: PreparedCommandToken,
    limits: ResourceLimits,
    broker: Option<&dyn ExecutorSecretBroker>,
) -> io::Result<OwnedProcess> {
    let budget = OutputBudget::new(limits.output_bytes)?;
    let limit_deadline = Instant::now()
        .checked_add(Duration::from_millis(limits.wall_time_millis))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "wall-time limit overflows clock",
            )
        })?;
    let deadline = prepared.deadline.min(limit_deadline);
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "wall-time limit elapsed before spawn",
        ));
    }
    let mut custody = match prepared.secrets.take() {
        Some(plan) => {
            let broker = broker.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "CredentialBrokerUnavailable: owned secret spawn requires an executor broker and dedicated helper channel",
                )
            })?;
            Some(
                resolve_for_spawn(
                    plan,
                    prepared.claim,
                    &mut prepared.command,
                    broker,
                    deadline,
                )
                .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?,
            )
        }
        None => None,
    };
    let command =
        crate::executor::backends::windows_job::WindowsCommand::from_std(&prepared.command)
            .map_err(|error| io::Error::other(error.to_string()))?;
    prepared.command.env_clear();
    let composite_profile = prepared.profile.as_ref().and_then(|profile| {
        (crate::executor::backends::windows_job::required_production_spawn_backend(profile)
            == Some(
                crate::executor::backends::windows_job::ProductionSpawnBackend::WindowsComposite,
            ))
        .then(|| profile.clone())
    });
    let mut job = composite_profile
        .is_none()
        .then(|| {
            crate::executor::backends::windows_job::Job::create(prepared.ownership.clone(), limits)
                .map_err(|error| io::Error::other(error.to_string()))
        })
        .transpose()?;
    let conpty = if prepared.terminal.request.transport == TerminalTransport::Pty {
        let observer = prepared.observer.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "PTY execution requires a process registry binding",
            )
        })?;
        Some(
            observer.prepare_conpty(
                prepared.claim,
                job.as_ref()
                    .map_or("windows-composite-pending", |job| job.identity().locator()),
            )?,
        )
    } else {
        None
    };
    let pty_output = match conpty
        .as_ref()
        .map(crate::executor::terminal::ConPtyBinding::output_reader)
        .transpose()
    {
        Ok(output) => output,
        Err(error) => {
            if let Some(observer) = &prepared.observer {
                observer.abort_conpty(prepared.claim.process_id);
            }
            settle_custody(&mut custody, LifecycleState::Quiescent);
            return Err(error);
        }
    };
    if prepared.terminal.request.transport == TerminalTransport::Pty {
        let observer = prepared.observer.as_ref().expect("PTY observer checked");
        if let Err(error) = observer.registry.set_terminal_capture_policy(
            observer.context,
            prepared.claim.process_id,
            custody.as_ref().map_or_else(
                CapturePersistencePolicy::no_secrets,
                PreparedSecrets::capture_policy,
            ),
        ) {
            observer.abort_conpty(prepared.claim.process_id);
            settle_custody(&mut custody, LifecycleState::Quiescent);
            return Err(error);
        }
    }
    let mut boundary = prepared.boundary.take().expect("prepared boundary exists");
    let claim = prepared.claim;
    let observer = prepared.observer.as_ref();
    let spawn: Result<
        (
            crate::executor::backends::windows_job::Job,
            crate::executor::backends::windows_job::JobProcess,
            Box<dyn BoundaryControl>,
        ),
        crate::executor::backends::windows_job::JobError,
    > = if let Some(profile) = &composite_profile {
        (|| {
            let evidence = crate::executor::backends::windows_job::runtime::probe_for_terminal(
                profile,
                conpty.is_some(),
            )
            .map_err(|error| {
                if error.reason
                    == crate::executor::backends::windows_job::UnavailableReason::OutcomeUnknown
                {
                    crate::executor::backends::windows_job::JobError::OutcomeUnknown(error.detail)
                } else {
                    crate::executor::backends::windows_job::JobError::PlatformUnavailable(
                        io::Error::other(error.to_string()),
                    )
                }
            })?;
            let pending = crate::executor::backends::windows_job::spawn_composite_suspended(
                profile,
                &prepared.ownership,
                &command,
                limits,
                evidence,
                custody.as_ref(),
                conpty.as_ref(),
                deadline,
            )?;
            let (job, runtime, process) = pending.register_and_resume(
                prepared.ownership.clone(),
                |composite| {
                    boundary.persistence.persist(composite)?;
                    (boundary.register)(&claim, composite)?;
                    if let Some(observer) = observer {
                        observer.registry.prepared(
                            observer.context,
                            claim,
                            composite,
                            observer.terminal,
                        )?;
                    }
                    Ok(())
                },
                deadline,
            )?;
            Ok((job, process, Box::new(runtime)))
        })()
    } else {
        (|| {
            let mut local_job = job.take().expect("Job-only launch created a Job");
            let process = local_job.spawn(
                &prepared.ownership,
                &command,
                conpty.as_ref(),
                |persisted: &crate::executor::process::tree::PersistedBoundary| {
                    boundary.persistence.persist(persisted)?;
                    (boundary.register)(&claim, persisted)?;
                    if let Some(observer) = observer {
                        observer.registry.prepared(
                            observer.context,
                            claim,
                            persisted,
                            observer.terminal,
                        )?;
                    }
                    Ok(())
                },
            )?;
            Ok((local_job, process, boundary.control))
        })()
    };
    let (mut job, mut process, mut secondary) = match spawn {
        Ok(spawned) => spawned,
        Err(error) => {
            if let Some(observer) = &prepared.observer {
                observer.abort_conpty(prepared.claim.process_id);
                let _ = observer
                    .registry
                    .outcome_unknown(observer.context, prepared.claim.process_id);
            }
            let cleanup_state = if let Some(job) = job.as_mut() {
                cleanup_windows_boundary(job, boundary.control.as_mut())
            } else {
                cleanup_windows_secondary(boundary.control.as_mut())
            };
            let state = if cleanup_state == LifecycleState::Quiescent
                && !matches!(
                    &error,
                    crate::executor::backends::windows_job::JobError::OutcomeUnknown(_)
                ) {
                LifecycleState::Quiescent
            } else {
                LifecycleState::OutcomeUnknown
            };
            settle_custody(&mut custody, state);
            return Err(io::Error::other(error.to_string()));
        }
    };
    let capture = match pty_output.map_or_else(
        || {
            let stdout = process
                .take_stdout()
                .ok_or_else(|| io::Error::other("Windows launcher did not provide stdout"))?;
            let stderr = process
                .take_stderr()
                .ok_or_else(|| io::Error::other("Windows launcher did not provide stderr"))?;
            CaptureThreads::from_readers(stdout, stderr, budget)
        },
        |reader| {
            let observer = prepared.observer.as_ref().expect("PTY observer checked");
            let capture = custody.as_ref().map_or_else(
                || {
                    observer
                        .custody
                        .redactor()
                        .start(CaptureBoundary::TerminalMetadata)
                },
                |custody| {
                    custody.start_capture_with_custody(
                        CaptureBoundary::TerminalMetadata,
                        &observer.custody,
                    )
                },
            );
            CaptureThreads::from_pty(
                reader,
                budget,
                Arc::clone(&observer.registry),
                observer.context,
                prepared.claim.process_id,
                capture,
                prepared.terminal.retention.max_bytes,
            )
        },
    ) {
        Ok(capture) => capture,
        Err(error) => {
            if let Some(observer) = &prepared.observer {
                observer.abort_conpty(prepared.claim.process_id);
                let _ = observer
                    .registry
                    .outcome_unknown(observer.context, prepared.claim.process_id);
            }
            process.close_conpty();
            let state = cleanup_windows_spawn(&mut job, secondary.as_mut(), &mut process, None);
            settle_custody(&mut custody, state);
            return Err(error);
        }
    };
    let token = match BackendBoundaryToken::from_hex(job.identity().ownership_token()) {
        Ok(token) => token,
        Err(error) => {
            if let Some(observer) = &prepared.observer {
                observer.abort_conpty(prepared.claim.process_id);
                let _ = observer
                    .registry
                    .outcome_unknown(observer.context, prepared.claim.process_id);
            }
            process.close_conpty();
            let state =
                cleanup_windows_spawn(&mut job, secondary.as_mut(), &mut process, Some(capture));
            settle_custody(&mut custody, state);
            return Err(error);
        }
    };
    let record = ProcessRecord::started(prepared.claim, process.id(), token);
    if let Some(observer) = &prepared.observer
        && let Err(error) = observer.registry.started(observer.context, &record)
    {
        observer.abort_conpty(prepared.claim.process_id);
        process.close_conpty();
        let state =
            cleanup_windows_spawn(&mut job, secondary.as_mut(), &mut process, Some(capture));
        let _ = observer
            .registry
            .outcome_unknown(observer.context, prepared.claim.process_id);
        settle_custody(&mut custody, state);
        return Err(error);
    }
    Ok(OwnedProcess {
        job,
        process,
        secondary,
        record,
        capture: Some(capture),
        deadline,
        custody,
        observer: prepared.observer.take(),
        registry_terminal: false,
        quiescent: false,
        job_retired: false,
    })
}

#[cfg(windows)]
fn cleanup_windows_secondary(secondary: &mut dyn BoundaryControl) -> LifecycleState {
    let deadline = cleanup_deadline();
    let killed = secondary.kill_boundary(deadline).is_ok();
    let reaped = secondary.wait_and_reap(deadline).is_ok();
    let empty = secondary
        .inspect(deadline)
        .is_ok_and(|inspection| inspection.survivors == Some(0) && inspection.quiescent);
    if killed && reaped && empty {
        LifecycleState::Quiescent
    } else {
        LifecycleState::OutcomeUnknown
    }
}

#[cfg(windows)]
fn cleanup_windows_boundary(
    job: &mut crate::executor::backends::windows_job::Job,
    secondary: &mut dyn BoundaryControl,
) -> LifecycleState {
    let deadline = cleanup_deadline();
    let job_killed = job.terminate().is_ok();
    let secondary_killed = secondary.kill_boundary(deadline).is_ok();
    let job_reaped = job.wait_and_reap(deadline).is_ok();
    let secondary_reaped = secondary.wait_and_reap(deadline).is_ok();
    let inspected = matches!(job.active_processes(), Ok(0))
        && secondary
            .inspect(deadline)
            .is_ok_and(|inspection| inspection.survivors == Some(0) && inspection.quiescent);
    if job_killed && secondary_killed && job_reaped && secondary_reaped && inspected {
        LifecycleState::Quiescent
    } else {
        LifecycleState::OutcomeUnknown
    }
}

#[cfg(windows)]
fn cleanup_windows_spawn(
    job: &mut crate::executor::backends::windows_job::Job,
    secondary: &mut dyn BoundaryControl,
    process: &mut crate::executor::backends::windows_job::JobProcess,
    capture: Option<CaptureThreads>,
) -> LifecycleState {
    let deadline = cleanup_deadline();
    process.close_conpty();
    let job_killed = job.terminate().is_ok();
    let secondary_killed = secondary.kill_boundary(deadline).is_ok();
    let process_reaped = process.wait(deadline).is_ok();
    if let Some(capture) = capture {
        capture.cancel();
    }
    let job_reaped = job.wait_and_reap(deadline).is_ok();
    let secondary_reaped = secondary.wait_and_reap(deadline).is_ok();
    let inspected = matches!(job.active_processes(), Ok(0))
        && secondary
            .inspect(deadline)
            .is_ok_and(|inspection| inspection.survivors == Some(0) && inspection.quiescent);
    if job_killed
        && secondary_killed
        && process_reaped
        && job_reaped
        && secondary_reaped
        && inspected
    {
        LifecycleState::Quiescent
    } else {
        LifecycleState::OutcomeUnknown
    }
}

#[cfg(not(windows))]
fn spawn_owned_inner_portable(
    mut prepared: PreparedCommandToken,
    limits: ResourceLimits,
    broker: Option<&dyn ExecutorSecretBroker>,
) -> io::Result<OwnedProcess> {
    let budget = OutputBudget::new(limits.output_bytes)?;
    let pty_reader = if prepared.terminal.request.transport == TerminalTransport::Pty {
        let observer = prepared.observer.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "PTY execution requires a process registry binding",
            )
        })?;
        Some(observer.registry.bind_terminal(
            observer.context,
            prepared.claim.process_id,
            &mut prepared.command,
        )?)
    } else {
        prepared
            .command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        None
    };
    let limit_deadline = Instant::now()
        .checked_add(Duration::from_millis(limits.wall_time_millis))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "wall-time limit overflows clock",
            )
        })?;
    let deadline = prepared.deadline.min(limit_deadline);
    if Instant::now() >= deadline {
        let cleanup = prepared
            .boundary
            .as_mut()
            .expect("prepared boundary exists")
            .cancel(&prepared.ownership, cleanup_deadline());
        return Err(cleanup.err().map_or_else(
            || {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "wall-time limit elapsed before spawn",
                )
            },
            |error| io::Error::other(error.to_string()),
        ));
    }
    let mut custody = match prepared.secrets.take() {
        Some(plan) => {
            let broker = broker.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "owned secret spawn requires an executor broker",
                )
            })?;
            Some(
                resolve_for_spawn(
                    plan,
                    prepared.claim,
                    &mut prepared.command,
                    broker,
                    deadline,
                )
                .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?,
            )
        }
        None => None,
    };
    if prepared.terminal.request.transport == TerminalTransport::Pty {
        let observer = prepared.observer.as_ref().expect("PTY observer checked");
        if let Err(error) = observer.registry.set_terminal_capture_policy(
            observer.context,
            prepared.claim.process_id,
            custody.as_ref().map_or_else(
                CapturePersistencePolicy::no_secrets,
                PreparedSecrets::capture_policy,
            ),
        ) {
            cancel_prepared_and_settle(&mut prepared, &mut custody);
            return Err(error);
        }
    }
    let spawn = prepared.command.spawn();
    // Remove parent-side environment copies regardless of spawn outcome.
    prepared.command.env_clear();
    let mut child = match spawn {
        Ok(child) => child,
        Err(error) => {
            let cleanup = prepared
                .boundary
                .as_mut()
                .expect("prepared boundary exists")
                .cancel(&prepared.ownership, cleanup_deadline());
            settle_custody(
                &mut custody,
                prepared
                    .boundary
                    .as_ref()
                    .expect("prepared boundary exists")
                    .state(),
            );
            return Err(cleanup
                .err()
                .map_or(error, |cleanup| io::Error::other(cleanup.to_string())));
        }
    };
    let execution_id = child.id();
    let capture = match pty_reader.map_or_else(
        || CaptureThreads::from_child(&mut child, budget),
        |reader| {
            let observer = prepared.observer.as_ref().expect("PTY observer checked");
            let capture = custody.as_ref().map_or_else(
                || {
                    observer
                        .custody
                        .redactor()
                        .start(CaptureBoundary::TerminalMetadata)
                },
                |custody| {
                    custody.start_capture_with_custody(
                        CaptureBoundary::TerminalMetadata,
                        &observer.custody,
                    )
                },
            );
            CaptureThreads::from_pty(
                reader,
                budget,
                Arc::clone(&observer.registry),
                observer.context,
                prepared.claim.process_id,
                capture,
                prepared.terminal.retention.max_bytes,
            )
        },
    ) {
        Ok(capture) => capture,
        Err(error) => {
            let _ = child.kill();
            reap_until(&mut child, cleanup_deadline());
            cancel_prepared_and_settle(&mut prepared, &mut custody);
            return Err(error);
        }
    };
    let record = ProcessRecord {
        claim: prepared.claim,
        execution_id,
        boundary_token: prepared.boundary_token,
        state: ProcessState::Started,
    };
    if let Some(observer) = &prepared.observer
        && let Err(error) = observer.registry.started(observer.context, &record)
    {
        let _ = child.kill();
        reap_until(&mut child, cleanup_deadline());
        cancel_prepared_and_settle(&mut prepared, &mut custody);
        return Err(error);
    }
    Ok(OwnedProcess {
        child,
        record,
        capture: Some(capture),
        ownership: prepared.ownership.clone(),
        boundary: prepared.boundary.take().expect("prepared boundary exists"),
        deadline,
        custody,
        observer: prepared.observer.take(),
        registry_terminal: false,
    })
}

fn cancel_prepared_and_settle(
    prepared: &mut PreparedCommandToken,
    custody: &mut Option<PreparedSecrets>,
) {
    let boundary = prepared
        .boundary
        .as_mut()
        .expect("prepared boundary exists");
    let _ = boundary.cancel(&prepared.ownership, cleanup_deadline());
    settle_custody(custody, boundary.state());
}

pub(crate) fn settle_custody(custody: &mut Option<PreparedSecrets>, state: LifecycleState) {
    let Some(custody) = custody.take() else {
        return;
    };
    if state == LifecycleState::Quiescent {
        drop(custody);
    } else {
        QUARANTINED_SECRET_CUSTODIES.fetch_add(1, Ordering::Relaxed);
        std::mem::forget(custody);
    }
}

fn validate_spawn_limits(limits: ResourceLimits, deadline: Instant) -> io::Result<()> {
    OutputBudget::new(limits.output_bytes)?;
    Instant::now()
        .checked_add(Duration::from_millis(limits.wall_time_millis))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "wall-time limit overflows clock",
            )
        })?;
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "wall-time limit elapsed before boundary issue",
        ));
    }
    Ok(())
}

/// Drains two pipes concurrently using an explicit deterministic split: stdout
/// receives the odd byte and stderr receives the even half of `output_bytes`.
pub fn capture_bounded<Out, Err>(
    stdout: Out,
    stderr: Err,
    output_bytes: u64,
) -> io::Result<ProcessOutput>
where
    Out: Read + Send + 'static,
    Err: Read + Send + 'static,
{
    CaptureThreads::start(stdout, stderr, OutputBudget::new(output_bytes)?)?.finish()
}

#[derive(Clone, Copy)]
pub(crate) struct OutputBudget {
    stdout: usize,
    stderr: usize,
}

impl OutputBudget {
    pub(crate) fn new(total: u64) -> io::Result<Self> {
        let marker_bytes = u64::try_from(TRUNCATION_MARKER.len()).expect("marker length fits u64");
        if total < marker_bytes * 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "output_bytes must be at least {} so both streams can carry a truncation marker",
                    marker_bytes * 2
                ),
            ));
        }
        let stdout = usize::try_from(total.div_ceil(2)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "output_bytes exceeds address space",
            )
        })?;
        let stderr = usize::try_from(total / 2).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "output_bytes exceeds address space",
            )
        })?;
        Ok(Self { stdout, stderr })
    }
}

pub(crate) struct CaptureThreads {
    stdout: Option<JoinHandle<io::Result<CapturedStream>>>,
    stderr: Option<JoinHandle<io::Result<CapturedStream>>>,
    cancelled: Arc<AtomicBool>,
}

struct PtyCaptureTarget {
    registry: Arc<dyn ProcessRegistry>,
    context: ProcessRegistrationContext,
    process_id: ProcessId,
    retained_chunk_bytes: usize,
}

impl CaptureThreads {
    fn from_child(child: &mut Child, budget: OutputBudget) -> io::Result<Self> {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("backend command did not provide stdout pipe"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("backend command did not provide stderr pipe"))?;
        set_nonblocking(&stdout)?;
        set_nonblocking(&stderr)?;
        Self::start_cancellable(stdout, stderr, budget)
    }

    #[cfg(windows)]
    pub(crate) fn from_readers<Out, Err>(
        stdout: Out,
        stderr: Err,
        budget: OutputBudget,
    ) -> io::Result<Self>
    where
        Out: Read + Send + 'static,
        Err: Read + Send + 'static,
    {
        Self::start_cancellable(stdout, stderr, budget)
    }

    pub(crate) fn from_pty(
        reader: Box<dyn Read + Send>,
        budget: OutputBudget,
        registry: Arc<dyn ProcessRegistry>,
        context: ProcessRegistrationContext,
        process_id: ProcessId,
        capture: SanitizedCapture,
        retained_chunk_bytes: usize,
    ) -> io::Result<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let output_cancelled = cancelled.clone();
        let stdout = thread::Builder::new()
            .name("process-pty".to_owned())
            .spawn(move || {
                capture_pty_stream(
                    reader,
                    budget.stdout,
                    &output_cancelled,
                    capture,
                    PtyCaptureTarget {
                        registry,
                        context,
                        process_id,
                        retained_chunk_bytes,
                    },
                )
            })?;
        let stderr = match thread::Builder::new()
            .name("process-pty-stderr".to_owned())
            .spawn(move || capture_stream(io::empty(), budget.stderr))
        {
            Ok(stderr) => stderr,
            Err(error) => {
                cancelled.store(true, Ordering::Release);
                drop(stdout);
                return Err(error);
            }
        };
        Ok(Self {
            stdout: Some(stdout),
            stderr: Some(stderr),
            cancelled,
        })
    }

    fn start<Out, Err>(stdout: Out, stderr: Err, budget: OutputBudget) -> io::Result<Self>
    where
        Out: Read + Send + 'static,
        Err: Read + Send + 'static,
    {
        let cancelled = Arc::new(AtomicBool::new(false));
        let stdout = thread::spawn(move || capture_stream(stdout, budget.stdout));
        let stderr = thread::spawn(move || capture_stream(stderr, budget.stderr));
        Ok(Self {
            stdout: Some(stdout),
            stderr: Some(stderr),
            cancelled,
        })
    }

    fn start_cancellable<Out, Err>(
        stdout: Out,
        stderr: Err,
        budget: OutputBudget,
    ) -> io::Result<Self>
    where
        Out: Read + Send + 'static,
        Err: Read + Send + 'static,
    {
        let cancelled = Arc::new(AtomicBool::new(false));
        let stdout_cancelled = cancelled.clone();
        let stderr_cancelled = cancelled.clone();
        let stdout = thread::Builder::new()
            .name("process-stdout".to_owned())
            .spawn(move || capture_stream_cancellable(stdout, budget.stdout, &stdout_cancelled))?;
        let stderr = match thread::Builder::new()
            .name("process-stderr".to_owned())
            .spawn(move || capture_stream_cancellable(stderr, budget.stderr, &stderr_cancelled))
        {
            Ok(stderr) => stderr,
            Err(error) => {
                cancelled.store(true, Ordering::Release);
                drop(stdout);
                return Err(error);
            }
        };
        Ok(Self {
            stdout: Some(stdout),
            stderr: Some(stderr),
            cancelled,
        })
    }

    fn finish(mut self) -> io::Result<ProcessOutput> {
        let stdout = join_capture(self.stdout.take().expect("stdout capture exists"))?;
        let stderr = join_capture(self.stderr.take().expect("stderr capture exists"))?;
        Ok(ProcessOutput { stdout, stderr })
    }

    pub(crate) fn finish_by(mut self, deadline: Instant) -> io::Result<ProcessOutput> {
        while !self.stdout.as_ref().is_none_or(JoinHandle::is_finished)
            || !self.stderr.as_ref().is_none_or(JoinHandle::is_finished)
        {
            if Instant::now() >= deadline {
                self.cancelled.store(true, Ordering::Release);
                if let Some(stdout) = self.stdout.take() {
                    let _ = stdout.join();
                }
                if let Some(stderr) = self.stderr.take() {
                    let _ = stderr.join();
                }
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process output pipes did not close before the wall-time deadline",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
        let stdout = join_capture(self.stdout.take().expect("stdout capture exists"))?;
        let stderr = join_capture(self.stderr.take().expect("stderr capture exists"))?;
        Ok(ProcessOutput { stdout, stderr })
    }

    pub(crate) fn cancel(mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(stdout) = self.stdout.take() {
            let _ = stdout.join();
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

fn capture_pty_stream(
    mut reader: Box<dyn Read + Send>,
    limit: usize,
    cancelled: &AtomicBool,
    mut sanitized: SanitizedCapture,
    target: PtyCaptureTarget,
) -> io::Result<CapturedStream> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(limit)
        .map_err(|error| io::Error::other(format!("output buffer allocation failed: {error}")))?;
    let mut original_bytes = 0_u64;
    let mut chunk = [0_u8; 8192];
    let chunk_limit = target.retained_chunk_bytes.clamp(1, chunk.len());
    let result = (|| {
        loop {
            if cancelled.load(Ordering::Acquire) {
                break Ok(());
            }
            match reader.read(&mut chunk[..chunk_limit]) {
                Ok(0) => break Ok(()),
                Ok(read) => {
                    original_bytes = original_bytes
                        .checked_add(u64::try_from(read).expect("read length fits u64"))
                        .ok_or_else(|| io::Error::other("output byte accounting overflow"))?;
                    let retained = limit.saturating_sub(bytes.len()).min(read);
                    bytes.extend_from_slice(&chunk[..retained]);
                    sanitized.push(&chunk[..read]).map_err(io::Error::other)?;
                    if let Some(ready) = sanitized.take_ready() {
                        target.registry.append_terminal_output(
                            target.context,
                            target.process_id,
                            &ready,
                        )?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                #[cfg(unix)]
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break Ok(()),
                Err(error) => break Err(error),
            }
        }
    })();
    let finish = if result.is_ok() && !cancelled.load(Ordering::Acquire) {
        sanitized.finish().map_err(io::Error::other).and_then(|()| {
            sanitized.take_ready().map_or(Ok(()), |ready| {
                target
                    .registry
                    .append_terminal_output(target.context, target.process_id, &ready)
            })
        })
    } else {
        Ok(())
    };
    let close = target
        .registry
        .close_terminal(target.context, target.process_id);
    result?;
    finish?;
    close?;
    complete_capture(bytes, original_bytes, limit)
}

impl Drop for CaptureThreads {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(stdout) = self.stdout.take() {
            let _ = stdout.join();
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

fn capture_stream(mut reader: impl Read, limit: usize) -> io::Result<CapturedStream> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(limit)
        .map_err(|error| io::Error::other(format!("output buffer allocation failed: {error}")))?;
    let mut original_bytes = 0_u64;
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        original_bytes = original_bytes
            .checked_add(u64::try_from(read).expect("read length fits u64"))
            .ok_or_else(|| io::Error::other("output byte accounting overflow"))?;
        let retained = limit.saturating_sub(bytes.len()).min(read);
        bytes.extend_from_slice(&chunk[..retained]);
    }

    let original_retained = u64::try_from(bytes.len()).expect("buffer length fits u64");
    let truncated_bytes = original_bytes.saturating_sub(original_retained);
    if truncated_bytes != 0 {
        bytes.truncate(limit - TRUNCATION_MARKER.len());
        let retained_after_marker = u64::try_from(bytes.len()).expect("buffer length fits u64");
        bytes.extend_from_slice(TRUNCATION_MARKER);
        return Ok(CapturedStream {
            bytes,
            original_bytes,
            truncated_bytes: original_bytes - retained_after_marker,
        });
    }
    Ok(CapturedStream {
        bytes,
        original_bytes,
        truncated_bytes,
    })
}

fn capture_stream_cancellable(
    mut reader: impl Read,
    limit: usize,
    cancelled: &AtomicBool,
) -> io::Result<CapturedStream> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(limit)
        .map_err(|error| io::Error::other(format!("output buffer allocation failed: {error}")))?;
    let mut original_bytes = 0_u64;
    let mut chunk = [0_u8; 8192];
    loop {
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                original_bytes = original_bytes
                    .checked_add(u64::try_from(read).expect("read length fits u64"))
                    .ok_or_else(|| io::Error::other("output byte accounting overflow"))?;
                let retained = limit.saturating_sub(bytes.len()).min(read);
                bytes.extend_from_slice(&chunk[..retained]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if cancelled.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error),
        }
    }
    complete_capture(bytes, original_bytes, limit)
}

fn complete_capture(
    mut bytes: Vec<u8>,
    original_bytes: u64,
    limit: usize,
) -> io::Result<CapturedStream> {
    let original_retained = u64::try_from(bytes.len()).expect("buffer length fits u64");
    let truncated_bytes = original_bytes.saturating_sub(original_retained);
    if truncated_bytes != 0 {
        bytes.truncate(limit - TRUNCATION_MARKER.len());
        let retained_after_marker = u64::try_from(bytes.len()).expect("buffer length fits u64");
        bytes.extend_from_slice(TRUNCATION_MARKER);
        return Ok(CapturedStream {
            bytes,
            original_bytes,
            truncated_bytes: original_bytes - retained_after_marker,
        });
    }
    Ok(CapturedStream {
        bytes,
        original_bytes,
        truncated_bytes,
    })
}

fn join_capture(handle: JoinHandle<io::Result<CapturedStream>>) -> io::Result<CapturedStream> {
    handle
        .join()
        .map_err(|_| io::Error::other("output capture thread panicked"))?
}

fn exit_state(status: ExitStatus) -> ProcessState {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal = None;
    ProcessState::Exited {
        success: status.success(),
        code: status.code(),
        signal,
    }
}

fn parse_token(value: &str) -> io::Result<[u8; 32]> {
    if value.len() != 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "complete boundary ownership token must be 256-bit lowercase hex",
        ));
    }
    let mut token = [0_u8; 32];
    for (output, pair) in token.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        let digit = |value| match value {
            b'0'..=b'9' => Some(value - b'0'),
            b'a'..=b'f' => Some(value - b'a' + 10),
            _ => None,
        };
        *output = (digit(pair[0]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid boundary ownership token",
            )
        })? << 4)
            | digit(pair[1]).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid boundary ownership token",
                )
            })?;
    }
    Ok(token)
}

fn cleanup_deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

fn reap_until(child: &mut Child, deadline: Instant) {
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(Duration::from_millis(2)),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_nonblocking<T: std::os::fd::AsRawFd>(pipe: &T) -> io::Result<()> {
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    #[cfg(target_os = "linux")]
    const O_NONBLOCK: i32 = 0x800;
    #[cfg(target_os = "macos")]
    const O_NONBLOCK: i32 = 0x4;
    let descriptor = pipe.as_raw_fd();
    let flags = unsafe { fcntl(descriptor, F_GETFL) };
    if flags < 0 || unsafe { fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn set_nonblocking<T>(_pipe: &T) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cancellable output capture is unavailable on this host",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe extern "C" {
    fn fcntl(descriptor: i32, command: i32, ...) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use crate::executor::terminal::PtyDriver as _;
    use crate::{
        api::auth::{
            contract::{Authenticator, GrantSnapshot},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        api::http::exec::{
            AllocateTerminalBody, ExecError, ExecService, ManagerExecService, TerminalResizeBody,
            WriterLeaseBody,
        },
        domain::{
            config::Grant,
            ids::{AttemptId, DaemonServiceId, PrincipalId, ProjectId, TerminalId},
            lifecycle::{AttemptOwnership, FencingToken},
        },
        executor::process::tree::{BoundaryIdentity, BoundaryKind, Inspection, PersistedBoundary},
        executor::{
            cancel::SqliteCancellationCoordinator,
            profile::CredentialInjectionMode,
            secrets::{ExecutorSecretContext, SecretBrokerError},
            terminal::{
                FakePtyDriver, NativePtyDriver, OutputRead, OutputRetention,
                SqliteTerminalSnapshotStore, TerminalAllocation, TerminalControl, TerminalError,
                TerminalLifecycle, TerminalManager, TerminalRequest, TerminalSize,
                TerminalSnapshot,
            },
        },
        store::sqlite::idempotency::IdempotencyKey,
    };
    use std::{
        collections::BTreeMap,
        io::Cursor,
        sync::{Arc, Mutex, atomic::AtomicUsize},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TestBoundary {
        identity: BoundaryIdentity,
        calls: Arc<Mutex<Vec<&'static str>>>,
        failure: Option<&'static str>,
    }

    impl TestBoundary {
        fn new(calls: Arc<Mutex<Vec<&'static str>>>) -> Self {
            Self {
                identity: BoundaryIdentity::new(
                    if cfg!(windows) {
                        BoundaryKind::WindowsContainerOrVm
                    } else {
                        BoundaryKind::Container
                    },
                    "test-boundary",
                    "a".repeat(64),
                    "test-start",
                )
                .unwrap(),
                calls,
                failure: None,
            }
        }

        fn outcome_unknown(calls: Arc<Mutex<Vec<&'static str>>>) -> Self {
            let mut boundary = Self::new(calls);
            boundary.failure = Some("inspect");
            boundary
        }

        fn failing(calls: Arc<Mutex<Vec<&'static str>>>, operation: &'static str) -> Self {
            let mut boundary = Self::new(calls);
            boundary.failure = Some(operation);
            boundary
        }
    }

    impl BoundaryControl for TestBoundary {
        fn identity(&self) -> &BoundaryIdentity {
            &self.identity
        }

        fn containment(&self) -> Containment {
            Containment::Complete
        }

        fn release(&mut self, _deadline: Instant) -> io::Result<()> {
            self.calls.lock().unwrap().push("release");
            Ok(())
        }

        fn kill_boundary(&mut self, _deadline: Instant) -> io::Result<()> {
            self.calls.lock().unwrap().push("kill");
            if self.failure == Some("kill") {
                Err(io::Error::other("injected kill failure"))
            } else {
                Ok(())
            }
        }

        fn wait_and_reap(&mut self, _deadline: Instant) -> io::Result<()> {
            self.calls.lock().unwrap().push("reap");
            if self.failure == Some("reap") {
                Err(io::Error::other("injected reap failure"))
            } else {
                Ok(())
            }
        }

        fn inspect(&mut self, _deadline: Instant) -> io::Result<Inspection> {
            self.calls.lock().unwrap().push("inspect");
            if self.failure == Some("inspect") {
                return Err(io::Error::other("injected inspection failure"));
            }
            Ok(Inspection {
                identity: self.identity.clone(),
                survivors: Some(0),
                quiescent: true,
            })
        }

        #[cfg(windows)]
        fn bind_windows_root(
            &mut self,
            _job: &BoundaryIdentity,
            _deadline: Instant,
        ) -> io::Result<()> {
            Ok(())
        }
    }

    fn limits(output_bytes: u64) -> ResourceLimits {
        ResourceLimits::new(1, 1, 1, 1, 1, 1, output_bytes, 1_000)
    }

    struct TestRegistry {
        calls: Arc<Mutex<Vec<&'static str>>>,
        fail_started: bool,
        fail_exited: bool,
    }

    impl ProcessRegistry for TestRegistry {
        fn prepared(
            &self,
            _context: ProcessRegistrationContext,
            _claim: ProcessClaim,
            _boundary: &PersistedBoundary,
            _terminal: ProcessTerminalConfig,
        ) -> io::Result<()> {
            self.calls.lock().unwrap().push("prepared");
            Ok(())
        }

        fn started(
            &self,
            _context: ProcessRegistrationContext,
            _record: &ProcessRecord,
        ) -> io::Result<()> {
            self.calls.lock().unwrap().push("started");
            if self.fail_started {
                Err(io::Error::other("injected registry start failure"))
            } else {
                Ok(())
            }
        }

        fn exited(
            &self,
            _context: ProcessRegistrationContext,
            _record: &ProcessRecord,
        ) -> io::Result<()> {
            self.calls.lock().unwrap().push("exited");
            if self.fail_exited {
                Err(io::Error::other("injected registry exit failure"))
            } else {
                Ok(())
            }
        }

        fn outcome_unknown(
            &self,
            _context: ProcessRegistrationContext,
            _process_id: ProcessId,
        ) -> io::Result<()> {
            self.calls.lock().unwrap().push("outcome_unknown");
            Ok(())
        }
    }

    #[cfg(unix)]
    fn timeout_process(boundary: TestBoundary, registry: Arc<TestRegistry>) -> OwnedProcess {
        let context = registry_context();
        let owner = ProcessOwnership::Attempt(AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            context.principal_id,
            FencingToken::new(1),
        ));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "while :; do :; done"]);
        let timed_limits = ResourceLimits::new(1, 1, 1, 1, 1, 1, 128, 10);
        let token = PreparedCommandToken::issue_observed_registered(
            command,
            owner,
            boundary,
            |_: &PersistedBoundary| Ok(()),
            |_, _| Ok(()),
            Some(ProcessRegistryRegistration::new(registry, context)),
            Instant::now() + Duration::from_secs(1),
            timed_limits,
        )
        .unwrap();
        spawn_owned(token, timed_limits).unwrap()
    }

    fn registry_context() -> ProcessRegistrationContext {
        ProcessRegistrationContext {
            project_id: ProjectId::generate().unwrap(),
            principal_id: PrincipalId::generate().unwrap(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminalizes_registry_only_after_kill_reap_and_inspection() {
        let boundary_calls = Arc::new(Mutex::new(Vec::new()));
        let registry_calls = Arc::new(Mutex::new(Vec::new()));
        let registry = Arc::new(TestRegistry {
            calls: registry_calls.clone(),
            fail_started: false,
            fail_exited: false,
        });
        let mut process = timeout_process(TestBoundary::new(boundary_calls.clone()), registry);

        let error = process.wait().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            *boundary_calls.lock().unwrap(),
            ["release", "kill", "reap", "inspect"]
        );
        assert_eq!(
            *registry_calls.lock().unwrap(),
            ["prepared", "started", "exited"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_cleanup_or_registry_faults_are_outcome_unknown() {
        for failure in ["kill", "reap", "inspect"] {
            let boundary_calls = Arc::new(Mutex::new(Vec::new()));
            let registry_calls = Arc::new(Mutex::new(Vec::new()));
            let registry = Arc::new(TestRegistry {
                calls: registry_calls.clone(),
                fail_started: false,
                fail_exited: false,
            });
            let mut process =
                timeout_process(TestBoundary::failing(boundary_calls, failure), registry);
            assert_ne!(process.wait().unwrap_err().kind(), io::ErrorKind::TimedOut);
            assert_eq!(
                registry_calls.lock().unwrap().last(),
                Some(&"outcome_unknown")
            );
        }

        let registry_calls = Arc::new(Mutex::new(Vec::new()));
        let registry = Arc::new(TestRegistry {
            calls: registry_calls.clone(),
            fail_started: false,
            fail_exited: true,
        });
        let mut process = timeout_process(
            TestBoundary::new(Arc::new(Mutex::new(Vec::new()))),
            registry,
        );
        assert_ne!(process.wait().unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            *registry_calls.lock().unwrap(),
            ["prepared", "started", "exited", "outcome_unknown"]
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn token_preflight_precedes_persistence_and_drop_cancels_an_issued_boundary() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let persisted = Arc::new(AtomicBool::new(false));
        let persisted_for_issue = persisted.clone();
        let invalid = PreparedCommandToken::issue(
            Command::new("true"),
            ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap()),
            TestBoundary::new(calls.clone()),
            move |_: &PersistedBoundary| {
                persisted_for_issue.store(true, Ordering::Release);
                Ok(())
            },
            Instant::now() + Duration::from_secs(1),
            limits(1),
        );
        assert!(invalid.is_err());
        assert!(!persisted.load(Ordering::Acquire));
        assert!(calls.lock().unwrap().is_empty());

        let token = PreparedCommandToken::issue(
            Command::new("true"),
            ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap()),
            TestBoundary::new(calls.clone()),
            |_: &PersistedBoundary| Ok(()),
            Instant::now() + Duration::from_secs(1),
            limits(128),
        )
        .unwrap();
        drop(token);
        assert_eq!(
            *calls.lock().unwrap(),
            ["release", "kill", "reap", "inspect"]
        );

        calls.lock().unwrap().clear();
        let token = PreparedCommandToken::issue(
            Command::new("true"),
            ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap()),
            TestBoundary::new(calls.clone()),
            |_: &PersistedBoundary| Ok(()),
            Instant::now() + Duration::from_secs(1),
            limits(128),
        )
        .unwrap();
        assert!(spawn_owned(token, limits(1)).is_err());
        assert_eq!(
            *calls.lock().unwrap(),
            ["release", "kill", "reap", "inspect"]
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn process_claim_registration_precedes_release_and_failure_cleans_the_boundary() {
        let owner = ProcessOwnership::Attempt(AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            PrincipalId::generate().unwrap(),
            FencingToken::new(9),
        ));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let persisted = calls.clone();
        let registered = calls.clone();
        let token = PreparedCommandToken::issue_registered(
            Command::new("true"),
            owner,
            TestBoundary::new(calls.clone()),
            move |_: &PersistedBoundary| {
                persisted.lock().unwrap().push("persist");
                Ok(())
            },
            move |claim, boundary| {
                assert_eq!(claim.owner, owner);
                assert_eq!(boundary.identity.locator(), "test-boundary");
                registered.lock().unwrap().push("register");
                Ok(())
            },
            Instant::now() + Duration::from_secs(1),
            limits(128),
        )
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), ["persist", "register", "release"]);
        drop(token);

        calls.lock().unwrap().clear();
        let persisted = calls.clone();
        let registered = calls.clone();
        let error = PreparedCommandToken::issue_registered(
            Command::new("true"),
            owner,
            TestBoundary::new(calls.clone()),
            move |_: &PersistedBoundary| {
                persisted.lock().unwrap().push("persist");
                Ok(())
            },
            move |_, _| {
                registered.lock().unwrap().push("register");
                Err(io::Error::other("injected registry failure"))
            },
            Instant::now() + Duration::from_secs(1),
            limits(128),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("durable boundary persistence failed")
        );
        assert_eq!(
            *calls.lock().unwrap(),
            ["persist", "register", "kill", "reap", "inspect"]
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn spawned_records_retain_the_exact_caller_owner() {
        let owners = [
            ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap()),
            ProcessOwnership::Attempt(AttemptOwnership::new(
                AttemptId::generate().unwrap(),
                PrincipalId::generate().unwrap(),
                FencingToken::new(7),
            )),
        ];
        for owner in owners {
            let token = PreparedCommandToken::issue(
                Command::new("true"),
                owner,
                TestBoundary::new(Arc::new(Mutex::new(Vec::new()))),
                |_: &PersistedBoundary| Ok(()),
                Instant::now() + Duration::from_secs(1),
                limits(128),
            )
            .unwrap();
            assert_eq!(token.owner(), owner);
            let mut process = spawn_owned(token, limits(128)).unwrap();
            assert_eq!(process.record().owner(), owner);
            process.wait().unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_spawn_owned_routes_attempts_through_the_job_lifecycle() {
        let context = registry_context();
        let owner = ProcessOwnership::Attempt(AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            context.principal_id,
            FencingToken::new(3),
        ));
        let boundary_calls = Arc::new(Mutex::new(Vec::new()));
        let registry_calls = Arc::new(Mutex::new(Vec::new()));
        let registry = Arc::new(TestRegistry {
            calls: registry_calls.clone(),
            fail_started: false,
            fail_exited: false,
        });
        let persisted = boundary_calls.clone();
        let registered = boundary_calls.clone();
        let mut command = Command::new(r"C:\Windows\System32\cmd.exe");
        command.args(["/d", "/c", "echo windows-owned"]);
        let limits = ResourceLimits::new(5_000, 256 * 1024 * 1024, 4, 1, 1, 1, 4_096, 5_000);
        let token = PreparedCommandToken::issue_observed_registered(
            command,
            owner,
            TestBoundary::new(boundary_calls.clone()),
            move |_: &PersistedBoundary| {
                persisted.lock().unwrap().push("persist");
                Ok(())
            },
            move |_, _| {
                registered.lock().unwrap().push("register");
                Ok(())
            },
            Some(ProcessRegistryRegistration::new(registry, context)),
            Instant::now() + Duration::from_secs(5),
            limits,
        )
        .unwrap();
        let mut process = spawn_owned(token, limits).unwrap();
        let output = process.wait().unwrap();
        assert_eq!(process.record().owner(), owner);
        assert!(
            output
                .stdout
                .raw_bytes()
                .windows(b"windows-owned".len())
                .any(|window| window == b"windows-owned")
        );
        assert_eq!(
            *boundary_calls.lock().unwrap(),
            ["persist", "register", "release", "reap", "inspect"]
        );
        assert_eq!(
            *registry_calls.lock().unwrap(),
            ["prepared", "started", "exited"]
        );
    }

    #[cfg(windows)]
    struct WindowsPostResumeFaultRegistry {
        driver: NativePtyDriver,
        terminal: Mutex<Option<TerminalId>>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[cfg(windows)]
    impl ProcessRegistry for WindowsPostResumeFaultRegistry {
        fn prepared(
            &self,
            _context: ProcessRegistrationContext,
            _claim: ProcessClaim,
            boundary: &PersistedBoundary,
            _terminal: ProcessTerminalConfig,
        ) -> io::Result<()> {
            assert_eq!(boundary.identity.kind(), BoundaryKind::WindowsComposite);
            self.calls.lock().unwrap().push("prepared");
            Ok(())
        }

        fn prepare_conpty(
            &self,
            context: ProcessRegistrationContext,
            claim: ProcessClaim,
            boundary_id: &str,
            terminal: ProcessTerminalConfig,
        ) -> io::Result<crate::executor::terminal::ConPtyBinding> {
            let ProcessOwnership::Attempt(owner) = claim.owner else {
                return Err(io::Error::other("test requires attempt ownership"));
            };
            let terminal_id = TerminalId::generate().map_err(io::Error::other)?;
            let terminal_owner = crate::executor::terminal::TerminalOwner {
                project_id: context.project_id,
                process_id: claim.process_id,
                attempt_id: owner.attempt_id,
                principal_id: owner.principal_id,
                process_fence: owner.fencing_token,
                boundary_id: boundary_id.to_owned(),
            };
            self.driver
                .allocate(terminal_id, &terminal_owner, terminal.size)
                .map_err(io::Error::other)?;
            *self.terminal.lock().unwrap() = Some(terminal_id);
            self.driver.binding(terminal_id)
        }

        fn abort_conpty(
            &self,
            _context: ProcessRegistrationContext,
            _process_id: ProcessId,
        ) -> io::Result<()> {
            self.calls.lock().unwrap().push("abort_conpty");
            if let Some(terminal) = *self.terminal.lock().unwrap() {
                self.driver.interrupt(terminal)?;
            }
            Ok(())
        }

        fn started(
            &self,
            _context: ProcessRegistrationContext,
            _record: &ProcessRecord,
        ) -> io::Result<()> {
            self.calls.lock().unwrap().push("started");
            Err(io::Error::other("injected post-resume registry failure"))
        }

        fn exited(
            &self,
            _context: ProcessRegistrationContext,
            _record: &ProcessRecord,
        ) -> io::Result<()> {
            Ok(())
        }

        fn outcome_unknown(
            &self,
            _context: ProcessRegistrationContext,
            _process_id: ProcessId,
        ) -> io::Result<()> {
            self.calls.lock().unwrap().push("outcome_unknown");
            Ok(())
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_post_resume_registry_fault_aborts_conpty_and_both_boundaries() {
        let context = registry_context();
        let owner = ProcessOwnership::Attempt(AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            context.principal_id,
            FencingToken::new(4),
        ));
        let registry_calls = Arc::new(Mutex::new(Vec::new()));
        let registry = Arc::new(WindowsPostResumeFaultRegistry {
            driver: NativePtyDriver::new(),
            terminal: Mutex::new(None),
            calls: registry_calls.clone(),
        });
        registry.driver.ensure_available().unwrap();
        let boundary_calls = Arc::new(Mutex::new(Vec::new()));
        let mut command = Command::new(r"C:\Windows\System32\cmd.exe");
        command.args(["/d", "/c", "ping -n 30 127.0.0.1 >nul"]);
        let limits = ResourceLimits::new(5_000, 256 * 1024 * 1024, 4, 1, 1, 1, 4_096, 5_000);
        let token = PreparedCommandToken::issue_observed_registered(
            command,
            owner,
            TestBoundary::new(boundary_calls.clone()),
            |_: &PersistedBoundary| Ok(()),
            |_, _| Ok(()),
            Some(
                ProcessRegistryRegistration::new(registry.clone(), context).with_pty(
                    TerminalSize::new(80, 24).unwrap(),
                    OutputRetention::new(4_096, 60_000),
                    CapturePersistencePolicy::no_secrets(),
                ),
            ),
            Instant::now() + Duration::from_secs(5),
            limits,
        )
        .unwrap();

        assert!(spawn_owned(token, limits).is_err());
        let terminal = registry.terminal.lock().unwrap().unwrap();
        assert!(registry.driver.binding(terminal).is_err());
        assert_eq!(
            *registry_calls.lock().unwrap(),
            ["prepared", "started", "abort_conpty", "outcome_unknown"]
        );
        assert_eq!(
            *boundary_calls.lock().unwrap(),
            ["release", "kill", "reap", "inspect"]
        );
    }

    struct ContinuouslyReadable;

    impl Read for ContinuouslyReadable {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            bytes.fill(b'x');
            Ok(bytes.len())
        }
    }

    #[test]
    fn continuously_readable_capture_stops_and_joins_when_cancelled() {
        let capture = CaptureThreads::start_cancellable(
            ContinuouslyReadable,
            Cursor::new(Vec::<u8>::new()),
            OutputBudget::new(128).unwrap(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(10));
        let started = Instant::now();
        capture.cancel();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    struct TestBroker {
        calls: AtomicUsize,
        value: &'static [u8],
    }

    impl ExecutorSecretBroker for TestBroker {
        fn authorize_and_resolve(
            &self,
            _handle: &crate::domain::secret::SecretHandle,
            context: &ExecutorSecretContext<'_>,
        ) -> Result<crate::domain::secret::SecretLease, SecretBrokerError> {
            assert_eq!(context.grant(), crate::domain::config::Grant::ProcessSpawn);
            assert_eq!(context.owner(), context.claim().owner);
            assert_eq!(context.process_id(), context.claim().process_id);
            match context.owner() {
                ProcessOwnership::Attempt(owner) => {
                    assert_eq!(context.principal(), Some(owner.principal_id));
                    assert_eq!(context.fence(), Some(owner.fencing_token));
                }
                ProcessOwnership::DaemonService(_) => {
                    assert_eq!(context.principal(), None);
                    assert_eq!(context.fence(), None);
                }
            }
            assert_eq!(context.acquisition_id(), "test-acquisition");
            assert_eq!(context.workspace_identity(), "test-workspace");
            assert_eq!(context.profile_digest(), "test-profile");
            assert_eq!(context.invocation_intent(), "test-spawn");
            assert!(context.program().is_absolute());
            assert!(Instant::now() < context.deadline());
            assert!(Instant::now() < context.expires_at());
            self.calls.fetch_add(1, Ordering::AcqRel);
            Ok(crate::domain::secret::SecretLease::new(self.value))
        }
    }

    fn secret_token(
        owner: ProcessOwnership,
        command: Command,
        modes: impl IntoIterator<Item = CredentialInjectionMode>,
        expires_at: Instant,
    ) -> PreparedCommandToken {
        let program = std::path::PathBuf::from(command.get_program());
        PreparedCommandToken::issue(
            command,
            owner,
            TestBoundary::new(Arc::new(Mutex::new(Vec::new()))),
            |_: &PersistedBoundary| Ok(()),
            Instant::now() + Duration::from_secs(2),
            limits(256),
        )
        .unwrap()
        .bind_secrets(SecretSpawnPlan::for_test(modes, owner, expires_at, program))
    }

    static CUSTODY_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    #[test]
    fn stdio_environment_secret_exists_only_in_spawned_child() {
        let variable =
            format!("KIT_MCP_CHILD_ONLY_{}", ProcessId::generate().unwrap()).replace('-', "_");
        assert!(std::env::var_os(&variable).is_none());
        let owner = ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &format!("printf '%s\\n' \"${variable}\"")]);
        let token = PreparedCommandToken::issue(
            command,
            owner,
            TestBoundary::new(Arc::new(Mutex::new(Vec::new()))),
            |_: &PersistedBoundary| Ok(()),
            Instant::now() + Duration::from_secs(2),
            limits(256),
        )
        .unwrap();
        let environment = [(
            variable.clone(),
            crate::domain::secret::SecretLease::new(b"child-only-canary".to_vec()),
        )];
        let child = OwnedStdioChild::spawn_with_environment(token, 256, &environment).unwrap();
        assert_eq!(
            child.receive_frame().unwrap().unwrap(),
            b"child-only-canary"
        );
        child.close_and_reap().unwrap();
        assert!(std::env::var_os(variable).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn quiescent_fake_boundary_releases_secret_custody_after_cancellation() {
        let _guard = CUSTODY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = quarantined_secret_custodies();
        let owner = ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap());
        let broker = TestBroker {
            calls: AtomicUsize::new(0),
            value: b"released-custody",
        };
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "while :; do :; done"]);
        let token = secret_token(
            owner,
            command,
            [CredentialInjectionMode::FileDescriptor],
            Instant::now() + Duration::from_secs(2),
        );
        let process = spawn_owned_with_broker(token, limits(256), &broker).unwrap();
        drop(process);
        assert_eq!(quarantined_secret_custodies(), before);
    }

    #[cfg(unix)]
    #[test]
    fn outcome_unknown_fake_boundary_quarantines_secret_custody() {
        let _guard = CUSTODY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = quarantined_secret_custodies();
        let owner = ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "true"]);
        let program = std::path::PathBuf::from(command.get_program());
        let token = PreparedCommandToken::issue(
            command,
            owner,
            TestBoundary::outcome_unknown(Arc::new(Mutex::new(Vec::new()))),
            |_: &PersistedBoundary| Ok(()),
            Instant::now() + Duration::from_secs(2),
            limits(256),
        )
        .unwrap()
        .bind_secrets(SecretSpawnPlan::for_test(
            [CredentialInjectionMode::FileDescriptor],
            owner,
            Instant::now() + Duration::from_secs(2),
            program,
        ));
        let broker = TestBroker {
            calls: AtomicUsize::new(0),
            value: b"quarantined-custody",
        };
        let mut process = spawn_owned_with_broker(token, limits(256), &broker).unwrap();
        assert!(process.wait().is_err());
        drop(process);
        assert_eq!(quarantined_secret_custodies(), before + 1);
    }

    #[cfg(unix)]
    #[test]
    fn started_registry_error_quarantines_custody_when_cleanup_is_unconfirmed() {
        let _guard = CUSTODY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut expected = quarantined_secret_custodies();
        for failure in ["kill", "reap", "inspect"] {
            let context = registry_context();
            let owner = ProcessOwnership::Attempt(AttemptOwnership::new(
                AttemptId::generate().unwrap(),
                context.principal_id,
                FencingToken::new(1),
            ));
            let calls = Arc::new(Mutex::new(Vec::new()));
            let registry = Arc::new(TestRegistry {
                calls: calls.clone(),
                fail_started: true,
                fail_exited: false,
            });
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "while :; do :; done"]);
            let program = std::path::PathBuf::from(command.get_program());
            let token = PreparedCommandToken::issue_observed_registered(
                command,
                owner,
                TestBoundary::failing(Arc::new(Mutex::new(Vec::new())), failure),
                |_: &PersistedBoundary| Ok(()),
                |_, _| Ok(()),
                Some(ProcessRegistryRegistration::new(registry, context)),
                Instant::now() + Duration::from_secs(2),
                limits(256),
            )
            .unwrap()
            .bind_secrets(SecretSpawnPlan::for_test(
                [CredentialInjectionMode::FileDescriptor],
                owner,
                Instant::now() + Duration::from_secs(2),
                program,
            ));
            let broker = TestBroker {
                calls: AtomicUsize::new(0),
                value: b"started-registry-fault",
            };

            assert!(spawn_owned_with_broker(token, limits(256), &broker).is_err());
            expected += 1;
            assert_eq!(quarantined_secret_custodies(), expected);
            assert_eq!(calls.lock().unwrap().last(), Some(&"outcome_unknown"));
        }
    }

    #[test]
    fn denied_and_stale_contexts_never_resolve_a_handle() {
        let owner = ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap());
        let broker = TestBroker {
            calls: AtomicUsize::new(0),
            value: b"never-resolved",
        };
        let denied = secret_token(
            owner,
            Command::new("true"),
            [CredentialInjectionMode::FileDescriptor],
            Instant::now() + Duration::from_secs(1),
        )
        .bind_secrets(
            SecretSpawnPlan::for_test(
                [CredentialInjectionMode::FileDescriptor],
                owner,
                Instant::now() + Duration::from_secs(1),
                "true",
            )
            .authorize(crate::executor::secrets::SecretAuthorization::new(
                owner,
                crate::domain::config::Grant::WorkspaceRead,
                Instant::now() + Duration::from_secs(1),
            )),
        );
        assert!(spawn_owned_with_broker(denied, limits(256), &broker).is_err());

        let stale = secret_token(
            owner,
            Command::new("true"),
            [CredentialInjectionMode::ScopedEnvironment {
                variable: "KIT_SECRET".to_owned(),
            }],
            Instant::now(),
        );
        assert!(spawn_owned_with_broker(stale, limits(256), &broker).is_err());
        assert_eq!(broker.calls.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn authorized_owned_spawn_keeps_custody_and_redacts_public_surfaces() {
        const CANARY: &[u8] = b"owned-spawn-canary";
        let owner = ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap());
        let broker = TestBroker {
            calls: AtomicUsize::new(0),
            value: CANARY,
        };
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "cat <&100; test -z \"${KIT_SECRET+x}\" || exit 90; test -z \"${INHERITED_CANARY+x}\" || exit 91; KIT_SECRET=\"$(cat <&101)\" /bin/sh -c 'printf %s \"$KIT_SECRET\"'",
        ]);
        command.env("INHERITED_CANARY", std::str::from_utf8(CANARY).unwrap());
        let token = secret_token(
            owner,
            command,
            [
                CredentialInjectionMode::FileDescriptor,
                CredentialInjectionMode::ScopedEnvironment {
                    variable: "KIT_SECRET".to_owned(),
                },
            ],
            Instant::now() + Duration::from_secs(2),
        );
        assert!(!format!("{token:?}").contains(std::str::from_utf8(CANARY).unwrap()));
        let mut process = spawn_owned_with_broker(token, limits(256), &broker).unwrap();
        assert_eq!(process.secret_bindings().len(), 2);
        let output = process.wait().unwrap();
        assert_eq!(output.stdout.raw_bytes(), [CANARY, CANARY].concat());
        let sanitized = process.sanitize_capture(CaptureBoundary::Log, output.stdout.raw_bytes());
        assert!(
            !String::from_utf8_lossy(sanitized.bytes().unwrap())
                .contains(std::str::from_utf8(CANARY).unwrap())
        );
        assert!(!format!("{output:?}").contains(std::str::from_utf8(CANARY).unwrap()));
        assert_eq!(broker.calls.load(Ordering::Acquire), 2);
    }

    #[cfg(unix)]
    #[test]
    fn terminal_rejects_general_redaction_and_accepts_process_bound_streaming_capture() {
        const CANARY: &[u8] = b"process-bound-canary";
        let principal_id = PrincipalId::generate().unwrap();
        let project_id = ProjectId::generate().unwrap();
        let owner = ProcessOwnership::Attempt(AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal_id,
            FencingToken::new(1),
        ));
        let broker = TestBroker {
            calls: AtomicUsize::new(0),
            value: CANARY,
        };
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "test -z \"${KIT_SECRET+x}\" || exit 90; KIT_SECRET=\"$(cat <&100)\" /bin/sh -c 'printf %s \"$KIT_SECRET\"'",
        ]);
        let token = secret_token(
            owner,
            command,
            [CredentialInjectionMode::ScopedEnvironment {
                variable: "KIT_SECRET".to_owned(),
            }],
            Instant::now() + Duration::from_secs(2),
        );
        let claim = ProcessClaim::new(token.process_id(), owner);
        let mut process = spawn_owned_with_broker(token, limits(256), &broker).unwrap();
        let policy = process.capture_persistence_policy();
        let output = process.wait().unwrap();

        let principal = LocalPeerAuthenticator::new(BTreeMap::from([(
            1_000,
            GrantSnapshot::new(principal_id, project_id, [Grant::ProcessSpawn]),
        )]))
        .authenticate(&LocalPeerObservation::from_transport(1_000, 7, 1_000))
        .unwrap();
        let manager = TerminalManager::new(
            project_id,
            FakePtyDriver::default(),
            (|_: &TerminalSnapshot| Ok(())) as fn(&TerminalSnapshot) -> io::Result<()>,
        );
        let TerminalAllocation::Pty { control, .. } = manager
            .allocate(
                TerminalRequest::pty(policy),
                &principal,
                claim,
                "process-boundary",
                TerminalSize::new(80, 24).unwrap(),
                OutputRetention::new(256, 1_000),
            )
            .unwrap()
        else {
            unreachable!()
        };

        let attacker = CaptureRedactor::new(&[])
            .sanitize(CaptureBoundary::TerminalMetadata, output.stdout.raw_bytes());
        assert!(matches!(
            manager.append_output(&control, &attacker, 1),
            Err(TerminalError::UnsanitizedCapture)
        ));

        let mut capture = process.start_sanitized_capture(CaptureBoundary::TerminalMetadata);
        capture.push(&CANARY[..7]).unwrap();
        capture.push(&CANARY[7..]).unwrap();
        capture.finish().unwrap();
        manager.append_output(&control, &capture, 1).unwrap();
        assert!(
            !serde_json::to_string(&manager.snapshot(&control).unwrap())
                .unwrap()
                .contains(std::str::from_utf8(CANARY).unwrap())
        );
        assert_eq!(
            format!("{policy:?}"),
            "CapturePersistencePolicy(Some(SanitizerProvenance(REDACTED)))"
        );
    }

    #[cfg(unix)]
    type NativeTestManager =
        TerminalManager<NativePtyDriver, fn(&TerminalSnapshot) -> io::Result<()>>;

    #[cfg(unix)]
    struct NativeTerminalRegistry {
        manager: Arc<NativeTestManager>,
        control: Mutex<Option<TerminalControl>>,
        now: AtomicU64,
    }

    #[cfg(unix)]
    fn discard_terminal_snapshot(_: &TerminalSnapshot) -> io::Result<()> {
        Ok(())
    }

    #[cfg(unix)]
    impl ProcessRegistry for NativeTerminalRegistry {
        fn prepared(
            &self,
            _context: ProcessRegistrationContext,
            claim: ProcessClaim,
            boundary: &PersistedBoundary,
            terminal: ProcessTerminalConfig,
        ) -> io::Result<()> {
            let TerminalAllocation::Pty { control, .. } = self
                .manager
                .allocate_registered(
                    terminal.request,
                    claim,
                    boundary.encode(),
                    terminal.size,
                    terminal.retention,
                )
                .map_err(io::Error::other)?
            else {
                return Err(io::Error::other("expected PTY allocation"));
            };
            *self.control.lock().unwrap() = Some(control);
            Ok(())
        }

        fn bind_terminal(
            &self,
            _context: ProcessRegistrationContext,
            _process_id: ProcessId,
            command: &mut Command,
        ) -> io::Result<Box<dyn Read + Send>> {
            let control = self.control.lock().unwrap();
            self.manager
                .bind_process(control.as_ref().expect("terminal prepared"), command)
                .map_err(io::Error::other)
        }

        fn append_terminal_output(
            &self,
            _context: ProcessRegistrationContext,
            _process_id: ProcessId,
            capture: &SanitizedCapture,
        ) -> io::Result<()> {
            let control = self.control.lock().unwrap();
            self.manager
                .append_output(
                    control.as_ref().expect("terminal prepared"),
                    capture,
                    self.now.fetch_add(1, Ordering::Relaxed),
                )
                .map(|_| ())
                .map_err(io::Error::other)
        }

        fn close_terminal(
            &self,
            _context: ProcessRegistrationContext,
            _process_id: ProcessId,
        ) -> io::Result<()> {
            let control = self.control.lock().unwrap();
            self.manager
                .close(control.as_ref().expect("terminal prepared"))
                .map_err(io::Error::other)
        }

        fn started(
            &self,
            _context: ProcessRegistrationContext,
            _record: &ProcessRecord,
        ) -> io::Result<()> {
            Ok(())
        }

        fn exited(
            &self,
            _context: ProcessRegistrationContext,
            _record: &ProcessRecord,
        ) -> io::Result<()> {
            Ok(())
        }

        fn outcome_unknown(
            &self,
            _context: ProcessRegistrationContext,
            _process_id: ProcessId,
        ) -> io::Result<()> {
            let control = self.control.lock().unwrap();
            if let Some(control) = control.as_ref() {
                self.manager.interrupt(control).map_err(io::Error::other)?;
            }
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_pty_is_bound_to_owned_child_and_retains_history_after_restart() {
        let principal_id = PrincipalId::generate().unwrap();
        let project_id = ProjectId::generate().unwrap();
        let attempt_id = AttemptId::generate().unwrap();
        let owner = ProcessOwnership::Attempt(AttemptOwnership::new(
            attempt_id,
            principal_id,
            FencingToken::new(1),
        ));
        let principal = LocalPeerAuthenticator::new(BTreeMap::from([(
            1_000,
            GrantSnapshot::new(
                principal_id,
                project_id,
                [Grant::ProcessSpawn, Grant::WorkspaceRead],
            ),
        )]))
        .authenticate(&LocalPeerObservation::from_transport(1_000, 7, 1_000))
        .unwrap();
        let manager = Arc::new(TerminalManager::new(
            project_id,
            NativePtyDriver::new(),
            discard_terminal_snapshot as fn(&TerminalSnapshot) -> io::Result<()>,
        ));
        let registry = Arc::new(NativeTerminalRegistry {
            manager: Arc::clone(&manager),
            control: Mutex::new(None),
            now: AtomicU64::new(1),
        });
        let context = ProcessRegistrationContext {
            project_id,
            principal_id,
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let path =
            std::env::temp_dir().join(format!("kit-native-pty-{}", ProcessId::generate().unwrap()));
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            &format!(
                "stty raw -echo; dd of={} bs=4 count=1 2>/dev/null; stty size",
                path.display()
            ),
        ]);
        let limits = limits(4_096);
        let observer: Arc<dyn ProcessRegistry> = registry.clone();
        let registration = ProcessRegistryRegistration::new(observer, context).with_pty(
            TerminalSize::new(80, 24).unwrap(),
            OutputRetention::new(4_096, 60_000),
            CapturePersistencePolicy::no_secrets(),
        );
        let token = PreparedCommandToken::issue_observed_registered(
            command,
            owner,
            TestBoundary::new(Arc::clone(&calls)),
            |_: &PersistedBoundary| Ok(()),
            |_, _| Ok(()),
            Some(registration),
            Instant::now() + Duration::from_secs(5),
            limits,
        )
        .unwrap();
        let mut process = spawn_owned(token, limits).unwrap();
        let mut writer = {
            let control = registry.control.lock().unwrap();
            manager
                .claim_writer(control.as_ref().unwrap(), &principal, 1, 5_000)
                .unwrap()
        };
        manager
            .resize(&writer, TerminalSize::new(100, 40).unwrap(), 2)
            .unwrap();
        manager
            .write_input(&writer, &[0, 0xff, b'A', b'\n'], 2)
            .unwrap();
        let output = process.wait().unwrap();
        assert!(matches!(
            process.record().state(),
            ProcessState::Exited { .. }
        ));
        assert_eq!(std::fs::read(&path).unwrap(), [0, 0xff, b'A', b'\n']);
        assert!(String::from_utf8_lossy(output.stdout.raw_bytes()).contains("40 100"));
        let snapshot = {
            let control = registry.control.lock().unwrap();
            manager.snapshot(control.as_ref().unwrap()).unwrap()
        };
        assert_eq!(snapshot.lifecycle, TerminalLifecycle::Exited);
        assert!(!snapshot.output.is_empty());
        assert_eq!(*calls.lock().unwrap(), ["release", "reap", "inspect"]);

        let restored = TerminalManager::new(
            project_id,
            NativePtyDriver::new(),
            discard_terminal_snapshot as fn(&TerminalSnapshot) -> io::Result<()>,
        );
        let controls = restored
            .restore_snapshots([snapshot], 100, |_| Ok(()))
            .unwrap();
        let viewer = restored.attach_viewer(&controls[0], &principal).unwrap();
        assert!(matches!(
            restored.read_output(&viewer, 1, 100).unwrap(),
            OutputRead::Chunks { ref chunks, .. }
                if chunks.iter().any(|chunk| String::from_utf8_lossy(chunk.bytes()).contains("40 100"))
        ));
        assert!(matches!(
            restored.claim_writer(&controls[0], &principal, 100, 1_000),
            Err(TerminalError::TerminalInactive)
        ));
        assert!(matches!(
            restored.write_input(&writer, b"not persisted", 100),
            Err(TerminalError::PermissionDenied)
        ));
        writer = viewer;
        restored.detach(&mut writer).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn production_exec_registry_drives_pty_api_and_restart_history_end_to_end() {
        let root = std::env::temp_dir().join(format!(
            "kit-native-pty-api-{}",
            ProcessId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("state.sqlite3");
        let input_path = root.join("input.bin");
        let project_id = ProjectId::generate().unwrap();
        let principal_id = PrincipalId::generate().unwrap();
        let owner = ProcessOwnership::Attempt(AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal_id,
            FencingToken::new(9),
        ));
        let principal = LocalPeerAuthenticator::new(BTreeMap::from([(
            1_000,
            GrantSnapshot::new(
                principal_id,
                project_id,
                [Grant::ProcessSpawn, Grant::WorkspaceRead],
            ),
        )]))
        .authenticate(&LocalPeerObservation::from_transport(1_000, 7, 1_000))
        .unwrap();
        let snapshots = SqliteTerminalSnapshotStore::open(&database).unwrap();
        let service = Arc::new(
            ManagerExecService::open(
                &database,
                TerminalManager::new(project_id, NativePtyDriver::new(), snapshots.clone()),
                SqliteCancellationCoordinator::new(&database),
            )
            .unwrap(),
        );
        let context = ProcessRegistrationContext {
            project_id,
            principal_id,
        };
        let registry: Arc<dyn ProcessRegistry> = service.clone();
        let registration = ProcessRegistryRegistration::new(registry, context).with_pty(
            TerminalSize::new(80, 24).unwrap(),
            OutputRetention::new(4_096, 60_000),
            CapturePersistencePolicy::no_secrets(),
        );
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            &format!(
                "stty raw -echo; dd of={} bs=4 count=1 2>/dev/null; stty size; printf api-ready",
                input_path.display()
            ),
        ]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let limits = limits(4_096);
        let token = PreparedCommandToken::issue_observed_registered(
            command,
            owner,
            TestBoundary::new(Arc::clone(&calls)),
            |_: &PersistedBoundary| Ok(()),
            |_, _| Ok(()),
            Some(registration),
            Instant::now() + Duration::from_secs(5),
            limits,
        )
        .unwrap();
        let process_id = token.process_id();
        let mut process = spawn_owned(token, limits).unwrap();
        assert_eq!(
            service.get_process(&principal, process_id).unwrap()["terminal_transport"],
            "pty"
        );
        let allocation = AllocateTerminalBody {
            columns: 80,
            rows: 24,
            max_output_bytes: 4_096,
            max_output_age_millis: 60_000,
        };
        let allocation_response = service
            .allocate_terminal(
                &principal,
                process_id,
                &IdempotencyKey::parse("native-allocate").unwrap(),
                allocation,
            )
            .unwrap();
        assert_eq!(allocation_response["changed"], false);
        let terminal_id = allocation_response["resource"]["terminal_id"]
            .as_str()
            .and_then(|value| TerminalId::parse(value).ok())
            .unwrap();
        let attachment_id = service
            .claim_writer(
                &principal,
                terminal_id,
                &IdempotencyKey::parse("native-writer").unwrap(),
                WriterLeaseBody {
                    lease_millis: 5_000,
                },
            )
            .unwrap()["resource"]["attachment_id"]
            .as_str()
            .unwrap()
            .to_owned();
        service
            .resize(
                &principal,
                &attachment_id,
                &IdempotencyKey::parse("native-resize").unwrap(),
                TerminalResizeBody {
                    columns: 100,
                    rows: 40,
                },
            )
            .unwrap();
        service
            .write_input(
                &principal,
                &attachment_id,
                &IdempotencyKey::parse("native-input").unwrap(),
                &[0, 0xff, b'Z', b'\n'],
            )
            .unwrap();
        process.wait().unwrap();
        assert_eq!(std::fs::read(&input_path).unwrap(), [0, 0xff, b'Z', b'\n']);
        let live_history = service.read_output(&principal, &attachment_id, 1).unwrap();
        assert!(
            live_history["chunks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|chunk| {
                    chunk["bytes"].as_array().is_some_and(|bytes| {
                        bytes.windows(9).any(|window| {
                            window
                                == b"api-ready"
                                    .iter()
                                    .map(|byte| serde_json::Value::from(*byte))
                                    .collect::<Vec<_>>()
                        })
                    })
                })
        );
        assert_eq!(*calls.lock().unwrap(), ["release", "reap", "inspect"]);

        let persisted = snapshots.load().unwrap();
        drop(process);
        drop(service);
        let restored_store = SqliteTerminalSnapshotStore::open(&database).unwrap();
        let restored_manager =
            TerminalManager::new(project_id, NativePtyDriver::new(), restored_store);
        let controls = restored_manager
            .restore_snapshots(
                persisted.clone(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
                |_| Ok(()),
            )
            .unwrap();
        let restarted = ManagerExecService::open(
            &database,
            restored_manager,
            SqliteCancellationCoordinator::new(&database),
        )
        .unwrap();
        for (control, snapshot) in controls.into_iter().zip(&persisted) {
            restarted.restore_terminal(control, snapshot).unwrap();
        }
        assert_eq!(
            restarted.get_process(&principal, process_id).unwrap()["terminal_transport"],
            "pty"
        );
        assert!(matches!(
            restarted.get_attachment(&principal, &attachment_id),
            Err(ExecError::NotFound)
        ));
        let replacement_id = restarted
            .attach_viewer(
                &principal,
                terminal_id,
                &IdempotencyKey::parse("replacement-viewer").unwrap(),
            )
            .unwrap()["resource"]["attachment_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let retained = restarted
            .read_output(&principal, &replacement_id, 1)
            .unwrap();
        assert!(!retained["chunks"].as_array().unwrap().is_empty());
        let retained_resizes = restarted
            .read_resizes(&principal, &replacement_id, 1)
            .unwrap();
        assert_eq!(retained_resizes["events"][0]["size"]["columns"], 100);
        assert!(matches!(
            restarted.claim_writer(
                &principal,
                terminal_id,
                &IdempotencyKey::parse("restart-writer").unwrap(),
                WriterLeaseBody { lease_millis: 100 },
            ),
            Err(ExecError::Conflict(_))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_drop_and_descriptor_reuse_never_rebinds_secret_custody() {
        let broker = Arc::new(TestBroker {
            calls: AtomicUsize::new(0),
            value: b"descriptor-canary",
        });
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let broker = broker.clone();
                scope.spawn(move || {
                    let owner =
                        ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap());
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", "cat <&100"]);
                    let token = secret_token(
                        owner,
                        command,
                        [CredentialInjectionMode::FileDescriptor],
                        Instant::now() + Duration::from_secs(2),
                    );
                    let mut process =
                        spawn_owned_with_broker(token, limits(256), broker.as_ref()).unwrap();
                    assert_eq!(
                        process.wait().unwrap().stdout.raw_bytes(),
                        b"descriptor-canary"
                    );
                    drop(process);
                    let files = (0..128)
                        .map(|_| std::fs::File::open("/dev/null").unwrap())
                        .collect::<Vec<_>>();
                    drop(files);
                });
            }
        });
        assert_eq!(broker.calls.load(Ordering::Acquire), 8);
    }
}
