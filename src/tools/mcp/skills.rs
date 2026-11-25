//! Connection-scoped inventory of untrusted Skills extension metadata.
use super::{Inner, McpRuntime, McpServerId};
use agentkit_mcp::{
    McpConnection, McpSkill, McpSkillResources, McpSkillScanLimit, McpSkillScanLimits,
    McpSkillScanOutcome,
};
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
const PAGE_TIMEOUT: Duration = Duration::from_secs(2);
const LIST_TIMEOUT_MAX: Duration = Duration::from_secs(20);
const RETRY_INITIAL: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(30);
pub(super) const INVENTORY_MAX_ENTRIES: usize = 256;
pub(super) const INVENTORY_MAX_BYTES: usize = 256 * 1024;
const DESCRIPTION_MAX_BYTES: usize = 1024;

#[derive(Clone)]
struct Catalog {
    skills: Arc<[Arc<SkillMetadata>]>,
    truncated: bool,
}

#[derive(Default)]
pub(crate) struct SkillInventorySnapshot {
    pub(crate) skills: Vec<RemoteSkill>,
    pub(crate) truncated: bool,
}

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

fn compact_skill(
    server: &str,
    mut manifest: McpSkill,
    remaining_bytes: usize,
    max_row_bytes: usize,
) -> Option<(SkillMetadata, usize)> {
    let name = &manifest.frontmatter.name;
    let uri = &manifest.uri;
    // Check decoded sizes before encoding an exact selector. The selector
    // itself is JSON embedded inside a JSON string, so escaping counts twice.
    if server
        .len()
        .saturating_add(uri.len())
        .saturating_add(name.len())
        > max_row_bytes
    {
        return None;
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
    if crate::compose_output::model_json_string_bytes(row.to_string()) > max_row_bytes {
        return None;
    }
    let description = &manifest.frontmatter.description;
    let description = &description[..description.floor_char_boundary(DESCRIPTION_MAX_BYTES)];
    // Charge for owned fields and the shared record, not discarded files or extra frontmatter.
    let charge = minimum_skill_charge(server) + uri.len() + name.len() + description.len();
    if charge > remaining_bytes {
        return None;
    }
    let uri = uri.to_owned();
    let name = name.to_owned();
    let description = description.to_owned();

    manifest.meta = None;
    if let McpSkillResources::Files(files) = &mut manifest.resources {
        files
            .sort_unstable_by(|a, b| (&a.uri, &a.digest, a.size).cmp(&(&b.uri, &b.digest, b.size)));
    }
    let mut hash = blake3::Hasher::new();
    if serde_json::to_writer(&mut hash, &manifest).is_err() {
        return None;
    }
    Some((
        SkillMetadata {
            uri,
            name,
            description,
            fingerprint: hash.finalize(),
        },
        charge,
    ))
}

fn scan_limits(retry_delay: Duration) -> McpSkillScanLimits {
    McpSkillScanLimits {
        max_entries: INVENTORY_MAX_ENTRIES,
        page_timeout: (retry_delay * 8).clamp(PAGE_TIMEOUT, LIST_TIMEOUT_MAX),
        ..McpSkillScanLimits::default()
    }
}

fn minimum_skill_charge(server: &str) -> usize {
    server.len()
        + size_of::<SkillMetadata>()
        + size_of::<Arc<SkillMetadata>>()
        + 2 * size_of::<usize>()
}

async fn scan_catalog(
    connection: &McpConnection,
    server: &str,
    retry_delay: Duration,
) -> Option<Catalog> {
    let limits = scan_limits(retry_delay);
    let deadline = Instant::now() + (limits.page_timeout * 2).min(LIST_TIMEOUT_MAX);
    let (prefix, full, outcome) = scan_prefix(connection, server, limits, deadline).await?;
    if full
        || outcome
            != Some(McpSkillScanOutcome::Truncated(
                McpSkillScanLimit::EntryCount,
            ))
    {
        return Some(prefix);
    }
    // The SDK visitor cannot stop on a local acceptance condition. Keep the
    // efficient first pass for compact catalogs; if some rows were rejected,
    // allow a bounded second pass under the same deadline. A failed second
    // pass cannot erase the prefix from the successful first pass.
    let expanded = McpSkillScanLimits {
        page_timeout: (retry_delay * 8).clamp(PAGE_TIMEOUT, LIST_TIMEOUT_MAX),
        ..McpSkillScanLimits::default()
    };
    Some(
        scan_prefix(connection, server, expanded, deadline)
            .await
            .map_or(prefix, |(catalog, _, _)| catalog),
    )
}

async fn scan_prefix(
    connection: &McpConnection,
    server: &str,
    limits: McpSkillScanLimits,
    deadline: Instant,
) -> Option<(Catalog, bool, Option<McpSkillScanOutcome>)> {
    let max_row_bytes = crate::runtime::mcp_skills::max_selector_row_bytes();
    let minimum_charge = minimum_skill_charge(server);
    let mut retained = Vec::new();
    let mut remaining_bytes = INVENTORY_MAX_BYTES;
    let mut truncated = false;
    let outcome = tokio::time::timeout_at(
        deadline,
        connection.visit_skills(limits, |manifest| {
            if retained.len() < INVENTORY_MAX_ENTRIES
                && let Some((skill, charge)) =
                    compact_skill(server, manifest, remaining_bytes, max_row_bytes)
            {
                retained.push(Arc::new(skill));
                remaining_bytes -= charge;
            } else {
                truncated = true;
            }
        }),
    )
    .await
    .ok()
    .and_then(Result::ok);
    let full = retained.len() == INVENTORY_MAX_ENTRIES
        || (remaining_bytes <= minimum_charge && !retained.is_empty());
    if outcome.is_none() && !full {
        return None;
    }
    Some((
        Catalog {
            skills: retained.into(),
            truncated: truncated || outcome != Some(McpSkillScanOutcome::Complete),
        },
        full,
        outcome,
    ))
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
        self.skill_inventory_snapshot().skills
    }

    pub(crate) fn skill_inventory_snapshot(&self) -> SkillInventorySnapshot {
        if self.ensure_healthy().is_err() {
            return SkillInventorySnapshot::default();
        }
        let Ok(entries) = self.inner.skills.entries.lock() else {
            return SkillInventorySnapshot::default();
        };
        let snapshot = entries.clone();
        drop(entries);
        let mut result = SkillInventorySnapshot::default();
        for (server, entry) in snapshot {
            if let EntryState::Ready(catalog) = entry.state
                && entry.connection.upgrade().is_some_and(|connection| {
                    connection.connection_epoch() == entry.epoch
                        && !connection.is_transport_closed()
                })
            {
                result.truncated |= catalog.truncated;
                result
                    .skills
                    .extend(catalog.skills.iter().cloned().map(|skill| RemoteSkill {
                        server: server.clone(),
                        skill,
                    }));
            }
        }
        if self.ensure_healthy().is_err() {
            return SkillInventorySnapshot::default();
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
            if connection.is_transport_closed() {
                inner.skills.remove(&name);
                continue;
            }
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
                    let next_state = match scan_catalog(&connection, &name, retry_delay).await {
                        Some(catalog) => EntryState::Ready(catalog),
                        None => EntryState::Retry {
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
                    && !connection.is_transport_closed()
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

#[cfg(test)]
#[allow(clippy::disallowed_macros)]
mod tests {
    #[test]
    fn mcp_skill_inventory_retries_allow_an_individual_page_up_to_the_scan_ceiling() {
        assert_eq!(
            super::scan_limits(super::RETRY_MAX).page_timeout,
            super::LIST_TIMEOUT_MAX
        );
    }
}
