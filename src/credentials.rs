use std::{
    collections::HashMap,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
    time::{Duration, Instant},
};

use crate::resilient_fs as fs;
use keyring::{Entry, Error as KeyringError};
use zeroize::Zeroizing;

const KEYCHAIN_SERVICE: &str = "com.danielkov.kit.credentials";
const MCP_KEYCHAIN_SERVICE: &str = "com.danielkov.kit.mcp.oauth";
const OPENAI_KEYCHAIN_SERVICE: &str = "dev.kit.openai";
const REFRESH_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
static MEMORY: LazyLock<Mutex<HashMap<String, Zeroizing<Vec<u8>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static MEMORY_REFRESH_LOCK: LazyLock<Arc<tokio::sync::Mutex<()>>> =
    LazyLock::new(|| Arc::new(tokio::sync::Mutex::new(())));

// Authority only: credential bytes, tombstones, retry, and lease retention live
// exclusively in the shared filesystem. Weak references do not extend a caller's
// refresh guard lifetime. The guarded Fs carried by each mutation retains its lease.
static FILESYSTEM_SCOPES: LazyLock<Mutex<Vec<Weak<CredentialFilesystemScope>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

pub(crate) struct CredentialFilesystemScope {
    path: PathBuf,
    filesystem: fs::Fs,
}

impl CredentialFilesystemScope {
    pub(crate) fn register(
        lease: &fs::Lease,
        path: &Path,
    ) -> Result<Arc<Self>, CredentialStoreError> {
        let scope = Arc::new(Self {
            // The final credential file may not exist yet (first login).
            path: match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => {
                    fs::canonicalize(parent).map(|parent| parent.join(name))
                }
                _ => fs::canonicalize(path),
            }
            .map_err(|value| context("could not resolve credential scope", value))?,
            filesystem: fs::global()
                .guarded(lease)
                .map_err(|value| context("could not guard credential scope", value))?,
        });
        let mut scopes = FILESYSTEM_SCOPES
            .lock()
            .map_err(|_| error("credential lease registry is poisoned"))?;
        scopes.retain(|scope| scope.strong_count() != 0);
        scopes.push(Arc::downgrade(&scope));
        Ok(scope)
    }
}

fn mutation_filesystem(path: &Path) -> Result<fs::Fs, CredentialStoreError> {
    let directory = path
        .parent()
        .ok_or_else(|| error("credential path has no directory"))?;
    let normalized = fs::canonicalize(directory)
        .map_err(|value| context("could not resolve credential directory", value))?
        .join(
            path.file_name()
                .ok_or_else(|| error("credential path has no filename"))?,
        );
    {
        let scopes = FILESYSTEM_SCOPES
            .lock()
            .map_err(|_| error("credential lease registry is poisoned"))?;
        if let Some(scope) = scopes
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|scope| normalized.starts_with(&scope.path))
            .max_by_key(|scope| scope.path.components().count())
        {
            return Ok(scope.filesystem.clone());
        }
    }
    // Standalone login/delete callers also need real authority before a write can
    // be queued. Never replace an unavailable cross-process lock with a mutex.
    let guard = acquire_refresh_lock(&directory.join(".refresh.lock"))?;
    Ok(guard
        ._scope
        .as_ref()
        .expect("filesystem refresh scope")
        .filesystem
        .clone())
}

#[derive(Clone, Debug, Default)]
pub enum CredentialStorage {
    #[default]
    Memory,
    Keychain,
    Filesystem(PathBuf),
}

impl CredentialStorage {
    pub(crate) fn entry(&self, namespace: &str, identity: &str) -> CredentialEntry {
        let key = namespaced_key(namespace, identity);
        let backend = match self {
            Self::Memory => EntryBackend::Memory,
            Self::Keychain => EntryBackend::Keychain {
                namespace: namespace.to_owned(),
                identity: identity.to_owned(),
            },
            Self::Filesystem(directory) => {
                EntryBackend::Filesystem(directory.join(format!("{key}.json")))
            }
        };
        CredentialEntry { key, backend }
    }

