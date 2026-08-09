//! Production stdio LSP launcher: a spawned, owned child process speaking
//! Content-Length framing over piped stdin/stdout.
//!
//! The transport owns the complete child lifecycle: the child is spawned with a
//! cleared environment in its own process group, every pipe interaction is
//! bounded by the caller's [`SendContext`] budget, and `close_and_reap` (or a
//! best-effort `Drop`) kills the whole process group and proves the child was
//! reaped. This is a self-contained owned child: it is not registered with the
//! durable executor process registry, because shadow LSP sessions are
//! short-lived request-scoped helpers whose lifetime is fenced by
//! [`crate::verify::lsp::session::LspSessionManager`] deadlines.
//!
//! Frame filtering: the session manager treats every received frame as a
//! `textDocument/publishDiagnostics` notification and hard-fails on anything
//! else, while real servers also emit an `initialize` response, progress and
//! log notifications, and server-to-client requests. The reader thread
//! therefore forwards only `publishDiagnostics` notifications, answers
//! server-to-client requests with a JSON-RPC `-32601` error so the server's
//! pipe keeps flowing, and drops everything else.

use std::{
    io::{BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::{
    domain::{ids::ProcessId, lifecycle::ProcessClaim},
    verify::lsp::session::{
        CodecLimits, LaunchRequest, LspCodec, OwnedLspLauncher, OwnedLspTransport, SendContext,
        TransportError,
    },
};

/// Hard ceiling for the configured shadow LSP wall time.
pub const MAX_NATIVE_LSP_WALL_TIME: Duration = Duration::from_secs(60);
/// Default shadow LSP wall time when the config omits `wall_time_millis`.
pub const DEFAULT_NATIVE_LSP_WALL_TIME: Duration = Duration::from_secs(5);
/// Hard ceiling for the configured per-run diagnostic count.
pub const MAX_NATIVE_LSP_DIAGNOSTICS: u64 = 10_000;
/// Default per-run diagnostic bound when the config omits `max_diagnostics`.
pub const DEFAULT_NATIVE_LSP_DIAGNOSTICS: u64 = 200;
const MAX_NATIVE_LSP_ARGUMENTS: usize = 64;
const MAX_NATIVE_LSP_LANGUAGES: usize = 32;
const MAX_NATIVE_LSP_STRING_BYTES: usize = 4 * 1024;
/// Backpressure bound on buffered publishDiagnostics frames.
const READER_CHANNEL_FRAMES: usize = 64;
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(5);
const DROP_REAP_BUDGET: Duration = Duration::from_secs(1);

/// Validated `.kit/native.json` `lsp` object: the operator-trusted shadow LSP
/// server declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeLspServerConfig {
    command: String,
    arguments: Vec<String>,
    languages: Vec<String>,
    wall_time: Duration,
    max_diagnostics: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLspConfigError {
    EmptyCommand,
    OversizedCommand,
    TooManyArguments,
    OversizedArgument,
    NoLanguages,
    TooManyLanguages,
    InvalidLanguage,
    WallTimeOutOfBounds,
    DiagnosticBoundOutOfBounds,
}

impl std::fmt::Display for NativeLspConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::EmptyCommand => "lsp.command must not be empty",
            Self::OversizedCommand => "lsp.command exceeds 4096 bytes",
            Self::TooManyArguments => "lsp.arguments allows at most 64 entries",
            Self::OversizedArgument => "lsp.arguments entries are capped at 4096 bytes",
            Self::NoLanguages => "lsp.languages must list at least one language or extension",
            Self::TooManyLanguages => "lsp.languages allows at most 32 entries",
            Self::InvalidLanguage => {
                "lsp.languages entries must be non-empty, at most 64 bytes, without control bytes"
            }
            Self::WallTimeOutOfBounds => "lsp.wall_time_millis must be within 1..=60000",
            Self::DiagnosticBoundOutOfBounds => "lsp.max_diagnostics must be within 1..=10000",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for NativeLspConfigError {}

impl NativeLspServerConfig {
    pub fn new(
        command: String,
        arguments: Vec<String>,
        languages: Vec<String>,
        wall_time_millis: u64,
        max_diagnostics: u64,
    ) -> Result<Self, NativeLspConfigError> {
        if command.is_empty() || command.bytes().any(|byte| byte == 0) {
            return Err(NativeLspConfigError::EmptyCommand);
        }
        if command.len() > MAX_NATIVE_LSP_STRING_BYTES {
            return Err(NativeLspConfigError::OversizedCommand);
        }
        if arguments.len() > MAX_NATIVE_LSP_ARGUMENTS {
            return Err(NativeLspConfigError::TooManyArguments);
        }
        if arguments
            .iter()
            .any(|argument| argument.len() > MAX_NATIVE_LSP_STRING_BYTES)
        {
            return Err(NativeLspConfigError::OversizedArgument);
        }
        if languages.is_empty() {
            return Err(NativeLspConfigError::NoLanguages);
        }
        if languages.len() > MAX_NATIVE_LSP_LANGUAGES {
            return Err(NativeLspConfigError::TooManyLanguages);
        }
        if languages.iter().any(|language| {
            language.is_empty()
                || language.len() > 64
                || language.bytes().any(|byte| byte.is_ascii_control())
        }) {
            return Err(NativeLspConfigError::InvalidLanguage);
        }
        let wall_time = Duration::from_millis(wall_time_millis);
        if wall_time.is_zero() || wall_time > MAX_NATIVE_LSP_WALL_TIME {
            return Err(NativeLspConfigError::WallTimeOutOfBounds);
        }
        if max_diagnostics == 0 || max_diagnostics > MAX_NATIVE_LSP_DIAGNOSTICS {
            return Err(NativeLspConfigError::DiagnosticBoundOutOfBounds);
        }
        Ok(Self {
            command,
            arguments,
            languages,
            wall_time,
            max_diagnostics,
        })
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub fn languages(&self) -> &[String] {
        &self.languages
    }

    pub const fn wall_time(&self) -> Duration {
        self.wall_time
    }

    pub const fn max_diagnostics(&self) -> u64 {
        self.max_diagnostics
    }

    /// Whether a root-relative file path matches the configured language list,
    /// using the same convention as the syntax pipeline: a language key
    /// ("rust", "json", "text") or a bare file extension ("rs", "toml").
    pub fn matches_path(&self, path: &str, language_key: Option<&str>) -> bool {
        let extension = std::path::Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str());
        self.languages.iter().any(|language| {
            language_key.is_some_and(|key| key.eq_ignore_ascii_case(language))
                || extension.is_some_and(|extension| extension.eq_ignore_ascii_case(language))
        })
    }
}

/// Launches the operator-configured LSP server as an owned child process.
pub struct StdioLspLauncher {
    command: String,
    arguments: Vec<String>,
    working_dir: PathBuf,
    codec: CodecLimits,
}

impl StdioLspLauncher {
    /// `working_dir` must be the trusted canonical workspace root the shadow
    /// runner validates; staged buffers are delivered over `didOpen`, never
    /// written to disk.
    pub fn new(config: &NativeLspServerConfig, working_dir: PathBuf, codec: CodecLimits) -> Self {
        Self {
            command: config.command().to_owned(),
            arguments: config.arguments().to_vec(),
            working_dir,
            codec,
        }
    }
}

impl OwnedLspLauncher for StdioLspLauncher {
    type Transport = StdioLspTransport;

    fn launch(&mut self, request: LaunchRequest<'_>) -> Result<Self::Transport, TransportError> {
        let process_id = ProcessId::generate().map_err(|_| TransportError::LaunchFailed)?;
        let mut command = Command::new(&self.command);
        command
            .args(&self.arguments)
            .current_dir(&self.working_dir)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|_| TransportError::LaunchFailed)?;
        let stdin = child.stdin.take().ok_or(TransportError::LaunchFailed)?;
        let stdout = child.stdout.take().ok_or(TransportError::LaunchFailed)?;
        let (write_tx, write_rx) = sync_channel::<WriteRequest>(READER_CHANNEL_FRAMES);
        let (ack_tx, ack_rx) = sync_channel::<bool>(1);
        let (frame_tx, frame_rx) = sync_channel::<Result<Vec<u8>, ()>>(READER_CHANNEL_FRAMES);
        let reply_tx = write_tx.clone();
        let codec = self.codec;
        let writer = std::thread::Builder::new()
            .name("kit-lsp-writer".to_owned())
            .spawn(move || writer_loop(stdin, write_rx, ack_tx))
            .map_err(|_| TransportError::LaunchFailed);
        let writer = match writer {
            Ok(writer) => writer,
            Err(error) => {
                let _ = kill_process_group(&mut child);
                let _ = child.wait();
                return Err(error);
            }
        };
        let reader = std::thread::Builder::new()
            .name("kit-lsp-reader".to_owned())
            .spawn(move || reader_loop(stdout, codec, frame_tx, reply_tx))
            .map_err(|_| TransportError::LaunchFailed);
        let reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                drop(write_tx);
                let _ = kill_process_group(&mut child);
                let _ = child.wait();
                let _ = writer.join();
                return Err(error);
            }
        };
        Ok(StdioLspTransport {
            claim: ProcessClaim::new(process_id, request.ownership),
            child,
            write_tx: Some(write_tx),
            ack_rx,
            frame_rx: Some(frame_rx),
            writer: Some(writer),
            reader: Some(reader),
            poisoned: false,
            reaped: false,
        })
    }
}

