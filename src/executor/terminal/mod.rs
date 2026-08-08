use std::{
    collections::{HashMap, VecDeque},
    fmt, io,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Mutex,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{ConPtyBinding, NativePtyDriver};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::{
    api::auth::contract::AuthenticatedPrincipal,
    domain::{
        config::Grant,
        ids::{AttemptId, PrincipalId, ProcessId, ProjectId, TerminalId},
        lifecycle::{FencingToken, ProcessClaim, ProcessOwnership},
    },
    telemetry::redact::{CaptureBoundary, CapturePersistencePolicy, SanitizedCapture},
};

const MAX_RETAINED_OUTPUT_CHUNKS: usize = 1_024;
const MAX_RETAINED_RESIZES: usize = 1_024;
const MAX_ATTACHMENTS: usize = 1_024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalTransport {
    #[default]
    Pipes,
    Pty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalRequest {
    pub transport: TerminalTransport,
    capture_policy: CapturePersistencePolicy,
}

impl TerminalRequest {
    pub const fn pty(capture_policy: CapturePersistencePolicy) -> Self {
        Self {
            transport: TerminalTransport::Pty,
            capture_policy,
        }
    }
}

impl Default for TerminalRequest {
    fn default() -> Self {
        Self {
            transport: TerminalTransport::Pipes,
            capture_policy: CapturePersistencePolicy::no_secrets(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TerminalOwner {
    pub project_id: ProjectId,
    pub process_id: ProcessId,
    pub attempt_id: AttemptId,
    pub principal_id: PrincipalId,
    pub process_fence: FencingToken,
    pub boundary_id: String,
}

impl TerminalOwner {
    fn from_claim(
        project_id: ProjectId,
        claim: ProcessClaim,
        boundary_id: impl Into<String>,
    ) -> Result<Self, TerminalError> {
        let boundary_id = boundary_id.into();
        if boundary_id.trim().is_empty() {
            return Err(TerminalError::InvalidRequest("boundary identity is empty"));
        }
        let ProcessOwnership::Attempt(owner) = claim.owner else {
            return Err(TerminalError::InvalidRequest(
                "a PTY must be owned by an attempt process",
            ));
        };
        Ok(Self {
            project_id,
            process_id: claim.process_id,
            attempt_id: owner.attempt_id,
            principal_id: owner.principal_id,
            process_fence: owner.fencing_token,
            boundary_id,
        })
    }

    const fn process_claim(&self) -> ProcessClaim {
        ProcessClaim::new(
            self.process_id,
            ProcessOwnership::Attempt(crate::domain::lifecycle::AttemptOwnership::new(
                self.attempt_id,
                self.principal_id,
                self.process_fence,
            )),
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
}

impl TerminalSize {
    pub fn new(columns: u16, rows: u16) -> Result<Self, TerminalError> {
        if columns == 0 || rows == 0 {
            return Err(TerminalError::InvalidRequest(
                "terminal dimensions must be non-zero",
            ));
        }
        Ok(Self { columns, rows })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutputRetention {
    pub max_bytes: usize,
    pub max_age_millis: u64,
}

impl OutputRetention {
    pub const fn new(max_bytes: usize, max_age_millis: u64) -> Self {
        Self {
            max_bytes,
            max_age_millis,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalLifecycle {
    Allocating,
    Active,
    Exited,
    Interrupted,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutputChunk {
    pub sequence: u64,
    pub recorded_at_millis: u64,
    bytes: Vec<u8>,
}

impl OutputChunk {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for OutputChunk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutputChunk")
            .field("sequence", &self.sequence)
            .field("recorded_at_millis", &self.recorded_at_millis)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResizeEvent {
    pub sequence: u64,
    pub recorded_at_millis: u64,
    pub size: TerminalSize,
    pub writer_epoch: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WriterMetadata {
    pub principal_id: PrincipalId,
    pub epoch: u64,
    pub expires_at_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TerminalSnapshot {
    pub terminal_id: TerminalId,
    pub owner: TerminalOwner,
    pub lifecycle: TerminalLifecycle,
    pub resumable: bool,
    pub size: TerminalSize,
    pub retention: OutputRetention,
    pub next_output_sequence: u64,
    pub next_resize_sequence: u64,
    pub retained_output_bytes: usize,
    pub writer: Option<WriterMetadata>,
    pub output: Vec<OutputChunk>,
    pub resizes: Vec<ResizeEvent>,
}

pub trait TerminalSnapshotStore: Send + Sync + 'static {
    /// Persists terminal metadata and retained output. Raw terminal input is never supplied.
    fn save(&self, snapshot: &TerminalSnapshot) -> io::Result<()>;
}

#[derive(Clone, Debug)]
pub struct SqliteTerminalSnapshotStore {
    database: PathBuf,
}

impl SqliteTerminalSnapshotStore {
    pub fn open(database: impl Into<PathBuf>) -> io::Result<Self> {
        let store = Self {
            database: database.into(),
        };
        store.connection()?;
        Ok(store)
    }

    pub fn load(&self) -> io::Result<Vec<TerminalSnapshot>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT snapshot FROM executor_terminal_snapshots ORDER BY terminal_id")
            .map_err(sqlite_io)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_io)?
            .map(|row| {
                let encoded = row.map_err(sqlite_io)?;
                serde_json::from_str(&encoded).map_err(io::Error::other)
            })
            .collect()
    }

    fn connection(&self) -> io::Result<Connection> {
        let connection = Connection::open(&self.database).map_err(sqlite_io)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS executor_terminal_snapshots (
                   terminal_id TEXT PRIMARY KEY,
                   snapshot TEXT NOT NULL
                 );",
            )
            .map_err(sqlite_io)?;
        Ok(connection)
    }
}

impl TerminalSnapshotStore for SqliteTerminalSnapshotStore {
    fn save(&self, snapshot: &TerminalSnapshot) -> io::Result<()> {
        let encoded = serde_json::to_string(snapshot).map_err(io::Error::other)?;
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO executor_terminal_snapshots (terminal_id, snapshot)
                 VALUES (?1, ?2)
                 ON CONFLICT(terminal_id) DO UPDATE SET snapshot=excluded.snapshot",
                params![snapshot.terminal_id.to_string(), encoded],
            )
            .map_err(sqlite_io)?;
        connection
            .execute(
                "DELETE FROM executor_terminal_snapshots WHERE rowid IN (
                   SELECT rowid FROM executor_terminal_snapshots ORDER BY rowid DESC
                   LIMIT -1 OFFSET 4096
                 )",
                [],
            )
            .map_err(sqlite_io)?;
        Ok(())
    }
}

fn sqlite_io(error: rusqlite::Error) -> io::Error {
    io::Error::other(error.to_string())
}

impl<F> TerminalSnapshotStore for F
where
    F: Fn(&TerminalSnapshot) -> io::Result<()> + Send + Sync + 'static,
{
    fn save(&self, snapshot: &TerminalSnapshot) -> io::Result<()> {
        self(snapshot)
    }
}

pub trait PtyDriver: Send + Sync + 'static {
    fn ensure_available(&self) -> Result<(), TerminalError> {
        Ok(())
    }

    fn allocate(
        &self,
        terminal_id: TerminalId,
        owner: &TerminalOwner,
        size: TerminalSize,
    ) -> Result<(), TerminalError>;
    fn write_input(&self, terminal_id: TerminalId, bytes: &[u8]) -> io::Result<()>;
    fn resize(&self, terminal_id: TerminalId, size: TerminalSize) -> io::Result<()>;
    fn interrupt(&self, terminal_id: TerminalId) -> io::Result<()>;
    fn bind_process(
        &self,
        _terminal_id: TerminalId,
        _command: &mut Command,
    ) -> io::Result<Box<dyn io::Read + Send>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "PTY driver cannot bind an owned process",
        ))
    }
    #[cfg(windows)]
    fn conpty_binding(&self, _terminal_id: TerminalId) -> io::Result<ConPtyBinding> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "PTY driver does not provide a ConPTY binding",
        ))
    }
}

/// The standard library has no portable PTY allocator. Platform integration must provide a
/// driver rather than silently substituting pipes for a requested PTY.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativePtyUnavailable;

impl NativePtyUnavailable {
    pub const fn new() -> Self {
        Self
    }
}

impl PtyDriver for NativePtyUnavailable {
    fn ensure_available(&self) -> Result<(), TerminalError> {
        Err(TerminalError::PlatformUnavailable)
    }

    fn allocate(
        &self,
        _terminal_id: TerminalId,
        _owner: &TerminalOwner,
        _size: TerminalSize,
    ) -> Result<(), TerminalError> {
        Err(TerminalError::PlatformUnavailable)
    }

    fn write_input(&self, _terminal_id: TerminalId, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "PTY unavailable",
        ))
    }

    fn resize(&self, _terminal_id: TerminalId, _size: TerminalSize) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "PTY unavailable",
        ))
    }

    fn interrupt(&self, _terminal_id: TerminalId) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct NativePty {
    master: File,
    slave: Option<File>,
}