    pub(crate) fn is_persistent(&self) -> bool {
        !matches!(self, Self::Memory)
    }

    pub(crate) async fn lock_refresh(&self) -> Result<CredentialRefreshLock, CredentialStoreError> {
        let path = match self {
            Self::Memory => {
                return Ok(CredentialRefreshLock {
                    _file: None,
                    _scope: None,
                    _memory: Some(Arc::clone(&MEMORY_REFRESH_LOCK).lock_owned().await),
                });
            }
            Self::Keychain => keychain_refresh_lock_path()?,
            Self::Filesystem(directory) => directory.join(".refresh.lock"),
        };
        tokio::task::spawn_blocking(move || acquire_refresh_lock(&path))
            .await
            .map_err(|value| error(format!("credential refresh lock task failed: {value}")))?
    }

    pub fn cli_name(&self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Keychain => "keychain",
            Self::Filesystem(_) => "file",
        }
    }

    pub fn directory(&self) -> Option<&Path> {
        match self {
            Self::Filesystem(path) => Some(path),
            Self::Memory | Self::Keychain => None,
        }
    }

    pub(crate) fn append_cli_args(&self, command: &mut tokio::process::Command) {
        command.arg("--credential-store").arg(self.cli_name());
        if let Some(directory) = self.directory() {
            command.arg("--credential-dir").arg(directory);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CredentialEntry {
    key: String,
    backend: EntryBackend,
}

#[derive(Clone, Debug)]
enum EntryBackend {
    Memory,
    Keychain { namespace: String, identity: String },
    Filesystem(PathBuf),
}

impl CredentialEntry {
    pub(crate) fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, CredentialStoreError> {
        match &self.backend {
            EntryBackend::Memory => Ok(MEMORY
                .lock()
                .map_err(|_| error("memory credential store is poisoned"))?
                .get(&self.key)
                .map(|bytes| Zeroizing::new(bytes.as_slice().to_vec()))),
            EntryBackend::Keychain {
                namespace,
                identity,
            } => keychain_load(namespace, identity),
            EntryBackend::Filesystem(path) => filesystem_load(path),
        }
    }

    pub(crate) fn filesystem_path(&self) -> Option<&Path> {
        match &self.backend {
            EntryBackend::Filesystem(path) => Some(path),
            EntryBackend::Memory | EntryBackend::Keychain { .. } => None,
        }
    }

    /// A user-facing persistence barrier, not a requirement for ongoing refresh.
    pub(crate) fn require_disk(&self) -> Result<(), CredentialStoreError> {
        match &self.backend {
            EntryBackend::Filesystem(path) => fs::global().require_disk(path).map_err(|value| {
                context(
                    "credential changes are retained only in this process; free disk space or quota and retry before exiting Kit",
                    value,
                )
            }),
            EntryBackend::Memory => Err(error(
                "credentials are stored only in memory; select --credential-store file or keychain for durable authentication",
            )),
            EntryBackend::Keychain { .. } => Ok(()),
        }
    }

    pub(crate) fn save(&self, bytes: &[u8]) -> Result<(), CredentialStoreError> {
        match &self.backend {
            EntryBackend::Memory => {
                MEMORY
                    .lock()
                    .map_err(|_| error("memory credential store is poisoned"))?
                    .insert(self.key.clone(), Zeroizing::new(bytes.to_vec()));
                Ok(())
            }
            EntryBackend::Keychain {
                namespace,
                identity,
            } => keychain_save(namespace, identity, bytes),
            EntryBackend::Filesystem(path) => write_private_file(path, bytes),
        }
    }

    pub(crate) fn delete(&self) -> Result<bool, CredentialStoreError> {
        match &self.backend {
            EntryBackend::Memory => Ok(MEMORY
                .lock()
                .map_err(|_| error("memory credential store is poisoned"))?
                .remove(&self.key)
                .is_some()),
            EntryBackend::Keychain {
                namespace,
                identity,
            } => keychain_delete(namespace, identity),
            EntryBackend::Filesystem(path) => filesystem_delete(path),
        }
    }
}

pub(crate) struct CredentialRefreshLock {
    // Pending facade mutations retain the real lease after this observer drops.
    _file: Option<fs::Lease>,
    _scope: Option<Arc<CredentialFilesystemScope>>,
    _memory: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl std::fmt::Debug for CredentialRefreshLock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialRefreshLock")
            .field("real_lease", &self._file.is_some())
            .field("memory_backend", &self._memory.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(crate) struct CredentialStoreError(String);

impl std::fmt::Display for CredentialStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CredentialStoreError {}

fn keychain_refresh_lock_path() -> Result<PathBuf, CredentialStoreError> {
    os_user_data_dir()
        .map(|directory| directory.join(".kit-auth/mcp-oauth-refresh.lock"))
        .ok_or_else(|| error("the OS user directory is unavailable for the OAuth refresh lock"))
}

#[cfg(unix)]
fn os_user_data_dir() -> Option<PathBuf> {
    use std::{ffi::CStr, os::unix::ffi::OsStrExt};

    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0_u8; 16 * 1024];
    // SAFETY: all pointers refer to live writable storage for the duration of getpwuid_r.
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r initialized entry and pw_dir points into buffer.
    let home = unsafe { CStr::from_ptr(entry.assume_init().pw_dir) };
    (!home.to_bytes().is_empty())
        .then(|| PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes())))
}

#[cfg(windows)]
fn os_user_data_dir() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath},
    };

    let mut raw = std::ptr::null_mut();
    // SAFETY: SHGetKnownFolderPath initializes raw on success; CoTaskMemFree releases it.
    let status =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, std::ptr::null_mut(), &mut raw) };
    if status < 0 || raw.is_null() {
        return None;
    }
    let mut length = 0;
    // SAFETY: raw is a NUL-terminated string returned by SHGetKnownFolderPath.
    while unsafe { *raw.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: the preceding scan established this initialized UTF-16 slice.
    let path = PathBuf::from(std::ffi::OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, length)
    }));
    // SAFETY: raw was allocated by SHGetKnownFolderPath.
    unsafe { CoTaskMemFree(raw.cast()) };
    Some(path)
}

