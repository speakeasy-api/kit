use std::{
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use super::{ClientError, ClientErrorKind};

pub const DISCOVERY_FILE: &str = "daemon.json";
const MAX_DISCOVERY_BYTES: u64 = 16 * 1024;

/// Default state root for the project rooted at the current directory: a
/// per-project directory under the user's state home, never inside the project
/// itself. A state root inside the served repository makes every run fail with
/// "source and managed root must not overlap", so `./.kit` is only the fallback
/// when neither the working directory nor a state home can be resolved.
pub fn default_state_root() -> PathBuf {
    let Ok(project_root) = std::env::current_dir().and_then(fs::canonicalize) else {
        return PathBuf::from(".kit");
    };
    let Some(state_home) = state_home() else {
        return PathBuf::from(".kit");
    };
    let digest = crate::domain::crypto::sha256(project_root.as_os_str().as_encoded_bytes());
    let short = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let name = project_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_owned());
    state_home.join("kit/projects").join(format!("{name}-{short}"))
}

#[cfg(windows)]
fn state_home() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

#[cfg(not(windows))]
fn state_home() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_STATE_HOME").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(|home| PathBuf::from(home).join(".local/state"))
}

/// Identity of the daemon executable, recorded in the discovery file at boot
/// so auto-start can detect a daemon left running from a previous build.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableIdentity {
    pub path: String,
    pub len: u64,
    pub modified_unix_micros: u64,
}

impl ExecutableIdentity {
    pub fn current() -> Option<Self> {
        Self::of(&std::env::current_exe().ok()?)
    }

