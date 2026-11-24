//! Connection-scoped inventory of untrusted Skills extension metadata.
use super::{Inner, McpRuntime, McpServerId};
use agentkit_mcp::{McpConnection, McpSkill};
use futures_util::{
    FutureExt, StreamExt,
    future::{self, BoxFuture, Either},
    stream::FuturesUnordered,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock, Weak, atomic::Ordering},
    time::Duration,
};
use tokio::task::AbortHandle;

const CONCURRENCY: usize = 8;
const LIST_TIMEOUT: Duration = Duration::from_secs(2);

type Completion = (String, Arc<McpConnection>, u64, Vec<McpSkill>);

#[derive(Clone)]
pub(crate) struct RemoteSkill {
    pub(crate) server: String,
    pub(crate) skill: McpSkill,
}

#[derive(Default)]
pub(super) struct Inventory {
    worker: OnceLock<AbortHandle>,
    // Only the worker and config retirement write entries, both with the
    // manager locked first. Readers need only this lock and the live SDK
    // epoch. No inventory lock spans an await; poison hides the inventory.
    entries: Mutex<BTreeMap<String, Entry>>,
}

#[derive(Clone)]
struct Entry {
    connection: Weak<McpConnection>,
    epoch: u64,
    skills: Arc<[McpSkill]>,
}

impl Inventory {
    // Like publication, called with the manager locked. Invalidate before
    // disconnect can suspend or fail, even if a listing still holds the peer.
    pub(super) fn remove(&self, name: &str) {
        let retired = self
            .entries
            .lock()
            .ok()
            .and_then(|mut entries| entries.remove(name));
        drop(retired);
    }
}

impl Drop for Inventory {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.get() {
            worker.abort();
        }
    }
}

impl McpRuntime {
    pub(crate) fn skill_inventory(&self) -> Vec<RemoteSkill> {
        if self.ensure_healthy().is_err() {
            return Vec::new();
        }
        let Ok(entries) = self.inner.skills.entries.lock() else {
            return Vec::new();
        };
        let snapshot = entries.clone();
        drop(entries);
        let mut result = Vec::new();
        for (server, entry) in snapshot {
            if entry
                .connection
                .upgrade()
                .is_some_and(|connection| connection.connection_epoch() == entry.epoch)
            {
                result.extend(entry.skills.iter().cloned().map(|skill| RemoteSkill {
                    server: server.clone(),
                    skill,
                }));
            }
        }
        if self.ensure_healthy().is_err() {
            result.clear();
        }
        result
    }
}

pub(super) fn start(inner: &Arc<Inner>) {
    inner.skills.worker.get_or_init(|| {
        let weak = Arc::downgrade(inner);
        tokio::spawn(run(weak)).abort_handle()
    });
}

async fn run(weak: Weak<Inner>) {
    let mut pending = FuturesUnordered::<BoxFuture<'static, Completion>>::new();
    let mut active = BTreeSet::new();
    loop {
        let Some(inner) = weak.upgrade() else { return };
        let manager = inner.manager.lock().await;
        if !inner.healthy.load(Ordering::Acquire) {
            return;
        }
        let handles = manager.connected_servers();
        let mut wake = manager.catalog_refresh_wake();
        // Consume initially dirty watches before capturing identities. A later
        // connection or epoch change still wakes us, even if no tools changed.
        for _ in 0..=handles.len() {
            if wake.changed().now_or_never().is_none() {
                break;
            }
        }
        {
            let Ok(entries) = inner.skills.entries.lock() else {
                return;
            };
            for handle in handles {
                if pending.len() == CONCURRENCY {
                    break;
                }
                let name = handle.server_id().to_string();
                if active.contains(&name) {
                    continue;
                }
                let connection = handle.connection();
                let epoch = connection.connection_epoch();
                if entries.get(&name).is_some_and(|entry| {
                    entry.epoch == epoch && entry.connection.ptr_eq(&Arc::downgrade(&connection))
                }) || !connection
                    .wire_capabilities()
                    .is_some_and(|caps| caps.resources.is_some() && caps.supports_skills())
                {
                    continue;
                }
                active.insert(name.clone());
                pending.push(
                    async move {
                        // The SDK owns protocol paging and captures one peer. A
                        // failed or stalled list is advisory, never a connection failure.
                        let skills = tokio::time::timeout(LIST_TIMEOUT, connection.list_skills())
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .unwrap_or_default();
                        (name, connection, epoch, skills)
                    }
                    .boxed(),
                );
            }
        }
        drop(manager);
        drop(inner);
        let completion = async {
            if pending.is_empty() {
                future::pending().await
            } else {
                pending.next().await
            }
        };
        let Either::Left((Some((name, connection, epoch, skills)), _)) =
            future::select(completion.boxed(), wake.changed().boxed()).await
        else {
            continue;
        };
        active.remove(&name);
        let Some(inner) = weak.upgrade() else { return };
        let manager = inner.manager.lock().await;
        if !inner.healthy.load(Ordering::Acquire) {
            return;
        }
        if !manager
            .connected_server(&McpServerId::new(&name))
            .is_some_and(|handle| {
                Arc::ptr_eq(&handle.connection(), &connection)
                    && connection.connection_epoch() == epoch
            })
        {
            continue;
        }
        let entry = Entry {
            connection: Arc::downgrade(&connection),
            epoch,
            skills: skills.into(),
        };
        let Ok(mut entries) = inner.skills.entries.lock() else {
            return;
        };
        let retired = entries.insert(name, entry);
        drop(entries);
        drop(manager);
        drop(retired);
    }
}
