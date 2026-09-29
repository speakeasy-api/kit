//! Single-owner reconnect state. Only initialize and same-session resume retry.
use super::*;
use agent_client_protocol::schema::v2::UpdateSessionNotification;
use std::time::{Duration, Instant};

const PREFIX: &str = "kit.gateway.internal/";
const MAX_PENDING: usize = 128;
const META: &str = "kit/gatewayRecovery";
type Error = Box<dyn std::error::Error>;
type Lines = tokio::sync::mpsc::Receiver<(Instant, io::Result<String>)>;

fn emit(output: &mut impl io::Write, value: &Value) -> Result<(), Error> {
    writeln!(output, "{value}")?;
    output.flush()?;
    Ok(())
}
fn entries(value: &Value) -> &[Value] {
    match value {
        Value::Array(values) => values,
        _ => std::slice::from_ref(value),
    }
}
fn reserved(id: &Value) -> bool {
    id.as_str().is_some_and(|id| id.starts_with(PREFIX))
}
fn reject(
    output: &mut impl io::Write,
    value: &Value,
    reason: &str,
    session: &Option<String>,
) -> Result<(), Error> {
    for value in entries(value) {
        if value.get("method").is_some() && value.get("id").is_some() {
            emit(
                output,
                &object([
                    ("jsonrpc", "2.0".into()),
                    ("id", value["id"].clone()),
                    (
                        "error",
                        object([
                            ("code", (-32000).into()),
                            (
                                "message",
                                "Gateway unavailable; inspect session before resubmitting".into(),
                            ),
                            (
                                "data",
                                object([
                                    ("reason", reason.into()),
                                    ("method", value["method"].clone()),
                                    (
                                        "sessionId",
                                        session.clone().map_or(Value::Null, Value::String),
                                    ),
                                ]),
                            ),
                        ]),
                    ),
                ]),
            )?;
        }
    }
    Ok(())
}
fn notice(
    output: &mut impl io::Write,
    session: &str,
    epoch: u64,
    kind: &str,
    attempt: u32,
    message: &str,
    bits: Value,
) -> Result<(), Error> {
    let mut meta = object([
        ("epoch", epoch.into()),
        ("kind", kind.into()),
        ("attempt", attempt.into()),
        ("maxAttempts", 5.into()),
        ("message", message.into()),
    ]);
    if let Some(bits) = bits.as_object() {
        for (key, value) in bits {
            meta[key] = value.clone();
        }
    }
    emit(
        output,
        &object([
            ("jsonrpc", "2.0".into()),
            ("method", "session/update".into()),
            (
                "params",
                object([
                    ("sessionId", session.into()),
                    (
                        "update",
                        object([
                            ("sessionUpdate", "notice".into()),
                            (
                                "severity",
                                (if kind == "failed" { "error" } else { "info" }).into(),
                            ),
                            ("title", "Gateway recovery".into()),
                            ("description", message.into()),
                        ]),
                    ),
                    ("_meta", object([(META, meta)])),
                ]),
            ),
        ]),
    )
}
fn annotate(value: &mut Value, session: &str, epoch: u64, kind: &str) {
    if value["method"] == "session/update"
        && value["params"]["sessionId"] == session
        && let Some(params) = value["params"].as_object_mut()
    {
        let meta = params.entry("_meta").or_insert_with(|| object([]));
        if !meta.is_object() {
            *meta = object([]);
        }
        meta[META] = object([("epoch", epoch.into()), ("kind", kind.into())]);
    }
}

fn snapshot(result: &Value, no_replay: bool) -> Option<Value> {
    let bits = &result["_meta"]["kit/gateway"];
    if bits["stateSnapshot"] != true
        || bits["configSnapshot"] != true
        || bits["historyAvailable"].as_bool()? == no_replay
    {
        return None;
    }
    Some(object([
        ("stateSnapshot", true.into()),
        ("configSnapshot", true.into()),
        ("historyAvailable", (!no_replay).into()),
    ]))
}
fn internal(epoch: u64, method: &str, params: Value) -> Value {
    object([
        ("jsonrpc", "2.0".into()),
        ("id", format!("{PREFIX}{epoch}/{method}").into()),
        ("method", method.into()),
        ("params", params),
    ])
}
fn send(channel: &mut BoundedChannel, value: &Value) -> Result<(), Error> {
    channel
        .tx
        .try_send(TransportFrame::parse_json(&value.to_string()))
        .map_err(|_| "Gateway transport unavailable; submission outcome may be unknown".into())
}