struct WriteRequest {
    frame: Vec<u8>,
    ack: bool,
}

/// Owned stdio transport over one spawned LSP server process.
pub struct StdioLspTransport {
    claim: ProcessClaim,
    child: Child,
    write_tx: Option<SyncSender<WriteRequest>>,
    ack_rx: Receiver<bool>,
    frame_rx: Option<Receiver<Result<Vec<u8>, ()>>>,
    writer: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    poisoned: bool,
    reaped: bool,
}

impl StdioLspTransport {
    fn send_with_deadline(
        &mut self,
        frame: &[u8],
        context: SendContext,
    ) -> Result<(), TransportError> {
        if self.poisoned || self.reaped {
            return Err(TransportError::WriteFailed);
        }
        let remaining = context.remaining();
        if remaining.is_zero() {
            return Err(TransportError::WriteDeadlineExceeded);
        }
        let Some(write_tx) = self.write_tx.as_ref() else {
            return Err(TransportError::WriteFailed);
        };
        if write_tx
            .send(WriteRequest {
                frame: frame.to_vec(),
                ack: true,
            })
            .is_err()
        {
            self.poisoned = true;
            return Err(TransportError::WriteFailed);
        }
        match self.ack_rx.recv_timeout(remaining) {
            Ok(true) => Ok(()),
            Ok(false) | Err(RecvTimeoutError::Disconnected) => {
                self.poisoned = true;
                Err(TransportError::WriteFailed)
            }
            Err(RecvTimeoutError::Timeout) => {
                self.poisoned = true;
                Err(TransportError::WriteDeadlineExceeded)
            }
        }
    }

