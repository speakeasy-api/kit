use std::{
    ffi::OsString,
    fmt,
    io::{self, Read, Write},
    process::ExitCode,
    time::Instant,
};

use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
use crate::executor::backends::container::limits::bounded_output;
use crate::{
    executor::backends::container::limits::bounded_output_with_bounded_input,
    workspace::edit::format::{
        NATIVE_JSON_VERSION, NATIVE_TEXT_VERSION, RUST_GRAMMAR_VERSION,
        SYNTAX_EXECUTOR_CONTRACT_VERSION, SyntaxRequest, SyntaxStatus,
    },
};

const WORKER_MODE: &str = "--__kit-syntax-worker";
const WORKER_SOURCE_LIMIT: usize = 64 * 1024 * 1024;
const WORKER_OUTPUT_LIMIT: usize = 128;
#[cfg(target_os = "macos")]
const MEMORY_PROBE_MODE: &str = "--__kit-syntax-memory-probe";
#[cfg(target_os = "macos")]
const MEMORY_PROBE_ALLOCATION_MODE: &str = "--__kit-syntax-memory-probe-allocation";
#[cfg(target_os = "macos")]
const MEMORY_PROBE_LIMIT: usize = 64 * 1024 * 1024;
#[cfg(target_os = "macos")]
const MEMORY_PROBE_ALLOCATION: usize = 128 * 1024 * 1024 * 1024;
#[cfg(target_os = "macos")]
const MEMORY_PROBE_DENIED: &[u8] = b"memory-denied";
#[cfg(target_os = "macos")]
const MEMORY_PROBE_ALLOWED: &[u8] = b"memory-allowed";

#[derive(Debug)]
pub enum SyntaxExecutorError {
    Rejected,
    Timeout,
    Unavailable,
}

impl fmt::Display for SyntaxExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Rejected => "syntax executor rejected the request",
            Self::Timeout => "syntax execution exceeded its deadline",
            Self::Unavailable => "syntax executor is unavailable",
        })
    }
}

impl std::error::Error for SyntaxExecutorError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntaxCompletion {
    contract_version: u16,
    status: SyntaxStatus,
    authoritative: bool,
}

impl SyntaxCompletion {
    pub const fn contract_version(self) -> u16 {
        self.contract_version
    }

    pub const fn status(self) -> SyntaxStatus {
        self.status
    }

    pub const fn authoritative(self) -> bool {
        self.authoritative
    }
}

enum Backend {
    Unavailable,
    Worker,
    #[cfg(any(test, debug_assertions))]
    Debug(DebugSyntaxAction),
}

/// Sealed syntax execution service. Callers can select a registered executor,
/// but cannot supply synchronous parser code to the staging thread.
pub struct SyntaxExecutor {
    language: String,
    version: String,
    backend: Backend,
}

impl SyntaxExecutor {
    pub(crate) fn available(&self) -> bool {
        !matches!(self.backend, Backend::Unavailable)
    }

    pub fn production(language: impl Into<String>, version: impl Into<String>) -> Self {
        let language = language.into();
        let version = version.into();
        let backend = production_backend(&language, &version);
        Self {
            language,
            version,
            backend,
        }
    }

    pub fn unavailable(language: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            language: language.into(),
            version: version.into(),
            backend: Backend::Unavailable,
        }
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn debug(
        language: impl Into<String>,
        version: impl Into<String>,
        action: DebugSyntaxAction,
    ) -> Self {
        Self {
            language: language.into(),
            version: version.into(),
            backend: Backend::Debug(action),
        }
    }