fn acquire_refresh_lock(path: &Path) -> Result<CredentialRefreshLock, CredentialStoreError> {
    let directory = path
        .parent()
        .ok_or_else(|| error("credential refresh lock has no directory"))?;
    prepare_directory(directory, true)?;
    let deadline = Instant::now() + REFRESH_LOCK_TIMEOUT;
    loop {
        // The facade owns no-follow/owner/identity validation and real OS locking.
        // In particular, ENOSPC here must not grant in-process-only authority.
        match fs::global().acquire_lease(path, directory, fs::LeaseMode::ExistingOrNew) {
            Ok(lease) => {
                return Ok(CredentialRefreshLock {
                    _scope: Some(CredentialFilesystemScope::register(&lease, directory)?),
                    _file: Some(lease),
                    _memory: None,
                });
            }
            Err(value) if value.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(value) if value.kind() == ErrorKind::WouldBlock => {
                return Err(error(format!(
                    "timed out waiting for OAuth refresh lock {}",
                    path.display()
                )));
            }
            Err(value) => {
                return Err(context(
                    &format!(
                        "could not acquire real OAuth refresh lock {}",
                        path.display()
                    ),
                    value,
                ));
            }
        }
    }
}

fn namespaced_key(namespace: &str, identity: &str) -> String {
    // Preserve the existing MCP file names while namespacing all other records.
    if namespace == "mcp-oauth" {
        return blake3::hash(identity.as_bytes()).to_hex().to_string();
    }
    let mut input = Vec::with_capacity(namespace.len() + identity.len() + 1);
    input.extend_from_slice(namespace.as_bytes());
    input.push(0);
    input.extend_from_slice(identity.as_bytes());
    blake3::hash(&input).to_hex().to_string()
}