fn terminal_reason(value: &Value) -> Option<&str> {
    let data = if value.get("error").is_some() {
        &value["error"]["data"]
    } else if value["method"] == "session/update"
        && value["params"]["update"]["sessionUpdate"] == "_gateway_controller"
    {
        &value["params"]["update"]["_meta"]["kit/gateway"]
    } else {
        return None;
    };
    if data["terminal"] != true {
        return None;
    }
    Some(match data["reason"].as_str() {
        Some(
            reason @ ("controller_replaced"
            | "replay_unavailable"
            | "root_denied"
            | "protocol"
            | "conflict"
            | "session_unavailable"),
        ) => reason,
        _ => "protocol",
    })
}

// SDK 423ba's bounded transport exposes status only in Error.data diagnostics.
// Match its exact wrappers and canonical status phrase. The bounded path never
// includes response bodies here; do not accept legacy wrappers or body suffixes.
fn diagnostic_status(error: &agent_client_protocol::Error) -> Option<u16> {
    let data = error.data.as_ref()?.as_str()?;
    let http = [
        "bounded HTTP: initialize HTTP ",
        "bounded HTTP: POST HTTP ",
        "bounded HTTP: SSE HTTP ",
    ]
    .iter()
    .find_map(|prefix| data.strip_prefix(prefix))?;
    let (status, reason) = http.split_once(' ')?;
    if status.len() != 3 || !status.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let status: u16 = status.parse().ok()?;
    let canonical = reqwest::StatusCode::from_u16(status)
        .ok()?
        .canonical_reason()?;
    (reason == canonical).then_some(status)
}

