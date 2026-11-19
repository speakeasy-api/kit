//! SDK credential records in Kit's credential storage, one lock per record.
use std::collections::BTreeSet;

use agenthooksprotocol::{
    auth::{
        AuthErrorCode,
        managed::{CredentialGuard, CredentialStore, OperationBudget, SessionKey, SessionRecord},
    },
    client::{HookError, LocalFuture},
};

use crate::credentials::{CredentialEntryLock, CredentialStorage};

/// SDK binding, grant and registration records.
pub(super) const RECORDS: &str = "ahp-oauth";
/// Client secrets issued by dynamic registration.
pub(super) const SECRETS: &str = "ahp-oauth-secret";
/// Per backend, every record and secret Kit wrote for it, so that logging out
/// of one backend removes exactly its own state.
const INDEX: &str = "ahp-oauth-index";
/// Every scope with an index, so a backend's state can be found after its
/// hooks file moved (a renamed or re-cloned project).
const SCOPES: (&str, &str) = ("ahp-oauth-scopes", "all");

fn unavailable() -> HookError {
    HookError::authentication(AuthErrorCode::ProviderFailed)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, crate::credentials::CredentialStoreError> + Send + 'static,
) -> Result<T, HookError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())
}

/// The store one backend's OAuth uses. Records are shared by every Kit
/// process using the same credential storage.
#[derive(Clone)]
pub(super) struct Store {
    storage: CredentialStorage,
    backend: String,
}

impl Store {
    pub(super) fn new(storage: CredentialStorage, backend: impl Into<String>) -> Self {
        Self {
            storage,
            backend: backend.into(),
        }
    }

    pub(super) async fn lock(
        &self,
        namespace: &str,
        identity: &str,
        budget: OperationBudget,
    ) -> Result<CredentialEntryLock, HookError> {
        self.storage
            .lock_entry(namespace, identity, budget.deadline())
            .await
            .map_err(|_| {
                HookError::authentication(AuthErrorCode::ProviderFailed).classified(
                    agenthooksprotocol::generated::DeliveryDiagnosticCode::DeadlineExceeded,
                )
            })
    }

    async fn index(&self) -> Result<BTreeSet<(String, String)>, HookError> {
        let entry = self.storage.entry(INDEX, &self.backend);
        let bytes = blocking(move || entry.load()).await?;
        Ok(bytes
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default())
    }

    async fn save_index(&self, index: &BTreeSet<(String, String)>) -> Result<(), HookError> {
        let entry = self.storage.entry(INDEX, &self.backend);
        let bytes = serde_json::to_vec(index).map_err(|_| unavailable())?;
        blocking(move || entry.save(&bytes)).await
    }

    /// Records that `identity` in `namespace` belongs to this backend. The
    /// index lock is only ever taken last, under at most record locks.
    pub(super) async fn remember(
        &self,
        namespace: &str,
        identity: &str,
        budget: OperationBudget,
    ) -> Result<(), HookError> {
        let _lock = self.lock(INDEX, &self.backend, budget).await?;
        let mut index = self.index().await?;
        let first = index.is_empty();
        if index.insert((namespace.to_owned(), identity.to_owned())) {
            if first {
                self.list_scope(true, budget).await?;
            }
            self.save_index(&index).await?;
        }
        Ok(())
    }

    /// Adds this scope to, or removes it from, the list of all scopes.
    async fn list_scope(&self, present: bool, budget: OperationBudget) -> Result<(), HookError> {
        let _lock = self.lock(SCOPES.0, SCOPES.1, budget).await?;
        let mut scopes = scopes(&self.storage).await?;
        let changed = if present {
            scopes.insert(self.backend.clone())
        } else {
            scopes.remove(&self.backend)
        };
        if changed {
            let entry = self.storage.entry(SCOPES.0, SCOPES.1);
            let bytes = serde_json::to_vec(&scopes).map_err(|_| unavailable())?;
            blocking(move || entry.save(&bytes)).await?;
        }
        Ok(())
    }

