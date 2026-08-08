use std::{
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;

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

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonDiscovery {
    pub endpoint: String,
    pub credential: String,
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
            Ok(connection) => {
                return Ok(DaemonConnection {
                    discovery,
                    connection,
                });
            }
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
            DiscoveryError::AutoStartDisabled | DiscoveryError::Exited(_) => {
                ClientError::unavailable(message)
            }
            DiscoveryError::Invalid(_)
            | DiscoveryError::UnsafeFile
            | DiscoveryError::UnsafePermissions => {
                ClientError::new(ClientErrorKind::Invalid, message)
            }
            DiscoveryError::Io(_) => ClientError::internal(message),
        }
    }
}
