//! Runtime-owned catalog reconciliation. Watch messages are invalidations, not
//! publications: only a successful, still-current fetch acknowledges them.
use super::{Inner, McpRuntime, ServerRecord};
use agentkit_mcp::{McpCatalogRefreshResult, McpConnection, McpServerId};
use futures_util::{
    FutureExt, StreamExt,
    future::{AbortHandle, Abortable, BoxFuture},
    stream::FuturesUnordered,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock, Weak},
    time::Duration,
};
use tokio::{sync::Mutex, task::AbortHandle as TaskAbortHandle, time::Instant};

const CONCURRENCY: usize = 8;
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct Refresh {
    task: OnceLock<TaskAbortHandle>,
    errors: Mutex<BTreeMap<String, Failure>>,
}

impl Drop for Refresh {
    fn drop(&mut self) {
        if let Some(task) = self.task.get() {
            task.abort();
        }
    }
}

#[derive(Clone)]
struct Identity {
    connection: Arc<McpConnection>,
    epoch: u64,
}

impl Identity {
    fn new(connection: Arc<McpConnection>) -> Self {
        let epoch = connection.connection_epoch();
        Self { connection, epoch }
    }

    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.connection, &other.connection) && self.epoch == other.epoch
    }
}

struct Failure {
    connection: Weak<McpConnection>,
    epoch: u64,
    message: String,
}

struct Retry {
    identity: Identity,
    delay: Duration,
    at: Instant,
}

struct Completed {
    name: String,
    identity: Identity,
    revision: u64,
    result: Option<McpCatalogRefreshResult>,
}

pub(super) fn start(inner: &Arc<Inner>) {
    inner.refresh.task.get_or_init(|| {
        let weak = Arc::downgrade(inner);
        tokio::spawn(run(weak)).abort_handle()
    });
}

// Called while the manager is locked, like publication. Diagnostics are only
// projected onto this discovery snapshot; authentication status stays untouched.
pub(super) async fn annotate(
    inner: &Inner,
    manager: &agentkit_mcp::McpServerManager,
    records: &mut BTreeMap<String, ServerRecord>,
) {
    let errors = inner.refresh.errors.lock().await;
    for request in manager.prepare_changed_catalogs() {
        if let Some(record) = records.get_mut(&request.server_id().to_string()) {
            record.catalog_stale = true;
        }
    }
    for (name, record) in records {
        if let Some(failure) = errors.get(name)
            && let Some(handle) = manager.connected_server(&McpServerId::new(name.as_str()))
        {
            let connection = handle.connection();
            if failure.epoch == connection.connection_epoch()
                && failure.connection.ptr_eq(&Arc::downgrade(&connection))
            {
                record.catalog_error = Some(failure.message.clone());
            }
        }
    }
}

pub(super) async fn subscription_status(connection: &McpConnection) -> Option<serde_json::Value> {
    use agentkit_mcp::McpSubscriptionStatus;
    use serde_json::{Map, Value};
    let mut value = Map::new();
    match connection.subscription_status().await? {
        McpSubscriptionStatus::Pending => {
            value.insert("state".into(), "pending".into());
        }
        McpSubscriptionStatus::Active {
            requested,
            acknowledged,
        } => {
            value.insert("state".into(), "active".into());
            value.insert("requested".into(), serde_json::to_value(requested).ok()?);
            value.insert(
                "acknowledged".into(),
                serde_json::to_value(acknowledged).ok()?,
            );
        }
        McpSubscriptionStatus::Closed { graceful, error } => {
            value.insert("state".into(), "closed".into());
            value.insert("graceful".into(), graceful.into());
            if let Some(error) = error {
                value.insert("error".into(), super::bounded_server_text(&error).into());
            }
        }
    }
    Some(Value::Object(value))
}

/// Keep Runlet's closed tool output contract in step with the diagnostics.
pub(super) fn subscription_schema() -> serde_json::Value {
    use serde_json::{Map, Value};
    fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
        Value::Object(Map::from_iter(
            fields.into_iter().map(|(key, value)| (key.into(), value)),
        ))
    }
    fn typed(kind: &str) -> Value {
        object([("type", kind.into())])
    }
    let filter = object([
        ("type", "object".into()),
        (
            "properties",
            object([
                ("toolsListChanged", typed("boolean")),
                ("promptsListChanged", typed("boolean")),
                ("resourcesListChanged", typed("boolean")),
                (
                    "resourceSubscriptions",
                    object([("type", "array".into()), ("items", typed("string"))]),
                ),
            ]),
        ),
        ("additionalProperties", false.into()),
    ]);
    object([
        ("type", "object".into()),
        (
            "properties",
            object([
                (
                    "state",
                    object([
                        ("type", "string".into()),
                        (
                            "enum",
                            Value::Array(vec!["pending".into(), "active".into(), "closed".into()]),
                        ),
                    ]),
                ),
                ("requested", filter.clone()),
                ("acknowledged", filter),
                ("graceful", typed("boolean")),
                ("error", typed("string")),
            ]),
        ),
        ("required", Value::Array(vec!["state".into()])),
        ("additionalProperties", false.into()),
    ])
}

pub(super) fn render_diagnostics(output: &mut serde_json::Value, record: &ServerRecord) {
    if let Some(subscription) = &record.subscription {
        output["subscription"] = subscription.clone();
    }
    if record.catalog_stale {
        output["catalog_stale"] = true.into();
    }
    if let Some(error) = &record.catalog_error {
        output["catalog_error"] = super::bounded_server_text(error).into();
    }
}