    /// Deletes every record and secret this backend wrote, returning how
    /// many existed. Records are
    /// deleted under their own locks, so a concurrent writer finishes first.
    pub(super) async fn forget(&self, budget: OperationBudget) -> Result<usize, HookError> {
        let index = {
            let _lock = self.lock(INDEX, &self.backend, budget).await?;
            self.index().await?
        };
        let mut deleted = 0;
        for (namespace, identity) in &index {
            // The record and its index entry go together, under the record's
            // lock (taken before the index, as everywhere): a writer that
            // recreates the record afterwards also re-indexes it.
            let _record = self.lock(namespace, identity, budget).await?;
            let entry = self.storage.entry(namespace, identity);
            if blocking(move || entry.delete()).await? {
                deleted += 1;
            }
            let _index = self.lock(INDEX, &self.backend, budget).await?;
            let mut current = self.index().await?;
            current.remove(&(namespace.clone(), identity.clone()));
            if current.is_empty() {
                let entry = self.storage.entry(INDEX, &self.backend);
                blocking(move || entry.delete()).await?;
                self.list_scope(false, budget).await?;
            } else {
                self.save_index(&current).await?;
            }
        }
        Ok(deleted)
    }

    pub(super) fn storage(&self) -> &CredentialStorage {
        &self.storage
    }
}

/// Every scope that has stored records.
pub(super) async fn scopes(storage: &CredentialStorage) -> Result<BTreeSet<String>, HookError> {
    let entry = storage.entry(SCOPES.0, SCOPES.1);
    let bytes = blocking(move || entry.load()).await?;
    Ok(bytes
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default())
}

impl CredentialStore for Store {
    fn acquire(
        &self,
        key: SessionKey,
        budget: OperationBudget,
    ) -> LocalFuture<'_, Result<Box<dyn CredentialGuard>, HookError>> {
        Box::pin(async move {
            let identity = key.as_str().to_owned();
            let lock = self.lock(RECORDS, &identity, budget).await?;
            // Indexed before any write: a commit may outlive the caller's
            // budget, and must then never wait on the index. Checked every
            // time, since another process's logout may have removed it.
            self.remember(RECORDS, &identity, budget).await?;
            Ok(Box::new(Guard {
                store: self.clone(),
                identity,
                _lock: lock,
            }) as Box<dyn CredentialGuard>)
        })
    }
}

struct Guard {
    store: Store,
    identity: String,
    _lock: CredentialEntryLock,
}

impl Guard {
    async fn read(&self) -> Result<Option<SessionRecord>, HookError> {
        let entry = self.store.storage.entry(RECORDS, &self.identity);
        let bytes = blocking(move || entry.load()).await?;
        // An unreadable record is a fault, never an absent one: treating it
        // as absent would let a write replace a grant nobody could read.
        bytes
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| unavailable()))
            .transpose()
    }
}