    pub(crate) fn execute(
        &mut self,
        request: SyntaxRequest<'_>,
        max_memory_bytes: usize,
        max_output_bytes: usize,
        deadline: Instant,
    ) -> Result<SyntaxCompletion, SyntaxExecutorError> {
        if Instant::now() >= deadline {
            return Err(SyntaxExecutorError::Timeout);
        }
        if request.source().len() > max_memory_bytes || max_output_bytes == 0 {
            return Err(SyntaxExecutorError::Rejected);
        }
        match &mut self.backend {
            Backend::Unavailable => Err(SyntaxExecutorError::Unavailable),
            Backend::Worker => run_worker_process(
                &self.language,
                &self.version,
                request.source(),
                max_memory_bytes.min(WORKER_SOURCE_LIMIT),
                max_memory_bytes,
                max_output_bytes.min(WORKER_OUTPUT_LIMIT),
                deadline,
            ),
            #[cfg(any(test, debug_assertions))]
            Backend::Debug(action) => {
                let status = match action {
                    DebugSyntaxAction::Pass(capture) => {
                        if let Some(capture) = capture {
                            *capture.lock().unwrap() = request.source().to_vec();
                        }
                        SyntaxStatus::Pass
                    }
                    DebugSyntaxAction::Fail => SyntaxStatus::Fail,
                    DebugSyntaxAction::Stuck => {
                        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                        return Err(SyntaxExecutorError::Timeout);
                    }
                    DebugSyntaxAction::GateSecond {
                        calls,
                        entered,
                        release,
                    } => {
                        *calls += 1;
                        if *calls == 2 {
                            entered
                                .send(())
                                .map_err(|_| SyntaxExecutorError::Unavailable)?;
                            release
                                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                                .map_err(|error| match error {
                                    std::sync::mpsc::RecvTimeoutError::Timeout => {
                                        SyntaxExecutorError::Timeout
                                    }
                                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                                        SyntaxExecutorError::Unavailable
                                    }
                                })?;
                        }
                        SyntaxStatus::Pass
                    }
                };
                if Instant::now() >= deadline {
                    return Err(SyntaxExecutorError::Timeout);
                }
                Ok(SyntaxCompletion {
                    contract_version: SYNTAX_EXECUTOR_CONTRACT_VERSION,
                    status,
                    authoritative: true,
                })
            }
        }
    }
}

fn production_backend(language: &str, version: &str) -> Backend {
    if !cfg!(unix) || !supported(language, version) {
        return Backend::Unavailable;
    }
    platform_production_backend()
}

#[cfg(target_os = "macos")]
fn platform_production_backend() -> Backend {
    darwin_production_backend(darwin_memory_limits_enforced())
}

#[cfg(not(target_os = "macos"))]
fn platform_production_backend() -> Backend {
    Backend::Worker
}

#[cfg(target_os = "macos")]
fn darwin_production_backend(memory_limits_enforced: bool) -> Backend {
    if memory_limits_enforced {
        Backend::Worker
    } else {
        Backend::Unavailable
    }
}

#[cfg(target_os = "macos")]
fn darwin_memory_limits_enforced() -> bool {
    static ENFORCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENFORCED.get_or_init(probe_darwin_memory_limits)
}

#[cfg(target_os = "macos")]
fn probe_darwin_memory_limits() -> bool {
    let Ok(executable) = worker_executable() else {
        return false;
    };
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    let Ok(control) = bounded_output(
        &executable,
        [MEMORY_PROBE_ALLOCATION_MODE],
        deadline,
        WORKER_OUTPUT_LIMIT,
    ) else {
        return false;
    };
    if !control.status.success()
        || control.stdout != MEMORY_PROBE_ALLOWED
        || !control.stderr.is_empty()
    {
        return false;
    }
    let Ok(output) = bounded_output_with_bounded_input(
        &executable,
        [MEMORY_PROBE_ALLOCATION_MODE],
        &[],
        0,
        MEMORY_PROBE_LIMIT,
        deadline,
        WORKER_OUTPUT_LIMIT,
    ) else {
        return false;
    };
    output.status.success() && output.stdout == MEMORY_PROBE_DENIED && output.stderr.is_empty()
}

fn supported(language: &str, version: &str) -> bool {
    matches!(
        (language, version),
        ("rust", RUST_GRAMMAR_VERSION)
            | ("json", NATIVE_JSON_VERSION)
            | ("text", NATIVE_TEXT_VERSION)
    )
}

