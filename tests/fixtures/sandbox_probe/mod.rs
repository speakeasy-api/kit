use std::collections::BTreeMap;
use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    DockerSocket(PathBuf),
    SshAgent,
    HostDaemonSocket(PathBuf),
    CloudMetadata,
    IsolationBackend(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeStatus {
    Reachable,
    Unreachable(String),
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    pub seed: u64,
    pub capability: Capability,
    pub status: ProbeStatus,
}

pub trait ProbeBackend {
    fn probe(&self, capability: &Capability) -> ProbeStatus;
}

#[derive(Clone, Debug)]
pub struct CapabilityProbe {
    seed: u64,
    capabilities: Vec<Capability>,
}

impl CapabilityProbe {
    pub fn new(seed: u64, capabilities: Vec<Capability>) -> Self {
        Self { seed, capabilities }
    }

    pub fn run<B: ProbeBackend>(&self, backend: &B) -> Vec<ProbeResult> {
        self.capabilities
            .iter()
            .cloned()
            .map(|capability| ProbeResult {
                seed: self.seed,
                status: backend.probe(&capability),
                capability,
            })
            .collect()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScriptedProbe {
    statuses: BTreeMap<Capability, ProbeStatus>,
}

impl ScriptedProbe {
    pub fn new(statuses: impl IntoIterator<Item = (Capability, ProbeStatus)>) -> Self {
        Self {
            statuses: statuses.into_iter().collect(),
        }
    }
}

impl ProbeBackend for ScriptedProbe {
    fn probe(&self, capability: &Capability) -> ProbeStatus {
        self.statuses.get(capability).cloned().unwrap_or_else(|| {
            ProbeStatus::Unavailable("capability has no scripted observation".to_owned())
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct SystemProbe;

impl ProbeBackend for SystemProbe {
    fn probe(&self, capability: &Capability) -> ProbeStatus {
        match capability {
            Capability::DockerSocket(path) | Capability::HostDaemonSocket(path) => {
                probe_socket(path)
            }
            Capability::SshAgent => env::var_os("SSH_AUTH_SOCK").map_or_else(
                || ProbeStatus::Unavailable("SSH_AUTH_SOCK is not set".to_owned()),
                |path| probe_socket(Path::new(&path)),
            ),
            Capability::CloudMetadata => {
                let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)), 80);
                match TcpStream::connect_timeout(&address, Duration::from_millis(25)) {
                    Ok(_) => ProbeStatus::Reachable,
                    Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                        ProbeStatus::Unavailable(error.to_string())
                    }
                    Err(error) => ProbeStatus::Unreachable(error.kind().to_string()),
                }
            }
            Capability::IsolationBackend(path) => {
                if is_executable(path) {
                    ProbeStatus::Reachable
                } else {
                    ProbeStatus::Unavailable(format!(
                        "{} is not installed or executable",
                        path.display()
                    ))
                }
            }
        }
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
        && path.extension().is_some_and(|extension| {
            ["exe", "com", "bat", "cmd"]
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

#[cfg(not(any(unix, windows)))]
fn is_executable(_path: &Path) -> bool {
    false
}

#[cfg(unix)]
fn probe_socket(path: &Path) -> ProbeStatus {
    use std::os::unix::net::UnixStream;

    if !path.exists() {
        return ProbeStatus::Unavailable(format!("{} does not exist", path.display()));
    }
    match UnixStream::connect(path) {
        Ok(_) => ProbeStatus::Reachable,
        Err(error) => ProbeStatus::Unreachable(error.kind().to_string()),
    }
}

#[cfg(not(unix))]
fn probe_socket(path: &Path) -> ProbeStatus {
    ProbeStatus::Unavailable(format!(
        "Unix socket probing is unavailable for {} on this platform",
        path.display()
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebindOutcome {
    Connected,
    Blocked,
    Unavailable,
}

pub fn probe_dns_rebinding(
    seed: u64,
    validated: Option<IpAddr>,
    connected: Option<IpAddr>,
    allowed: &[IpAddr],
) -> (u64, RebindOutcome) {
    let outcome = match (validated, connected) {
        (Some(first), Some(second)) if first == second && allowed.contains(&second) => {
            RebindOutcome::Connected
        }
        (Some(_), Some(_)) => RebindOutcome::Blocked,
        _ => RebindOutcome::Unavailable,
    };
    (seed, outcome)
}