impl CredentialGuard for Guard {
    fn load(&mut self) -> LocalFuture<'_, Result<Option<SessionRecord>, HookError>> {
        Box::pin(async move { self.read().await })
    }

    fn compare_and_swap(
        &mut self,
        expected: Option<String>,
        replacement: SessionRecord,
    ) -> LocalFuture<'_, Result<bool, HookError>> {
        Box::pin(async move {
            let current = self.read().await?;
            if current.as_ref().map(|record| record.version().to_owned()) != expected {
                return Ok(false);
            }
            let bytes = serde_json::to_vec(&replacement).map_err(|_| unavailable())?;
            let entry = self.store.storage.entry(RECORDS, &self.identity);
            blocking(move || entry.save(&bytes)).await?;
            Ok(true)
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn record(version: &str) -> SessionRecord {
        serde_json::from_value(serde_json::json!({
            "version": version,
            "metadata": {"resource": "https://hooks.example/ahp", "issuer": "https://auth.example",
                "authorization_endpoint": null, "authorization_response_iss_parameter_supported": false,
                "token_endpoint": "https://auth.example/token", "grant_types_supported": [],
                "token_endpoint_auth_methods_supported": [], "code_challenge_methods_supported": []},
            "client_id": "kit", "flow": "authorization_code_pkce", "configured_binding": true,
            "resource_metadata_hint": null, "token": null, "pending": null
        }))
        .unwrap()
    }

    fn budget() -> OperationBudget {
        OperationBudget::until(Instant::now() + Duration::from_secs(5))
    }

    /// A storage key as the SDK persists one.
    fn key(backend: &str) -> SessionKey {
        serde_json::from_value(serde_json::json!({
            "storage_key": format!("ahp.oauth.binding:test:{backend}"),
            "namespace": "kit", "backend_id": backend,
            "destination": "https://hooks.example/ahp", "purpose": "event"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn compare_and_swap_honours_the_expected_version_across_instances() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().to_path_buf());
        let first = Store::new(storage.clone(), "test#cas");
        let second = Store::new(storage, "test#cas");
        {
            let mut guard = first.acquire(key("cas"), budget()).await.unwrap();
            assert!(guard.load().await.unwrap().is_none());
            // Expecting a record that does not exist fails.
            assert!(
                !guard
                    .compare_and_swap(Some("v0".into()), record("v1"))
                    .await
                    .unwrap()
            );
            assert!(guard.compare_and_swap(None, record("v1")).await.unwrap());
            // Expecting absence once a record exists fails.
            assert!(!guard.compare_and_swap(None, record("v2")).await.unwrap());
        }
        // Another instance (another process) sees the write, and a stale
        // expectation cannot overwrite it.
        let mut guard = second.acquire(key("cas"), budget()).await.unwrap();
        assert_eq!(guard.load().await.unwrap().unwrap().version(), "v1");
        assert!(
            !guard
                .compare_and_swap(Some("v0".into()), record("v2"))
                .await
                .unwrap()
        );
        assert!(
            guard
                .compare_and_swap(Some("v1".into()), record("v2"))
                .await
                .unwrap()
        );
        assert_eq!(guard.load().await.unwrap().unwrap().version(), "v2");
        drop(guard);
        // Logout through either instance removes it.
        assert_eq!(first.forget(budget()).await.unwrap(), 1);
        let mut guard = second.acquire(key("cas"), budget()).await.unwrap();
        assert!(guard.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn logout_unindexes_a_record_before_releasing_it() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().to_path_buf());
        let store = Store::new(storage.clone(), "test#forget-order");
        store.remember(RECORDS, "grant", budget()).await.unwrap();
        storage.entry(RECORDS, "grant").save(b"{}").unwrap();
        // Logout waits for the record; then the index is held too.
        let record = store.lock(RECORDS, "grant", budget()).await.unwrap();
        let forgetting = tokio::spawn({
            let store = store.clone();
            async move { store.forget(budget()).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let index = store
            .lock(INDEX, "test#forget-order", budget())
            .await
            .unwrap();
        drop(record);
        // Logout deletes the record, then waits for the index while still
        // holding the record: no writer can recreate it unindexed meanwhile.
        tokio::time::timeout(Duration::from_secs(5), async {
            while storage.entry(RECORDS, "grant").load().unwrap().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the record is deleted");
        let short = OperationBudget::until(Instant::now() + Duration::from_millis(200));
        assert!(
            store.lock(RECORDS, "grant", short).await.is_err(),
            "the record was released before it left the index"
        );
        drop(index);
        assert_eq!(forgetting.await.unwrap().unwrap(), 1);
        assert!(store.index().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_record_is_owned_by_one_guard_at_a_time() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(
            CredentialStorage::Filesystem(directory.path().to_path_buf()),
            "test#owned",
        );
        let held = store.acquire(key("owned"), budget()).await.unwrap();
        let short = OperationBudget::until(Instant::now() + Duration::from_millis(200));
        assert!(store.acquire(key("owned"), short).await.is_err());
        // Another record is independent, so the SDK may nest them.
        let other = store.acquire(key("other"), budget()).await.unwrap();
        drop((held, other));
        assert!(store.acquire(key("owned"), budget()).await.is_ok());
    }
}
