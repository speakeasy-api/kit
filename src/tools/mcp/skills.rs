//! Connection-scoped inventory of untrusted Skills extension metadata.
use super::{Inner, McpRuntime, McpServerId};
use agentkit_mcp::{McpConnection, McpSkill, McpSkillResources};
use futures_util::{
    FutureExt, StreamExt,
    future::{self, BoxFuture},
    stream::FuturesUnordered,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock, Weak, atomic::Ordering},
    time::Duration,
};
use tokio::{task::AbortHandle, time::Instant};

const CONCURRENCY: usize = 8;
const LIST_TIMEOUT: Duration = Duration::from_secs(2);
const RETRY_INITIAL: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(30);
pub(super) const INVENTORY_MAX_ENTRIES: usize = 256;
pub(super) const INVENTORY_MAX_BYTES: usize = 256 * 1024;
const DESCRIPTION_MAX_BYTES: usize = 1024;
// Leave room for the envelope and pagination cursor in an 8 KiB selector page.
const SELECTOR_ROW_MAX_BYTES: usize = 8 * 1024 - 128;

type Catalog = Arc<[Arc<SkillMetadata>]>;

struct Completion {
    name: String,
    connection: Arc<McpConnection>,
    epoch: u64,
    next_state: EntryState,
}

#[derive(Clone)]
pub(crate) struct RemoteSkill {
    pub(crate) server: String,
    pub(crate) skill: Arc<SkillMetadata>,
}

impl RemoteSkill {
    pub(crate) fn selector(&self) -> String {
        selector(&self.server, &self.skill.uri)
    }
}

fn selector(server: &str, uri: &str) -> String {
    let origin = serde_json::Value::Array(vec![server.into(), uri.into()]);
    format!("mcp:{origin}")
}

pub(crate) struct SkillMetadata {
    pub(crate) uri: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) fingerprint: blake3::Hash,
}

fn compact_catalog(server: &str, manifests: Vec<McpSkill>) -> Catalog {
    let mut retained = Vec::new();
    let mut bytes = 0;
    for mut manifest in manifests {
        let name = &manifest.frontmatter.name;
        let uri = &manifest.uri;
        // Check decoded sizes before encoding an exact selector. The selector
        // itself is JSON embedded inside a JSON string, so escaping counts twice.
        if server
            .len()
            .saturating_add(uri.len())
            .saturating_add(name.len())
            > SELECTOR_ROW_MAX_BYTES
        {
            continue;
        }
        let selector = selector(server, uri);
        let row = serde_json::Value::Object(
            [
                ("selector".into(), serde_json::Value::from(selector)),
                ("name".into(), serde_json::Value::from(name.as_str())),
            ]
            .into_iter()
            .collect(),
        );
        if row.to_string().len() > SELECTOR_ROW_MAX_BYTES {
            continue;
        }
        let description = &manifest.frontmatter.description;
        let description = &description[..description.floor_char_boundary(DESCRIPTION_MAX_BYTES)];
        // Charge for owned fields and the shared record, not discarded files or extra frontmatter.
        let charge = server.len()
            + uri.len()
            + name.len()
            + description.len()
            + size_of::<SkillMetadata>()
            + size_of::<Arc<SkillMetadata>>()
            + 2 * size_of::<usize>();
        if bytes + charge > INVENTORY_MAX_BYTES {
            continue;
        }
        let uri = uri.to_owned();
        let name = name.to_owned();
        let description = description.to_owned();

        manifest.meta = None;
        if let McpSkillResources::Files(files) = &mut manifest.resources {
            files.sort_unstable_by(|a, b| {
                (&a.uri, &a.digest, a.size).cmp(&(&b.uri, &b.digest, b.size))
            });
        }
        let mut hash = blake3::Hasher::new();
        if serde_json::to_writer(&mut hash, &manifest).is_err() {
            continue;
        }
        retained.push(Arc::new(SkillMetadata {
            uri,
            name,
            description,
            fingerprint: hash.finalize(),
        }));
        bytes += charge;
        if retained.len() == INVENTORY_MAX_ENTRIES {
            break;
        }
    }
    retained.into()
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
    state: EntryState,
}

#[derive(Clone)]
enum EntryState {
    Ready(Catalog),
    Retry { at: Instant, next_delay: Duration },
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
            if let EntryState::Ready(skills) = entry.state
                && entry
                    .connection
                    .upgrade()
                    .is_some_and(|connection| connection.connection_epoch() == entry.epoch)
            {
                result.extend(skills.iter().cloned().map(|skill| RemoteSkill {
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
        let mut retry_at: Option<Instant> = None;
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
            let retry_delay = {
                let Ok(entries) = inner.skills.entries.lock() else {
                    return;
                };
                let previous = entries.get(&name).filter(|entry| {
                    entry.epoch == epoch && entry.connection.ptr_eq(&Arc::downgrade(&connection))
                });
                match previous.map(|entry| &entry.state) {
                    Some(EntryState::Ready(_)) => continue,
                    Some(EntryState::Retry { at, next_delay }) if *at <= Instant::now() => {
                        *next_delay
                    }
                    Some(EntryState::Retry { at, .. }) => {
                        retry_at = Some(retry_at.map_or(*at, |earliest| earliest.min(*at)));
                        continue;
                    }
                    None => RETRY_INITIAL,
                }
            };
            if !connection
                .wire_capabilities()
                .is_some_and(|caps| caps.resources.is_some() && caps.supports_skills())
            {
                continue;
            }
            active.insert(name.clone());
            pending.push(
                async move {
                    // The public SDK API captures one peer and aggregates all pages.
                    // Bound the wait and discard full manifests before publication;
                    // this cannot bound the SDK's transient response allocations.
                    let next_state =
                        match tokio::time::timeout(LIST_TIMEOUT, connection.list_skills()).await {
                            Ok(Ok(manifests)) => {
                                EntryState::Ready(compact_catalog(&name, manifests))
                            }
                            _ => EntryState::Retry {
                                at: Instant::now() + retry_delay,
                                next_delay: (retry_delay * 2).min(RETRY_MAX),
                            },
                        };
                    Completion {
                        name,
                        connection,
                        epoch,
                        next_state,
                    }
                }
                .boxed(),
            );
        }
        drop(manager);
        drop(inner);
        // A retry timer cannot make progress while all request slots are occupied.
        let retry_at = retry_at.filter(|_| pending.len() < CONCURRENCY);
        let completion = async {
            if pending.is_empty() {
                future::pending().await
            } else {
                pending.next().await
            }
        };
        let retry = async {
            if let Some(at) = retry_at {
                tokio::time::sleep_until(at).await;
            } else {
                future::pending().await
            }
        };
        let future::Either::Left((
            Some(Completion {
                name,
                connection,
                epoch,
                next_state,
            }),
            _,
        )) = future::select(
            completion.boxed(),
            future::select(retry.boxed(), wake.changed().boxed()),
        )
        .await
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
            state: next_state,
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
