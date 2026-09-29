//! Private-network supervisor. ACP execution belongs to a gateway actor, never
//! to an HTTP request or terminal attachment. The only durable format is Kit's
//! existing session transcript; attachment leases and wire replay are ephemeral.
use crate::protocols::http::BearerToken;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use futures_util::future::{Either, select};
#[cfg(test)]
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, mpsc, oneshot, watch},
};
const LEASE: Duration = Duration::from_secs(15);
const MAX_REPLAY: usize = 8 * 1024 * 1024;
const MAX_HTTP_FRAME: usize = 1024 * 1024;
const MAX_QUEUE: usize = 4096;
const MAX_SESSIONS: usize = 64;

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

#[derive(Debug)]
pub struct Args {
    listen: SocketAddr,
    credential_file: Option<PathBuf>,
    project: Vec<PathBuf>,
    allow_private_network: bool,
    command: Option<InternalCommand>,
}
#[derive(Debug)]
enum InternalCommand {
    List {
        url: String,
        credential_file: PathBuf,
    },
    Bridge {
        url: String,
        credential_file: PathBuf,
        root: PathBuf,
        session: Option<String>,
        no_replay: bool,
    },
}
impl Args {
    pub fn command() -> clap::Command {
        use clap::{Arg, ArgAction, Command, value_parser};
        let client = |name| {
            Command::new(name)
                .arg(Arg::new("url").long("url").required(true))
                .arg(
                    Arg::new("credential_file")
                        .long("credential-file")
                        .required(true)
                        .value_parser(value_parser!(PathBuf)),
                )
        };
        Command::new("gateway")
            .about("Experimental private-network session supervisor (bounded HTTP transport)")
            .arg(
                Arg::new("listen")
                    .long("listen")
                    .default_value("127.0.0.1:7766")
                    .value_parser(value_parser!(SocketAddr)),
            )
            .arg(
                Arg::new("credential_file")
                    .long("credential-file")
                    .value_parser(value_parser!(PathBuf)),
            )
            .arg(
                Arg::new("project")
                    .long("project")
                    .action(ArgAction::Append)
                    .value_parser(value_parser!(PathBuf))
                    .help("Approve an exact project directory (repeatable)"),
            )
            .arg(
                Arg::new("allow_private_network")
                    .long("allow-private-network")
                    .action(ArgAction::SetTrue)
                    .help("Allow binding an explicit private-network IP (never a wildcard)"),
            )
            .subcommand(
                client("list").about("List resident and restorable sessions in approved projects"),
            )
            .subcommand(
                client("bridge")
                    .hide(true)
                    .arg(
                        Arg::new("root")
                            .long("root")
                            .required(true)
                            .value_parser(value_parser!(PathBuf)),
                    )
                    .arg(Arg::new("session").long("session"))
                    .arg(
                        Arg::new("no_replay")
                            .long("no-replay")
                            .action(ArgAction::SetTrue),
                    ),
            )
    }
    pub fn from_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        let command = match matches.subcommand() {
            Some(("list", args)) => Some(InternalCommand::List {
                url: required_arg(args, "url")?,
                credential_file: required_arg(args, "credential_file")?,
            }),
            Some(("bridge", args)) => Some(InternalCommand::Bridge {
                url: required_arg(args, "url")?,
                credential_file: required_arg(args, "credential_file")?,
                root: required_arg(args, "root")?,
                session: optional_arg(args, "session")?,
                no_replay: required_arg(args, "no_replay")?,
            }),
            Some((name, _)) => {
                return Err(clap::Error::raw(
                    clap::error::ErrorKind::InvalidSubcommand,
                    name,
                ));
            }
            None => None,
        };
        Ok(Self {
            listen: required_arg(matches, "listen")?,
            credential_file: optional_arg(matches, "credential_file")?,
            project: matches
                .try_get_many::<PathBuf>("project")
                .map_err(|error| clap::Error::raw(clap::error::ErrorKind::InvalidValue, error))?
                .map(|roots| roots.cloned().collect())
                .unwrap_or_default(),
            allow_private_network: required_arg(matches, "allow_private_network")?,
            command,
        })
    }
}
fn optional_arg<T: Clone + Send + Sync + 'static>(
    matches: &clap::ArgMatches,
    name: &str,
) -> Result<Option<T>, clap::Error> {
    matches
        .try_get_one::<T>(name)
        .map(|value| value.cloned())
        .map_err(|error| clap::Error::raw(clap::error::ErrorKind::InvalidValue, error))
}
fn required_arg<T: Clone + Send + Sync + 'static>(
    matches: &clap::ArgMatches,
    name: &str,
) -> Result<T, clap::Error> {
    optional_arg(matches, name)?.ok_or_else(|| {
        clap::Error::raw(
            clap::error::ErrorKind::MissingRequiredArgument,
            format!("missing required argument {name}"),
        )
    })
}
#[derive(Clone, Debug)]
pub struct Remote {
    pub url: String,
    pub credential_file: PathBuf,
    pub session: Option<String>,
    pub no_replay: bool,
}
impl Remote {
    pub fn command(&self, root: &Path) -> io::Result<tokio::process::Command> {
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command
            .args(["gateway", "bridge", "--url", &self.url, "--credential-file"])
            .arg(&self.credential_file)
            .arg("--root")
            .arg(root);
        if let Some(id) = &self.session {
            command.arg("--session").arg(id);
        }
        if self.no_replay {
            command.arg("--no-replay");
        }
        Ok(command)
    }
}
// Actor mailbox commands, never a public transport or durable format.
#[derive(Debug)]
#[cfg_attr(test, derive(Deserialize))]
#[cfg_attr(
    test,
    serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)
)]
enum Request {
    Shutdown,
    List,
    Create {
        root: PathBuf,
        #[cfg_attr(test, serde(default))]
        session: Option<String>,
        #[cfg_attr(test, serde(default))]
        force: bool,
    },
    Attach {
        session: String,
        replay: bool,
        #[cfg_attr(test, serde(default))]
        replace: bool,
        #[cfg_attr(test, serde(default))]
        startup: Option<Value>,
    },
    Poll {
        session: String,
        attachment: String,
        cursor: u64,
    },
    Send {
        session: String,
        attachment: String,
        message: Value,
        // Private transport ownership, never part of ACP or durable state.
        #[cfg_attr(test, serde(skip))]
        charge: Option<Arc<agent_client_protocol::ChargedFrame>>,
    },
    Detach {
        session: String,
        attachment: String,
    },
}
type ResultValue = Result<Value, Failure>;
#[derive(Debug)]
struct Failure(StatusCode, String, &'static str);
impl Failure {
    fn replay(message: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, message.into(), "replay_unavailable")
    }
    fn data(&self) -> Value {
        object([
            ("reason", self.2.into()),
            ("terminal", (self.2 != "unavailable").into()),
        ])
    }
    fn root(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into(), "root_denied")
    }
    fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into(), "protocol")
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, message.into(), "conflict")
    }
    fn unavailable(message: impl Into<String>) -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            message.into(),
            "unavailable",
        )
    }
}
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(object([("data", self.data()), ("error", self.1.into())])),
        )
            .into_response()
    }
}
struct Gateway {
    // Set only by shutdown while holding sessions; checked again under that
    // lock by create/restore so late HTTP requests cannot escape the drain.
    stopping: AtomicBool,
    token: BearerToken,
    roots: Vec<PathBuf>,
    // Writers: create/restore inserts a fully spawned actor; reclamation removes
    // completed actors; shutdown takes all entries. No guard spans await or child
    // teardown. Before insertion Child is kill-on-drop; afterwards actor owns it.
    sessions: Mutex<HashMap<String, Entry>>,
}
impl Gateway {
    async fn reclaim_exited(&self) {
        let exited: Vec<_> = {
            let mut entries = self.sessions.lock().await;
            // Select the current generation while holding the registry lock, not
            // an ID captured by an old actor's completion callback. Command-channel
            // closure alone does not establish that the actor has finished cleanup.
            entries
                .extract_if(|_, entry| entry.stopped.has_changed().is_err())
                .collect()
        };
        // Dropping channel handles can wake other tasks. Do that outside the lock.
        drop(exited);
    }
}
#[derive(Clone)]
struct Entry {
    root: PathBuf,
    sender: mpsc::Sender<Envelope>,
    // Closed when the actor task releases its completion sender. Normal return
    // follows child cleanup; unwind relies on Child's kill-on-drop instead.
    stopped: watch::Receiver<()>,
}
#[derive(Debug)]
struct ChildWrite {
    message: Value,
    // A batch lease can be shared by several accepted child writes. Retain it
    // through serialization/write completion, even after the HTTP owner drops.
    _charge: Option<Arc<agent_client_protocol::ChargedFrame>>,
}
struct Envelope {
    request: Request,
    reply: oneshot::Sender<ResultValue>,
}
fn credential(path: &Path) -> io::Result<String> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "gateway credential must be a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "gateway credential file must not be accessible to group or others (chmod 600)",
            ));
        }
    }
    let token = std::fs::read_to_string(path)?;
    let token = token.strip_suffix('\n').unwrap_or(&token);
    let token = token.strip_suffix('\r').unwrap_or(token);
    if token.is_empty()
        || token
            .bytes()
            .any(|b| b.is_ascii_whitespace() || !b.is_ascii_graphic())
    {
        return Err(io::Error::other(
            "gateway credential must contain one non-empty ASCII bearer token",
        ));
    }
    Ok(token.to_owned())
}
fn permitted_address(address: SocketAddr, private: bool) -> bool {
    address.ip().is_loopback()
        || private
            && match address.ip() {
                std::net::IpAddr::V4(ip) => ip.is_private(),
                std::net::IpAddr::V6(ip) => ip.is_unique_local(),
            }
}
pub async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(command) = args.command {
        return match command {
            InternalCommand::Bridge {
                url,
                credential_file,
                root,
                session,
                no_replay,
            } => {
                http_client::bridge(
                    Remote {
                        url,
                        credential_file,
                        session,
                        no_replay,
                    },
                    root,
                )
                .await
            }
            InternalCommand::List {
                url,
                credential_file,
            } => {
                http_client::list(Remote {
                    url,
                    credential_file,
                    session: None,
                    no_replay: false,
                })
                .await
            }
        };
    }
    if !permitted_address(args.listen, args.allow_private_network) {
        return Err("gateway must bind loopback or an explicitly enabled private-network IP (not a wildcard)".into());
    }
    let path = args
        .credential_file
        .ok_or("--credential-file is required")?;
    credential(&path)?;
    let mut roots = Vec::new();
    for root in args.project {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err("registered project must be a directory".into());
        }
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    if roots.is_empty() {
        return Err("at least one --project directory is required".into());
    }
    let gateway = Arc::new(Gateway {
        stopping: AtomicBool::new(false),
        token: BearerToken::load(&path)?,
        roots,
        sessions: Mutex::new(HashMap::new()),
    });
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    eprintln!(
        "Kit gateway listening on http://{} (private trusted clients only)",
        listener.local_addr()?
    );
    // HTTP connections can own long-lived SSE streams. Stop the listener and
    // release resident ownership without waiting indefinitely for clients.
    eprintln!(
        "Experimental gateway: bounded HTTP transport (1 MiB frames, 64 connections); live replay is limited to 8 MiB / 4093 events. Resume without replay when history exceeds these limits; transcripts remain on the host."
    );
    let served = {
        use std::future::IntoFuture as _;
        let server = std::pin::pin!(axum::serve(listener, router(gateway.clone())?).into_future());
        let shutdown = std::pin::pin!(tokio::signal::ctrl_c());
        match select(server, shutdown).await {
            Either::Left((result, _)) => result,
            Either::Right((result, _)) => result,
        }
    };
    // Stop accepting commands before releasing every resident sender. Actor
    // completion closes the watch channel after Child has been killed/reaped.
    // Neither child teardown nor a completion wait holds the registry lock.
    let entries = {
        let mut entries = gateway.sessions.lock().await;
        gateway.stopping.store(true, Ordering::Release);
        std::mem::take(&mut *entries)
    };
    let mut stopped = Vec::new();
    for entry in entries.into_values() {
        let (reply, _) = oneshot::channel();
        let _ = entry
            .sender
            .send(Envelope {
                request: Request::Shutdown,
                reply,
            })
            .await;
        stopped.push(entry.stopped);
    }
    for mut stopped in stopped {
        let _ = stopped.changed().await;
    }
    served?;
    Ok(())
}
mod framing;
mod http_boundary;
mod http_client;
mod http_server;
fn router(gateway: Arc<Gateway>) -> Result<Router, agent_client_protocol_http::ServerLimitsError> {
    let state = gateway.clone();
    let boundary = Arc::new(http_boundary::Boundary::default());
    let sdk = agent_client_protocol_http::AcpHttpServer::new_bounded(
        move || http_server::Connection::new(state.clone()),
        agent_client_protocol_http::ServerLimits {
            channel_limits: agent_client_protocol::ChannelLimits::default(),
            max_frame_bytes: MAX_HTTP_FRAME,
            ..Default::default()
        },
    )?
    .with_graceful_delete(Duration::from_secs(30))
    .with_options(agent_client_protocol_http::ServerOptions {
        path: "/acp/v2".into(),
        health_endpoint: false,
        ..Default::default()
    })
    .into_router();
    Ok(sdk
        .layer(middleware::from_fn_with_state(
            boundary,
            http_boundary::handle,
        ))
        .layer(middleware::from_fn_with_state(gateway, authorize)))
}