fn run_worker_process(
    language: &str,
    version: &str,
    source: &[u8],
    source_limit: usize,
    memory_limit: usize,
    output_limit: usize,
    deadline: Instant,
) -> Result<SyntaxCompletion, SyntaxExecutorError> {
    if source.len() > source_limit {
        return Err(SyntaxExecutorError::Rejected);
    }
    let executable = worker_executable().map_err(|_| SyntaxExecutorError::Unavailable)?;
    let output = bounded_output_with_bounded_input(
        &executable,
        [WORKER_MODE, language, version],
        source,
        source_limit,
        memory_limit,
        deadline,
        output_limit,
    )
    .map_err(|error| match error.kind() {
        io::ErrorKind::TimedOut => SyntaxExecutorError::Timeout,
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => SyntaxExecutorError::Rejected,
        _ => SyntaxExecutorError::Unavailable,
    })?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(SyntaxExecutorError::Unavailable);
    }
    let result: WorkerResult =
        serde_json::from_slice(&output.stdout).map_err(|_| SyntaxExecutorError::Unavailable)?;
    if Instant::now() >= deadline {
        return Err(SyntaxExecutorError::Timeout);
    }
    if result.contract_version != SYNTAX_EXECUTOR_CONTRACT_VERSION {
        return Err(SyntaxExecutorError::Unavailable);
    }
    Ok(SyntaxCompletion {
        contract_version: result.contract_version,
        status: result.status.into(),
        authoritative: true,
    })
}

fn worker_executable() -> io::Result<std::path::PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_kit") {
        return Ok(path.into());
    }
    std::env::current_exe()
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkerStatus {
    Pass,
    Fail,
}

impl From<WorkerStatus> for SyntaxStatus {
    fn from(value: WorkerStatus) -> Self {
        match value {
            WorkerStatus::Pass => Self::Pass,
            WorkerStatus::Fail => Self::Fail,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkerResult {
    contract_version: u16,
    status: WorkerStatus,
}

#[doc(hidden)]
pub fn worker_main(arguments: &[OsString]) -> Option<ExitCode> {
    #[cfg(target_os = "macos")]
    if arguments.get(1).and_then(|value| value.to_str()) == Some(MEMORY_PROBE_MODE) {
        return Some(if arguments.len() == 2 && probe_darwin_memory_limits() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(78)
        });
    }
    #[cfg(target_os = "macos")]
    if arguments.get(1).and_then(|value| value.to_str()) == Some(MEMORY_PROBE_ALLOCATION_MODE) {
        return Some(memory_probe_allocation(arguments));
    }
    if arguments.get(1).and_then(|value| value.to_str()) != Some(WORKER_MODE) {
        return None;
    }
    let result = if let [_, _, language, version] = arguments
        && let (Some(language), Some(version)) = (language.to_str(), version.to_str())
    {
        worker_parse(language, version, &mut io::stdin().lock())
    } else {
        Err(())
    };
    match result {
        Ok(status) => {
            let result = WorkerResult {
                contract_version: SYNTAX_EXECUTOR_CONTRACT_VERSION,
                status,
            };
            let mut stdout = io::stdout().lock();
            match serde_json::to_writer(&mut stdout, &result)
                .and_then(|()| stdout.flush().map_err(serde_json::Error::io))
            {
                Ok(()) => Some(ExitCode::SUCCESS),
                Err(_) => Some(ExitCode::FAILURE),
            }
        }
        Err(()) => Some(ExitCode::FAILURE),
    }
}

#[cfg(target_os = "macos")]
fn memory_probe_allocation(arguments: &[OsString]) -> ExitCode {
    if arguments.len() != 2 {
        return ExitCode::FAILURE;
    }
    // SAFETY: malloc returns either a uniquely owned allocation or null; the allocation is freed
    // without dereferencing because address-space denial is the capability being tested.
    let allocation = unsafe { libc::malloc(MEMORY_PROBE_ALLOCATION) };
    let result = if allocation.is_null() {
        io::stdout().lock().write_all(MEMORY_PROBE_DENIED)
    } else {
        // SAFETY: allocation came from malloc above and has not been freed.
        unsafe { libc::free(allocation) };
        io::stdout().lock().write_all(MEMORY_PROBE_ALLOWED)
    };
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn worker_parse(language: &str, version: &str, input: &mut impl Read) -> Result<WorkerStatus, ()> {
    if !supported(language, version) {
        return Err(());
    }
    let mut source = Vec::new();
    input
        .take((WORKER_SOURCE_LIMIT + 1) as u64)
        .read_to_end(&mut source)
        .map_err(|_| ())?;
    if source.len() > WORKER_SOURCE_LIMIT {
        return Err(());
    }
    let valid_text = std::str::from_utf8(&source)
        .ok()
        .filter(|_| !source.contains(&0) && coherent_newlines(&source));
    let pass = match (language, version, valid_text) {
        ("rust", RUST_GRAMMAR_VERSION, Some(text)) => syn::parse_file(text).is_ok(),
        ("json", NATIVE_JSON_VERSION, Some(text)) => {
            let mut parser = serde_json::Deserializer::from_str(text);
            serde::de::IgnoredAny::deserialize(&mut parser).is_ok() && parser.end().is_ok()
        }
        ("text", NATIVE_TEXT_VERSION, Some(_)) => true,
        _ => false,
    };
    Ok(if pass {
        WorkerStatus::Pass
    } else {
        WorkerStatus::Fail
    })
}

fn coherent_newlines(bytes: &[u8]) -> bool {
    let mut style = None;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                if style == Some(false) {
                    return false;
                }
                style = Some(true);
                index += 2;
            }
            b'\r' => return false,
            b'\n' => {
                if style == Some(true) {
                    return false;
                }
                style = Some(false);
                index += 1;
            }
            _ => index += 1,
        }
    }
    true
}