    pub fn of(path: &Path) -> Option<Self> {
        let canonical = fs::canonicalize(path).ok()?;
        let metadata = fs::metadata(&canonical).ok()?;
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some(Self {
            path: canonical.to_string_lossy().into_owned(),
            len: metadata.len(),
            modified_unix_micros: u64::try_from(modified.as_micros()).ok()?,
        })
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonDiscovery {
    pub endpoint: String,
    pub credential: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub executable: Option<ExecutableIdentity>,
}

impl fmt::Debug for DaemonDiscovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonDiscovery")
            .field("endpoint", &self.endpoint)
            .field("credential", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct AutoStart {
    pub enabled: bool,
    pub executable: PathBuf,
    pub timeout: Duration,
}

impl AutoStart {
    pub fn disabled(executable: impl Into<PathBuf>) -> Self {
        Self {
            enabled: false,
            executable: executable.into(),
            timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
pub struct DaemonConnection<T> {
    pub discovery: DaemonDiscovery,
    pub connection: T,
}

pub fn read_discovery(state_root: &Path) -> Result<DaemonDiscovery, DiscoveryError> {
    let path = state_root.join(DISCOVERY_FILE);
    let path_metadata = fs::symlink_metadata(&path).map_err(DiscoveryError::Io)?;
    if !path_metadata.file_type().is_file() || path_metadata.file_type().is_symlink() {
        return Err(DiscoveryError::UnsafeFile);
    }
    let mut file = fs::File::open(path).map_err(DiscoveryError::Io)?;
    let metadata = file.metadata().map_err(DiscoveryError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != metadata.dev() || path_metadata.ino() != metadata.ino() {
            return Err(DiscoveryError::UnsafeFile);
        }
    }
    if metadata.len() > MAX_DISCOVERY_BYTES {
        return Err(DiscoveryError::UnsafeFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(DiscoveryError::UnsafePermissions);
        }
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes).map_err(DiscoveryError::Io)?;
    let discovery: DaemonDiscovery = serde_json::from_slice(&bytes)
        .map_err(|error| DiscoveryError::Invalid(error.to_string()))?;
    validate_discovery(&discovery)?;
    Ok(discovery)
}

pub fn connect_daemon<T, F>(
    state_root: &Path,
    auto_start: &AutoStart,
    mut connect_and_check_ready: F,
) -> Result<DaemonConnection<T>, DiscoveryError>
where
    F: FnMut(&DaemonDiscovery) -> Result<T, ClientError>,
{
    let mut child = match read_discovery(state_root) {
        Ok(discovery) => match connect_and_check_ready(&discovery) {
            Ok(connection) => match stale_running_daemon(&discovery, auto_start) {
                None => {
                    return Ok(DaemonConnection {
                        discovery,
                        connection,
                    });
                }
                Some(pid) => {
                    drop(connection);
                    terminate_stale_daemon(state_root, pid, auto_start.timeout)?;
                    Some(spawn_daemon(state_root, auto_start)?)
                }
            },
            Err(_) if !auto_start.enabled => {
                return Err(DiscoveryError::AutoStartDisabled);
            }
            Err(_) => Some(spawn_daemon(state_root, auto_start)?),
        },
        Err(DiscoveryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            if !auto_start.enabled {
                return Err(DiscoveryError::AutoStartDisabled);
            }
            Some(spawn_daemon(state_root, auto_start)?)
        }
        Err(error) => return Err(error),
    };

    let deadline = Instant::now() + auto_start.timeout;
    let mut exited = None;
    loop {
        if let Some(started) = &mut child
            && let Some(status) = started.try_wait().map_err(DiscoveryError::Io)?
        {
            exited = Some(status.code());
            child = None;
        }
        if let Ok(discovery) = read_discovery(state_root)
            && stale_running_daemon(&discovery, auto_start).is_none()
            && let Ok(connection) = connect_and_check_ready(&discovery)
        {
            return Ok(DaemonConnection {
                discovery,
                connection,
            });
        }
        if Instant::now() >= deadline {
            if let Some(started) = &mut child {
                let _ = started.kill();
                let _ = started.wait();
            }
            return match exited {
                Some(code) => Err(DiscoveryError::Exited(code)),
                None => Err(DiscoveryError::Timeout),
            };
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// A running daemon is stale when the discovery-recorded executable identity
/// points at the same binary path the CLI would spawn but with a different
/// size or mtime: the daemon outlived a rebuild. Discovery files without an
/// identity (older daemons) and daemons started from a different binary path
/// are never treated as stale.
fn stale_running_daemon(discovery: &DaemonDiscovery, auto_start: &AutoStart) -> Option<u32> {
    if !auto_start.enabled || !cfg!(unix) {
        return None;
    }
    let pid = discovery.pid.filter(|pid| *pid != std::process::id())?;
    let recorded = discovery.executable.as_ref()?;
    let current = ExecutableIdentity::of(&auto_start.executable)?;
    (recorded.path == current.path && *recorded != current).then_some(pid)
}

#[cfg(unix)]
fn terminate_stale_daemon(
    state_root: &Path,
    pid: u32,
    timeout: Duration,
) -> Result<(), DiscoveryError> {
    let Ok(target) = i32::try_from(pid) else {
        return Err(DiscoveryError::Invalid(format!(
            "stale daemon pid {pid} is out of range"
        )));
    };
    // SAFETY: kill(2) with SIGTERM/0 takes no pointers and cannot corrupt
    // process state; the target pid was just confirmed live over its own
    // authenticated discovery endpoint.
    if unsafe { libc::kill(target, libc::SIGTERM) } == -1 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(DiscoveryError::Io(error))
        };
    }
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: see above; signal 0 only probes liveness.
        if unsafe { libc::kill(target, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return Ok(());
        }
        // Process exit is the only signal that the state-root lock is
        // released; discovery-file removal happens earlier in shutdown and
        // must not trigger the respawn. A discovery file re-written by a
        // different pid means another client already completed the
        // replacement.
        if let Ok(discovery) = read_discovery(state_root)
            && discovery.pid.is_some_and(|current| current != pid)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(DiscoveryError::StaleDaemon(pid));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(not(unix))]
fn terminate_stale_daemon(
    _state_root: &Path,
    _pid: u32,
    _timeout: Duration,
) -> Result<(), DiscoveryError> {
    Ok(())
}

fn spawn_daemon(state_root: &Path, auto_start: &AutoStart) -> Result<Child, DiscoveryError> {
    if !auto_start.enabled {
        return Err(DiscoveryError::AutoStartDisabled);
    }
    let mut command = Command::new(&auto_start.executable);
    command
        .arg("daemon")
        .arg("--state-root")
        .arg(state_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // SAFETY: setsid is async-signal-safe and the closure performs no allocation.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    command.spawn().map_err(DiscoveryError::Io)
}

fn validate_discovery(discovery: &DaemonDiscovery) -> Result<(), DiscoveryError> {
    let endpoint_ok = discovery
        .endpoint
        .strip_prefix("unix:")
        .is_some_and(|path| Path::new(path).is_absolute())
        || discovery
            .endpoint
            .strip_prefix("http://127.0.0.1:")
            .is_some_and(valid_port)
        || discovery
            .endpoint
            .strip_prefix("http://[::1]:")
            .is_some_and(valid_port);
    let credential_ok = !discovery.credential.is_empty()
        && discovery.credential.len() <= 4096
        && discovery
            .credential
            .bytes()
            .all(|byte| byte.is_ascii_graphic());
    if endpoint_ok && credential_ok {
        Ok(())
    } else {
        Err(DiscoveryError::Invalid(
            "daemon discovery must contain a local endpoint and visible ASCII credential"
                .to_owned(),
        ))
    }
}

fn valid_port(value: &str) -> bool {
    value
        .parse::<u16>()
        .is_ok_and(|port| port > 0 && port.to_string() == value)
}

#[derive(Debug)]
pub enum DiscoveryError {
    Io(std::io::Error),
    Invalid(String),
    UnsafeFile,
    UnsafePermissions,
    AutoStartDisabled,
    Timeout,
    Exited(Option<i32>),
    StaleDaemon(u32),
    Client(ClientError),
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "daemon discovery failed: {error}"),
            Self::Invalid(error) => write!(f, "invalid daemon discovery: {error}"),
            Self::UnsafeFile => f.write_str("daemon discovery is not a regular file"),
            Self::UnsafePermissions => {
                f.write_str("daemon discovery must not be accessible by group or other users")
            }
            Self::AutoStartDisabled => {
                f.write_str("daemon is unavailable and auto-start is disabled")
            }
            Self::Timeout => f.write_str("daemon did not become ready before the timeout"),
            Self::Exited(code) => write!(f, "auto-started daemon exited with status {code:?}"),
            Self::StaleDaemon(pid) => write!(
                f,
                "daemon {pid} runs a previous build of this binary and did not exit after SIGTERM before the timeout; stop it manually and retry"
            ),
            Self::Client(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for DiscoveryError {}

impl From<DiscoveryError> for ClientError {
    fn from(error: DiscoveryError) -> Self {
        let message = error.to_string();
        match error {
            DiscoveryError::Client(error) => error,
            DiscoveryError::Timeout => ClientError::timeout(message),
            DiscoveryError::AutoStartDisabled
            | DiscoveryError::Exited(_)
            | DiscoveryError::StaleDaemon(_) => ClientError::unavailable(message),
            DiscoveryError::Invalid(_)
            | DiscoveryError::UnsafeFile
            | DiscoveryError::UnsafePermissions => {
                ClientError::new(ClientErrorKind::Invalid, message)
            }
            DiscoveryError::Io(_) => ClientError::internal(message),
        }
    }
}
