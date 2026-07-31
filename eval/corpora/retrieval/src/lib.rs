#![forbid(unsafe_code)]

mod archive;
mod grader;
mod model;
mod protocol;
mod run;
mod sandbox;
mod startup_probe;
mod verifier;
mod worker;

pub use archive::{archive_check, archive_verify, evidence_size_check};
pub use grader::{grade, project};
pub use model::*;
pub use protocol::{prepare, refresh_frozen, verify, verify_with_vendor};
pub use run::{cleanup_failed_run, run_canary, run_local, run_trusted};
pub use sandbox::{
    LocalSandboxRequest, LocalWorkerSandboxRequest, SandboxOutcome, run_local_sandbox,
    run_local_worker_sandbox,
};
pub use startup_probe::run_worker_startup_probe;
pub use worker::run_worker;

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::{
    error::Error,
    fmt, fs,
    path::{Component, Path, PathBuf},
};

pub const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SOURCE_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;
pub const UNITS_PER_CLASS: usize = 24;
pub const UNIT_COUNT: usize = 72;

#[derive(Debug)]
pub struct ProtocolError(pub String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ProtocolError {}

pub(crate) type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub(crate) fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.is_empty() || bytes.len() > MAX_JSON_BYTES {
        return Err(ProtocolError("canonical JSON exceeds its bound".into()).into());
    }
    Ok(bytes)
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn canonicalize_vendor_root(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        return Err(ProtocolError("vendor root is not an existing directory".into()).into());
    }
    let root = path.canonicalize()?;
    reject_symlink_components(&root, false)?;
    if !fs::symlink_metadata(&root)?.file_type().is_dir() {
        return Err(ProtocolError("canonical vendor root is not a directory".into()).into());
    }
    Ok(root)
}

pub(crate) fn reject_symlink_components(path: &Path, allow_missing: bool) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(value) => current.push(value.as_os_str()),
            Component::RootDir => current.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                current.pop();
            }
            Component::Normal(value) => current.push(value),
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ProtocolError(format!(
                    "symlink path component rejected: {}",
                    current.display()
                ))
                .into());
            }
            Ok(_) => {}
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod io_tests {
    use super::*;

    #[test]
    fn rejects_symlinked_ancestor_components() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-w07-io-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert!(reject_symlink_components(&root.join("link/file"), true).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn canonicalizes_system_root_alias_but_rejects_descendant_symlinks() {
        let alias = PathBuf::from("/var/tmp").join(format!(
            "kit-w07-vendor-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        fs::create_dir_all(alias.join("real")).unwrap();
        assert!(reject_symlink_components(&alias, false).is_err());

        let root = canonicalize_vendor_root(&alias).unwrap();
        assert!(root.starts_with("/private/var/"));
        reject_symlink_components(&root.join("real"), false).unwrap();

        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert!(reject_symlink_components(&root.join("link/file"), true).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
