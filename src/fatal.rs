use std::{
    fs::{self, File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agentkit_loop::LoopError;
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u64 = 1;
const MAX_MESSAGE_BYTES: usize = 2 * 1024;
const MAX_RECORD_BYTES: usize = 32 * 1024;
const MAX_RECORDS_PER_SESSION: usize = 50;
const STALE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);
static NEXT_EVENT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
pub(crate) enum Surface {
    A2a,
    Acp,
    Prompt,
}

impl Surface {
    const fn as_str(self) -> &'static str {
        match self {
            Self::A2a => "a2a",
            Self::Acp => "acp",
            Self::Prompt => "prompt",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct FatalRecord {
    schema_version: u64,
    event_id: String,
    occurred_at_ms: u64,
    kit_version: String,
    session_id: String,
    surface: String,
    kind: String,
    code: String,
    message: String,
}

pub(crate) fn record_loop_error(
    session_id: &str,
    surface: Surface,
    error: &LoopError,
) -> Result<Option<PathBuf>, String> {
    let Some((kind, code, message)) = classify(error) else {
        return Ok(None);
    };
    write_default(session_id, surface, kind, code, &message).map(Some)
}

pub(crate) fn record_runtime_error(
    session_id: &str,
    surface: Surface,
    code: &str,
) -> Result<PathBuf, String> {
    write_default(
        session_id,
        surface,
        "runtime",
        canonical_code(code),
        "runtime failed before the session could continue",
    )
}

fn classify(error: &LoopError) -> Option<(&'static str, &'static str, String)> {
    match error {
        LoopError::Cancelled => None,
        LoopError::Provider(message) => {
            if message.starts_with("openai-subscription ") {
                let code = provider_code(message);
                Some(("provider", code, provider_message(code, message)))
            } else {
                Some((
                    "provider",
                    "provider_error",
                    "provider request failed".into(),
                ))
            }
        }
        LoopError::Tool(_) => Some(("tool", "tool_error", "tool execution failed".into())),
        LoopError::Mutator(_) => Some(("runtime", "mutator_error", "loop mutator failed".into())),
        LoopError::InvalidState(_) => Some((
            "runtime",
            "invalid_state",
            "runtime entered an invalid state".into(),
        )),
        LoopError::Unsupported(_) => Some((
            "runtime",
            "unsupported",
            "runtime operation is unsupported".into(),
        )),
    }
}

fn provider_code(message: &str) -> &'static str {
    if message.contains("stream transport failed") {
        "stream_transport"
    } else if message.contains("request transport failed") {
        "request_transport"
    } else if message.contains("SSE idle timeout") {
        "stream_idle_timeout"
    } else if message.contains("stream closed") {
        "stream_closed"
    } else if message.contains("unauthorized") || message.contains("authentication failed") {
        "authentication"
    } else if message.contains("retry budget")
        || (message.contains("after ") && message.contains("attempts"))
    {
        "retry_exhausted"
    } else if message.contains("transient response failed:") {
        "response_transient"
    } else if message.contains("response failed:") {
        "response_failed"
    } else if message.contains("protocol error:") {
        "protocol_error"
    } else if message.contains("auth worker failed") {
        "credential_error"
    } else if message.contains("HTTP") || message.contains("returned") {
        "http_error"
    } else {
        "provider_error"
    }
}

fn provider_message(code: &str, message: &str) -> String {
    match code {
        "stream_transport" | "request_transport" => format!(
            "openai-subscription {code} (timeout={}, connect={}, request={}, body={}, decode={})",
            message.contains("timeout=true"),
            message.contains("connect=true"),
            message.contains("request=true"),
            message.contains("body=true"),
            message.contains("decode=true"),
        ),
        _ => format!("openai-subscription failed ({code})"),
    }
}

fn bounded(message: &str) -> String {
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut output = String::with_capacity(collapsed.len().min(MAX_MESSAGE_BYTES));
    let mut remaining = collapsed.as_str();
    while let Some(start) = [remaining.find("http://"), remaining.find("https://")]
        .into_iter()
        .flatten()
        .min()
    {
        output.push_str(&remaining[..start]);
        output.push_str("[url]");
        let url = &remaining[start..];
        let end = url.find(char::is_whitespace).unwrap_or(url.len());
        remaining = &url[end..];
    }
    output.push_str(remaining);
    if output.len() > MAX_MESSAGE_BYTES {
        let end = output.floor_char_boundary(MAX_MESSAGE_BYTES);
        output.truncate(end);
    }
    output
}

fn canonical_code(code: &str) -> &str {
    if !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        code
    } else {
        "runtime_error"
    }
}

fn write_default(
    session_id: &str,
    surface: Surface,
    kind: &str,
    code: &str,
    message: &str,
) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| "HOME is unset; cannot store fatal error log".to_owned())?;
    write_in(
        &PathBuf::from(home).join(".kit/errors"),
        session_id,
        surface,
        kind,
        code,
        message,
    )
}

