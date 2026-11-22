//! Client secrets issued by dynamic client registration.
use agenthooksprotocol::{
    auth::{
        AuthErrorCode,
        managed::{ClientSecret, ClientSecretStorage, OperationBudget},
    },
    client::{HookError, LocalFuture},
};
use zeroize::Zeroizing;

use super::store::{SECRETS, Store};

fn unavailable() -> HookError {
    HookError::authentication(AuthErrorCode::CredentialUnavailable)
}

/// Kept in Kit's credential storage beside the backend's records, under the
/// SDK-chosen reference, and removed with them on logout.
pub(super) struct Secrets(pub(super) Store);

impl ClientSecretStorage for Secrets {
    fn store(
        &self,
        reference: String,
        secret: ClientSecret,
        budget: OperationBudget,
    ) -> LocalFuture<'_, Result<(), HookError>> {
        Box::pin(async move {
            // The record lock first, held through the save, then the index
            // (the order logout takes them): a logout cannot remove the
            // index entry between indexing and saving.
            let _lock = self.0.lock(SECRETS, &reference, budget).await?;
            self.0.remember(SECRETS, &reference, budget).await?;
            let entry = self.0.storage().entry(SECRETS, &reference);
            let bytes = Zeroizing::new(secret.secret().as_bytes().to_vec());
            tokio::task::spawn_blocking(move || entry.save(&bytes))
                .await
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())
        })
    }

    fn resolve(
        &self,
        reference: String,
        budget: OperationBudget,
    ) -> LocalFuture<'_, Result<Option<ClientSecret>, HookError>> {
        Box::pin(async move {
            let _lock = self.0.lock(SECRETS, &reference, budget).await?;
            let entry = self.0.storage().entry(SECRETS, &reference);
            let bytes = tokio::task::spawn_blocking(move || entry.load())
                .await
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())?;
            Ok(bytes
                .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
                .map(ClientSecret::new))
        })
    }

    fn delete(
        &self,
        reference: String,
        budget: OperationBudget,
    ) -> LocalFuture<'_, Result<(), HookError>> {
        Box::pin(async move {
            let _lock = self.0.lock(SECRETS, &reference, budget).await?;
            let entry = self.0.storage().entry(SECRETS, &reference);
            tokio::task::spawn_blocking(move || entry.delete())
                .await
                .map_err(|_| unavailable())?
                .map(|_| ())
                .map_err(|_| unavailable())
        })
    }
}

/// Secrets the user names by reference in hooks.json (`clientSecretRef`,
/// `initialAccessTokenRef`, a bearer `tokenRef`): `~/.kit/ahp-secrets.json`,
/// `{ "version": 1, "secrets": { "<ref>": "<secret>" } }`, readable only by
/// its owner. The same layout other AHP adapters keep in their own directory.
pub(crate) fn references() -> std::collections::BTreeMap<String, String> {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return Default::default();
    };
    references_in(&std::path::PathBuf::from(home).join(".kit/ahp-secrets.json"))
}

/// Reads the secrets file at `path` through one handle: its permissions are
/// checked on the file that supplies the bytes, and a file whose
/// permissions cannot be checked is refused.
fn references_in(path: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    use std::io::Read as _;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Default::default(),
        Err(error) => {
            eprintln!("kit: ignoring {}: {error}", path.display());
            return Default::default();
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => {
            eprintln!(
                "kit: ignoring {}: not a readable regular file",
                path.display()
            );
            return Default::default();
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        // SAFETY: geteuid has no preconditions and cannot fail.
        let owner = unsafe { libc::geteuid() };
        if metadata.permissions().mode() & 0o077 != 0 || metadata.uid() != owner {
            eprintln!(
                "kit: ignoring {}: it must be yours and readable only by you; chmod 600 it",
                path.display()
            );
            return Default::default();
        }
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        eprintln!("kit: ignoring {}: could not read it", path.display());
        return Default::default();
    }
    #[derive(serde::Deserialize)]
    struct File {
        version: u32,
        #[serde(default)]
        secrets: std::collections::BTreeMap<String, String>,
    }
    match serde_json::from_slice::<File>(&bytes) {
        Ok(file) if file.version == 1 => file.secrets,
        _ => {
            eprintln!(
                "kit: ignoring {}: not a version 1 secrets file",
                path.display()
            );
            Default::default()
        }
    }
}

#[cfg(all(test, unix))]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    use agenthooksprotocol::auth::managed::{ClientSecret, ClientSecretStorage, OperationBudget};

    use super::{SECRETS, Secrets, Store, references_in};
    use crate::credentials::CredentialStorage;

    #[tokio::test]
    async fn a_secret_is_indexed_only_under_its_record_lock() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().to_path_buf());
        let store = Store::new(storage, "test#secret-lock");
        let budget = || OperationBudget::until(Instant::now() + Duration::from_secs(10));
        // Logout holds the record lock: the writer must not index meanwhile.
        let held = store.lock(SECRETS, "dcr-secret", budget()).await.unwrap();
        let secrets = Secrets(store.clone());
        let write = secrets.store(
            "dcr-secret".into(),
            ClientSecret::new("value".into()),
            budget(),
        );
        let check = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                store.indexed().await.is_empty(),
                "indexed before taking the record lock"
            );
            drop(held);
        };
        let (written, ()) = tokio::join!(write, check);
        written.unwrap();
        assert!(
            store
                .indexed()
                .await
                .contains(&(SECRETS.to_owned(), "dcr-secret".to_owned()))
        );
        // And logout finds it.
        assert_eq!(store.forget(budget()).await.unwrap(), 1);
    }

    #[test]
    fn the_secrets_file_must_be_private() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ahp-secrets.json");
        std::fs::write(&path, r#"{"version": 1, "secrets": {"ref": "value"}}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(references_in(&path).is_empty());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(references_in(&path)["ref"], "value");
        // Not a file: refused rather than read through another lookup.
        assert!(references_in(directory.path()).is_empty());
        assert!(references_in(&directory.path().join("missing.json")).is_empty());
    }
}