fn keychain_entry(namespace: &str, identity: &str) -> Result<Entry, CredentialStoreError> {
    let (service, account) = match (namespace, identity) {
        ("mcp-oauth", identity) => (
            MCP_KEYCHAIN_SERVICE,
            blake3::hash(identity.as_bytes()).to_hex().to_string(),
        ),
        ("openai-subscription", "subscription") => {
            (OPENAI_KEYCHAIN_SERVICE, "subscription".to_owned())
        }
        _ => (KEYCHAIN_SERVICE, namespaced_key(namespace, identity)),
    };
    Entry::new(service, &account).map_err(|value| context("could not open keychain", value))
}

fn keychain_load(
    namespace: &str,
    identity: &str,
) -> Result<Option<Zeroizing<Vec<u8>>>, CredentialStoreError> {
    match keychain_entry(namespace, identity)?.get_secret() {
        Ok(secret) => Ok(Some(Zeroizing::new(secret))),
        Err(KeyringError::NoEntry) => Ok(None),
        Err(value) => Err(context("could not read keychain", value)),
    }
}

fn keychain_save(
    namespace: &str,
    identity: &str,
    bytes: &[u8],
) -> Result<(), CredentialStoreError> {
    keychain_entry(namespace, identity)?
        .set_secret(bytes)
        .map_err(|value| context("could not write keychain", value))
}

fn keychain_delete(namespace: &str, identity: &str) -> Result<bool, CredentialStoreError> {
    match keychain_entry(namespace, identity)?.delete_credential() {
        Ok(()) => Ok(true),
        Err(KeyringError::NoEntry) => Ok(false),
        Err(value) => Err(context("could not clear keychain", value)),
    }
}