fn write_in(
    base: &Path,
    session_id: &str,
    surface: Surface,
    kind: &str,
    code: &str,
    message: &str,
) -> Result<PathBuf, String> {
    crate::session::validate_id(session_id)?;
    let occurred_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?
        .as_millis() as u64;
    let event_id = format!(
        "e-{occurred_at_ms}-{}-{}",
        std::process::id(),
        NEXT_EVENT.fetch_add(1, Ordering::Relaxed)
    );
    let record = FatalRecord {
        schema_version: SCHEMA_VERSION,
        event_id: event_id.clone(),
        occurred_at_ms,
        kit_version: env!("CARGO_PKG_VERSION").into(),
        session_id: session_id.into(),
        surface: surface.as_str().into(),
        kind: kind.into(),
        code: canonical_code(code).into(),
        message: bounded(message),
    };
    let mut bytes = serde_json::to_vec_pretty(&record)
        .map_err(|error| format!("could not encode fatal error log: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("fatal error log exceeds size limit".into());
    }

    let directory = base.join(session_id);
    create_private_directory(base)?;
    create_private_directory(&directory)?;
    let path = directory.join(format!("{event_id}.json"));
    let temporary = directory.join(format!(".{event_id}.tmp"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("could not create fatal error log: {error}"))?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_data()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("could not write fatal error log: {error}"));
    }
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("could not commit fatal error log: {error}")
    })?;
    #[cfg(unix)]
    File::open(&directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("could not sync fatal error log directory: {error}"))?;
    prune(&directory);
    Ok(path)
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("could not create fatal error log directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure fatal error log directory: {error}"))?;
    }
    Ok(())
}

fn prune(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "tmp") {
                let stale = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|age| age >= STALE_TEMP_AGE);
                if stale {
                    let _ = fs::remove_file(path);
                }
                None
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                Some(path)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    paths.sort_by_key(|path| event_order(path).unwrap_or((u64::MAX, u32::MAX, u64::MAX)));
    let remove = paths.len().saturating_sub(MAX_RECORDS_PER_SESSION);
    for path in paths.into_iter().take(remove) {
        let _ = fs::remove_file(path);
    }
}

fn event_order(path: &Path) -> Option<(u64, u32, u64)> {
    let stem = path.file_stem()?.to_str()?;
    let mut fields = stem.split('-');
    if fields.next()? != "e" {
        return None;
    }
    let timestamp = fields.next()?.parse().ok()?;
    let pid = fields.next()?.parse().ok()?;
    let counter = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some((timestamp, pid, counter))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use agentkit_loop::LoopError;

    use super::{
        FatalRecord, MAX_RECORDS_PER_SESSION, Surface, bounded, classify, event_order,
        record_loop_error, write_in,
    };

    #[test]
    fn writes_versioned_session_scoped_record() {
        let root = tempfile::tempdir().unwrap();
        let path = write_in(
            root.path(),
            "session-1",
            Surface::Prompt,
            "provider",
            "stream_transport",
            "openai-subscription stream transport failed",
        )
        .unwrap();
        assert_eq!(path.parent().unwrap(), root.path().join("session-1"));
        let record: FatalRecord = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(record.schema_version, 1);
        assert_eq!(record.session_id, "session-1");
        assert_eq!(record.surface, "prompt");
        assert_eq!(record.code, "stream_transport");
    }

    #[test]
    fn provider_records_exclude_response_content_but_keep_transport_flags() {
        let (_, code, message) = classify(&LoopError::Provider(
            "openai-subscription returned 400: secret prompt https://example.invalid/token".into(),
        ))
        .unwrap();
        assert_eq!(code, "http_error");
        assert_eq!(message, "openai-subscription failed (http_error)");
        assert!(!message.contains("secret"));

        let (_, code, message) = classify(&LoopError::Provider(
            "openai-subscription stream transport failed (timeout=true, connect=false, request=false, body=true, decode=false)".into(),
        ))
        .unwrap();
        assert_eq!(code, "stream_transport");
        assert!(message.contains("timeout=true"));
        assert!(message.contains("body=true"));
    }

    #[test]
    fn provider_records_keep_safe_failure_categories() {
        for (error, expected_code) in [
            (
                "openai-subscription transient response failed: error/server_error",
                "response_transient",
            ),
            (
                "openai-subscription response failed: invalid_request_error/secret_code",
                "response_failed",
            ),
            (
                "openai-subscription protocol error: secret response at https://example.invalid",
                "protocol_error",
            ),
            (
                "openai-subscription response exceeded retry budget",
                "retry_exhausted",
            ),
            ("openai-subscription auth worker failed", "credential_error"),
        ] {
            let (_, code, message) =
                classify(&LoopError::Provider(error.to_owned())).expect("provider error");
            assert_eq!(code, expected_code);
            assert_eq!(
                message,
                format!("openai-subscription failed ({expected_code})")
            );
            assert!(!message.contains("secret"));
            assert!(!message.contains("example.invalid"));
        }
    }

    #[test]
    fn cancellation_is_not_recorded() {
        let result = record_loop_error("session-1", Surface::Acp, &LoopError::Cancelled).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn bounds_messages_and_removes_urls() {
        let message = format!(
            "failed (https://example.invalid/secret) and prefixhttp://other.invalid {}",
            "x".repeat(4096)
        );
        let bounded = bounded(&message);
        assert!(!bounded.contains("example.invalid"));
        assert!(!bounded.contains("other.invalid"));
        assert!(bounded.len() <= 2 * 1024);
    }

    #[test]
    fn retains_at_most_fifty_records_per_session() {
        let root = tempfile::tempdir().unwrap();
        for _ in 0..(MAX_RECORDS_PER_SESSION + 5) {
            write_in(
                root.path(),
                "session-1",
                Surface::Prompt,
                "runtime",
                "runtime_error",
                "failed",
            )
            .unwrap();
        }
        let records = fs::read_dir(root.path().join("session-1"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "json")
            })
            .count();
        assert_eq!(records, MAX_RECORDS_PER_SESSION);
    }

    #[test]
    fn event_names_sort_by_numeric_time_pid_and_counter() {
        let low = std::path::Path::new("e-100-20-2.json");
        let high = std::path::Path::new("e-100-20-10.json");
        assert!(event_order(low) < event_order(high));
    }

    #[cfg(unix)]
    #[test]
    fn files_and_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let path = write_in(
            root.path(),
            "session-1",
            Surface::Acp,
            "runtime",
            "runtime_error",
            "failed",
        )
        .unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(root.path().join("session-1"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