    fn reap_with_deadline(&mut self, budget: Duration) -> Result<(), TransportError> {
        if self.reaped {
            return Ok(());
        }
        if budget.is_zero() {
            return Err(TransportError::CloseOrReapDeadlineExceeded);
        }
        // Closing the writer channel lets the writer thread drain and exit;
        // dropping the frame receiver unblocks a reader stuck on a full queue.
        self.write_tx = None;
        self.frame_rx = None;
        kill_process_group(&mut self.child)?;
        let deadline = Instant::now()
            .checked_add(budget)
            .ok_or(TransportError::CloseOrReapDeadlineExceeded)?;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        return Err(TransportError::CloseOrReapDeadlineExceeded);
                    }
                    std::thread::sleep(REAP_POLL_INTERVAL.min(budget));
                }
                Err(_) => return Err(TransportError::CloseOrReapFailed),
            }
        }
        self.reaped = true;
        // Both threads observe dead pipes/closed channels once the process
        // group is gone; joining proves the pipe handles are released.
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        Ok(())
    }
}

impl OwnedLspTransport for StdioLspTransport {
    fn claim(&self) -> ProcessClaim {
        self.claim
    }

    fn initialize(
        &mut self,
        request_frame: &[u8],
        _codec_limits: CodecLimits,
        context: SendContext,
    ) -> Result<(), TransportError> {
        // The session manager's handshake is write-only: the server's
        // `initialize` response is consumed and discarded by the reader thread.
        self.send_with_deadline(request_frame, context)
    }

    fn send_frame(&mut self, frame: &[u8], context: SendContext) -> Result<(), TransportError> {
        self.send_with_deadline(frame, context)
    }

    fn receive_frame(
        &mut self,
        codec_limits: CodecLimits,
        context: SendContext,
    ) -> Result<Vec<u8>, TransportError> {
        if self.poisoned || self.reaped {
            return Err(TransportError::ReadFailed);
        }
        let remaining = context.remaining();
        if remaining.is_zero() {
            return Err(TransportError::ReadDeadlineExceeded);
        }
        let Some(frame_rx) = self.frame_rx.as_ref() else {
            return Err(TransportError::ReadFailed);
        };
        match frame_rx.recv_timeout(remaining) {
            Ok(Ok(frame)) => {
                if frame.len() > codec_limits.max_frame_bytes {
                    self.poisoned = true;
                    return Err(TransportError::ReadFailed);
                }
                Ok(frame)
            }
            Ok(Err(())) | Err(RecvTimeoutError::Disconnected) => {
                self.poisoned = true;
                Err(TransportError::ReadFailed)
            }
            Err(RecvTimeoutError::Timeout) => Err(TransportError::ReadDeadlineExceeded),
        }
    }

    fn close_and_reap(&mut self, context: SendContext) -> Result<(), TransportError> {
        self.reap_with_deadline(context.remaining())
    }
}