pub(super) async fn run(
    remote: super::super::Remote,
    root: PathBuf,
    mut lines: Lines,
    mut output: impl io::Write,
) -> Result<(), Error> {
    root.to_str().ok_or("remote root must be valid UTF-8")?;
    let mut session = remote.session.clone();
    let mut attachment: Option<String> = None;
    let mut recovery_id = String::new();
    let mut initial = true;
    let mut resumed = None;
    let mut initialize: Option<Value> = None;
    let mut initialized = false;
    let mut pending: Vec<Value> = Vec::new();
    let mut epoch = 1;
    let mut attempt = 0;
    let mut recovery_start = None;
    let mut gate = None;
    let mut terminal: Option<String> = None;
    loop {
        if let Some(message) = terminal.take() {
            for request in pending.drain(..) {
                reject(&mut output, &request, "outcome_unknown", &session)?;
            }
            if let Some(session) = &session {
                notice(
                    &mut output,
                    session,
                    epoch,
                    "failed",
                    attempt,
                    &message,
                    Value::Null,
                )?;
            }
            return Err(message.into());
        }
        let recovering = recovery_start.is_some();
        if recovering {
            if attempt >= 5
                || recovery_start
                    .is_some_and(|start: Instant| start.elapsed() >= Duration::from_secs(30))
            {
                terminal = Some("Recovery exhausted; submission outcome unknown. Inspect with kit gateway list; restart with explicit --remote-session (or --remote-no-replay if history unavailable).".into());
                continue;
            }
            attempt += 1;
            if session.is_some() {
                epoch += 1;
            }
            if let Some(session) = &session {
                notice(
                    &mut output,
                    session,
                    epoch,
                    "begin",
                    attempt,
                    "Reconnecting; previous submissions and notifications may have unknown outcomes",
                    object([("delayMs", (250u64 << (attempt - 1)).into())]),
                )?;
            }
            let remaining = recovery_start.map_or(Duration::ZERO, |start: Instant| {
                Duration::from_secs(30).saturating_sub(start.elapsed())
            });
            let delay =
                tokio::time::sleep(Duration::from_millis(250u64 << (attempt - 1)).min(remaining));
            tokio::pin!(delay);
            loop {
                let line = std::pin::pin!(lines.recv());
                match select(&mut delay, line).await {
                    Either::Left(_) => break,
                    Either::Right((Some((_, line)), _)) => reject(
                        &mut output,
                        &serde_json::from_str::<Value>(&line?)?,
                        "not_sent",
                        &session,
                    )?,
                    Either::Right((None, _)) => return Ok(()),
                }
            }
        }
        let (mut channel, mut transport) = client(&remote)?.into_bounded_channel_and_future();
        let remaining = recovery_start.map_or(Duration::from_secs(30), |start: Instant| {
            Duration::from_secs(30).saturating_sub(start.elapsed())
        });
        let deadline = tokio::time::sleep(Duration::from_secs(5).min(remaining));
        tokio::pin!(deadline);
        let mut phase = if recovering { "initialize" } else { "live" };
        let mut expected = Value::Null;
        let mut saw_state = false;
        let mut saw_config = false;
        let mut replay_bytes = 0usize;
        let mut replay_events = 0usize;
        if recovering {
            let request = internal(
                epoch,
                "initialize",
                initialize.clone().ok_or("Missing initialize template")?,
            );
            expected = request["id"].clone();
            send(&mut channel, &request)?;
        }
        let mut lost = false;
        let mut input_open = true;
        let mut frames_open = true;
        while !lost {
            enum Event {
                Frame(Option<ChargedFrame>),
                Driver(Result<(), agent_client_protocol::Error>),
                Line(Option<(Instant, io::Result<String>)>),
                Timeout,
            }
            let event = {
                // select polls its left branch first. Keep timeout > driver >
                // inbound frame > stdin priority; disabled branches never poll
                // their underlying receiver or deadline.
                let timeout = std::pin::pin!(async {
                    if phase != "live" {
                        deadline.as_mut().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                });
                let frame = std::pin::pin!(async {
                    if frames_open {
                        channel.rx.next().await
                    } else {
                        std::future::pending().await
                    }
                });
                let line = std::pin::pin!(async {
                    if input_open {
                        lines.recv().await
                    } else {
                        std::future::pending().await
                    }
                });
                match select(timeout, select(&mut transport, select(frame, line))).await {
                    Either::Left(_) => Event::Timeout,
                    Either::Right((Either::Left((result, _)), _)) => Event::Driver(result),
                    Either::Right((Either::Right((Either::Left((frame, _)), _)), _)) => {
                        Event::Frame(frame)
                    }
                    Either::Right((Either::Right((Either::Right((line, _)), _)), _)) => {
                        Event::Line(line)
                    }
                }
            };
            match event {
                Event::Line(None) => {
                    if phase != "live" {
                        return Ok(());
                    }
                    input_open = false;
                    channel.tx.close_channel();
                }
                Event::Line(Some((admitted, line))) => {
                    let value: Value = serde_json::from_str(&line?)?;
                    if phase != "live"
                        || resumed.is_some()
                        || gate.is_some_and(|gate| admitted <= gate)
                    {
                        reject(&mut output, &value, "not_sent", &session)?;
                        continue;
                    }
                    if entries(&value).iter().any(|v| v["method"] == "initialize")
                        && (initialized || entries(&value).len() != 1)
                    {
                        reject(&mut output, &value, "not_sent", &session)?;
                        continue;
                    }
                    if entries(&value).iter().any(|v| reserved(&v["id"])) {
                        reject(&mut output, &value, "reserved_id", &session)?;
                        continue;
                    }
                    let requests = entries(&value)
                        .iter()
                        .filter(|v| v.get("id").is_some() && v.get("method").is_some())
                        .count();
                    let duplicate_batch = entries(&value).iter().enumerate().any(|(index, v)| {
                        v.get("id").is_some()
                            && v.get("method").is_some()
                            && entries(&value)[..index]
                                .iter()
                                .any(|p| p["id"] == v["id"] && p.get("method").is_some())
                    });
                    if duplicate_batch
                        || pending.len() + requests > MAX_PENDING
                        || entries(&value).iter().any(|v| {
                            v.get("method").is_some()
                                && v.get("id").is_some()
                                && pending.iter().any(|p| p["id"] == v["id"])
                        })
                    {
                        reject(&mut output, &value, "not_sent", &session)?;
                        continue;
                    }
                    for entry in entries(&value) {
                        if entry["method"] == "initialize" {
                            initialize = Some(entry["params"].clone());
                        }
                        if entry.get("id").is_some() && entry.get("method").is_some() {
                            pending.push(object([
                                ("id", entry["id"].clone()),
                                ("method", entry["method"].clone()),
                            ]));
                        }
                    }
                    let mut frame = TransportFrame::parse_json(&value.to_string());
                    rewrite(&mut frame, &root, &mut session, &mut initial, &mut resumed)?;
                    if let Some((_, id)) = &resumed {
                        deadline
                            .as_mut()
                            .reset(tokio::time::Instant::now() + Duration::from_secs(5));
                        phase = "initial_replay";
                        notice(
                            &mut output,
                            id,
                            epoch,
                            "begin",
                            0,
                            "Attaching durable session",
                            Value::Null,
                        )?;
                        if remote.no_replay {
                            let mut value: Value = serde_json::from_str(&frame.to_json()?)?;
                            let values = match &mut value {
                                Value::Array(values) => values.as_mut_slice(),
                                value => std::slice::from_mut(value),
                            };
                            for value in values {
                                if value["method"] == "session/resume"
                                    && let Some(params) = value["params"].as_object_mut()
                                {
                                    params.remove("replayFrom");
                                }
                            }
                            frame = TransportFrame::parse_json(&value.to_string());
                        }
                    }
                    if entries(&value).iter().any(|v| v["method"] == "initialize") {
                        phase = "initial_initialize";
                        deadline
                            .as_mut()
                            .reset(tokio::time::Instant::now() + Duration::from_secs(5));
                    }
                    if channel.tx.try_send(frame).is_err() {
                        lost = true;
                    }
                }
                Event::Frame(Some(charged)) => {
                    let value: Value = serde_json::from_slice(charged.as_bytes())?;
                    for mut value in entries(&value).iter().cloned() {
                        if value.get("method").is_none()
                            && reserved(&value["id"])
                            && value["id"] != expected
                        {
                            continue;
                        }
                        if value["method"] == "session/update"
                            && value["params"]["sessionId"].as_str() == session.as_deref()
                        {
                            if matches!(
                                value["params"]["update"]["sessionUpdate"].as_str(),
                                Some("state_update" | "config_option_update")
                            ) && serde_json::from_value::<UpdateSessionNotification>(
                                value["params"].clone(),
                            )
                            .is_err()
                            {
                                terminal = Some(
                                    "Gateway protocol: invalid session snapshot notification"
                                        .into(),
                                );
                                lost = true;
                                break;
                            }
                            saw_state |=
                                value["params"]["update"]["sessionUpdate"] == "state_update";
                            saw_config |= value["params"]["update"]["sessionUpdate"]
                                == "config_option_update";
                        }
                        if let Some(reason) = terminal_reason(&value) {
                            if value.get("method").is_none()
                                && let Some(index) =
                                    pending.iter().position(|p| p["id"] == value["id"])
                            {
                                emit(
                                    &mut output,
                                    &object([
                                        ("jsonrpc", "2.0".into()),
                                        ("id", value["id"].clone()),
                                        (
                                            "error",
                                            object([
                                                ("code", (-32000).into()),
                                                ("message", "Gateway rejected request".into()),
                                                (
                                                    "data",
                                                    object([
                                                        ("reason", reason.into()),
                                                        ("terminal", true.into()),
                                                    ]),
                                                ),
                                            ]),
                                        ),
                                    ]),
                                )?;
                                pending.remove(index);
                            }
                            terminal = Some(format!(
                                "Gateway {reason}; submission outcome may be unknown. Inspect kit gateway list and restart explicitly; replay_unavailable requires explicit --remote-no-replay opt-in."
                            ));
                            lost = true;
                            break;
                        }
                        if value.get("id").is_some()
                            && value.get("method").is_none()
                            && reserved(&value["id"])
                        {
                            if value["id"] != expected {
                                continue;
                            }
                            if value.get("error").is_some() {
                                if value["error"]["data"]["terminal"] != false {
                                    terminal = Some("Gateway rejected recovery; inspect session and restart explicitly. Use --remote-no-replay only to opt out of history.".into());
                                }
                                lost = true;
                                break;
                            }
                            if phase == "initialize" {
                                if value["result"]["protocolVersion"] != 2 {
                                    terminal = Some("Gateway protocol mismatch".into());
                                    lost = true;
                                    break;
                                }
                                if !initialized {
                                    // Open admission before publishing readiness: a local reader
                                    // can enqueue its next request before emit/flush returns.
                                    initialized = true;
                                    phase = "live";
                                    gate = Some(Instant::now());
                                    recovery_start = None;
                                    attempt = 0;
                                    // Initial initialize is safe to retry; preserve its original local ID.
                                    if let Some(index) =
                                        pending.iter().position(|v| v["method"] == "initialize")
                                    {
                                        value["id"] = pending.remove(index)["id"].clone();
                                        emit(&mut output, &value)?;
                                    }
                                } else {
                                    let mut params = object([
                                        (
                                            "sessionId",
                                            session.clone().map_or(Value::Null, Value::String),
                                        ),
                                        (
                                            "cwd",
                                            root.to_str()
                                                .ok_or("remote root must be valid UTF-8")?
                                                .into(),
                                        ),
                                        ("mcpServers", Value::Array(Vec::new())),
                                        (
                                            "_meta",
                                            object([(
                                                "kit/gateway",
                                                object([
                                                    (
                                                        "previousAttachment",
                                                        attachment
                                                            .clone()
                                                            .map_or(Value::Null, Value::String),
                                                    ),
                                                    ("recoveryId", recovery_id.clone().into()),
                                                    ("recoveryAttempt", attempt.into()),
                                                ]),
                                            )]),
                                        ),
                                    ]);
                                    if !remote.no_replay {
                                        params["replayFrom"] = object([("type", "start".into())]);
                                    }
                                    let request = internal(epoch, "session/resume", params);
                                    expected = request["id"].clone();
                                    send(&mut channel, &request)?;
                                    phase = "replay";
                                }
                            } else if phase == "replay" {
                                if value["result"]
                                    .get("sessionId")
                                    .is_some_and(|id| id.as_str() != session.as_deref())
                                {
                                    terminal = Some(
                                        "Gateway protocol: resumed a different durable session"
                                            .into(),
                                    );
                                    lost = true;
                                    break;
                                }
                                let token = value["result"]["_meta"]["kit/gateway"]["attachment"]
                                    .as_str()
                                    .filter(|s| !s.is_empty() && s.len() <= 128);
                                if let (Some(bits), Some(token)) = (
                                    snapshot(&value["result"], remote.no_replay)
                                        .filter(|_| saw_state && saw_config),
                                    token,
                                ) {
                                    attachment = Some(token.to_owned());
                                    // Commit is the local readiness publication. Fence old
                                    // queued input before it becomes visible to the consumer.
                                    phase = "live";
                                    gate = Some(Instant::now());
                                    recovery_start = None;
                                    notice(
                                        &mut output,
                                        session.as_deref().ok_or("Missing durable target")?,
                                        epoch,
                                        "commit",
                                        attempt,
                                        "Reconnected",
                                        bits,
                                    )?;
                                    attempt = 0;
                                } else {
                                    terminal = Some("Gateway recovery snapshot unavailable; session left unchanged".into());
                                    lost = true;
                                }
                            }
                            continue;
                        }
                        if (phase == "replay" || phase == "initial_replay")
                            && value.get("method").is_some()
                        {
                            if value["method"] != "session/update"
                                || value["params"]["sessionId"].as_str() != session.as_deref()
                            {
                                continue;
                            }
                            replay_events += 1;
                            replay_bytes = replay_bytes.saturating_add(value.to_string().len());
                            if replay_events > 4096 || replay_bytes > 8 * 1024 * 1024 {
                                terminal = Some("Replay unavailable within local bounds; restart with explicit --remote-no-replay".into());
                                lost = true;
                                break;
                            }
                        }
                        let mut completed = None;
                        let mut initial_commit = None;
                        if value.get("method").is_none()
                            && let Some(index) = pending.iter().position(|p| p["id"] == value["id"])
                        {
                            let request = pending[index].clone();
                            completed = Some(index);
                            if value.get("error").is_some() && resumed.is_some() {
                                emit(&mut output, &value)?;
                                pending.remove(index);
                                terminal = Some("Explicit session resume rejected".into());
                                lost = true;
                                break;
                            }
                            if value.get("result").is_some() {
                                if request["method"] == "initialize" {
                                    initialized = true;
                                    phase = "live";
                                }
                                if matches!(
                                    request["method"].as_str(),
                                    Some("session/new" | "session/resume" | "session/load")
                                ) {
                                    if let Some(id) = value["result"]["sessionId"]
                                        .as_str()
                                        .filter(|id| !id.is_empty())
                                    {
                                        if resumed.as_ref().is_some_and(|(_, target)| target != id)
                                        {
                                            terminal = Some("Gateway protocol: resumed a different durable session".into());
                                            lost = true;
                                            break;
                                        }
                                        session = Some(id.to_owned());
                                    } else if request["method"] == "session/new"
                                        && resumed.is_none()
                                    {
                                        terminal = Some("Gateway protocol: missing confirmed session ID; use kit gateway list and explicit --remote-session".into());
                                        lost = true;
                                        break;
                                    }
                                    attachment =
                                        value["result"]["_meta"]["kit/gateway"]["attachment"]
                                            .as_str()
                                            .filter(|s| !s.is_empty() && s.len() <= 128)
                                            .map(str::to_owned);
                                    if let Some((id, target)) = &resumed
                                        && serde_json::to_value(id)? == value["id"]
                                    {
                                        value["result"]["sessionId"] =
                                            Value::String(target.clone());
                                        initial_commit =
                                            snapshot(&value["result"], remote.no_replay)
                                                .filter(|_| saw_state && saw_config);
                                        if initial_commit.is_none() {
                                            terminal = Some(
                                                "Gateway snapshot missing for initial resume"
                                                    .into(),
                                            );
                                            lost = true;
                                            break;
                                        }
                                        resumed = None;
                                        phase = "live";
                                        gate = Some(Instant::now());
                                    }
                                }
                            }
                        }
                        if let Some(session) = &session {
                            annotate(
                                &mut value,
                                session,
                                epoch,
                                if phase == "replay" || resumed.is_some() {
                                    "replay"
                                } else {
                                    "live"
                                },
                            );
                            if let Some(bits) = initial_commit {
                                notice(&mut output, session, epoch, "commit", 0, "Attached", bits)?;
                            }
                        }
                        if session.is_none()
                            && let Some(id) =
                                value["params"]["sessionId"].as_str().map(str::to_owned)
                        {
                            annotate(&mut value, &id, epoch, "live");
                        }
                        emit(&mut output, &value)?;
                        if let Some(index) = completed {
                            pending.remove(index);
                        }
                    }
                    // Keep SDK byte/frame admission charged through every stdout flush.
                    drop(charged);
                }
                Event::Frame(None) => {
                    frames_open = false;
                    if phase == "live" {
                        phase = "transport_closing";
                        deadline
                            .as_mut()
                            .reset(tokio::time::Instant::now() + Duration::from_secs(5));
                    }
                }
                Event::Timeout => lost = true,
                Event::Driver(result) => {
                    if !input_open {
                        return result
                            .map_err(|_| "Gateway transport disconnected during shutdown".into());
                    }
                    if let Err(error) = result
                        && let Some(status) = diagnostic_status(&error)
                        && matches!(status, 400 | 401 | 403 | 404 | 405 | 409 | 410 | 422 | 426)
                    {
                        terminal = Some(format!(
                            "Gateway HTTP {status}; authentication/protocol/session recovery rejected. Inspect session and restart explicitly."
                        ));
                    }
                    lost = true;
                }
            }
        }
        drop(transport);
        drop(channel);
        let keep_initialize = !initialized
            && pending.iter().all(|p| p["method"] == "initialize")
            && initialize.is_some();
        if !keep_initialize {
            for request in pending.drain(..) {
                reject(&mut output, &request, "outcome_unknown", &session)?;
            }
        }
        if terminal.is_none() && session.is_none() && !keep_initialize {
            return Err("Session creation outcome unknown; use kit gateway list and explicit --remote-session. No new session was retried.".into());
        }
        if recovery_start.is_none() {
            if terminal.is_none() && session.is_some() && attachment.is_none() && initialized {
                terminal = Some("Gateway attachment metadata unavailable; automatic recovery disabled. Use explicit --remote-session selection.".into());
            }
            let mut nonce = [0u8; 32];
            getrandom::fill(&mut nonce).map_err(|_| "Recovery entropy unavailable")?;
            recovery_id = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
            recovery_start = Some(Instant::now());
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests;
