use crate::{ProtocolError, Result, sha256};
use std::{
    ffi::OsString,
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub struct LocalSandboxRequest {
    pub executable: PathBuf,
    pub expected_executable_digest: String,
    pub allowed_executables: Vec<PathBuf>,
    pub arguments: Vec<OsString>,
    pub source_snapshot: PathBuf,
    pub readonly_roots: Vec<PathBuf>,
    pub request_files: Vec<PathBuf>,
    pub writable_roots: Vec<PathBuf>,
    pub forbidden_paths: Vec<PathBuf>,
    pub max_duration: Duration,
    pub capture_stderr: bool,
}

#[derive(Debug)]
pub enum SandboxOutcome {
    Exited {
        status: ExitStatus,
        stderr_first_line: Option<String>,
    },
    TimedOut {
        stderr_first_line: Option<String>,
    },
}

pub fn run_local_sandbox(request: LocalSandboxRequest) -> Result<SandboxOutcome> {
    if !cfg!(target_os = "macos") {
        return Err(ProtocolError("sandbox-exec fallback is macOS-only".into()).into());
    }
    if request.request_files.is_empty()
        || request.writable_roots.is_empty()
        || request.forbidden_paths.is_empty()
        || request.max_duration.is_zero()
    {
        return Err(ProtocolError("local sandbox requires explicit bounded roots".into()).into());
    }
    let executable = existing_file(&request.executable)?;
    if file_digest(&executable, 256 << 20)? != request.expected_executable_digest {
        return Err(ProtocolError("sandbox executable digest mismatch".into()).into());
    }
    let executable_identity = identity(&executable)?;
    let allowed_executables = request
        .allowed_executables
        .iter()
        .map(|path| existing_file(path))
        .collect::<Result<Vec<_>>>()?;
    let source = existing_directory(&request.source_snapshot)?;
    let readonly = request
        .readonly_roots
        .iter()
        .map(|path| existing_directory(path))
        .collect::<Result<Vec<_>>>()?;
    let inputs = request
        .request_files
        .iter()
        .map(|path| existing_file(path))
        .collect::<Result<Vec<_>>>()?;
    let writable = request
        .writable_roots
        .iter()
        .map(|path| existing_directory(path))
        .collect::<Result<Vec<_>>>()?;
    let forbidden = request
        .forbidden_paths
        .iter()
        .map(|path| path.canonicalize())
        .collect::<std::io::Result<Vec<_>>>()?;
    let protected = std::iter::once(&source)
        .chain(std::iter::once(&executable))
        .chain(readonly.iter())
        .chain(allowed_executables.iter())
        .chain(inputs.iter())
        .collect::<Vec<_>>();
    if writable.iter().any(|write| {
        protected.iter().any(|read| overlaps(write, read))
            || forbidden.iter().any(|deny| overlaps(write, deny))
    }) || forbidden
        .iter()
        .any(|path| protected.iter().any(|other| overlaps(path, other)))
    {
        return Err(ProtocolError("sandbox inputs and writable roots overlap".into()).into());
    }
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(deny network*)\n(allow process-fork)\n(allow signal (target self))\n(allow sysctl-read (sysctl-name \"security.mac.lockdown_mode_state\") (sysctl-name \"kern.bootargs\") (sysctl-name \"kern.osproductversion\") (sysctl-name \"kern.iossupportversion\") (sysctl-name \"kern.osvariant_status\") (sysctl-name \"hw.ephemeral_storage\") (sysctl-name \"hw.pagesize_compat\"))\n(allow file-read* (literal \"/\") (subpath \"/System/Library\") (subpath \"/usr/lib\") (subpath \"/private/var/db/timezone\") (literal \"/dev/null\") (literal \"/dev/urandom\"))\n",
    );
    profile.push_str(&format!(
        "(allow process-exec (literal {}))\n(allow file-read* (literal {}) (subpath {}))\n",
        sandbox_string(&executable)?,
        sandbox_string(&executable)?,
        sandbox_string(&source)?,
    ));
    for root in &readonly {
        profile.push_str(&format!(
            "(allow file-read* (literal {}) (subpath {}))\n",
            sandbox_string(root)?,
            sandbox_string(root)?
        ));
    }
    for allowed in &allowed_executables {
        profile.push_str(&format!(
            "(allow process-exec (literal {}))\n(allow file-read* (literal {}))\n",
            sandbox_string(allowed)?,
            sandbox_string(allowed)?,
        ));
    }
    for input in &inputs {
        profile.push_str(&format!(
            "(allow file-read* (literal {}))\n",
            sandbox_string(input)?
        ));
    }
    for root in &writable {
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath {}))\n",
            sandbox_string(root)?
        ));
    }
    for path in &forbidden {
        let rule = if path.is_dir() { "subpath" } else { "literal" };
        profile.push_str(&format!(
            "(deny file-read* file-write* ({rule} {}))\n",
            sandbox_string(path)?
        ));
    }
    let home = writable
        .last()
        .ok_or_else(|| ProtocolError("sandbox has no writable temporary root".into()))?;
    let path = allowed_executables
        .first()
        .and_then(|executable| executable.parent())
        .unwrap_or_else(|| Path::new("/usr/bin"));
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .args(["-p", &profile])
        .arg(&executable)
        .args(request.arguments)
        .env_clear()
        .env("HOME", home)
        .env("TMPDIR", home)
        .env("LC_ALL", "C")
        .env("PATH", path)
        .current_dir(source)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(if request.capture_stderr {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = command.spawn()?;
    let stderr = child.stderr.take().map(|mut stderr| {
        thread::spawn(move || {
            let mut retained = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stderr.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                let remaining = 4096_usize.saturating_sub(retained.len());
                retained.extend_from_slice(&buffer[..read.min(remaining)]);
            }
            Ok(retained)
        })
    });
    if identity(&executable)? != executable_identity
        || file_digest(&executable, 256 << 20)? != request.expected_executable_digest
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ProtocolError("sandbox executable changed during launch".into()).into());
    }
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(SandboxOutcome::Exited {
                status,
                stderr_first_line: finish_stderr(stderr)?,
            });
        }
        if started.elapsed() >= request.max_duration {
            child.kill()?;
            let _ = child.wait();
            return Ok(SandboxOutcome::TimedOut {
                stderr_first_line: finish_stderr(stderr)?,
            });
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn finish_stderr(stderr: Option<JoinHandle<std::io::Result<Vec<u8>>>>) -> Result<Option<String>> {
    let Some(stderr) = stderr else {
        return Ok(None);
    };
    let bytes = stderr
        .join()
        .map_err(|_| ProtocolError("sandbox stderr diagnostic thread failed".into()))??;
    Ok(sanitize_stderr_first_line(&bytes))
}

fn sanitize_stderr_first_line(bytes: &[u8]) -> Option<String> {
    let line = String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .map(|token| {
            if token.contains(['/', '\\', '=']) {
                "$REDACTED"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let line = line
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect::<String>();
    (!line.is_empty()).then_some(line)
}

fn existing_file(path: &Path) -> Result<PathBuf> {
    crate::reject_symlink_components(path, false)?;
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(ProtocolError("sandbox file input is not a regular file".into()).into());
    }
    let path = path.canonicalize()?;
    if !path.is_file() {
        return Err(ProtocolError("sandbox file input is not a regular file".into()).into());
    }
    Ok(path)
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn identity(path: &Path) -> Result<(u64, u64, u64)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino(), metadata.len()))
}

fn file_digest(path: &Path, maximum: u64) -> Result<String> {
    crate::reject_symlink_components(path, false)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > maximum {
        return Err(ProtocolError("invalid bounded sandbox executable".into()).into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref().take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() || file.metadata()?.len() != metadata.len() {
        return Err(ProtocolError("sandbox executable changed while hashing".into()).into());
    }
    Ok(sha256(&bytes))
}

fn existing_directory(path: &Path) -> Result<PathBuf> {
    crate::reject_symlink_components(path, false)?;
    let path = path.canonicalize()?;
    if !path.is_dir() {
        return Err(ProtocolError("sandbox directory input is not a directory".into()).into());
    }
    Ok(path)
}

fn sandbox_string(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .filter(|path| !path.contains(['\\', '"', '\n', '\r']))
        .ok_or_else(|| ProtocolError("sandbox path is not representable in a profile".into()))?;
    Ok(format!("\"{path}\""))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static ORDINAL: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn startup_diagnostic_is_single_line_bounded_and_path_free() {
        let diagnostic = sanitize_stderr_first_line(
            b"fatal runtime error: /private/tmp/worker TOKEN=secret\nignored /other/path",
        )
        .unwrap();
        assert_eq!(diagnostic, "fatal runtime error: $REDACTED $REDACTED");
        assert!(!diagnostic.contains(['/', '=']));
        assert_eq!(
            sanitize_stderr_first_line("x".repeat(300).as_bytes())
                .unwrap()
                .len(),
            256
        );
    }

    #[test]
    fn local_backend_runs_and_denies_oracle() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-m005-sandbox-{}-{}",
            std::process::id(),
            ORDINAL.fetch_add(1, Ordering::Relaxed)
        ));
        let source = root.join("source");
        let output = root.join("output");
        let forbidden = root.join("forbidden");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::create_dir_all(&forbidden).unwrap();
        let task = root.join("task.txt");
        fs::write(&task, "locate item").unwrap();
        let status = run_local_sandbox(LocalSandboxRequest {
            executable: PathBuf::from("/usr/bin/true"),
            expected_executable_digest: file_digest(Path::new("/usr/bin/true"), 256 << 20).unwrap(),
            allowed_executables: Vec::new(),
            arguments: Vec::new(),
            source_snapshot: source.clone(),
            readonly_roots: Vec::new(),
            request_files: vec![task.clone()],
            writable_roots: vec![output.clone()],
            forbidden_paths: vec![forbidden.clone()],
            max_duration: Duration::from_secs(1),
            capture_stderr: false,
        })
        .unwrap();
        assert!(matches!(status, SandboxOutcome::Exited { status, .. } if status.success()));
        let secret = forbidden.join("oracle.json");
        fs::write(&secret, "hidden").unwrap();
        let denied = run_local_sandbox(LocalSandboxRequest {
            executable: PathBuf::from("/bin/cat"),
            expected_executable_digest: file_digest(Path::new("/bin/cat"), 256 << 20).unwrap(),
            allowed_executables: Vec::new(),
            arguments: vec![secret.into_os_string()],
            source_snapshot: source,
            readonly_roots: Vec::new(),
            request_files: vec![task],
            writable_roots: vec![output],
            forbidden_paths: vec![forbidden],
            max_duration: Duration::from_secs(1),
            capture_stderr: false,
        })
        .unwrap();
        assert!(matches!(denied, SandboxOutcome::Exited { status, .. } if !status.success()));

        let home_secret = PathBuf::from(std::env::var_os("HOME").unwrap()).join(format!(
            ".kit-w07-sandbox-secret-{}-{}",
            std::process::id(),
            ORDINAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&home_secret, "private").unwrap();
        let home_denied = run_local_sandbox(LocalSandboxRequest {
            executable: PathBuf::from("/bin/cat"),
            expected_executable_digest: file_digest(Path::new("/bin/cat"), 256 << 20).unwrap(),
            allowed_executables: Vec::new(),
            arguments: vec![home_secret.clone().into_os_string()],
            source_snapshot: root.join("source"),
            readonly_roots: Vec::new(),
            request_files: vec![root.join("task.txt")],
            writable_roots: vec![root.join("output")],
            forbidden_paths: vec![root.join("forbidden")],
            max_duration: Duration::from_secs(1),
            capture_stderr: false,
        })
        .unwrap();
        assert!(matches!(home_denied, SandboxOutcome::Exited { status, .. } if !status.success()));
        fs::remove_file(home_secret).unwrap();

        let network_denied = run_local_sandbox(LocalSandboxRequest {
            executable: PathBuf::from("/usr/bin/nc"),
            expected_executable_digest: file_digest(Path::new("/usr/bin/nc"), 256 << 20).unwrap(),
            allowed_executables: Vec::new(),
            arguments: vec!["-l".into(), "127.0.0.1".into(), "54321".into()],
            source_snapshot: root.join("source"),
            readonly_roots: Vec::new(),
            request_files: vec![root.join("task.txt")],
            writable_roots: vec![root.join("output")],
            forbidden_paths: vec![root.join("forbidden")],
            max_duration: Duration::from_secs(1),
            capture_stderr: false,
        })
        .unwrap();
        assert!(
            matches!(network_denied, SandboxOutcome::Exited { status, .. } if !status.success())
        );
        fs::remove_dir_all(root).unwrap();
    }
}
