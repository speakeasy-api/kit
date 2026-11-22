//! SDK credential records in Kit's credential storage, one lock per record.
use std::{collections::BTreeSet, sync::Arc};

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

/// A logout change that must reach disk before Kit goes on: file-backed
/// storage keeps a mutation only in this process under storage pressure, so
/// it is required on disk (as OpenAI logout does), and a failure is the
/// operation's. The keychain is durable as written; memory storage is never
/// shared with another process, so it has nothing to wait for.
async fn durably(
    entry: crate::credentials::CredentialEntry,
    change: impl FnOnce(
        &crate::credentials::CredentialEntry,
    ) -> Result<(), crate::credentials::CredentialStoreError>
    + Send
    + 'static,
) -> Result<(), HookError> {
    blocking(move || {
        change(&entry)?;
        if entry.filesystem_path().is_some() {
            entry.require_disk()?;
        }
        Ok(())
    })
    .await
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
            durably(entry, move |entry| entry.save(&bytes)).await?;
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
            // Each deletion reaches disk, under the record's lock, before
            // its index entry goes: logout never reports a removal that
            // another process can still read.
            let _record = self.lock(namespace, identity, budget).await?;
            let entry = self.storage.entry(namespace, identity);
            let existed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let found = Arc::clone(&existed);
            durably(entry, move |entry| {
                found.store(entry.delete()?, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            })
            .await?;
            if existed.load(std::sync::atomic::Ordering::Relaxed) {
                deleted += 1;
            }
            let _index = self.lock(INDEX, &self.backend, budget).await?;
            let mut current = self.index().await?;
            current.remove(&(namespace.clone(), identity.clone()));
            if current.is_empty() {
                let entry = self.storage.entry(INDEX, &self.backend);
                durably(entry, |entry| entry.delete().map(|_| ())).await?;
                self.list_scope(false, budget).await?;
            } else {
                let entry = self.storage.entry(INDEX, &self.backend);
                let bytes = serde_json::to_vec(&current).map_err(|_| unavailable())?;
                durably(entry, move |entry| entry.save(&bytes)).await?;
            }
        }
        Ok(deleted)
    }

    /// The token generation of every grant this backend holds, by record.
    pub(super) async fn token_generations(
        &self,
        budget: OperationBudget,
    ) -> Result<std::collections::HashMap<String, String>, HookError> {
        let index = {
            let _lock = self.lock(INDEX, &self.backend, budget).await?;
            self.index().await?
        };
        let mut generations = std::collections::HashMap::new();
        for (namespace, identity) in index {
            if namespace != RECORDS {
                continue;
            }
            if let Some(generation) = self.generation_of(&identity).await? {
                generations.insert(identity, generation);
            }
        }
        Ok(generations)
    }

    async fn generation_of(&self, identity: &str) -> Result<Option<String>, HookError> {
        let entry = self.storage.entry(RECORDS, identity);
        let bytes = blocking(move || entry.load()).await?;
        Ok(bytes
            .and_then(|bytes| serde_json::from_slice::<SessionRecord>(&bytes).ok())
            .and_then(|record| record.attempt_id().map(str::to_owned)))
    }

    /// Removes the grant one code exchange wrote, and nothing else: the
    /// record whose token generation is `ours`, or, when the exchange
    /// returned none (an under-granted step-up still stores its grant), the
    /// records whose generation is new since `before`. Each is checked
    /// again under its lock before it goes, so a newer grant written
    /// meanwhile stays. Returns how many it removed.
    pub(super) async fn discard_exchange(
        &self,
        before: &std::collections::HashMap<String, String>,
        ours: Option<&str>,
        budget: OperationBudget,
    ) -> Result<usize, HookError> {
        let now = self.token_generations(budget).await?;
        let mut removed = 0;
        for (identity, generation) in now {
            let written = match ours {
                Some(ours) => generation == ours,
                None => before.get(&identity) != Some(&generation),
            };
            if !written {
                continue;
            }
            let _record = self.lock(RECORDS, &identity, budget).await?;
            if self.generation_of(&identity).await?.as_deref() != Some(generation.as_str()) {
                continue;
            }
            let entry = self.storage.entry(RECORDS, &identity);
            durably(entry, |entry| entry.delete().map(|_| ())).await?;
            removed += 1;
        }
        Ok(removed)
    }

    /// Every record and secret indexed for this backend.
    #[cfg(test)]
    pub(super) async fn indexed(&self) -> BTreeSet<(String, String)> {
        self.index().await.unwrap_or_default()
    }

    pub(super) fn storage(&self) -> &CredentialStorage {
        &self.storage
    }
}