/// Unix PTY pairs keyed by the terminal that owns them. The slave is consumed exactly once by
/// `bind_process`; the master remains the sole input/resize/output endpoint in the daemon.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Default)]
pub struct NativePtyDriver {
    ptys: Mutex<HashMap<TerminalId, NativePty>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl NativePtyDriver {
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl PtyDriver for NativePtyDriver {
    fn allocate(
        &self,
        terminal_id: TerminalId,
        _owner: &TerminalOwner,
        size: TerminalSize,
    ) -> Result<(), TerminalError> {
        let pty = open_native_pty(size, |_| Ok(())).map_err(TerminalError::Driver)?;
        let mut ptys = self
            .ptys
            .lock()
            .map_err(|_| TerminalError::Driver(io::Error::other("native PTY lock poisoned")))?;
        if ptys.contains_key(&terminal_id) {
            return Err(TerminalError::Driver(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal already owns a PTY",
            )));
        }
        ptys.insert(terminal_id, pty);
        Ok(())
    }

    fn write_input(&self, terminal_id: TerminalId, bytes: &[u8]) -> io::Result<()> {
        use io::Write as _;
        let mut ptys = self
            .ptys
            .lock()
            .map_err(|_| io::Error::other("native PTY lock poisoned"))?;
        let master = &mut ptys
            .get_mut(&terminal_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PTY is not active"))?
            .master;
        master.write_all(bytes)
    }

    fn resize(&self, terminal_id: TerminalId, size: TerminalSize) -> io::Result<()> {
        let ptys = self
            .ptys
            .lock()
            .map_err(|_| io::Error::other("native PTY lock poisoned"))?;
        let descriptor = ptys
            .get(&terminal_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PTY is not active"))?
            .master
            .as_raw_fd();
        let dimensions = libc::winsize {
            ws_row: size.rows,
            ws_col: size.columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the master descriptor is live under the map lock and `dimensions` is a valid
        // winsize pointer for this ioctl.
        if unsafe { libc::ioctl(descriptor, libc::TIOCSWINSZ, &dimensions) } == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn interrupt(&self, terminal_id: TerminalId) -> io::Result<()> {
        self.ptys
            .lock()
            .map_err(|_| io::Error::other("native PTY lock poisoned"))?
            .remove(&terminal_id);
        Ok(())
    }

    fn bind_process(
        &self,
        terminal_id: TerminalId,
        command: &mut Command,
    ) -> io::Result<Box<dyn io::Read + Send>> {
        use std::os::unix::process::CommandExt as _;

        let mut ptys = self
            .ptys
            .lock()
            .map_err(|_| io::Error::other("native PTY lock poisoned"))?;
        let pty = ptys
            .get_mut(&terminal_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PTY is not allocated"))?;
        let output = duplicate_cloexec(&pty.master)?;
        let slave = pty
            .slave
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "PTY is already bound"))?;
        let stdin = duplicate_cloexec(slave)?;
        let stdout = duplicate_cloexec(slave)?;
        let stderr = pty.slave.take().expect("checked above");
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        // SAFETY: only async-signal-safe libc calls run after fork. The configured stdin has
        // already been duplicated to fd 0 when this callback executes.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::c_ulong::from(libc::TIOCSCTTY), 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(Box::new(output))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub type NativePtyDriver = NativePtyUnavailable;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_native_pty(
    size: TerminalSize,
    after_open: impl FnOnce(&NativePty) -> io::Result<()>,
) -> io::Result<NativePty> {
    // Unlike `openpty`, these opens set close-on-exec atomically, leaving no fork/exec race.
    // SAFETY: `posix_openpt` takes flags only and returns an owned descriptor.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `posix_openpt` returned a new owned descriptor.
    let master = unsafe { File::from_raw_fd(master) };
    // SAFETY: the master descriptor is live and both calls retain no pointers.
    if unsafe { libc::grantpt(master.as_raw_fd()) } == -1
        || unsafe { libc::unlockpt(master.as_raw_fd()) } == -1
    {
        return Err(io::Error::last_os_error());
    }

    let mut slave_name = [0; libc::PATH_MAX as usize];
    // SAFETY: the master is live and the output buffer is valid for its stated length.
    let result = unsafe {
        ptsname_r(
            master.as_raw_fd(),
            slave_name.as_mut_ptr(),
            slave_name.len(),
        )
    };
    if result != 0 {
        return Err(if result == -1 {
            io::Error::last_os_error()
        } else {
            io::Error::from_raw_os_error(result)
        });
    }
    // SAFETY: `ptsname_r` produced a NUL-terminated pathname in `slave_name`.
    let slave = unsafe {
        libc::open(
            slave_name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` returned a new owned descriptor.
    let slave = unsafe { File::from_raw_fd(slave) };
    let pty = NativePty {
        master,
        slave: Some(slave),
    };
    after_open(&pty)?;
    let dimensions = libc::winsize {
        ws_row: size.rows,
        ws_col: size.columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: the master is live and `dimensions` is a valid winsize pointer.
    if unsafe { libc::ioctl(pty.master.as_raw_fd(), libc::TIOCSWINSZ, &dimensions) } == -1 {
        return Err(io::Error::last_os_error());
    }
    set_fd_nonblocking(pty.master.as_raw_fd())?;
    Ok(pty)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn duplicate_cloexec(file: &File) -> io::Result<File> {
    // SAFETY: the source descriptor is live; F_DUPFD_CLOEXEC returns a new owned descriptor.
    let descriptor = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if descriptor == -1 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `fcntl` returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_fd_nonblocking(descriptor: i32) -> io::Result<()> {
    // SAFETY: `descriptor` is a live PTY master and `fcntl` does not retain pointers.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe extern "C" {
    fn ptsname_r(descriptor: i32, buffer: *mut libc::c_char, length: usize) -> i32;
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct SecretToken([u8; 32]);

impl SecretToken {
    fn generate() -> Result<Self, TerminalError> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| TerminalError::EntropyUnavailable)?;
        Ok(Self(bytes))
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretToken(REDACTED)")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttachmentRole {
    Viewer,
    Writer {
        epoch: u64,
        lease_token: SecretToken,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalAttachment {
    terminal_id: TerminalId,
    principal_id: PrincipalId,
    attachment_token: SecretToken,
    process_token: SecretToken,
    role: AttachmentRole,
}

impl TerminalAttachment {
    pub const fn terminal_id(&self) -> TerminalId {
        self.terminal_id
    }

    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    pub const fn is_writer(&self) -> bool {
        matches!(self.role, AttachmentRole::Writer { .. })
    }

    pub const fn writer_epoch(&self) -> Option<u64> {
        match self.role {
            AttachmentRole::Viewer => None,
            AttachmentRole::Writer { epoch, .. } => Some(epoch),
        }
    }
}

#[derive(Eq, PartialEq)]
pub struct TerminalControl {
    terminal_id: TerminalId,
    control_token: SecretToken,
    process_token: SecretToken,
}

impl fmt::Debug for TerminalControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalControl")
            .field("terminal_id", &self.terminal_id)
            .field("tokens", &"REDACTED")
            .finish()
    }
}

impl TerminalControl {
    pub const fn terminal_id(&self) -> TerminalId {
        self.terminal_id
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum TerminalAllocation {
    Pipes,
    Pty {
        terminal_id: TerminalId,
        control: TerminalControl,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputRead {
    Gap {
        requested: u64,
        oldest_available: u64,
    },
    Chunks {
        chunks: Vec<OutputChunk>,
        next_cursor: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResizeRead {
    Gap {
        requested: u64,
        oldest_available: u64,
    },
    Events {
        events: Vec<ResizeEvent>,
        next_cursor: u64,
    },
}

#[derive(Debug)]
pub enum TerminalError {
    InvalidRequest(&'static str),
    NotFound,
    PermissionDenied,
    ReadOnlyViewer,
    WriterOccupied,
    StaleWriter,
    StaleProcessClaim,
    LeaseExpired,
    TerminalInactive,
    DaemonUnavailable,
    PlatformUnavailable,
    AttachmentLimit,
    InvalidCursor { requested: u64, next: u64 },
    SequenceExhausted,
    EntropyUnavailable,
    Driver(io::Error),
    Persistence(io::Error),
    StatePoisoned,
    UnsanitizedCapture,
}

impl fmt::Display for TerminalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => {
                write!(formatter, "invalid terminal request: {message}")
            }
            Self::NotFound => formatter.write_str("terminal not found"),
            Self::PermissionDenied => formatter.write_str("terminal access denied"),
            Self::ReadOnlyViewer => formatter.write_str("read-only terminal viewers cannot write"),
            Self::WriterOccupied => formatter.write_str("terminal input writer is already leased"),
            Self::StaleWriter => formatter.write_str("stale terminal writer lease"),
            Self::StaleProcessClaim => formatter.write_str("stale terminal process claim"),
            Self::LeaseExpired => formatter.write_str("terminal writer lease expired"),
            Self::TerminalInactive => formatter.write_str("terminal is not active"),
            Self::DaemonUnavailable => formatter.write_str("terminal daemon is unavailable"),
            Self::PlatformUnavailable => {
                formatter.write_str("native PTY execution is unavailable on this platform")
            }
            Self::AttachmentLimit => formatter.write_str("terminal attachment limit reached"),
            Self::InvalidCursor { requested, next } => {
                write!(formatter, "terminal cursor {requested} is ahead of {next}")
            }
            Self::SequenceExhausted => formatter.write_str("terminal sequence exhausted"),
            Self::EntropyUnavailable => formatter.write_str("secure token generation failed"),
            Self::Driver(error) => write!(formatter, "PTY driver failed: {error}"),
            Self::Persistence(error) => write!(formatter, "terminal persistence failed: {error}"),
            Self::StatePoisoned => formatter.write_str("terminal state lock poisoned"),
            Self::UnsanitizedCapture => {
                formatter.write_str("terminal output did not cross the sanitization boundary")
            }
        }
    }
}

impl std::error::Error for TerminalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Driver(error) | Self::Persistence(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct WriterLease {
    principal_id: PrincipalId,
    epoch: u64,
    token: SecretToken,
    expires_at_millis: u64,
}

#[derive(Clone)]
struct TerminalState {
    terminal_id: TerminalId,
    owner: TerminalOwner,
    lifecycle: TerminalLifecycle,
    size: TerminalSize,
    retention: OutputRetention,
    next_output_sequence: u64,
    next_resize_sequence: u64,
    retained_output_bytes: usize,
    writer_epoch: u64,
    writer: Option<WriterLease>,
    viewers: HashMap<SecretToken, PrincipalId>,
    control_token: SecretToken,
    process_token: SecretToken,
    capture_policy: CapturePersistencePolicy,
    output: VecDeque<OutputChunk>,
    resizes: VecDeque<ResizeEvent>,
}

impl TerminalState {
    fn snapshot(&self) -> TerminalSnapshot {
        TerminalSnapshot {
            terminal_id: self.terminal_id,
            owner: self.owner.clone(),
            lifecycle: self.lifecycle,
            resumable: false,
            size: self.size,
            retention: self.retention,
            next_output_sequence: self.next_output_sequence,
            next_resize_sequence: self.next_resize_sequence,
            retained_output_bytes: self.retained_output_bytes,
            writer: self.writer.as_ref().map(|writer| WriterMetadata {
                principal_id: writer.principal_id,
                epoch: writer.epoch,
                expires_at_millis: writer.expires_at_millis,
            }),
            output: self.output.iter().cloned().collect(),
            resizes: self.resizes.iter().copied().collect(),
        }
    }

    fn require_active(&self) -> Result<(), TerminalError> {
        (self.lifecycle == TerminalLifecycle::Active)
            .then_some(())
            .ok_or(TerminalError::TerminalInactive)
    }

    fn authorize_attachment(&self, attachment: &TerminalAttachment) -> Result<(), TerminalError> {
        if attachment.terminal_id == self.terminal_id
            && attachment.process_token == self.process_token
            && self.viewers.get(&attachment.attachment_token) == Some(&attachment.principal_id)
        {
            Ok(())
        } else {
            Err(TerminalError::PermissionDenied)
        }
    }

    fn authorize_control(&self, control: &TerminalControl) -> Result<(), TerminalError> {
        if control.terminal_id == self.terminal_id
            && control.control_token == self.control_token
            && control.process_token == self.process_token
        {
            Ok(())
        } else {
            Err(TerminalError::PermissionDenied)
        }
    }

    fn prune(&mut self, now_millis: u64) {
        while self.output.front().is_some_and(|chunk| {
            chunk.bytes.is_empty()
                || now_millis.saturating_sub(chunk.recorded_at_millis)
                    >= self.retention.max_age_millis
                || self.retained_output_bytes > self.retention.max_bytes
                || self.output.len() > MAX_RETAINED_OUTPUT_CHUNKS
        }) {
            let chunk = self.output.pop_front().expect("front exists");
            self.retained_output_bytes =
                self.retained_output_bytes.saturating_sub(chunk.bytes.len());
        }
        while self.resizes.front().is_some_and(|event| {
            now_millis.saturating_sub(event.recorded_at_millis) >= self.retention.max_age_millis
                || self.resizes.len() > MAX_RETAINED_RESIZES
        }) {
            self.resizes.pop_front();
        }
    }
}

#[derive(Default)]
struct ManagerState {
    daemon_dead: bool,
    terminals: HashMap<TerminalId, TerminalState>,
    current_processes: HashMap<AttemptId, TerminalOwner>,
}

pub struct TerminalManager<D, S> {
    project_id: ProjectId,
    driver: D,
    store: S,
    state: Mutex<ManagerState>,
}

impl<D: PtyDriver, S: TerminalSnapshotStore> TerminalManager<D, S> {
    pub fn new(project_id: ProjectId, driver: D, store: S) -> Self {
        Self {
            project_id,
            driver,
            store,
            state: Mutex::new(ManagerState::default()),
        }
    }

    pub fn ensure_pty_available(&self) -> Result<(), TerminalError> {
        self.driver.ensure_available()
    }

    pub fn allocate(
        &self,
        request: TerminalRequest,
        authenticated: &AuthenticatedPrincipal,
        claim: ProcessClaim,
        boundary_id: impl Into<String>,
        size: TerminalSize,
        retention: OutputRetention,
    ) -> Result<TerminalAllocation, TerminalError> {
        let owner = TerminalOwner::from_claim(self.project_id, claim, boundary_id)?;
        if !request.capture_policy.is_for(claim) {
            return Err(TerminalError::PermissionDenied);
        }
        self.authorize(authenticated, &owner, Grant::ProcessSpawn)?;
        self.allocate_owner(request, owner, size, retention)
    }

    pub(crate) fn allocate_registered(
        &self,
        request: TerminalRequest,
        claim: ProcessClaim,
        boundary_id: impl Into<String>,
        size: TerminalSize,
        retention: OutputRetention,
    ) -> Result<TerminalAllocation, TerminalError> {
        let owner = TerminalOwner::from_claim(self.project_id, claim, boundary_id)?;
        if !request.capture_policy.is_for(claim) {
            return Err(TerminalError::PermissionDenied);
        }
        self.allocate_owner(request, owner, size, retention)
    }

    fn allocate_owner(
        &self,
        request: TerminalRequest,
        owner: TerminalOwner,
        size: TerminalSize,
        retention: OutputRetention,
    ) -> Result<TerminalAllocation, TerminalError> {
        if retention.max_bytes == 0 || retention.max_age_millis == 0 {
            return Err(TerminalError::InvalidRequest(
                "terminal retention must be non-zero",
            ));
        }
        if request.transport == TerminalTransport::Pipes {
            let superseded = self.reserve_process(&owner, None)?;
            self.interrupt_states(&superseded)?;
            return Ok(TerminalAllocation::Pipes);
        }
        self.ensure_pty_available()?;

        let terminal_id = TerminalId::generate().map_err(|_| TerminalError::EntropyUnavailable)?;
        let control_token = SecretToken::generate()?;
        let process_token = SecretToken::generate()?;
        let terminal = TerminalState {
            terminal_id,
            owner: owner.clone(),
            lifecycle: TerminalLifecycle::Allocating,
            size,
            retention,
            next_output_sequence: 1,
            next_resize_sequence: 1,
            retained_output_bytes: 0,
            writer_epoch: 0,
            writer: None,
            viewers: HashMap::new(),
            control_token,
            process_token,
            capture_policy: request.capture_policy,
            output: VecDeque::new(),
            resizes: VecDeque::new(),
        };

        let superseded = self.reserve_process(&owner, Some(terminal))?;
        self.interrupt_states(&superseded)?;

        if let Err(error) = self.driver.allocate(terminal_id, &owner, size) {
            let mut state = self
                .state
                .lock()
                .map_err(|_| TerminalError::StatePoisoned)?;
            state.terminals.remove(&terminal_id);
            return Err(error);
        }

        let activation = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| TerminalError::StatePoisoned)?;
            let is_current = state.current_processes.get(&owner.attempt_id) == Some(&owner);
            let daemon_dead = state.daemon_dead;
            let terminal = state
                .terminals
                .get_mut(&terminal_id)
                .ok_or(TerminalError::NotFound)?;
            if daemon_dead || !is_current || terminal.lifecycle != TerminalLifecycle::Allocating {
                terminal.lifecycle = TerminalLifecycle::Interrupted;
                terminal.writer = None;
                Err(if daemon_dead {
                    TerminalError::DaemonUnavailable
                } else {
                    TerminalError::StaleProcessClaim
                })
            } else {
                terminal.lifecycle = TerminalLifecycle::Active;
                if let Err(error) = self.store.save(&terminal.snapshot()) {
                    terminal.lifecycle = TerminalLifecycle::Interrupted;
                    Err(TerminalError::Persistence(error))
                } else {
                    Ok(())
                }
            }
        };
        if let Err(error) = activation {
            let _ = self.driver.interrupt(terminal_id);
            return Err(error);
        }
        Ok(TerminalAllocation::Pty {
            terminal_id,
            control: TerminalControl {
                terminal_id,
                control_token,
                process_token,
            },
        })
    }

    pub fn attach_viewer(
        &self,
        control: &TerminalControl,
        authenticated: &AuthenticatedPrincipal,
    ) -> Result<TerminalAttachment, TerminalError> {
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let owner = manager
            .terminals
            .get(&control.terminal_id)
            .ok_or(TerminalError::NotFound)?
            .owner
            .clone();
        self.authorize(authenticated, &owner, Grant::WorkspaceRead)?;
        require_current(&manager, &owner)?;
        let state = manager
            .terminals
            .get_mut(&control.terminal_id)
            .expect("checked");
        state.authorize_control(control)?;
        if state.viewers.len() >= MAX_ATTACHMENTS {
            return Err(TerminalError::AttachmentLimit);
        }
        let attachment_token = SecretToken::generate()?;
        state
            .viewers
            .insert(attachment_token, authenticated.principal_id());
        Ok(TerminalAttachment {
            terminal_id: state.terminal_id,
            principal_id: authenticated.principal_id(),
            attachment_token,
            process_token: state.process_token,
            role: AttachmentRole::Viewer,
        })
    }

    pub fn claim_writer(
        &self,
        control: &TerminalControl,
        authenticated: &AuthenticatedPrincipal,
        now_millis: u64,
        lease_millis: u64,
    ) -> Result<TerminalAttachment, TerminalError> {
        let expires_at_millis = lease_deadline(now_millis, lease_millis)?;
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&control.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        self.authorize(authenticated, &state.owner, Grant::ProcessSpawn)?;
        require_current(&manager, &state.owner)?;
        state.require_active()?;
        state.authorize_control(control)?;
        if state
            .writer
            .as_ref()
            .is_some_and(|writer| writer.expires_at_millis > now_millis)
        {
            return Err(TerminalError::WriterOccupied);
        }
        if state.viewers.len() >= MAX_ATTACHMENTS {
            return Err(TerminalError::AttachmentLimit);
        }
        let epoch = state
            .writer_epoch
            .checked_add(1)
            .ok_or(TerminalError::SequenceExhausted)?;
        let lease_token = SecretToken::generate()?;
        let attachment_token = SecretToken::generate()?;
        let mut candidate = state.clone();
        candidate.writer_epoch = epoch;
        candidate.writer = Some(WriterLease {
            principal_id: authenticated.principal_id(),
            epoch,
            token: lease_token,
            expires_at_millis,
        });
        candidate
            .viewers
            .insert(attachment_token, authenticated.principal_id());
        self.store
            .save(&candidate.snapshot())
            .map_err(TerminalError::Persistence)?;
        manager.terminals.insert(control.terminal_id, candidate);
        Ok(TerminalAttachment {
            terminal_id: control.terminal_id,
            principal_id: authenticated.principal_id(),
            attachment_token,
            process_token: control.process_token,
            role: AttachmentRole::Writer { epoch, lease_token },
        })
    }

    pub fn renew_writer(
        &self,
        attachment: &TerminalAttachment,
        now_millis: u64,
        lease_millis: u64,
    ) -> Result<u64, TerminalError> {
        let expires_at_millis = lease_deadline(now_millis, lease_millis)?;
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&attachment.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.authorize_attachment(attachment)?;
        state.require_active()?;
        let (epoch, lease_token) = writer_role(attachment)?;
        let writer = state.writer.as_ref().ok_or(TerminalError::StaleWriter)?;
        verify_writer(writer, attachment.principal_id, epoch, lease_token)?;
        if writer.expires_at_millis <= now_millis {
            return Err(TerminalError::LeaseExpired);
        }
        let mut candidate = state.clone();
        candidate
            .writer
            .as_mut()
            .expect("verified writer")
            .expires_at_millis = expires_at_millis;
        self.store
            .save(&candidate.snapshot())
            .map_err(TerminalError::Persistence)?;
        manager.terminals.insert(attachment.terminal_id, candidate);
        Ok(expires_at_millis)
    }

    pub fn release_writer(&self, attachment: &mut TerminalAttachment) -> Result<(), TerminalError> {
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&attachment.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.authorize_attachment(attachment)?;
        state.require_active()?;
        let (epoch, lease_token) = writer_role(attachment)?;
        verify_writer(
            state.writer.as_ref().ok_or(TerminalError::StaleWriter)?,
            attachment.principal_id,
            epoch,
            lease_token,
        )?;
        let mut candidate = state.clone();
        candidate.writer = None;
        self.store
            .save(&candidate.snapshot())
            .map_err(TerminalError::Persistence)?;
        manager.terminals.insert(attachment.terminal_id, candidate);
        attachment.role = AttachmentRole::Viewer;
        Ok(())
    }

    /// Invalidates an attachment token. Detaching a writer also releases its lease.
    pub fn detach(&self, attachment: &mut TerminalAttachment) -> Result<(), TerminalError> {
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&attachment.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.authorize_attachment(attachment)?;
        let mut candidate = state.clone();
        if let AttachmentRole::Writer { epoch, lease_token } = attachment.role {
            verify_writer(
                candidate
                    .writer
                    .as_ref()
                    .ok_or(TerminalError::StaleWriter)?,
                attachment.principal_id,
                epoch,
                lease_token,
            )?;
            candidate.writer = None;
        }
        candidate.viewers.remove(&attachment.attachment_token);
        self.store
            .save(&candidate.snapshot())
            .map_err(TerminalError::Persistence)?;
        manager.terminals.insert(attachment.terminal_id, candidate);
        attachment.role = AttachmentRole::Viewer;
        Ok(())
    }

    pub fn write_input(
        &self,
        attachment: &TerminalAttachment,
        bytes: &[u8],
        now_millis: u64,
    ) -> Result<(), TerminalError> {
        let manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&attachment.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.authorize_attachment(attachment)?;
        state.require_active()?;
        let (epoch, lease_token) = writer_role(attachment)?;
        let writer = state.writer.as_ref().ok_or(TerminalError::StaleWriter)?;
        verify_writer(writer, attachment.principal_id, epoch, lease_token)?;
        if writer.expires_at_millis <= now_millis {
            return Err(TerminalError::LeaseExpired);
        }
        // Input goes directly to the driver and never enters terminal state or snapshots.
        self.driver
            .write_input(attachment.terminal_id, bytes)
            .map_err(TerminalError::Driver)
    }

    pub fn resize(
        &self,
        attachment: &TerminalAttachment,
        size: TerminalSize,
        now_millis: u64,
    ) -> Result<ResizeEvent, TerminalError> {
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&attachment.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.authorize_attachment(attachment)?;
        state.require_active()?;
        let (writer_epoch, lease_token) = writer_role(attachment)?;
        let writer = state.writer.as_ref().ok_or(TerminalError::StaleWriter)?;
        verify_writer(writer, attachment.principal_id, writer_epoch, lease_token)?;
        if writer.expires_at_millis <= now_millis {
            return Err(TerminalError::LeaseExpired);
        }
        let next = state
            .next_resize_sequence
            .checked_add(1)
            .ok_or(TerminalError::SequenceExhausted)?;
        let event = ResizeEvent {
            sequence: state.next_resize_sequence,
            recorded_at_millis: now_millis,
            size,
            writer_epoch,
        };
        self.driver
            .resize(attachment.terminal_id, size)
            .map_err(TerminalError::Driver)?;
        let mut candidate = state.clone();
        candidate.size = size;
        candidate.next_resize_sequence = next;
        candidate.resizes.push_back(event);
        candidate.prune(now_millis);
        if let Err(error) = self.store.save(&candidate.snapshot()) {
            let _ = self.driver.interrupt(attachment.terminal_id);
            candidate.lifecycle = TerminalLifecycle::Interrupted;
            candidate.writer = None;
            manager.terminals.insert(attachment.terminal_id, candidate);
            return Err(TerminalError::Persistence(error));
        }
        manager.terminals.insert(attachment.terminal_id, candidate);
        Ok(event)
    }

    pub fn append_output(
        &self,
        control: &TerminalControl,
        capture: &SanitizedCapture,
        now_millis: u64,
    ) -> Result<u64, TerminalError> {
        if capture.boundary() != CaptureBoundary::TerminalMetadata {
            return Err(TerminalError::UnsanitizedCapture);
        }
        let bytes = capture
            .bytes()
            .map_err(|_| TerminalError::UnsanitizedCapture)?;
        if bytes.is_empty() {
            return Err(TerminalError::InvalidRequest("empty output chunk"));
        }
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&control.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.require_active()?;
        state.authorize_control(control)?;
        if !state
            .capture_policy
            .accepts(state.owner.process_claim(), capture)
        {
            return Err(TerminalError::UnsanitizedCapture);
        }
        let next = state
            .next_output_sequence
            .checked_add(1)
            .ok_or(TerminalError::SequenceExhausted)?;
        let sequence = state.next_output_sequence;
        let mut candidate = state.clone();
        candidate.next_output_sequence = next;
        if bytes.len() > candidate.retention.max_bytes || candidate.retention.max_age_millis == 0 {
            candidate.output.clear();
            candidate.retained_output_bytes = 0;
        } else {
            candidate.retained_output_bytes = candidate
                .retained_output_bytes
                .checked_add(bytes.len())
                .ok_or(TerminalError::SequenceExhausted)?;
            candidate.output.push_back(OutputChunk {
                sequence,
                recorded_at_millis: now_millis,
                bytes: bytes.to_vec(),
            });
        }
        candidate.prune(now_millis);
        self.store
            .save(&candidate.snapshot())
            .map_err(TerminalError::Persistence)?;
        manager.terminals.insert(control.terminal_id, candidate);
        Ok(sequence)
    }

    pub fn read_output(
        &self,
        attachment: &TerminalAttachment,
        cursor: u64,
        now_millis: u64,
    ) -> Result<OutputRead, TerminalError> {
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&attachment.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.authorize_attachment(attachment)?;
        if cursor > state.next_output_sequence {
            return Err(TerminalError::InvalidCursor {
                requested: cursor,
                next: state.next_output_sequence,
            });
        }
        let mut candidate = state.clone();
        candidate.prune(now_millis);
        if candidate.output.len() != state.output.len()
            || candidate.resizes.len() != state.resizes.len()
        {
            self.store
                .save(&candidate.snapshot())
                .map_err(TerminalError::Persistence)?;
            manager
                .terminals
                .insert(attachment.terminal_id, candidate.clone());
        }
        let oldest = candidate
            .output
            .front()
            .map_or(candidate.next_output_sequence, |chunk| chunk.sequence);
        if cursor < oldest {
            return Ok(OutputRead::Gap {
                requested: cursor,
                oldest_available: oldest,
            });
        }
        Ok(OutputRead::Chunks {
            chunks: candidate
                .output
                .iter()
                .filter(|chunk| chunk.sequence >= cursor)
                .cloned()
                .collect(),
            next_cursor: candidate.next_output_sequence,
        })
    }

    pub fn read_resizes(
        &self,
        attachment: &TerminalAttachment,
        cursor: u64,
        now_millis: u64,
    ) -> Result<ResizeRead, TerminalError> {
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&attachment.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.authorize_attachment(attachment)?;
        if cursor > state.next_resize_sequence {
            return Err(TerminalError::InvalidCursor {
                requested: cursor,
                next: state.next_resize_sequence,
            });
        }
        let mut candidate = state.clone();
        candidate.prune(now_millis);
        if candidate.output.len() != state.output.len()
            || candidate.resizes.len() != state.resizes.len()
        {
            self.store
                .save(&candidate.snapshot())
                .map_err(TerminalError::Persistence)?;
            manager
                .terminals
                .insert(attachment.terminal_id, candidate.clone());
        }
        let oldest = candidate
            .resizes
            .front()
            .map_or(candidate.next_resize_sequence, |event| event.sequence);
        if cursor < oldest {
            return Ok(ResizeRead::Gap {
                requested: cursor,
                oldest_available: oldest,
            });
        }
        Ok(ResizeRead::Events {
            events: candidate
                .resizes
                .iter()
                .filter(|event| event.sequence >= cursor)
                .copied()
                .collect(),
            next_cursor: candidate.next_resize_sequence,
        })
    }

    pub fn close(&self, control: &TerminalControl) -> Result<(), TerminalError> {
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&control.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.require_active()?;
        state.authorize_control(control)?;
        let mut candidate = state.clone();
        candidate.lifecycle = TerminalLifecycle::Exited;
        candidate.writer = None;
        self.store
            .save(&candidate.snapshot())
            .map_err(TerminalError::Persistence)?;
        manager.terminals.insert(control.terminal_id, candidate);
        self.driver
            .interrupt(control.terminal_id)
            .map_err(TerminalError::Driver)?;
        Ok(())
    }

    pub(crate) fn interrupt(&self, control: &TerminalControl) -> Result<(), TerminalError> {
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&control.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        state.authorize_control(control)?;
        let mut candidate = state.clone();
        candidate.lifecycle = TerminalLifecycle::Interrupted;
        candidate.writer = None;
        self.store
            .save(&candidate.snapshot())
            .map_err(TerminalError::Persistence)?;
        manager.terminals.insert(control.terminal_id, candidate);
        self.driver
            .interrupt(control.terminal_id)
            .map_err(TerminalError::Driver)
    }

    pub fn bind_process(
        &self,
        control: &TerminalControl,
        command: &mut Command,
    ) -> Result<Box<dyn io::Read + Send>, TerminalError> {
        let manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&control.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.require_active()?;
        state.authorize_control(control)?;
        self.driver
            .bind_process(control.terminal_id, command)
            .map_err(TerminalError::Driver)
    }

    #[cfg(windows)]
    pub(crate) fn conpty_binding(
        &self,
        control: &TerminalControl,
    ) -> Result<ConPtyBinding, TerminalError> {
        let manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&control.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.require_active()?;
        state.authorize_control(control)?;
        self.driver
            .conpty_binding(control.terminal_id)
            .map_err(TerminalError::Driver)
    }

    pub(crate) fn set_capture_policy(
        &self,
        control: &TerminalControl,
        capture_policy: CapturePersistencePolicy,
    ) -> Result<(), TerminalError> {
        let mut manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get_mut(&control.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        state.require_active()?;
        state.authorize_control(control)?;
        if !capture_policy.is_for(state.owner.process_claim()) {
            return Err(TerminalError::PermissionDenied);
        }
        state.capture_policy = capture_policy;
        Ok(())
    }

    /// Permanently stops PTY allocation and interrupts allocating and active local terminals.
    pub fn daemon_died(&self) -> Result<(), TerminalError> {
        let interrupted = {
            let mut manager = self
                .state
                .lock()
                .map_err(|_| TerminalError::StatePoisoned)?;
            manager.daemon_dead = true;
            manager
                .terminals
                .values_mut()
                .filter_map(|state| {
                    matches!(
                        state.lifecycle,
                        TerminalLifecycle::Allocating | TerminalLifecycle::Active
                    )
                    .then(|| {
                        state.lifecycle = TerminalLifecycle::Interrupted;
                        state.writer = None;
                        state.clone()
                    })
                })
                .collect::<Vec<_>>()
        };
        self.interrupt_states(&interrupted)
    }

    /// Reconciles persisted PTYs as interrupted. Local PTYs are never resumed after restart.
    pub fn restore_snapshots<I, F>(
        &self,
        snapshots: I,
        now_millis: u64,
        mut record_attempt_interruption: F,
    ) -> Result<Vec<TerminalControl>, TerminalError>
    where
        I: IntoIterator<Item = TerminalSnapshot>,
        F: FnMut(&TerminalOwner) -> io::Result<()>,
    {
        let mut restored = Vec::new();
        let mut controls = Vec::new();
        {
            let mut manager = self
                .state
                .lock()
                .map_err(|_| TerminalError::StatePoisoned)?;
            for snapshot in snapshots {
                if snapshot.owner.project_id != self.project_id
                    || snapshot.owner.boundary_id.trim().is_empty()
                {
                    return Err(TerminalError::PermissionDenied);
                }
                let control_token = SecretToken::generate()?;
                let process_token = SecretToken::generate()?;
                let mut terminal = TerminalState {
                    terminal_id: snapshot.terminal_id,
                    owner: snapshot.owner,
                    lifecycle: TerminalLifecycle::Interrupted,
                    size: snapshot.size,
                    retention: snapshot.retention,
                    next_output_sequence: snapshot.next_output_sequence,
                    next_resize_sequence: snapshot.next_resize_sequence,
                    retained_output_bytes: snapshot
                        .output
                        .iter()
                        .map(|chunk| chunk.bytes.len())
                        .sum(),
                    writer_epoch: snapshot.writer.map_or(0, |writer| writer.epoch),
                    writer: None,
                    viewers: HashMap::new(),
                    control_token,
                    process_token,
                    capture_policy: CapturePersistencePolicy::no_secrets(),
                    output: snapshot
                        .output
                        .into_iter()
                        .filter(|chunk| !chunk.bytes.is_empty())
                        .collect(),
                    resizes: snapshot.resizes.into(),
                };
                terminal.prune(now_millis);
                controls.push(TerminalControl {
                    terminal_id: terminal.terminal_id,
                    control_token,
                    process_token,
                });
                match manager.current_processes.get(&terminal.owner.attempt_id) {
                    Some(current)
                        if current.process_fence.get() > terminal.owner.process_fence.get() => {}
                    _ => {
                        manager
                            .current_processes
                            .insert(terminal.owner.attempt_id, terminal.owner.clone());
                    }
                }
                manager
                    .terminals
                    .insert(terminal.terminal_id, terminal.clone());
                restored.push(terminal);
            }
        }
        let mut first_error = None;
        for state in &restored {
            if let Err(error) = self.driver.interrupt(state.terminal_id) {
                first_error.get_or_insert(TerminalError::Driver(error));
            }
            if let Err(error) = self.store.save(&state.snapshot()) {
                first_error.get_or_insert(TerminalError::Persistence(error));
            }
            if let Err(error) = record_attempt_interruption(&state.owner) {
                first_error.get_or_insert(TerminalError::Persistence(error));
            }
        }
        first_error.map_or(Ok(controls), Err)
    }

    pub fn snapshot(&self, control: &TerminalControl) -> Result<TerminalSnapshot, TerminalError> {
        let manager = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        let state = manager
            .terminals
            .get(&control.terminal_id)
            .ok_or(TerminalError::NotFound)?;
        require_current(&manager, &state.owner)?;
        state.authorize_control(control)?;
        Ok(state.snapshot())
    }

    fn authorize(
        &self,
        authenticated: &AuthenticatedPrincipal,
        owner: &TerminalOwner,
        grant: Grant,
    ) -> Result<(), TerminalError> {
        let snapshot = authenticated.grant_snapshot();
        if owner.project_id == self.project_id
            && snapshot.project_id() == self.project_id
            && snapshot.principal_id() == owner.principal_id
            && snapshot.grants().contains(&grant)
        {
            Ok(())
        } else {
            Err(TerminalError::PermissionDenied)
        }
    }

    fn reserve_process(
        &self,
        owner: &TerminalOwner,
        terminal: Option<TerminalState>,
    ) -> Result<Vec<TerminalState>, TerminalError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        if terminal.is_some() && state.daemon_dead {
            return Err(TerminalError::DaemonUnavailable);
        }
        if let Some(current) = state.current_processes.get(&owner.attempt_id) {
            if current.process_fence.get() > owner.process_fence.get() {
                return Err(TerminalError::StaleProcessClaim);
            }
            if current.process_fence == owner.process_fence && current != owner {
                return Err(TerminalError::PermissionDenied);
            }
        }
        let mut superseded = Vec::new();
        if state
            .current_processes
            .get(&owner.attempt_id)
            .is_some_and(|current| current.process_fence.get() < owner.process_fence.get())
        {
            for existing in state.terminals.values_mut() {
                if existing.owner.attempt_id == owner.attempt_id
                    && matches!(
                        existing.lifecycle,
                        TerminalLifecycle::Allocating | TerminalLifecycle::Active
                    )
                {
                    existing.lifecycle = TerminalLifecycle::Interrupted;
                    existing.writer = None;
                    superseded.push(existing.clone());
                }
            }
        }
        state
            .current_processes
            .insert(owner.attempt_id, owner.clone());
        if let Some(terminal) = terminal {
            state.terminals.insert(terminal.terminal_id, terminal);
        }
        Ok(superseded)
    }

    fn interrupt_states(&self, states: &[TerminalState]) -> Result<(), TerminalError> {
        let mut first_error = None;
        for state in states {
            if let Err(error) = self.driver.interrupt(state.terminal_id) {
                first_error.get_or_insert(TerminalError::Driver(error));
            }
            if let Err(error) = self.store.save(&state.snapshot()) {
                first_error.get_or_insert(TerminalError::Persistence(error));
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn require_current(manager: &ManagerState, owner: &TerminalOwner) -> Result<(), TerminalError> {
    if manager.current_processes.get(&owner.attempt_id) == Some(owner) {
        Ok(())
    } else {
        Err(TerminalError::StaleProcessClaim)
    }
}

fn lease_deadline(now_millis: u64, lease_millis: u64) -> Result<u64, TerminalError> {
    if lease_millis == 0 {
        return Err(TerminalError::InvalidRequest(
            "writer lease must be renewable",
        ));
    }
    now_millis
        .checked_add(lease_millis)
        .ok_or(TerminalError::InvalidRequest(
            "writer lease deadline overflows",
        ))
}

fn writer_role(attachment: &TerminalAttachment) -> Result<(u64, SecretToken), TerminalError> {
    match attachment.role {
        AttachmentRole::Viewer => Err(TerminalError::ReadOnlyViewer),
        AttachmentRole::Writer { epoch, lease_token } => Ok((epoch, lease_token)),
    }
}

fn verify_writer(
    writer: &WriterLease,
    principal_id: PrincipalId,
    epoch: u64,
    token: SecretToken,
) -> Result<(), TerminalError> {
    if writer.principal_id == principal_id && writer.epoch == epoch && writer.token == token {
        Ok(())
    } else {
        Err(TerminalError::StaleWriter)
    }
}

#[derive(Clone, Debug, Default)]
pub struct FakePtyDriver {
    state: std::sync::Arc<Mutex<FakePtyState>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FakePtyState {
    pub allocated: Vec<TerminalId>,
    pub input_byte_count: usize,
    pub resizes: Vec<(TerminalId, TerminalSize)>,
    pub interrupted: Vec<TerminalId>,
}

impl FakePtyDriver {
    pub fn state(&self) -> FakePtyState {
        self.state.lock().expect("fake PTY lock poisoned").clone()
    }
}

impl PtyDriver for FakePtyDriver {
    fn allocate(
        &self,
        terminal_id: TerminalId,
        _owner: &TerminalOwner,
        _size: TerminalSize,
    ) -> Result<(), TerminalError> {
        self.state
            .lock()
            .map_err(|_| TerminalError::Driver(io::Error::other("fake PTY lock poisoned")))?
            .allocated
            .push(terminal_id);
        Ok(())
    }

    fn write_input(&self, _terminal_id: TerminalId, bytes: &[u8]) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("fake PTY lock poisoned"))?;
        state.input_byte_count = state.input_byte_count.saturating_add(bytes.len());
        Ok(())
    }

    fn resize(&self, terminal_id: TerminalId, size: TerminalSize) -> io::Result<()> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("fake PTY lock poisoned"))?
            .resizes
            .push((terminal_id, size));
        Ok(())
    }

    fn interrupt(&self, terminal_id: TerminalId) -> io::Result<()> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("fake PTY lock poisoned"))?
            .interrupted
            .push(terminal_id);
        Ok(())
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod native_pty_tests {
    use super::*;
    use std::{
        cell::Cell,
        sync::{Arc, Barrier, mpsc},
        time::{Duration, Instant},
    };

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    const LEAK_TEST_CHILD: &str = "KIT_PTY_LEAK_TEST_CHILD";
    const LEAK_TEST_NAME: &str =
        "executor::terminal::native_pty_tests::failed_slave_allocation_does_not_leak_descriptors";

    fn size() -> TerminalSize {
        TerminalSize::new(80, 24).unwrap()
    }

    fn assert_cloexec(file: &File) {
        // SAFETY: the test owns a live descriptor and F_GETFD has no pointer argument.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    fn assert_closed(descriptor: i32) {
        // SAFETY: F_GETFD accepts any descriptor value and has no pointer argument.
        let result = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        let error = io::Error::last_os_error();
        assert_eq!(result, -1, "descriptor {descriptor} is still open");
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }

    fn read_until_closed(reader: &mut dyn io::Read) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut buffer = [0; 256];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "PTY output did not close",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    #[test]
    fn native_pty_endpoints_and_duplicates_are_cloexec() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let pty = open_native_pty(size(), |_| Ok(())).unwrap();
        assert_cloexec(&pty.master);
        assert_cloexec(pty.slave.as_ref().unwrap());
        assert_cloexec(&duplicate_cloexec(&pty.master).unwrap());
        assert_cloexec(&duplicate_cloexec(pty.slave.as_ref().unwrap()).unwrap());
    }

    #[test]
    fn unrelated_concurrent_spawn_cannot_hold_pty_output_open() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let driver = NativePtyDriver::new();
        let terminal_id = TerminalId::generate().unwrap();
        let owner = TerminalOwner {
            project_id: ProjectId::generate().unwrap(),
            process_id: ProcessId::generate().unwrap(),
            attempt_id: AttemptId::generate().unwrap(),
            principal_id: PrincipalId::generate().unwrap(),
            process_fence: FencingToken::new(1),
            boundary_id: "native-pty-test".into(),
        };
        driver.allocate(terminal_id, &owner, size()).unwrap();

        let mut command = Command::new("/usr/bin/true");
        let barrier = Arc::new(Barrier::new(2));
        let (sender, receiver) = mpsc::sync_channel(0);
        let (mut output, mut unrelated) = std::thread::scope(|scope| {
            let other_barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                other_barrier.wait();
                sender
                    .send(Command::new("/bin/sleep").arg("5").spawn().unwrap())
                    .unwrap();
            });
            barrier.wait();
            let output = driver.bind_process(terminal_id, &mut command).unwrap();
            (output, receiver.recv().unwrap())
        });
        drop(command);
        let read = read_until_closed(output.as_mut());
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "unrelated child exited before the PTY closure check"
        );
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        read.unwrap();
        driver.interrupt(terminal_id).unwrap();
    }

    #[test]
    fn failed_slave_allocation_does_not_leak_descriptors() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        if std::env::var_os(LEAK_TEST_CHILD).is_none() {
            // Isolate the captured descriptor numbers from reuse by unrelated parallel tests.
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", LEAK_TEST_NAME, "--nocapture"])
                .env(LEAK_TEST_CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "isolated leak check failed");
            return;
        }

        let descriptors = Cell::new(None);
        assert!(
            open_native_pty(size(), |pty| {
                descriptors.set(Some((
                    pty.master.as_raw_fd(),
                    pty.slave.as_ref().unwrap().as_raw_fd(),
                )));
                Err(io::Error::other("injected allocation failure"))
            })
            .is_err()
        );
        let (master, slave) = descriptors.get().expect("allocation reached failure seam");
        assert_closed(master);
        assert_closed(slave);
    }
}