#[cfg(any(test, debug_assertions))]
pub(crate) enum DebugSyntaxAction {
    Pass(Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>),
    Fail,
    Stuck,
    GateSecond {
        calls: usize,
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_fully_parses_rust() {
        assert!(matches!(
            worker_parse("rust", RUST_GRAMMAR_VERSION, &mut &b"fn valid() {}\n"[..]),
            Ok(WorkerStatus::Pass)
        ));
        assert!(matches!(
            worker_parse("rust", RUST_GRAMMAR_VERSION, &mut &b"fn invalid(\n"[..]),
            Ok(WorkerStatus::Fail)
        ));
    }

    #[test]
    fn production_parent_rejects_oversize_and_expired_work() {
        assert!(matches!(
            run_worker_process(
                "text",
                NATIVE_TEXT_VERSION,
                b"oversize",
                1,
                WORKER_SOURCE_LIMIT,
                WORKER_OUTPUT_LIMIT,
                Instant::now() + std::time::Duration::from_secs(1),
            ),
            Err(SyntaxExecutorError::Rejected)
        ));
        assert!(matches!(
            run_worker_process(
                "text",
                NATIVE_TEXT_VERSION,
                b"text",
                WORKER_SOURCE_LIMIT,
                WORKER_SOURCE_LIMIT,
                WORKER_OUTPUT_LIMIT,
                Instant::now(),
            ),
            Err(SyntaxExecutorError::Timeout)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_unavailable_fallback_does_not_parse() {
        let path = crate::workspace::edit::ir::RootRelativePath::parse("invalid.rs", 64).unwrap();
        let mut executor = SyntaxExecutor {
            language: "rust".to_owned(),
            version: RUST_GRAMMAR_VERSION.to_owned(),
            backend: darwin_production_backend(false),
        };
        assert!(matches!(
            executor.execute(
                SyntaxRequest::new(&path, b"fn invalid("),
                1024,
                WORKER_OUTPUT_LIMIT,
                Instant::now() + std::time::Duration::from_secs(1),
            ),
            Err(SyntaxExecutorError::Unavailable)
        ));
    }
}