impl Drop for StdioLspTransport {
    fn drop(&mut self) {
        let _ = self.reap_with_deadline(DROP_REAP_BUDGET);
    }
}

fn kill_process_group(child: &mut Child) -> Result<(), TransportError> {
    #[cfg(unix)]
    {
        let pid = i32::try_from(child.id()).map_err(|_| TransportError::CloseOrReapFailed)?;
        // The child was spawned as its own process-group leader; ESRCH means
        // the whole group is already gone, which is the goal state.
        let result = unsafe { libc::killpg(pid, libc::SIGKILL) };
        if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        // Fall through to a direct kill: the group signal can fail if the
        // leader never finished exec, but the child handle remains killable.
    }
    match child.kill() {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        Err(_) => Err(TransportError::CloseOrReapFailed),
    }
}

fn writer_loop(mut stdin: ChildStdin, requests: Receiver<WriteRequest>, acks: SyncSender<bool>) {
    while let Ok(request) = requests.recv() {
        let ok = stdin
            .write_all(&request.frame)
            .and_then(|()| stdin.flush())
            .is_ok();
        if request.ack && acks.send(ok).is_err() {
            return;
        }
        if !ok {
            return;
        }
    }
}

fn reader_loop(
    stdout: ChildStdout,
    codec: CodecLimits,
    frames: SyncSender<Result<Vec<u8>, ()>>,
    replies: SyncSender<WriteRequest>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let decoded = match LspCodec::decode_from(&mut reader, codec) {
            Ok(decoded) => decoded,
            Err(_) => {
                let _ = frames.send(Err(()));
                return;
            }
        };
        let value = decoded.value();
        match (
            value.get("method").and_then(Value::as_str),
            value.get("id"),
        ) {
            (Some("textDocument/publishDiagnostics"), None) => {
                let Ok(frame) = LspCodec::encode(value, codec) else {
                    let _ = frames.send(Err(()));
                    return;
                };
                if frames.send(Ok(frame)).is_err() {
                    return;
                }
            }
            (Some(_), Some(id)) => {
                // Server-to-client request: refuse it so the server's request
                // queue keeps draining without stalling the diagnostics push.
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id.clone(),
                    "error": { "code": -32601, "message": "unsupported by kit shadow client" },
                });
                if let Ok(frame) = LspCodec::encode(&reply, codec) {
                    let _ = replies.try_send(WriteRequest { frame, ack: false });
                }
            }
            // Other notifications, the initialize response, and stray
            // responses carry no shadow diagnostics; drop them.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> NativeLspServerConfig {
        NativeLspServerConfig::new(
            "kit-lsp".to_owned(),
            vec!["--stdio".to_owned()],
            vec!["rust".to_owned(), "toml".to_owned()],
            5_000,
            200,
        )
        .unwrap()
    }

    #[test]
    fn config_bounds_are_enforced() {
        assert_eq!(
            NativeLspServerConfig::new(String::new(), Vec::new(), vec!["rust".into()], 1, 1),
            Err(NativeLspConfigError::EmptyCommand)
        );
        assert_eq!(
            NativeLspServerConfig::new("kit-lsp".into(), Vec::new(), Vec::new(), 1, 1),
            Err(NativeLspConfigError::NoLanguages)
        );
        assert_eq!(
            NativeLspServerConfig::new(
                "kit-lsp".into(),
                Vec::new(),
                vec!["rust".into()],
                0,
                1
            ),
            Err(NativeLspConfigError::WallTimeOutOfBounds)
        );
        assert_eq!(
            NativeLspServerConfig::new(
                "kit-lsp".into(),
                Vec::new(),
                vec!["rust".into()],
                MAX_NATIVE_LSP_WALL_TIME.as_millis() as u64 + 1,
                1
            ),
            Err(NativeLspConfigError::WallTimeOutOfBounds)
        );
        assert_eq!(
            NativeLspServerConfig::new(
                "kit-lsp".into(),
                Vec::new(),
                vec!["rust".into()],
                1,
                MAX_NATIVE_LSP_DIAGNOSTICS + 1
            ),
            Err(NativeLspConfigError::DiagnosticBoundOutOfBounds)
        );
    }

    #[test]
    fn language_matching_accepts_keys_and_extensions() {
        let config = config();
        assert!(config.matches_path("src/main.rs", Some("rust")));
        assert!(config.matches_path("Cargo.toml", None));
        // Mapping an extension to its language key is the caller's job; the
        // raw matcher only accepts the key or the literal extension.
        assert!(!config.matches_path("src/lib.rs", None));
        assert!(!config.matches_path("data.json", Some("json")));
        assert!(!config.matches_path("README", None));
    }
}