async fn authorize(
    State(gateway): State<Arc<Gateway>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if !gateway.token.authorizes(request.headers()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}
async fn handle(
    State(gateway): State<Arc<Gateway>>,
    Json(request): Json<Request>,
) -> Result<Json<Value>, Failure> {
    if gateway.stopping.load(Ordering::Acquire) {
        return Err(Failure::unavailable("gateway is stopping"));
    }
    match request {
        Request::List => {
            gateway.reclaim_exited().await;
            let entries = gateway.sessions.lock().await.clone();
            let roots = gateway.roots.clone();
            let rows = tokio::task::spawn_blocking(move || {
                let mut rows = Vec::new();
                for root in roots {
                    let catalog = crate::session::catalog(&root).map_err(Failure::unavailable)?;
                    for entry in catalog {
                        let state = if entries
                            .get(&entry.id)
                            .is_some_and(|e| !e.sender.is_closed())
                        {
                            "resident"
                        } else {
                            "restorable"
                        };
                        rows.push(object([
                            ("session", entry.id.into()),
                            ("root", root.to_string_lossy().into_owned().into()),
                            ("title", entry.title.into()),
                            ("preview", entry.preview.into()),
                            ("state", state.into()),
                        ]));
                    }
                }
                for (id, entry) in entries {
                    if !rows.iter().any(|row| row["session"] == id) {
                        rows.push(object([
                            ("session", id.into()),
                            ("root", entry.root.to_string_lossy().into_owned().into()),
                            (
                                "state",
                                if entry.sender.is_closed() {
                                    "exited"
                                } else {
                                    "resident"
                                }
                                .into(),
                            ),
                        ]));
                    }
                }
                Ok::<_, Failure>(rows)
            })
            .await
            .map_err(|_| Failure::unavailable("catalog task failed"))??;
            Ok(Json(object([("sessions", rows.into())])))
        }
        Request::Create {
            root,
            session,
            force,
        } => {
            let root = root
                .canonicalize()
                .map_err(|_| Failure::root("project is not registered"))?;
            if !gateway.roots.contains(&root) {
                return Err(Failure::root("project is not registered"));
            }
            gateway.reclaim_exited().await;
            let restore = session.is_some();
            let id = session.unwrap_or_else(crate::session::new_id);
            if restore {
                let entries = gateway.sessions.lock().await;
                if let Some(entry) = entries.get(&id).filter(|e| !e.sender.is_closed()) {
                    if entry.root != root {
                        return Err(Failure::root("session belongs to another project"));
                    }
                    return Ok(Json(object([
                        ("session", id.into()),
                        ("root", root.to_string_lossy().into_owned().into()),
                        ("state", "resident".into()),
                    ])));
                }
                drop(entries);
                if !crate::session::belongs_to_workspace(&root, &id).map_err(Failure::bad)? {
                    return Err(Failure::root("session does not belong to this project"));
                }
            }
            let mut entries = gateway.sessions.lock().await;
            if gateway.stopping.load(Ordering::Acquire) {
                return Err(Failure::unavailable("gateway is stopping"));
            }
            if let Some(entry) = entries.get(&id).filter(|e| !e.sender.is_closed()) {
                if entry.root != root {
                    return Err(Failure::root("session belongs to another project"));
                }
            } else {
                if entries.len() >= MAX_SESSIONS && !entries.contains_key(&id) {
                    return Err(Failure::unavailable(
                        "gateway session capacity reached; all slots are occupied by active or stopping actors",
                    ));
                }
                let entry = spawn(&root, &id, restore, force)
                    .map_err(|error| Failure::unavailable(error.to_string()))?;
                let replaced = entries.insert(id.clone(), entry);
                drop(entries);
                drop(replaced);
            }
            Ok(Json(object([
                ("session", id.into()),
                ("root", root.to_string_lossy().into_owned().into()),
                ("state", "resident".into()),
            ])))
        }
        request => {
            let id = match &request {
                Request::Attach { session, .. }
                | Request::Poll { session, .. }
                | Request::Send { session, .. }
                | Request::Detach { session, .. } => session,
                _ => return Err(Failure::bad("invalid operation")),
            };
            let entry = gateway
                .sessions
                .lock()
                .await
                .get(id)
                .cloned()
                .ok_or_else(|| {
                    Failure(
                        StatusCode::NOT_FOUND,
                        "session is not resident; create with session id to restore".into(),
                        "session_unavailable",
                    )
                })?;
            let (reply, result) = oneshot::channel();
            entry
                .sender
                .send(Envelope { request, reply })
                .await
                .map_err(|_| Failure::unavailable("child exited; restore its transcript"))?;
            result
                .await
                .map_err(|_| Failure::unavailable("child exited; restore its transcript"))?
                .map(Json)
        }
    }
}
struct CommittedOwner {
    id: String,
    recovery_id: Option<String>,
    recovery_attempt: Option<u64>,
}
struct Attachment {
    id: String,
    touched: Instant,
    next: u64,
    events: VecDeque<(u64, Value)>,
    bytes: usize,
    live: bool,
}
impl Attachment {
    fn push(&mut self, message: Value) -> bool {
        let bytes = message.to_string().len();
        if self.events.len() >= MAX_QUEUE || self.bytes + bytes > MAX_REPLAY {
            return false;
        }
        self.bytes += bytes;
        self.next += 1;
        self.events.push_back((self.next, message));
        true
    }
}
struct Pending {
    attachment: String,
    original: Value,
    method: String,
    // The caller's policy, independent of full internal child cache seeding.
    replay: bool,
}
struct Actor {
    id: String,
    root: PathBuf,
    restore: bool,
    attachment: Option<Attachment>,
    // Survives detach, expiry and replay overflow; only a committed claim replaces it.
    committed_owner: Option<CommittedOwner>,
    serial: u64,
    pending: HashMap<u64, Pending>,
    initialized: Option<Value>,
    session_result: Option<Value>,
    // Native child sessions start idle; only lifecycle notifications change this.
    // Independent of bounded transcript retention, never inferred from text.
    state: Value,
    journal: VecDeque<Value>,
    journal_bytes: usize,
    replay_complete: bool,
}
fn spawn(root: &Path, id: &str, restore: bool, force: bool) -> io::Result<Entry> {
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command
        .args(["acp", "--protocol-version", "2", "--root"])
        .arg(root)
        .arg("--session-id")
        .arg(id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if restore {
        command.arg("--resume");
        if force {
            command.arg("--force");
        }
    }
    let child = command.spawn()?;
    let actor = Actor {
        id: id.to_owned(),
        root: root.to_owned(),
        restore,
        attachment: None,
        committed_owner: None,
        serial: 0,
        pending: HashMap::new(),
        initialized: None,
        session_result: None,
        state: object([
            ("sessionUpdate", "state_update".into()),
            ("state", "idle".into()),
        ]),
        journal: VecDeque::new(),
        journal_bytes: 0,
        replay_complete: true,
    };
    let (sender, receiver) = mpsc::channel(32);
    let (completion, stopped) = watch::channel(());
    tokio::spawn(async move {
        actor.run(child, receiver).await;
        drop(completion);
    });
    Ok(Entry {
        root: root.to_owned(),
        sender,
        stopped,
    })
}
impl Actor {
    // All attachment, replay, and request-id writers are confined to this task.
    // No request future owns Child. Enqueued commands settle even when their
    // HTTP response is dropped. A transport failure isolates this actor; there
    // is no restart-and-retry of an ambiguously submitted prompt.
    async fn run(
        mut self,
        mut child: tokio::process::Child,
        mut commands: mpsc::Receiver<Envelope>,
    ) {
        let Some(mut stdin) = child.stdin.take() else {
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            return;
        };
        // Drain stdout independently of a bounded stdin writer to avoid pipe
        // backpressure deadlocks. JoinSet aborts the writer on actor unwind/drop.
        let (writes, mut queued) = mpsc::channel::<ChildWrite>(16);
        let mut writer = tokio::task::JoinSet::new();
        writer.spawn(async move {
            while let Some(message) = queued.recv().await {
                write_message(&mut stdin, &message.message).await?;
                drop(message);
            }
            Ok::<_, io::Error>(())
        });
        let mut lines = framing::Lines::new(stdout);
        loop {
            let event = {
                let command = std::pin::pin!(commands.recv());
                let line = std::pin::pin!(lines.next_line());
                let exited = std::pin::pin!(child.wait());
                let written = std::pin::pin!(writer.join_next());
                let io = std::pin::pin!(select(line, select(exited, written)));
                match select(command, io).await {
                    Either::Left((command, _)) => Either::Left(command),
                    Either::Right((Either::Left((line, _)), _)) => Either::Right(Some(line)),
                    Either::Right((Either::Right(_), _)) => Either::Right(None),
                }
            };
            match event {
                Either::Left(command) => {
                    let Some(command) = command else {
                        break;
                    };
                    if matches!(command.request, Request::Shutdown) {
                        break;
                    }
                    let result = self.command(command.request, &writes);
                    let failed = result
                        .as_ref()
                        .is_err_and(|e| e.0 == StatusCode::BAD_GATEWAY);
                    let _ = command.reply.send(result);
                    if failed {
                        break;
                    }
                }
                Either::Right(Some(line)) => match line {
                    Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                        Ok(message) => {
                            if let Some(reply) = self.output(message)
                                && writes
                                    .try_send(ChildWrite {
                                        message: reply,
                                        _charge: None,
                                    })
                                    .is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    _ => break,
                },
                Either::Right(None) => break,
            }
        }
        // EOF permits ACP's normal session cleanup to release transcript locks.
        // Abort the writer (including a stalled write) to close stdin, then drain
        // stdout concurrently with waiting: final notifications must not fill a
        // pipe and prevent the child from exiting. Hard kill is a bounded fallback.
        drop(writes);
        writer.shutdown().await;
        let cleanup = async {
            let drain = async { while matches!(lines.next_line().await, Ok(Some(_))) {} };
            let _ = futures_util::future::join(child.wait(), drain).await;
        };
        if tokio::time::timeout(Duration::from_secs(10), cleanup)
            .await
            .is_err()
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
    fn emit(&mut self, message: Value) {
        if let Some(attachment) = &mut self.attachment
            && !attachment.push(message)
        {
            self.attachment = None;
        }
    }
    fn remember(&mut self, message: Value) {
        self.journal_bytes += message.to_string().len();
        self.journal.push_back(message);
        while self.journal_bytes > MAX_REPLAY || self.journal.len() > MAX_QUEUE - 3 {
            if let Some(old) = self.journal.pop_front() {
                self.journal_bytes -= old.to_string().len();
            }
            self.replay_complete = false;
        }
    }
    fn output(&mut self, mut message: Value) -> Option<Value> {
        if message.get("method").is_some() {
            if let Some(id) = message.get("id") {
                return Some(object([
                    ("jsonrpc", "2.0".into()),
                    ("id", id.clone()),
                    (
                        "error",
                        object([
                            ("code", (-32601).into()),
                            ("message", "gateway has no client-side services".into()),
                        ]),
                    ),
                ]));
            }
            if message["method"] == "session/update"
                && message["params"]["sessionId"] == self.id
                && message["params"]["update"]["sessionUpdate"] == "state_update"
            {
                self.state = message["params"]["update"].clone();
            }
            if let Some(options) = message["params"]["update"].get("configOptions")
                && let Some(result) = &mut self.session_result
            {
                result["configOptions"] = options.clone();
            }
            self.remember(message.clone());
            if self.attachment.as_ref().is_some_and(|a| a.live) {
                self.emit(message);
            }
        } else if let Some(id) = message["id"].as_u64()
            && let Some(pending) = self.pending.remove(&id)
        {
            if pending.method == "initialize"
                && let Some(capabilities) =
                    message["result"]["capabilities"]["session"].as_object_mut()
            {
                for name in ["close", "delete", "fork", "list"] {
                    capabilities.remove(name);
                }
            }
            if let Some(result) = message.get("result") {
                match pending.method.as_str() {
                    "initialize" => self.initialized = Some(result.clone()),
                    "session/new" | "session/resume" => self.session_result = Some(result.clone()),
                    "session/set_config_option" => {
                        if let Some(options) = result.get("configOptions")
                            && let Some(snapshot) = &mut self.session_result
                        {
                            snapshot["configOptions"] = options.clone();
                        }
                    }
                    _ => (),
                }
            }
            if self
                .attachment
                .as_ref()
                .is_some_and(|a| a.id == pending.attachment)
            {
                message["id"] = pending.original;
                if matches!(pending.method.as_str(), "session/new" | "session/resume")
                    && message.get("result").is_some()
                {
                    let messages = self.startup_messages(
                        message["result"].clone(),
                        message["id"].clone(),
                        pending.replay,
                        &pending.attachment,
                    );
                    match messages {
                        Ok(messages) => {
                            // Include any unpolled initialization response in admission.
                            let fits = self.attachment.as_ref().is_some_and(|a| {
                                a.events.len() + messages.len() <= MAX_QUEUE
                                    && a.bytes
                                        + messages
                                            .iter()
                                            .map(|m| m.to_string().len())
                                            .sum::<usize>()
                                        <= MAX_REPLAY
                            });
                            if fits {
                                for update in messages {
                                    self.emit(update);
                                }
                                if let Some(attachment) = &mut self.attachment {
                                    attachment.live = true;
                                }
                                return None;
                            }
                            message = object([
                                ("jsonrpc", "2.0".into()),
                                ("id", message["id"].clone()),
                                (
                                    "error",
                                    object([
                                        ("code", (-32000).into()),
                                        (
                                            "message",
                                            "session snapshot exceeds the gateway limit".into(),
                                        ),
                                        ("data", Failure::replay("snapshot overflow").data()),
                                    ]),
                                ),
                            ]);
                        }
                        Err(error) => {
                            message = object([
                                ("jsonrpc", "2.0".into()),
                                ("id", message["id"].clone()),
                                (
                                    "error",
                                    object([
                                        ("code", (-32000).into()),
                                        ("data", error.data()),
                                        ("message", error.1.into()),
                                    ]),
                                ),
                            ]);
                        }
                    }
                }
                self.emit(message);
            }
        }
        None
    }
    fn startup_messages(
        &self,
        mut result: Value,
        id: Value,
        replay: bool,
        attachment: &str,
    ) -> Result<Vec<Value>, Failure> {
        if replay && !self.replay_complete {
            return Err(Failure::replay("full live replay is unavailable"));
        }
        let mut messages: Vec<Value> = if replay {
            self.journal.iter().cloned().collect()
        } else {
            Vec::new()
        };
        let options = result
            .get("configOptions")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        for update in [
            object([
                ("sessionUpdate", "config_option_update".into()),
                ("configOptions", options),
            ]),
            self.state.clone(),
        ] {
            messages.push(object([
                ("jsonrpc", "2.0".into()),
                ("method", "session/update".into()),
                (
                    "params",
                    object([("sessionId", self.id.clone().into()), ("update", update)]),
                ),
            ]));
        }
        result["sessionId"] = self.id.clone().into();
        result["_meta"]["kit/gateway"] = object([
            ("attachment", attachment.into()),
            ("historyAvailable", replay.into()),
            ("stateSnapshot", true.into()),
            ("configSnapshot", true.into()),
        ]);
        messages.push(object([
            ("jsonrpc", "2.0".into()),
            ("id", id),
            ("result", result),
        ]));
        if messages
            .iter()
            .any(|message| message.to_string().len() > MAX_HTTP_FRAME)
            || messages.len() > MAX_QUEUE
            || messages.iter().map(|m| m.to_string().len()).sum::<usize>() > MAX_REPLAY
        {
            return Err(Failure::replay(
                "session snapshot exceeds the gateway limit",
            ));
        }
        Ok(messages)
    }
    fn command(&mut self, request: Request, writes: &mpsc::Sender<ChildWrite>) -> ResultValue {
        if let Request::Attach {
            replace,
            replay,
            startup,
            ..
        } = request
        {
            let metadata = startup
                .as_ref()
                .map(|message| &message["params"]["_meta"]["kit/gateway"]);
            let token = |name: &str| -> Result<Option<String>, Failure> {
                metadata
                    .and_then(|metadata| metadata.get(name))
                    .map(|value| {
                        value
                            .as_str()
                            .filter(|value| !value.is_empty() && value.len() <= 128)
                            .map(str::to_owned)
                            .ok_or_else(|| {
                                Failure::bad(format!(
                                    "{name} must be a nonempty string of at most 128 bytes"
                                ))
                            })
                    })
                    .transpose()
            };
            let previous = token("previousAttachment")?;
            let recovery_id = token("recoveryId")?;
            let recovery_attempt = metadata
                .and_then(|metadata| metadata.get("recoveryAttempt"))
                .map(|value| {
                    value
                        .as_u64()
                        .filter(|attempt| *attempt > 0)
                        .ok_or_else(|| Failure::bad("recoveryAttempt must be a positive u64"))
                })
                .transpose()?;
            if recovery_id.is_some() != recovery_attempt.is_some() {
                return Err(Failure::bad(
                    "recoveryId and recoveryAttempt must be supplied together",
                ));
            }
            if recovery_id.is_some() && previous.is_none() {
                return Err(Failure::bad("recoveryId requires previousAttachment"));
            }
            // A matching predecessor must not bypass the sequence high-water mark.
            // Validate before preparing anything, and commit the mark only with ownership.
            if let Some(owner) = &self.committed_owner
                && recovery_id.is_some()
                && owner.recovery_id == recovery_id
                && recovery_attempt <= owner.recovery_attempt
            {
                return Err(Failure(
                    StatusCode::CONFLICT,
                    "recovery attempt is stale; automatic resume refused".into(),
                    "stale_recovery",
                ));
            }
            if let Some(previous) = previous
                && self.committed_owner.as_ref().is_some_and(|owner| {
                    owner.id != previous
                        && !(recovery_id.is_some() && owner.recovery_id == recovery_id)
                })
            {
                return Err(Failure(
                    StatusCode::CONFLICT,
                    "controller was replaced; automatic resume refused".into(),
                    "controller_replaced",
                ));
            }
            if !replace
                && self
                    .attachment
                    .as_ref()
                    .is_some_and(|a| a.touched.elapsed() < LEASE)
            {
                return Err(Failure::conflict(
                    "session already has a controlling attachment",
                ));
            }
            if replay && !self.replay_complete {
                return Err(Failure::replay(
                    "live replay limit reached; transcript is durable but a full live reattach is unavailable",
                ));
            }
            let id = crate::session::new_id();
            let mut attachment = Attachment {
                id: id.clone(),
                touched: Instant::now(),
                next: 0,
                events: VecDeque::new(),
                bytes: 0,
                live: false,
            };
            let started =
                if let (Some(startup), Some(result)) = (startup, self.session_result.clone()) {
                    let request_id = startup
                        .get("id")
                        .cloned()
                        .ok_or_else(|| Failure::bad("session startup requires a request id"))?;
                    for message in self.startup_messages(result, request_id, replay, &id)? {
                        if !attachment.push(message) {
                            return Err(Failure::replay(
                                "session snapshot exceeds the gateway limit",
                            ));
                        }
                    }
                    attachment.live = true;
                    true
                } else {
                    false
                };
            // No fallible work after the ownership commit. The previous controller
            // survives every replay/snapshot preparation failure.
            self.committed_owner = Some(CommittedOwner {
                id: id.clone(),
                recovery_id,
                recovery_attempt,
            });
            self.attachment = Some(attachment);
            return Ok(object([
                ("started", started.into()),
                ("attachment", id.into()),
                ("lease_seconds", LEASE.as_secs().into()),
            ]));
        }
        let token = match &request {
            Request::Poll { attachment, .. }
            | Request::Send { attachment, .. }
            | Request::Detach { attachment, .. } => attachment,
            _ => return Err(Failure::bad("invalid actor operation")),
        };
        let attachment = self
            .attachment
            .as_mut()
            .filter(|a| a.id == *token && a.touched.elapsed() < LEASE)
            .ok_or_else(|| {
                Failure(
                    StatusCode::CONFLICT,
                    "attachment expired or was replaced; reconnect".into(),
                    "controller_replaced",
                )
            })?;
        attachment.touched = Instant::now();
        match request {
            Request::Detach { .. } => {
                self.attachment = None;
                Ok(object([]))
            }
            Request::Poll { cursor, .. } => {
                if cursor > attachment.next {
                    return Err(Failure::bad("invalid event cursor"));
                }
                while attachment
                    .events
                    .front()
                    .is_some_and(|(sequence, _)| *sequence <= cursor)
                {
                    if let Some((_, message)) = attachment.events.pop_front() {
                        attachment.bytes -= message.to_string().len();
                    }
                }
                let events: Vec<_> = attachment
                    .events
                    .iter()
                    .take(128)
                    .map(|(sequence, message)| {
                        object([("cursor", (*sequence).into()), ("message", message.clone())])
                    })
                    .collect();
                Ok(object([("events", events.into())]))
            }
            Request::Send {
                attachment,
                mut message,
                charge,
                ..
            } => {
                let method = message["method"]
                    .as_str()
                    .ok_or_else(|| Failure::bad("ACP method required"))?
                    .to_owned();
                let original = message.get("id").cloned();
                if !message.is_object()
                    || !message["params"].is_object()
                    || message["jsonrpc"] != "2.0"
                {
                    return Err(Failure::bad("invalid ACP message"));
                }
                if let Some(session) = message["params"].get("sessionId")
                    && session != &Value::String(self.id.clone())
                {
                    return Err(Failure::bad("ACP session does not match gateway session"));
                }
                // Request-task cancellation is not an execution interrupt. In
                // particular, SDK teardown must not cancel detached work; use
                // explicit session/cancel (which is session-scoped) instead.
                if method == "$/cancel_request" {
                    return Ok(object([]));
                }
                if !matches!(
                    method.as_str(),
                    "initialize"
                        | "session/new"
                        | "session/resume"
                        | "session/prompt"
                        | "session/inject"
                        | "session/revoke_inject"
                        | "session/replace_inject"
                        | "session/cancel"
                        | "session/set_config_option"
                ) {
                    return Err(Failure::bad(
                        "ACP method is not supported by the gateway; disconnect to detach",
                    ));
                }
                let startup = matches!(method.as_str(), "session/new" | "session/resume");
                if startup {
                    if original.is_none() {
                        return Err(Failure::bad("session startup requires a request id"));
                    }
                    if self.attachment.as_ref().is_some_and(|a| a.live) {
                        return Err(Failure::conflict("attachment already opened its session"));
                    }
                    if let Some(cursor) = message["params"].get("replayFrom")
                        && !cursor.is_null()
                        && cursor != &object([("type", "start".into())])
                    {
                        return Err(Failure::bad("unsupported replay cursor"));
                    }
                }
                if let Some(id) = &original
                    && self
                        .pending
                        .values()
                        .any(|p| p.attachment == attachment && p.original == *id)
                {
                    return Err(Failure::conflict("duplicate pending request id"));
                }
                let replay = method == "session/new"
                    || message["params"]["replayFrom"] == object([("type", "start".into())]);
                if startup && replay && !self.replay_complete {
                    return Err(Failure::replay("full live replay is unavailable"));
                }
                let cached = match method.as_str() {
                    "initialize" => self.initialized.clone(),
                    "session/new" | "session/resume" => self.session_result.clone(),

                    _ => None,
                };
                if let Some(result) = cached {
                    if startup {
                        let Some(id) = original else {
                            return Err(Failure::bad("session startup requires a request id"));
                        };
                        let messages = self.startup_messages(result, id, replay, &attachment)?;
                        let fits = self.attachment.as_ref().is_some_and(|a| {
                            a.events.len() + messages.len() <= MAX_QUEUE
                                && a.bytes
                                    + messages.iter().map(|m| m.to_string().len()).sum::<usize>()
                                    <= MAX_REPLAY
                        });
                        if !fits {
                            return Err(Failure::replay(
                                "session snapshot exceeds the gateway limit",
                            ));
                        }
                        for update in messages {
                            self.emit(update);
                        }
                        if let Some(attachment) = &mut self.attachment {
                            attachment.live = true;
                        }
                    } else if let Some(id) = original {
                        self.emit(object([
                            ("jsonrpc", "2.0".into()),
                            ("id", id),
                            ("result", result),
                        ]));
                    }
                    return Ok(object([]));
                }
                if self.pending.len() >= 128 {
                    return Err(Failure::unavailable("too many pending ACP requests"));
                }
                if matches!(method.as_str(), "session/new" | "session/resume") {
                    if self
                        .pending
                        .values()
                        .any(|p| matches!(p.method.as_str(), "session/new" | "session/resume"))
                    {
                        return Err(Failure::conflict(
                            "session startup is pending; reconnect after it settles",
                        ));
                    }
                    message["params"]["cwd"] =
                        Value::String(self.root.to_string_lossy().into_owned());
                    message["params"]["additionalDirectories"] = Value::Array(Vec::new());
                    message["params"]["mcpServers"] = Value::Array(Vec::new());
                    if self.restore {
                        message["method"] = Value::String("session/resume".into());
                        message["params"]["sessionId"] = Value::String(self.id.clone());
                        message["params"]["replayFrom"] = object([("type", "start".into())]);
                    }
                }
                if method == "initialize" {
                    message["params"]["capabilities"] = object([]);
                }
                let pending = if let Some(original) = original {
                    self.serial = self
                        .serial
                        .checked_add(1)
                        .ok_or_else(|| Failure::unavailable("request id exhausted"))?;
                    message["id"] = Value::from(self.serial);
                    Some(Pending {
                        attachment,
                        original,
                        method,
                        replay,
                    })
                } else {
                    None
                };
                // Capacity is checked before publishing the pending record. No
                // await separates acceptance and commit; cancellation cannot
                // strand a reserved ID or suppress an accepted child write.
                writes
                    .try_send(ChildWrite {
                        message,
                        _charge: charge,
                    })
                    .map_err(|_| {
                        Failure::unavailable("child input queue unavailable; message not accepted")
                    })?;
                if let Some(pending) = pending {
                    self.pending.insert(self.serial, pending);
                }
                Ok(object([]))
            }
            _ => Err(Failure::bad("invalid actor operation")),
        }
    }
}
async fn write_message(stdin: &mut tokio::process::ChildStdin, message: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(10), stdin.write_all(&bytes))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "child stdin stalled"))?
}
#[cfg(test)]
#[path = "gateway/tests.rs"]
mod tests;
