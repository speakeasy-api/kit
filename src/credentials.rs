use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use keyring::{Entry, Error as KeyringError};
use zeroize::Zeroizing;

const KEYCHAIN_SERVICE: &str = "com.danielkov.kit.credentials";
const MCP_KEYCHAIN_SERVICE: &str = "com.danielkov.kit.mcp.oauth";
const OPENAI_KEYCHAIN_SERVICE: &str = "dev.kit.openai";
static MEMORY: LazyLock<Mutex<HashMap<String, Zeroizing<Vec<u8>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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

#[derive(Debug)]
pub(crate) struct CredentialStoreError(String);

impl std::fmt::Display for CredentialStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CredentialStoreError {}

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
            fs::remove_file(path)
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
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(error(format!(
            "OAuth credential path must be a regular file: {}",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    set_private_mode(&mut options);
    atomicwrites::AtomicFile::new(path, atomicwrites::AllowOverwrite)
        .write_with_options(|file| file.write_all(bytes), options)
        .map_err(|value| context(&format!("could not replace {}", path.display()), value))
}

fn prepare_directory(path: &Path, create: bool) -> Result<bool, CredentialStoreError> {
    if create {
        fs::create_dir_all(path)
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
    if create {
        make_directory_private(path)?;
    }
    Ok(true)
}

#[cfg(unix)]
fn set_private_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}
#[cfg(not(unix))]
fn set_private_mode(_: &mut OpenOptions) {}
#[cfg(unix)]
fn make_directory_private(path: &Path) -> Result<(), CredentialStoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|value| context(&format!("could not protect {}", path.display()), value))
}
#[cfg(not(unix))]
fn make_directory_private(_: &Path) -> Result<(), CredentialStoreError> {
    Ok(())
}
#[cfg(unix)]
fn check_private_file(path: &Path, metadata: &fs::Metadata) -> Result<(), CredentialStoreError> {
    use std::os::unix::fs::PermissionsExt;
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
mod tests {
    use super::CredentialStorage;

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
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().to_path_buf());
        storage
            .entry("test", "permissions")
            .save(b"secret")
            .unwrap();
        let file = std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
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
        let path = std::fs::read_dir(real)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(entry.load().is_err());
    }
}
