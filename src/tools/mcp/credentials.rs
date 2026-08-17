use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

#[cfg(target_os = "macos")]
use apple_native_keyring_store::keychain::{Cred, MacKeychainDomain};
use async_trait::async_trait;
#[cfg(target_os = "macos")]
use keyring_core::Error as KeyringError;
use rmcp::transport::auth::{AuthError, AuthorizationManager, CredentialStore, StoredCredentials};
use zeroize::Zeroizing;

const KEYCHAIN_SERVICE: &str = "com.danielkov.kit.mcp.oauth";

#[derive(Clone, Debug, Default)]
pub enum CredentialStorage {
    #[default]
    Memory,
    Keychain,
    Filesystem(PathBuf),
}

impl CredentialStorage {
    pub(crate) fn is_persistent(&self) -> bool {
        !matches!(self, Self::Memory)
    }

    pub(crate) fn configure(&self, manager: &mut AuthorizationManager, identity: &str) {
        if let Some(store) = self.store(identity) {
            manager.set_credential_store(store);
        }
    }

    fn store(&self, identity: &str) -> Option<PersistentStore> {
        let account = blake3::hash(identity.as_bytes()).to_hex().to_string();
        match self {
            Self::Memory => None,
            Self::Keychain => Some(PersistentStore::Keychain(KeychainStore { account })),
            Self::Filesystem(directory) => Some(PersistentStore::Filesystem(FilesystemStore {
                path: directory.join(format!("{account}.json")),
            })),
        }
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

enum PersistentStore {
    Keychain(KeychainStore),
    Filesystem(FilesystemStore),
}

#[async_trait]
impl CredentialStore for PersistentStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match self {
            Self::Keychain(store) => store.load().await,
            Self::Filesystem(store) => store.load().await,
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        match self {
            Self::Keychain(store) => store.save(credentials).await,
            Self::Filesystem(store) => store.save(credentials).await,
        }
    }

    async fn clear(&self) -> Result<(), AuthError> {
        match self {
            Self::Keychain(store) => store.clear().await,
            Self::Filesystem(store) => store.clear().await,
        }
    }
}

#[derive(Clone)]
struct KeychainStore {
    account: String,
}

#[cfg(target_os = "macos")]
#[async_trait]
impl CredentialStore for KeychainStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let account = self.account.clone();
        blocking(move || {
            let entry = Cred::build(MacKeychainDomain::User, KEYCHAIN_SERVICE, &account)
                .map_err(|error| auth_error("could not open keychain", error))?;
            let secret = match entry.get_password() {
                Ok(secret) => Zeroizing::new(secret),
                Err(KeyringError::NoEntry) => return Ok(None),
                Err(error) => return Err(auth_error("could not read keychain", error)),
            };
            serde_json::from_str(&secret)
                .map(Some)
                .map_err(|error| auth_error("invalid OAuth credentials in keychain", error))
        })
        .await
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let account = self.account.clone();
        let secret = Zeroizing::new(
            serde_json::to_string(&credentials)
                .map_err(|error| auth_error("could not encode OAuth credentials", error))?,
        );
        blocking(move || {
            Cred::build(MacKeychainDomain::User, KEYCHAIN_SERVICE, &account)
                .and_then(|entry| entry.set_password(&secret))
                .map_err(|error| auth_error("could not write keychain", error))
        })
        .await
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let account = self.account.clone();
        blocking(move || {
            let entry = Cred::build(MacKeychainDomain::User, KEYCHAIN_SERVICE, &account)
                .map_err(|error| auth_error("could not open keychain", error))?;
            match entry.delete_credential() {
                Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
                Err(error) => Err(auth_error("could not clear keychain", error)),
            }
        })
        .await
    }
}

#[cfg(not(target_os = "macos"))]
#[async_trait]
impl CredentialStore for KeychainStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        Err(keychain_unavailable(&self.account))
    }

    async fn save(&self, _: StoredCredentials) -> Result<(), AuthError> {
        Err(keychain_unavailable(&self.account))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        Err(keychain_unavailable(&self.account))
    }
}

#[cfg(not(target_os = "macos"))]
fn keychain_unavailable(_: &str) -> AuthError {
    AuthError::InternalError("keychain credential storage is only available on macOS".into())
}

#[derive(Clone)]
struct FilesystemStore {
    path: PathBuf,
}

