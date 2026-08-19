use async_trait::async_trait;
use rmcp::transport::auth::{AuthError, AuthorizationManager, CredentialStore, StoredCredentials};
use zeroize::Zeroizing;

use crate::credentials::{CredentialEntry, CredentialStorage, CredentialStoreError};

const NAMESPACE: &str = "mcp-oauth";

pub(super) fn configure(
    storage: &CredentialStorage,
    manager: &mut AuthorizationManager,
    identity: &str,
) {
    manager.set_credential_store(McpCredentialStore {
        entry: storage.entry(NAMESPACE, identity),
        source: match storage {
            CredentialStorage::Keychain => Source::Keychain,
            CredentialStorage::Memory | CredentialStorage::Filesystem(_) => Source::File,
        },
    });
}

#[derive(Clone, Copy)]
enum Source {
    Keychain,
    File,
}

struct McpCredentialStore {
    entry: CredentialEntry,
    source: Source,
}

#[async_trait]
impl CredentialStore for McpCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let entry = self.entry.clone();
        let bytes = blocking(move || entry.load()).await?;
        bytes
            .map(|bytes| {
                serde_json::from_slice(&bytes).map_err(|value| {
                    AuthError::InternalError(format!(
                        "{}: {value}",
                        match self.source {
                            Source::Keychain => "invalid OAuth credentials in keychain",
                            Source::File => "invalid OAuth credential file",
                        }
                    ))
                })
            })
            .transpose()
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let bytes = Zeroizing::new(serde_json::to_vec(&credentials).map_err(|value| {
            AuthError::InternalError(format!("could not encode OAuth credentials: {value}"))
        })?);
        let entry = self.entry.clone();
        blocking(move || entry.save(&bytes)).await
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let entry = self.entry.clone();
        blocking(move || entry.delete().map(|_| ())).await
    }
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, CredentialStoreError> + Send + 'static,
) -> Result<T, AuthError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|value| {
            AuthError::InternalError(format!("credential storage task failed: {value}"))
        })?
        .map_err(|value| AuthError::InternalError(value.to_string()))
}

#[cfg(test)]
mod tests {
    use rmcp::transport::auth::{CredentialStore, StoredCredentials};

    use super::{McpCredentialStore, Source};
    use crate::credentials::CredentialStorage;

    #[tokio::test]
    async fn adapter_round_trips_through_shared_memory() {
        let storage = CredentialStorage::Memory;
        let identity = format!("adapter-{}", std::process::id());
        let store = McpCredentialStore {
            entry: storage.entry("mcp-oauth", &identity),
            source: Source::File,
        };
        store
            .save(StoredCredentials::new(
                "client".into(),
                None,
                vec!["read".into()],
                None,
            ))
            .await
            .unwrap();
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "client");
        assert_eq!(loaded.granted_scopes, ["read"]);
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
    }
}
