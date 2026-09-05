use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::resilient_fs as fs;
use agentkit_loop::LoopError;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u64 = 2;
const MAX_MESSAGE_BYTES: usize = 2 * 1024;
const MAX_RECORD_BYTES: usize = 32 * 1024;
const MAX_RECORDS_PER_SESSION: usize = 50;
const MAX_DIAGNOSTIC_BYTES: usize = 4 * 1024;
const MAX_SOURCE_CHAIN: usize = 8;
const MAX_RESPONSE_REQUEST_ID_BYTES: usize = 128;
const DIAGNOSTIC_MARKER: &str = "\n[kit-internal-transport-v1:";
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostics: Option<TransportDiagnostics>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransportStage {
    Request,
    Stream,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransportDiagnostics {
    stage: TransportStage,
    retryable: bool,
    attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_request_id: Option<String>,
    reqwest: ReqwestDiagnostics,
    source_chain: Vec<TransportSource>,
    source_chain_unknown: bool,
    source_chain_truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReqwestDiagnostics {
    timeout: bool,
    connect: bool,
    request: bool,
    body: bool,
    decode: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TransportSource {
    Hyper {
        parse: bool,
        user: bool,
        canceled: bool,
        closed: bool,
        incomplete_message: bool,
        body_write_aborted: bool,
        shutdown: bool,
        timeout: bool,
    },
    H2 {
        io: bool,
        go_away: bool,
        reset: bool,
        remote: bool,
        library: bool,
        reason: H2Reason,
        #[serde(skip_serializing_if = "Option::is_none")]
        io_error: Option<IoDiagnostics>,
    },
    Io {
        classification: IoClassification,
        #[serde(skip_serializing_if = "Option::is_none")]
        os_code: Option<i32>,
    },
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum H2Reason {
    None,
    NoError,
    ProtocolError,
    InternalError,
    FlowControlError,
    SettingsTimeout,
    StreamClosed,
    FrameSizeError,
    RefusedStream,
    Cancel,
    CompressionError,
    ConnectError,
    EnhanceYourCalm,
    InadequateSecurity,
    Http11Required,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IoDiagnostics {
    classification: IoClassification,
    #[serde(skip_serializing_if = "Option::is_none")]
    os_code: Option<i32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum IoClassification {
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    NotConnected,
    BrokenPipe,
    TimedOut,
    UnexpectedEof,
    WouldBlock,
    Interrupted,
    Other,
    Unknown,
}

pub(crate) fn render_loop_error(error: &LoopError) -> String {
    match error {
        LoopError::Provider(message) => {
            let (message, _) = split_diagnostics(message);
            format!("provider error: {message}")
        }
        _ => error.to_string(),
    }
}

impl TransportDiagnostics {
    fn valid(&self) -> bool {
        self.attempt > 0
            && self.attempt <= 1_000
            && self.source_chain.len() <= MAX_SOURCE_CHAIN
            && self
                .response_request_id
                .as_deref()
                .is_none_or(valid_response_request_id)
            && self.source_chain_unknown
                == self
                    .source_chain
                    .iter()
                    .any(|source| matches!(source, TransportSource::Unknown))
            && self.source_chain.iter().all(|source| match source {
                TransportSource::H2 { io, io_error, .. } => *io == io_error.is_some(),
                _ => true,
            })
    }
}

fn valid_response_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RESPONSE_REQUEST_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn split_diagnostics(message: &str) -> (&str, Option<TransportDiagnostics>) {
    let Some(marker) = message.find(DIAGNOSTIC_MARKER) else {
        return (message, None);
    };
    let plain = &message[..marker];
    let suffix = &message[marker + DIAGNOSTIC_MARKER.len()..];
    if suffix.contains(DIAGNOSTIC_MARKER) || !suffix.ends_with(']') {
        return (plain, None);
    }
    let encoded = &suffix[..suffix.len() - 1];
    if encoded.is_empty() || encoded.len() > MAX_DIAGNOSTIC_BYTES.saturating_mul(2) {
        return (plain, None);
    }
    let Ok(decoded) = URL_SAFE_NO_PAD.decode(encoded) else {
        return (plain, None);
    };
    if decoded.len() > MAX_DIAGNOSTIC_BYTES {
        return (plain, None);
    }
    let Ok(diagnostics) = serde_json::from_slice::<TransportDiagnostics>(&decoded) else {
        return (plain, None);
    };
    if !diagnostics.valid() {
        return (plain, None);
    }
    (plain, Some(diagnostics))
}

pub(crate) fn record_loop_error(
    session_id: &str,
    surface: Surface,
    error: &LoopError,
) -> Result<Option<PathBuf>, String> {
    let Some((kind, code, message, diagnostics)) = classify(error) else {
        return Ok(None);
    };
    write_default(
        session_id,
        surface,
        kind,
        code,
        &message,
        diagnostics.as_ref(),
    )
    .map(Some)
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
        None,
    )
}

fn classify(
    error: &LoopError,
) -> Option<(
    &'static str,
    &'static str,
    String,
    Option<TransportDiagnostics>,
)> {
    match error {
        LoopError::Cancelled => None,
        LoopError::Provider(message) => {
            let (message, diagnostics) = split_diagnostics(message);
            if message.starts_with("openai-subscription ") {
                let code = provider_code(message);
                Some((
                    "provider",
                    code,
                    provider_message(code, message),
                    diagnostics,
                ))
            } else {
                // TODO(agentkit): AgentKit 0.10 flattens OpenAI Responses status, transport,
                // and protocol failures into LoopError::Provider(String). Keep this generic until
                // the terminal API exposes a stable typed classification; do not parse its display.
                Some((
                    "provider",
                    "provider_error",
                    "provider request failed".into(),
                    None,
                ))
            }
        }
        LoopError::Tool(_) => Some(("tool", "tool_error", "tool execution failed".into(), None)),
        LoopError::Mutator(_) => Some((
            "runtime",
            "mutator_error",
            "loop mutator failed".into(),
            None,
        )),
        LoopError::InvalidState(_) => Some((
            "runtime",
            "invalid_state",
            "runtime entered an invalid state".into(),
            None,
        )),
        LoopError::Unsupported(_) => Some((
            "runtime",
            "unsupported",
            "runtime operation is unsupported".into(),
            None,
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
    diagnostics: Option<&TransportDiagnostics>,
) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| "HOME is unset; cannot store fatal error log".to_owned())?;
    write_in_with_diagnostics(
        &PathBuf::from(home).join(".kit/errors"),
        session_id,
        surface,
        kind,
        code,
        message,
        diagnostics,
    )
}

#[cfg(test)]
fn write_in(
    base: &Path,
    session_id: &str,
    surface: Surface,
    kind: &str,
    code: &str,
    message: &str,
) -> Result<PathBuf, String> {
    write_in_with_diagnostics(base, session_id, surface, kind, code, message, None)
}

fn write_in_with_diagnostics(
    base: &Path,
    session_id: &str,
    surface: Surface,
    kind: &str,
    code: &str,
    message: &str,
    diagnostics: Option<&TransportDiagnostics>,
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
        diagnostics: diagnostics.filter(|value| value.valid()).cloned(),
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
    fs::replace_private(&path, &bytes)
        .map_err(|error| format!("could not retain fatal error log: {error}"))?;
    prune(&directory);
    Ok(path)
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    fs::create_private_dir_all(path)
        .map_err(|error| format!("could not create fatal error log directory: {error}"))
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
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;

    use super::{
        DIAGNOSTIC_MARKER, FatalRecord, H2Reason, IoClassification, MAX_DIAGNOSTIC_BYTES,
        MAX_RECORDS_PER_SESSION, ReqwestDiagnostics, Surface, TransportDiagnostics,
        TransportSource, TransportStage, bounded, classify, event_order, record_loop_error,
        render_loop_error, split_diagnostics, write_in, write_in_with_diagnostics,
    };

    fn append_diagnostics(message: String, diagnostics: &TransportDiagnostics) -> String {
        let encoded = serde_json::to_vec(diagnostics).unwrap();
        assert!(encoded.len() <= MAX_DIAGNOSTIC_BYTES);
        format!(
            "{message}{DIAGNOSTIC_MARKER}{}]",
            URL_SAFE_NO_PAD.encode(encoded)
        )
    }

    fn sample_diagnostics() -> TransportDiagnostics {
        TransportDiagnostics {
            stage: TransportStage::Stream,
            retryable: true,
            attempt: 2,
            response_request_id: Some("req_safe-123".to_owned()),
            reqwest: ReqwestDiagnostics {
                timeout: false,
                connect: false,
                request: false,
                body: false,
                decode: true,
            },
            source_chain: vec![
                TransportSource::Hyper {
                    parse: false,
                    user: false,
                    canceled: false,
                    closed: false,
                    incomplete_message: true,
                    body_write_aborted: false,
                    shutdown: false,
                    timeout: false,
                },
                TransportSource::H2 {
                    io: false,
                    go_away: false,
                    reset: true,
                    remote: true,
                    library: false,
                    reason: H2Reason::Cancel,
                    io_error: None,
                },
                TransportSource::Io {
                    classification: IoClassification::ConnectionReset,
                    os_code: Some(54),
                },
                TransportSource::Unknown,
            ],
            source_chain_unknown: true,
            source_chain_truncated: true,
        }
    }

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
        assert_eq!(record.schema_version, 2);
        assert_eq!(record.session_id, "session-1");
        assert_eq!(record.surface, "prompt");
        assert_eq!(record.code, "stream_transport");
    }

    #[test]
    fn schema_v1_records_remain_readable() {
        let record: FatalRecord = serde_json::from_value(json!({
            "schema_version": 1,
            "event_id": "e-1-2-3",
            "occurred_at_ms": 1,
            "kit_version": "0.1.82",
            "session_id": "session-1",
            "surface": "prompt",
            "kind": "provider",
            "code": "stream_transport",
            "message": "openai-subscription failed"
        }))
        .unwrap();

        assert_eq!(record.schema_version, 1);
        assert!(record.diagnostics.is_none());
    }

    #[test]
    fn structured_transport_diagnostics_are_allowlisted_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        let diagnostics = sample_diagnostics();
        let path = write_in_with_diagnostics(
            root.path(),
            "session-1",
            Surface::Prompt,
            "provider",
            "stream_transport",
            "openai-subscription stream_transport",
            Some(&diagnostics),
        )
        .unwrap();
        let encoded = fs::read_to_string(path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(value["schema_version"], 2);
        assert_eq!(value["diagnostics"]["response_request_id"], "req_safe-123");
        assert_eq!(value["diagnostics"]["stage"], "stream");
        assert_eq!(value["diagnostics"]["retryable"], true);
        assert_eq!(value["diagnostics"]["attempt"], 2);
        assert_eq!(value["diagnostics"]["source_chain"][0]["kind"], "hyper");
        assert_eq!(value["diagnostics"]["source_chain"][1]["reason"], "cancel");
        assert_eq!(value["diagnostics"]["source_chain"][2]["os_code"], 54);
        assert_eq!(value["diagnostics"]["source_chain"][3]["kind"], "unknown");
        assert_eq!(value["diagnostics"]["source_chain_truncated"], true);
        for forbidden in ["https://", "authorization", "credential", "response_body"] {
            assert!(!encoded.to_ascii_lowercase().contains(forbidden));
        }
        assert!(encoded.len() < super::MAX_RECORD_BYTES);
    }

    #[test]
    fn diagnostics_marker_decoder_is_strict() {
        let base = "openai-subscription stream transport failed".to_owned();
        let marked = append_diagnostics(base.clone(), &sample_diagnostics());
        let (plain, diagnostics) = split_diagnostics(&marked);
        assert_eq!(plain, base);
        assert!(diagnostics.is_some());

        let (_, code, message, diagnostics) =
            classify(&LoopError::Provider(marked.clone())).unwrap();
        assert_eq!(code, "stream_transport");
        assert!(!message.contains(DIAGNOSTIC_MARKER));
        assert!(message.len() < 256);
        assert!(diagnostics.is_some());

        for malformed in [
            format!("{marked} trailing"),
            format!("{marked}{DIAGNOSTIC_MARKER}e30]"),
            format!("{base}{DIAGNOSTIC_MARKER}not-base64!]"),
            format!(
                "{base}{DIAGNOSTIC_MARKER}{}]",
                "a".repeat(MAX_DIAGNOSTIC_BYTES * 2 + 1)
            ),
        ] {
            assert!(split_diagnostics(&malformed).1.is_none(), "{malformed}");
        }

        let mut value = serde_json::to_value(sample_diagnostics()).unwrap();
        value["peer_debug"] = json!("secret https://example.invalid");
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap());
        let unknown = format!("{base}{DIAGNOSTIC_MARKER}{encoded}]");
        assert!(split_diagnostics(&unknown).1.is_none());
    }

    #[test]
    fn provider_records_exclude_response_content_but_keep_transport_flags() {
        let (_, code, message, _) = classify(&LoopError::Provider(
            "openai-subscription returned 400: secret prompt https://example.invalid/token".into(),
        ))
        .unwrap();
        assert_eq!(code, "http_error");
        assert_eq!(message, "openai-subscription failed (http_error)");
        assert!(!message.contains("secret"));

        let (_, code, message, _) = classify(&LoopError::Provider(
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
            let (_, code, message, _) =
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
    fn erased_agentkit_provider_errors_use_sound_generic_fallback() {
        let (_, code, message, diagnostics) = classify(&LoopError::Provider(
            "OpenAI Responses returned HTTP 429 Too Many Requests".into(),
        ))
        .unwrap();
        assert_eq!(code, "provider_error");
        assert_eq!(message, "provider request failed");
        assert!(diagnostics.is_none());
    }

    #[test]
    fn public_error_rendering_strips_internal_diagnostics() {
        let message = append_diagnostics(
            "openai-subscription stream transport failed".to_owned(),
            &sample_diagnostics(),
        );
        let rendered = render_loop_error(&LoopError::Provider(message));
        assert_eq!(
            rendered,
            "provider error: openai-subscription stream transport failed"
        );
        assert!(!rendered.contains("kit-internal-transport"));
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