#[async_trait]
impl CredentialStore for FilesystemStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let path = self.path.clone();
        blocking(move || {
            let directory = path.parent().ok_or_else(|| {
                AuthError::InternalError("credential path has no directory".into())
            })?;
            if !prepare_directory(directory, false)? {
                return Ok(None);
            }
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(auth_error(
                        &format!("could not inspect {}", path.display()),
                        error,
                    ));
                }
            };
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(AuthError::InternalError(format!(
                    "OAuth credential path must be a regular file: {}",
                    path.display()
                )));
            }
            check_private_file(&path, &metadata)?;
            let bytes = Zeroizing::new(fs::read(&path).map_err(|error| {
                auth_error(&format!("could not read {}", path.display()), error)
            })?);
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| auth_error("invalid OAuth credential file", error))
        })
        .await
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let path = self.path.clone();
        let bytes = Zeroizing::new(
            serde_json::to_vec(&credentials)
                .map_err(|error| auth_error("could not encode OAuth credentials", error))?,
        );
        blocking(move || write_private_file(&path, &bytes)).await
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let path = self.path.clone();
        blocking(move || {
            let directory = path.parent().ok_or_else(|| {
                AuthError::InternalError("credential path has no directory".into())
            })?;
            if !prepare_directory(directory, false)? {
                return Ok(());
            }
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    Err(AuthError::InternalError(format!(
                        "OAuth credential path must be a regular file: {}",
                        path.display()
                    )))
                }
                Ok(_) => fs::remove_file(&path).map_err(|error| {
                    auth_error(&format!("could not remove {}", path.display()), error)
                }),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
                Err(error) => Err(auth_error(
                    &format!("could not inspect {}", path.display()),
                    error,
                )),
            }
        })
        .await
    }
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            AuthError::InternalError(format!("credential storage task failed: {error}"))
        })?
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
    let directory = path
        .parent()
        .ok_or_else(|| AuthError::InternalError("credential path has no directory".into()))?;
    prepare_directory(directory, true)?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(AuthError::InternalError(format!(
            "OAuth credential path must be a regular file: {}",
            path.display()
        )));
    }

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    set_private_mode(&mut options);
    atomicwrites::AtomicFile::new(path, atomicwrites::AllowOverwrite)
        .write_with_options(|file| file.write_all(bytes), options)
        .map_err(|error| auth_error(&format!("could not replace {}", path.display()), error))
}

fn prepare_directory(path: &Path, create: bool) -> Result<bool, AuthError> {
    if create {
        fs::create_dir_all(path)
            .map_err(|error| auth_error(&format!("could not create {}", path.display()), error))?;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if !create && error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(auth_error(
                &format!("could not inspect {}", path.display()),
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AuthError::InternalError(format!(
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
fn make_directory_private(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| auth_error(&format!("could not protect {}", path.display()), error))
}

#[cfg(not(unix))]
fn make_directory_private(_: &Path) -> Result<(), AuthError> {
    Ok(())
}

#[cfg(unix)]
fn check_private_file(path: &Path, metadata: &fs::Metadata) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(AuthError::InternalError(format!(
            "OAuth credential file is accessible by other users: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private_file(_: &Path, _: &fs::Metadata) -> Result<(), AuthError> {
    Ok(())
}

fn auth_error(context: &str, error: impl std::fmt::Display) -> AuthError {
    AuthError::InternalError(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use rmcp::transport::auth::{CredentialStore, StoredCredentials};

    use super::FilesystemStore;

    #[tokio::test]
    async fn filesystem_store_round_trips_and_clears_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let store = FilesystemStore {
            path: directory.path().join("credentials.json"),
        };
        let credentials = StoredCredentials::new("client".into(), None, vec!["read".into()], None);

        store.save(credentials).await.unwrap();
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "client");
        assert_eq!(loaded.granted_scopes, ["read"]);
        store
            .save(StoredCredentials::new(
                "replacement".into(),
                None,
                vec!["write".into()],
                None,
            ))
            .await
            .unwrap();
        assert_eq!(
            store.load().await.unwrap().unwrap().client_id,
            "replacement"
        );
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn filesystem_store_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        let store = FilesystemStore { path: path.clone() };
        store
            .save(StoredCredentials::new("client".into(), None, vec![], None))
            .await
            .unwrap();

        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            directory.path().metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn filesystem_store_rejects_symlinked_directories_and_insecure_files() {
        use std::{os::unix::fs::PermissionsExt, os::unix::fs::symlink};

        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let link = root.path().join("link");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();
        let linked = FilesystemStore {
            path: link.join("credentials.json"),
        };
        assert!(
            linked
                .save(StoredCredentials::new("client".into(), None, vec![], None))
                .await
                .is_err()
        );

        let path = real.join("credentials.json");
        let encoded =
            serde_json::to_vec(&StoredCredentials::new("client".into(), None, vec![], None))
                .unwrap();
        std::fs::write(&path, encoded).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let insecure = FilesystemStore { path };
        assert!(insecure.load().await.is_err());
    }
}