async fn run(weak: Weak<Inner>) {
    let mut pending: FuturesUnordered<BoxFuture<'static, Completed>> = FuturesUnordered::new();
    let mut active: BTreeMap<String, (Identity, AbortHandle)> = BTreeMap::new();
    let mut retries: BTreeMap<String, Retry> = BTreeMap::new();
    loop {
        let Some(inner) = weak.upgrade() else { return };
        if !inner.healthy.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let manager = inner.manager.lock().await;
        let current: BTreeMap<_, _> = manager
            .connected_servers()
            .into_iter()
            .map(|handle| {
                (
                    handle.server_id().to_string(),
                    Identity::new(handle.connection()),
                )
            })
            .collect();
        active.retain(|name, (identity, abort)| {
            let retain = current.get(name).is_some_and(|now| identity.matches(now));
            if !retain {
                abort.abort();
            }
            retain
        });
        retries.retain(|name, retry| {
            current
                .get(name)
                .is_some_and(|now| retry.identity.matches(now))
        });
        inner.refresh.errors.lock().await.retain(|name, failure| {
            current.get(name).is_some_and(|now| {
                failure.epoch == now.epoch
                    && failure.connection.ptr_eq(&Arc::downgrade(&now.connection))
            })
        });
        let mut wake = manager.catalog_refresh_wake();
        // The SDK marks already-dirty watches ready when capturing them. Consume
        // those initial versions before preparing work. Each source is consumed
        // at most once here; later revisions still wake the select below.
        for _ in 0..=current.len() {
            if wake.changed().now_or_never().is_none() {
                break;
            }
        }
        let requests = manager.prepare_changed_catalogs();
        let mut retry_at = None;
        for request in requests {
            let name = request.server_id().to_string();
            if active.contains_key(&name) {
                continue;
            }
            if let Some(retry) = retries.get(&name)
                && retry.at > Instant::now()
            {
                retry_at = Some(retry_at.map_or(retry.at, |at: Instant| at.min(retry.at)));
                continue;
            }
            // Aborted requests retain their connections until polled/dropped,
            // so bound the whole collection, including canceled old epochs.
            if pending.len() >= CONCURRENCY {
                continue;
            }
            let Some(identity) = current.get(&name).cloned() else {
                continue;
            };
            let revision = identity.connection.catalog_revision();
            let (abort, registration) = AbortHandle::new_pair();
            active.insert(name.clone(), (identity.clone(), abort));
            pending.push(
                async move {
                    let result = Abortable::new(request.fetch(), registration).await.ok();
                    Completed {
                        name,
                        identity,
                        revision,
                        result,
                    }
                }
                .boxed(),
            );
        }
        drop(manager);
        drop(current);
        // Waiting for notifications, retries, or fetch completion retains no
        // runtime owner. Dropping its last owner aborts this task and drops all
        // requests and subscription clones.
        drop(inner);
        // Use cancellation-safe futures without select!'s generated panic path.
        let completion = async {
            if pending.is_empty() {
                std::future::pending().await
            } else {
                pending.next().await
            }
        };
        let retry = async {
            if let Some(at) = retry_at {
                tokio::time::sleep_until(at).await
            } else {
                std::future::pending::<()>().await
            }
        };
        use futures_util::future::{Either, select};
        let completed = match select(
            completion.boxed(),
            select(retry.boxed(), wake.changed().boxed()),
        )
        .await
        {
            Either::Left((completed, _)) => completed,
            Either::Right(_) => continue,
        };
        let Some(completed) = completed else { continue };
        if active
            .get(&completed.name)
            .is_some_and(|(identity, _)| identity.matches(&completed.identity))
        {
            active.remove(&completed.name);
        }
        let Some(result) = completed.result else {
            continue;
        };
        let Some(inner) = weak.upgrade() else { return };
        let runtime = McpRuntime { inner };
        let gate = runtime.operation_gate(&completed.name).await;
        let Ok(_gate) = gate.try_read_owned() else {
            retries.insert(
                completed.name,
                Retry {
                    identity: completed.identity,
                    delay: RETRY_MIN,
                    at: Instant::now() + RETRY_MIN,
                },
            );
            continue;
        };
        let mut manager = runtime.inner.manager.lock().await;
        if runtime.ensure_healthy().is_err() {
            return;
        }
        let current = manager
            .connected_server(&McpServerId::new(&completed.name))
            .map(|handle| Identity::new(handle.connection()));
        if !current
            .as_ref()
            .is_some_and(|now| now.matches(&completed.identity))
            || completed.identity.connection.catalog_revision() != completed.revision
        {
            continue;
        }
        match manager.publish_catalog_refresh(result) {
            Ok(_) => {
                retries.remove(&completed.name);
                runtime
                    .inner
                    .refresh
                    .errors
                    .lock()
                    .await
                    .remove(&completed.name);
            }
            Err(error) => {
                let delay = retries
                    .get(&completed.name)
                    .map_or(RETRY_MIN, |retry| (retry.delay * 2).min(RETRY_MAX));
                runtime.inner.refresh.errors.lock().await.insert(
                    completed.name.clone(),
                    Failure {
                        connection: Arc::downgrade(&completed.identity.connection),
                        epoch: completed.identity.epoch,
                        message: error.to_string(),
                    },
                );
                retries.insert(
                    completed.name,
                    Retry {
                        identity: completed.identity,
                        delay,
                        at: Instant::now() + delay,
                    },
                );
            }
        }
    }
}