/// Logout markers, shared by every Kit process using the storage: one per
/// scope, and one per backend id for `--all-workspaces`.
const LOGOUTS: &str = "ahp-oauth-logout";

/// The marker `logout --all-workspaces` moves for every scope of `backend`.
fn everywhere(backend: &str) -> String {
    format!("*#{backend}")
}

/// Where `scope` is in its logout history: it changes with every logout of
/// the scope, in any Kit process, and every `--all-workspaces` logout of its
/// backend id. A login prepared at one generation must not complete at
/// another.
pub(super) async fn logout_generation(
    storage: &CredentialStorage,
    scope: &str,
) -> Result<String, HookError> {
    let backend = scope.rsplit_once('#').map_or(scope, |(_, id)| id);
    let mut generation = String::new();
    for identity in [scope.to_owned(), everywhere(backend)] {
        let entry = storage.entry(LOGOUTS, &identity);
        let bytes = blocking(move || entry.load()).await?;
        generation.push_str(&bytes.map_or_else(String::new, |bytes| {
            String::from_utf8_lossy(&bytes).into_owned()
        }));
        generation.push('|');
    }
    Ok(generation)
}

/// Where `scope`'s own logout marker is kept, for file-backed storage.
#[cfg(test)]
pub(super) fn logout_marker_path(
    storage: &CredentialStorage,
    scope: &str,
) -> Option<std::path::PathBuf> {
    storage
        .entry(LOGOUTS, scope)
        .filesystem_path()
        .map(std::path::Path::to_path_buf)
}