fn filesystem_load(path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>, CredentialStoreError> {
    let directory = path
        .parent()
        .ok_or_else(|| error("credential path has no directory"))?;
    if !prepare_directory(directory, false)? {
        return Ok(None);
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(value) if value.kind() == ErrorKind::NotFound => return Ok(None),
        Err(value) => {
            return Err(context(
                &format!("could not inspect {}", path.display()),
                value,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(error(format!(
            "OAuth credential path must be a regular file: {}",
            path.display()
        )));
    }
    check_private_file(path, &metadata)?;
    fs::read(path)
        .map(Zeroizing::new)
        .map(Some)
        .map_err(|value| context(&format!("could not read {}", path.display()), value))
}

fn filesystem_delete(path: &Path) -> Result<bool, CredentialStoreError> {
    let directory = path
        .parent()
        .ok_or_else(|| error("credential path has no directory"))?;
    if !prepare_directory(directory, false)? {
        return Ok(false);
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(error(format!(
                "OAuth credential path must be a regular file: {}",
                path.display()
            )))
        }
        Ok(_) => {
            mutation_filesystem(path)?
                .remove_file(path)
                .map_err(|value| context(&format!("could not remove {}", path.display()), value))?;
            Ok(true)
        }
        Err(value) if value.kind() == ErrorKind::NotFound => Ok(false),
        Err(value) => Err(context(
            &format!("could not inspect {}", path.display()),
            value,
        )),
    }
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), CredentialStoreError> {
    let directory = path
        .parent()
        .ok_or_else(|| error("credential path has no directory"))?;
    prepare_directory(directory, true)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(error(format!(
                "OAuth credential path must be a regular file: {}",
                path.display()
            )));
        }
        Ok(metadata) => check_private_file(path, &metadata)?,
        Err(value) if value.kind() == ErrorKind::NotFound => {}
        Err(value) => return Err(context("could not inspect credential file", value)),
    }
    mutation_filesystem(path)?
        .replace_private(path, bytes)
        .map_err(|value| context(&format!("could not replace {}", path.display()), value))
}

fn prepare_directory(path: &Path, create: bool) -> Result<bool, CredentialStoreError> {
    if create {
        fs::create_private_dir_all(path)
            .map_err(|value| context(&format!("could not create {}", path.display()), value))?;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(value) if !create && value.kind() == ErrorKind::NotFound => return Ok(false),
        Err(value) => {
            return Err(context(
                &format!("could not inspect {}", path.display()),
                value,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(error(format!(
            "OAuth credential directory must be a real directory, not a symlink: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Some(disk) = metadata.disk_metadata()
            && disk.uid() != unsafe { libc::geteuid() }
        {
            return Err(error(format!(
                "OAuth credential directory is owned by another user: {}",
                path.display()
            )));
        }
    }
    Ok(true)
}

#[cfg(unix)]
fn check_private_file(path: &Path, metadata: &fs::Metadata) -> Result<(), CredentialStoreError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if let Some(disk) = metadata.disk_metadata()
        && (disk.uid() != unsafe { libc::geteuid() } || disk.nlink() != 1)
    {
        return Err(error(format!(
            "OAuth credential file must be owned by this user and have one link: {}",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(error(format!(
            "OAuth credential file is accessible by other users: {}",
            path.display()
        )));
    }
    Ok(())
}
#[cfg(not(unix))]
fn check_private_file(_: &Path, _: &fs::Metadata) -> Result<(), CredentialStoreError> {
    Ok(())
}

fn error(value: impl Into<String>) -> CredentialStoreError {
    CredentialStoreError(value.into())
}
fn context(prefix: &str, value: impl std::fmt::Display) -> CredentialStoreError {
    error(format!("{prefix}: {value}"))
}

#[cfg(test)]
mod test_support {
    use super::*;

    impl CredentialStorage {
        pub(crate) fn make_entry_undeletable_for_test(&self, namespace: &str, identity: &str) {
            let entry = self.entry(namespace, identity);
            entry.save(b"blocked").unwrap();
            let EntryBackend::Filesystem(path) = entry.backend else {
                panic!("undeletable credential fixtures require filesystem storage");
            };
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use super::{CredentialStorage, acquire_refresh_lock};

    #[test]
    fn memory_is_shared_across_clones_and_namespaced() {
        let storage = CredentialStorage::Memory;
        let clone = storage.clone();
        let identity = format!("memory-{}", std::process::id());
        let first = storage.entry("first", &identity);
        let same = clone.entry("first", &identity);
        let other = clone.entry("second", &identity);
        first.save(b"secret").unwrap();
        assert_eq!(same.load().unwrap().unwrap().as_slice(), b"secret");
        assert!(other.load().unwrap().is_none());
        assert!(same.delete().unwrap());
        assert!(first.load().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn keychain_refresh_lock_uses_the_os_account_home() {
        let path = super::keychain_refresh_lock_path().unwrap();
        assert!(path.starts_with(super::os_user_data_dir().unwrap()));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("mcp-oauth-refresh.lock")
        );
    }

    #[test]
    fn refresh_lock_serializes_independent_open_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".refresh.lock");
        let first = acquire_refresh_lock(&path).unwrap();
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let second = acquire_refresh_lock(&path).unwrap();
            sender.send(second).unwrap();
        });
        assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
        drop(first);
        let second = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(second);
        thread.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refresh_lock_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"").unwrap();
        let path = directory.path().join(".refresh.lock");
        symlink(target, &path).unwrap();
        assert!(acquire_refresh_lock(&path).is_err());
    }

    #[test]
    fn memory_acceptance_does_not_claim_durable_credentials() {
        let entry = CredentialStorage::Memory.entry("durability-test", "memory");
        entry.save(b"secret").unwrap();
        assert!(
            entry
                .require_disk()
                .unwrap_err()
                .to_string()
                .contains("only in memory")
        );
        assert_eq!(entry.load().unwrap().unwrap().as_slice(), b"secret");
        entry.delete().unwrap();
    }

    #[test]
    fn standalone_filesystem_writes_and_deletions_have_durability_barriers() {
        let directory = tempfile::tempdir().unwrap();
        let entry = CredentialStorage::Filesystem(directory.path().to_path_buf())
            .entry("durability-test", "file");
        entry.save(b"secret").unwrap();
        entry.require_disk().unwrap();
        assert_eq!(
            std::fs::read(entry.filesystem_path().unwrap()).unwrap(),
            b"secret"
        );
        assert!(entry.delete().unwrap());
        entry.require_disk().unwrap();
        assert!(entry.load().unwrap().is_none());
        assert!(!entry.filesystem_path().unwrap().exists());
    }

    #[test]
    fn refresh_scope_is_reused_for_guarded_mutations() {
        let directory = tempfile::tempdir().unwrap();
        let entry = CredentialStorage::Filesystem(directory.path().to_path_buf())
            .entry("guard-test", "file");
        let guard = acquire_refresh_lock(&directory.path().join(".refresh.lock")).unwrap();
        // Reopening the OS lock here would deadlock against our own refresh guard.
        entry.save(b"secret").unwrap();
        assert!(entry.delete().unwrap());
        entry.require_disk().unwrap();
        drop(guard);
        assert!(entry.load().unwrap().is_none());
    }

    #[test]
    fn native_scope_guards_a_credential_outside_the_lock_directory() {
        let locks = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let entry = CredentialStorage::Filesystem(directory.path().to_path_buf())
            .entry("native-guard-test", "file");
        let path = entry.filesystem_path().unwrap();
        let lease = super::fs::global()
            .acquire_lease(
                locks.path().join("native.lock"),
                path,
                super::fs::LeaseMode::ExistingOrNew,
            )
            .unwrap();
        let scope = super::CredentialFilesystemScope::register(&lease, path).unwrap();
        entry.save(b"secret").unwrap();
        entry.require_disk().unwrap();
        // A separate .refresh.lock would prove the native authority was not used.
        assert!(!directory.path().join(".refresh.lock").exists());
        assert!(entry.delete().unwrap());
        entry.require_disk().unwrap();
        drop(scope);
        drop(lease);
    }

    #[test]
    fn mcp_files_keep_the_legacy_identity_hash() {
        let directory = tempfile::tempdir().unwrap();
        let identity = "https://example.com\0client\0metadata";
        CredentialStorage::Filesystem(directory.path().to_path_buf())
            .entry("mcp-oauth", identity)
            .save(b"record")
            .unwrap();
        assert!(
            directory
                .path()
                .join(format!(
                    "{}.json",
                    blake3::hash(identity.as_bytes()).to_hex()
                ))
                .is_file()
        );
    }

    #[test]
    fn filesystem_namespaces_use_distinct_files() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().to_path_buf());
        let first = storage.entry("first", "same");
        let second = storage.entry("second", "same");
        first.save(b"one").unwrap();
        second.save(b"two").unwrap();
        assert_eq!(first.load().unwrap().unwrap().as_slice(), b"one");
        assert_eq!(second.load().unwrap().unwrap().as_slice(), b"two");
        assert_eq!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "json"))
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().to_path_buf());
        let entry = storage.entry("test", "permissions");
        entry.save(b"secret").unwrap();
        entry.require_disk().unwrap();
        let file = entry.filesystem_path().unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            directory.path().metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_rejects_symlinked_directories_and_insecure_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let link = root.path().join("link");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();
        let linked = CredentialStorage::Filesystem(link).entry("test", "server");
        assert!(linked.save(b"secret").is_err());

        let storage = CredentialStorage::Filesystem(real.clone());
        let entry = storage.entry("test", "server");
        entry.save(b"secret").unwrap();
        let path = entry.filesystem_path().unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(entry.load().is_err());
    }
}
