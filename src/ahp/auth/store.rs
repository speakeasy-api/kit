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

/// SDK binding, grant, pending-login and registration records.
pub(super) const RECORDS: &str = "ahp-oauth";
/// Client secrets issued by dynamic registration.
pub(super) const SECRETS: &str = "ahp-oauth-secret";
/// Per backend, every record and secret Kit wrote for it, so that logging out
/// of one backend removes exactly its own state.
const INDEX: &str = "ahp-oauth-index";

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
        if index.insert((namespace.to_owned(), identity.to_owned())) {
            self.save_index(&index).await?;
        }
        Ok(())
    }

    /// Deletes every record and secret this backend wrote. Records are
    /// deleted under their own locks, so a concurrent writer finishes first.
    pub(super) async fn forget(&self, budget: OperationBudget) -> Result<usize, HookError> {
        let index = {
            let _lock = self.lock(INDEX, &self.backend, budget).await?;
            self.index().await?
        };
        for (namespace, identity) in &index {
            let _lock = self.lock(namespace, identity, budget).await?;
            let entry = self.storage.entry(namespace, identity);
            blocking(move || entry.delete()).await?;
        }
        let _lock = self.lock(INDEX, &self.backend, budget).await?;
        let mut current = self.index().await?;
        current.retain(|item| !index.contains(item));
        if current.is_empty() {
            let entry = self.storage.entry(INDEX, &self.backend);
            blocking(move || entry.delete()).await?;
        } else {
            self.save_index(&current).await?;
        }
        Ok(index.len())
    }

    pub(super) fn storage(&self) -> &CredentialStorage {
        &self.storage
    }
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