/// Moves `scope`'s logout marker, or with `all_workspaces` the marker of
/// every scope of the backend id `scope`. Written before any record is
/// removed, so a login that completes meanwhile sees it afterwards.
pub(super) async fn mark_logout(
    storage: &CredentialStorage,
    scope: &str,
    all_workspaces: bool,
) -> Result<(), HookError> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let identity = if all_workspaces {
        everywhere(scope)
    } else {
        scope.to_owned()
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let marker = format!(
        "{nanos}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    // On disk before any record is removed, or the logout fails.
    let entry = storage.entry(LOGOUTS, &identity);
    durably(entry, move |entry| entry.save(marker.as_bytes())).await
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

    /// The credential directory the child process fills up.
    #[cfg(unix)]
    const FULL_DIRECTORY: &str = "KIT_AHP_STORE_FULL_DIRECTORY";

    /// Disk full for files under one directory once armed: replacing or
    /// removing them fails ENOSPC, which the resilient filesystem accepts
    /// and keeps only in this process.
    #[cfg(unix)]
    struct Full {
        directory: std::path::PathBuf,
        armed: Arc<std::sync::atomic::AtomicBool>,
    }

    #[cfg(unix)]
    impl Full {
        fn check(&self, path: &std::path::Path) -> std::io::Result<()> {
            if self.armed.load(std::sync::atomic::Ordering::SeqCst)
                && path.starts_with(&self.directory)
            {
                return Err(std::io::Error::from_raw_os_error(libc::ENOSPC));
            }
            Ok(())
        }
    }

    #[cfg(unix)]
    impl crate::resilient_fs::Backend for Full {
        fn open(
            &self,
            path: &std::path::Path,
            options: &crate::resilient_fs::DiskOpenOptions,
        ) -> std::io::Result<Box<dyn crate::resilient_fs::BackendFile>> {
            crate::resilient_fs::DiskBackend.open(path, options)
        }
        fn metadata(
            &self,
            path: &std::path::Path,
            follow: bool,
        ) -> std::io::Result<std::fs::Metadata> {
            crate::resilient_fs::DiskBackend.metadata(path, follow)
        }
        fn identity(
            &self,
            path: &std::path::Path,
            follow: bool,
        ) -> std::io::Result<Option<crate::resilient_fs::FileIdentity>> {
            crate::resilient_fs::DiskBackend.identity(path, follow)
        }
        fn read_dir(
            &self,
            path: &std::path::Path,
        ) -> std::io::Result<Vec<crate::resilient_fs::DiskEntry>> {
            crate::resilient_fs::DiskBackend.read_dir(path)
        }
        fn read_link(&self, path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
            crate::resilient_fs::DiskBackend.read_link(path)
        }
        fn canonicalize(&self, path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
            crate::resilient_fs::DiskBackend.canonicalize(path)
        }
        fn create_dir(&self, path: &std::path::Path, private: bool) -> std::io::Result<()> {
            crate::resilient_fs::DiskBackend.create_dir(path, private)
        }
        fn remove_file(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.check(path)?;
            crate::resilient_fs::DiskBackend.remove_file(path)
        }
        fn remove_dir(&self, path: &std::path::Path) -> std::io::Result<()> {
            crate::resilient_fs::DiskBackend.remove_dir(path)
        }
        fn rename(&self, from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
            self.check(to)?;
            crate::resilient_fs::DiskBackend.rename(from, to)
        }
        fn set_permissions(
            &self,
            path: &std::path::Path,
            permissions: std::fs::Permissions,
        ) -> std::io::Result<()> {
            crate::resilient_fs::DiskBackend.set_permissions(path, permissions)
        }
        fn sync_directory(&self, path: &std::path::Path) -> std::io::Result<()> {
            crate::resilient_fs::DiskBackend.sync_directory(path)
        }
        fn acquire_lease(
            &self,
            request: &crate::resilient_fs::LeaseRequest,
        ) -> std::io::Result<Box<dyn crate::resilient_fs::BackendLease>> {
            crate::resilient_fs::DiskBackend.acquire_lease(request)
        }
        fn open_beneath(
            &self,
            root: &std::path::Path,
            relative: &std::path::Path,
        ) -> std::io::Result<Box<dyn crate::resilient_fs::BackendFile>> {
            crate::resilient_fs::DiskBackend.open_beneath(root, relative)
        }
    }

    /// Runs in its own process: the filesystem is global and set only once.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "run by logout_fails_when_its_changes_cannot_reach_disk in its own process"]
    async fn logout_durability_child() {
        let directory = std::path::PathBuf::from(std::env::var_os(FULL_DIRECTORY).unwrap());
        let directory = std::fs::canonicalize(directory).unwrap();
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(
            crate::resilient_fs::initialize_global(crate::resilient_fs::Fs::new(Arc::new(Full {
                directory: directory.clone(),
                armed: Arc::clone(&armed),
            })))
            .is_ok()
        );
        let storage = CredentialStorage::Filesystem(directory.clone());
        let store = Store::new(storage.clone(), "test#durable");
        store.remember(SECRETS, "secret", budget()).await.unwrap();
        let entry = storage.entry(SECRETS, "secret");
        let path = entry.filesystem_path().unwrap().to_path_buf();
        blocking(move || entry.save(b"value")).await.unwrap();
        assert!(path.is_file());
        // The disk fills up.
        armed.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(mark_logout(&storage, "test#durable", false).await.is_err());
        assert!(store.forget(budget()).await.is_err());
        // What logout could not remove is still on disk, and was not
        // reported removed.
        assert!(path.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn logout_fails_when_its_changes_cannot_reach_disk() {
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ahp::auth::store::tests::logout_durability_child",
                "--ignored",
                "--nocapture",
            ])
            .env(FULL_DIRECTORY, directory.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
    }

    /// A grant record whose token has `generation`.
    fn grant(version: &str, generation: &str) -> SessionRecord {
        let mut record = serde_json::to_value(record(version)).unwrap();
        record["token"] = serde_json::json!({
            "access_token": "t", "refresh_token": null, "expires_at": null,
            "scopes": [], "generation": generation,
            "access_rejected": false, "refresh_in_flight": false
        });
        serde_json::from_value(record).unwrap()
    }

    #[tokio::test]
    async fn discarding_an_exchange_removes_only_its_own_grant() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().to_path_buf());
        let store = Store::new(storage, "test#discard");
        for (name, generation) in [("old", "g-old"), ("ours", "g-ours"), ("newer", "g-newer")] {
            let mut guard = store.acquire(key(name), budget()).await.unwrap();
            assert!(
                guard
                    .compare_and_swap(None, grant("v1", generation))
                    .await
                    .unwrap()
            );
        }
        let generations = store.token_generations(budget()).await.unwrap();
        assert_eq!(generations.len(), 3);
        // Our exchange's generation is known: only that record goes.
        assert_eq!(
            store
                .discard_exchange(&generations, Some("g-ours"), budget())
                .await
                .unwrap(),
            1
        );
        let left: std::collections::BTreeSet<_> = store
            .token_generations(budget())
            .await
            .unwrap()
            .into_values()
            .collect();
        assert_eq!(left, ["g-newer".to_owned(), "g-old".to_owned()].into());
        // Unknown: only what is new since before the exchange goes.
        let before = store.token_generations(budget()).await.unwrap();
        let mut guard = store.acquire(key("stepped-up"), budget()).await.unwrap();
        assert!(
            guard
                .compare_and_swap(None, grant("v1", "g-step"))
                .await
                .unwrap()
        );
        drop(guard);
        assert_eq!(
            store
                .discard_exchange(&before, None, budget())
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.token_generations(budget()).await.unwrap().len(), 2);
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
