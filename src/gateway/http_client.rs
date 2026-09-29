//! Stdio relay for the SDK's ACP v2 HTTP transport.
use super::object;
use agent_client_protocol::schema::v1::RequestId;
#[cfg(test)]
use agent_client_protocol::schema::v1::Response;
use agent_client_protocol::{BoundedChannel, ChargedFrame, RawJsonRpcMessage, TransportFrame};
use agent_client_protocol_http::{
    BoundedHttpClient, HttpClient as AcpHttpClient, HttpClientLimits,
};
use futures_util::{
    StreamExt,
    future::{Either, select},
};
use serde_json::Value;
use std::{io, path::PathBuf};

fn client(remote: &super::Remote) -> Result<BoundedHttpClient, Box<dyn std::error::Error>> {
    let mut endpoint = reqwest::Url::parse(&remote.url)?;
    if !matches!(endpoint.scheme(), "http" | "https") {
        return Err("gateway URL must use HTTP or HTTPS".into());
    }
    let path = endpoint.path().trim_end_matches('/');
    let path = if path.ends_with("/acp/v2") {
        path.to_owned()
    } else {
        format!("{path}/acp/v2")
    };
    endpoint.set_path(&path);
    let mut authorization = reqwest::header::HeaderValue::from_str(&format!(
        "Bearer {}",
        super::credential(&remote.credential_file)?
    ))?;
    authorization.set_sensitive(true);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, authorization);
    let http = reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()?;
    Ok(
        AcpHttpClient::with_endpoint_and_client(endpoint.as_str(), http)?
            .with_limits(HttpClientLimits::default())?,
    )
}

fn rewrite(
    frame: &mut TransportFrame,
    root: &std::path::Path,
    session: &mut Option<String>,
    initial: &mut bool,
    resumed: &mut Option<(RequestId, String)>,
) -> Result<(), agent_client_protocol::Error> {
    fn message(
        message: &mut RawJsonRpcMessage,
        root: &std::path::Path,
        session: &mut Option<String>,
        initial: &mut bool,
        resumed: &mut Option<(RequestId, String)>,
    ) -> Result<(), agent_client_protocol::Error> {
        let RawJsonRpcMessage::Request(request) = message else {
            return Ok(());
        };
        if !matches!(
            request.method.as_ref(),
            "session/new" | "session/resume" | "session/load"
        ) {
            return Ok(());
        }
        let mut params = request
            .params
            .clone()
            .map(|p| p.into_value())
            .unwrap_or_else(|| object([]));
        if !params.is_object() {
            return Ok(());
        }
        params["cwd"] = Value::String(
            root.to_str()
                .ok_or_else(|| {
                    agent_client_protocol::Error::internal_error()
                        .data("remote root must be valid UTF-8")
                })?
                .to_owned(),
        );
        if request.method.as_ref() == "session/new" && *initial {
            *initial = false;
            if let Some(id) = session.clone() {
                *resumed = Some((request.id.clone(), id.clone()));
                request.method = "session/resume".into();
                params["sessionId"] = Value::String(id);
                params["replayFrom"] = object([("type", Value::String("start".into()))]);
            }
        }
        request.params = agent_client_protocol::RawJsonRpcParams::from_value(params)?;
        Ok(())
    }
    match frame {
        TransportFrame::Single(value) => message(value, root, session, initial, resumed)?,
        TransportFrame::Batch(batch) => {
            for entry in batch.entries_mut() {
                if let agent_client_protocol::TransportBatchEntry::Message(value) = entry {
                    message(value, root, session, initial, resumed)?;
                }
            }
        }
        TransportFrame::Malformed { .. } => {}
    }
    Ok(())
}

// The local TUI issued session/new, whose response requires sessionId. ACP
// session/resume omits it, so restore that field only on the rewritten response.
#[cfg(test)]
fn restore_new_response(frame: &mut TransportFrame, resumed: &mut Option<(RequestId, String)>) {
    fn message(value: &mut RawJsonRpcMessage, resumed: &mut Option<(RequestId, String)>) {
        let Some((expected, session)) = resumed.as_ref() else {
            return;
        };
        if let RawJsonRpcMessage::Response(response) = value {
            match response {
                Response::Result { id, result } if id == expected => {
                    if let Some(object) = result.as_object_mut() {
                        object.insert("sessionId".into(), Value::String(session.clone()));
                    }
                    *resumed = None;
                }
                Response::Error { id, .. } if id == expected => *resumed = None,
                _ => {}
            }
        }
    }
    match frame {
        TransportFrame::Single(value) => message(value, resumed),
        TransportFrame::Batch(batch) => {
            for entry in batch.entries_mut() {
                if let agent_client_protocol::TransportBatchEntry::Message(value) = entry {
                    message(value, resumed);
                }
            }
        }
        TransportFrame::Malformed { .. } => {}
    }
}

mod recovery;

pub(super) async fn bridge(
    remote: super::Remote,
    root: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let (send, lines) = tokio::sync::mpsc::channel(16);
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let line = match read_stdin_line(&mut stdin) {
                Ok(Some(line)) => Ok(line),
                Ok(None) => break,
                Err(error) => {
                    let _ = send.blocking_send((std::time::Instant::now(), Err(error)));
                    break;
                }
            };
            if send
                .blocking_send((std::time::Instant::now(), line))
                .is_err()
            {
                break;
            }
        }
    });
    recovery::run(remote, root, lines, io::stdout()).await
}

// Bound before parsing or entering the 16-slot local mailbox. BufRead::lines()
// would allocate an arbitrary unterminated line outside SDK admission.
fn read_stdin_line(reader: &mut impl io::BufRead) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(count) > super::MAX_HTTP_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gateway stdin frame exceeds 1 MiB",
            ));
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() || count == 0 {
            if line.is_empty() {
                return Ok(None);
            }
            return String::from_utf8(line)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
    }
}

fn transport_error(error: agent_client_protocol::Error) -> Box<dyn std::error::Error> {
    format!("gateway transport failed; submission outcome may be unknown. Reconnect and inspect before resubmitting: {error}").into()
}

async fn request(
    channel: &mut BoundedChannel,
    id: i64,
    method: &str,
    params: Value,
) -> Result<(Value, ChargedFrame), Box<dyn std::error::Error>> {
    let frame = TransportFrame::parse_json(
        &object([
            ("jsonrpc", Value::String("2.0".into())),
            ("id", Value::from(id)),
            ("method", Value::String(method.to_owned())),
            ("params", params),
        ])
        .to_string(),
    );
    channel.tx.try_send(frame).map_err(transport_error)?;
    while let Some(frame) = channel.rx.next().await {
        let value: Value = serde_json::from_slice(frame.as_bytes())?;
        let values = match &value {
            Value::Array(values) => values.as_slice(),
            _ => std::slice::from_ref(&value),
        };
        for value in values {
            if value.get("id") == Some(&Value::from(id)) && value.get("method").is_none() {
                if let Some(error) = value.get("error") {
                    return Err(format!("gateway {method}: {error}").into());
                }
                return value
                    .get("result")
                    .cloned()
                    .map(|result| (result, frame))
                    .ok_or_else(|| "gateway response omitted result".into());
            }
        }
    }
    Err("gateway transport closed before response".into())
}

pub(super) async fn list(remote: super::Remote) -> Result<(), Box<dyn std::error::Error>> {
    let (mut channel, mut transport) = client(&remote)?.into_bounded_channel_and_future();
    let result = {
        let requests = async {
            request(&mut channel, 1, "initialize", initialize_params()?).await?;
            let (response, charged) = request(&mut channel, 2, "session/list", object([])).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            drop(response);
            drop(charged);
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let requests = std::pin::pin!(requests);
        match select(requests, &mut transport).await {
            Either::Left((result, _)) => result,
            Either::Right((result, _)) => {
                result.map_err(transport_error)?;
                return Err("gateway transport closed before session list completed".into());
            }
        }
    };
    // Unlike dropping the driver, awaiting it guarantees that its graceful
    // connection DELETE finishes before this short-lived CLI exits.
    channel.tx.close_channel();
    let closed = transport.await.map_err(transport_error);
    result.and(closed)
}

fn initialize_params() -> Result<Value, serde_json::Error> {
    use agent_client_protocol::schema::{
        ProtocolVersion,
        v2::{Implementation, InitializeRequest},
    };
    serde_json::to_value(InitializeRequest::new(
        ProtocolVersion::V2,
        Implementation::new("kit-gateway", env!("CARGO_PKG_VERSION")),
    ))
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    clippy::disallowed_macros,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn list_waits_for_graceful_delete() {
        use axum::{Json, Router, http::StatusCode, response::Sse, routing::post};
        use std::{convert::Infallible, io::Write, time::Duration};

        let (deleted, mut deletes) = tokio::sync::mpsc::channel(1);
        let (events, _) = tokio::sync::broadcast::channel::<Value>(16);
        let outgoing = events.clone();
        let app = Router::new().route(
            "/acp/v2",
            post(move |Json(message): Json<Value>| {
                let events = events.clone();
                async move {
                    use axum::response::IntoResponse;
                    match message["method"].as_str() {
                        Some("initialize") => (
                            [("acp-connection-id", "test-connection")],
                            Json(json!({"jsonrpc": "2.0", "id": message["id"], "result": {
                                "protocolVersion": 2,
                                "info": {"name": "test", "version": "0"},
                                "capabilities": {}
                            }})),
                        ).into_response(),
                        Some("session/list") => {
                            events.send(json!({"jsonrpc": "2.0", "id": message["id"], "result": {"sessions": []}})).unwrap();
                            StatusCode::ACCEPTED.into_response()
                        }
                        other => panic!("unexpected request: {other:?}"),
                    }
                }
            })
            .get(move || {
                let receiver = outgoing.subscribe();
                async move {
                    Sse::new(futures_util::stream::unfold(receiver, |mut receiver| async move {
                        let message = receiver.recv().await.ok()?;
                        Some((Ok::<_, Infallible>(axum::response::sse::Event::default().data(message.to_string())), receiver))
                    }))
                }
            })
            .delete(move || {
                let deleted = deleted.clone();
                async move {
                    deleted.send(()).await.unwrap();
                    StatusCode::ACCEPTED
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut credential = tempfile::NamedTempFile::new().unwrap();
        writeln!(credential, "test-token").unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            list(super::super::Remote {
                url: format!("http://{address}"),
                credential_file: credential.path().to_owned(),
                session: None,
                no_replay: false,
            }),
        )
        .await;
        server.abort();
        result.expect("list did not shut down").unwrap();
        deletes
            .try_recv()
            .expect("list returned without graceful DELETE");
    }

    #[test]
    fn stdin_frames_are_bounded_before_mailbox_admission() {
        let mut reader = io::BufReader::with_capacity(7, io::Cursor::new(b"first\nsecond"));
        assert_eq!(
            read_stdin_line(&mut reader).unwrap().as_deref(),
            Some("first\n")
        );
        assert_eq!(
            read_stdin_line(&mut reader).unwrap().as_deref(),
            Some("second")
        );
        assert!(read_stdin_line(&mut reader).unwrap().is_none());

        let maximum = vec![b'x'; super::super::MAX_HTTP_FRAME];
        let mut reader = io::Cursor::new(maximum);
        assert_eq!(
            read_stdin_line(&mut reader).unwrap().unwrap().len(),
            super::super::MAX_HTTP_FRAME
        );
        let oversized = vec![b'x'; super::super::MAX_HTTP_FRAME + 1];
        let mut reader = io::BufReader::with_capacity(4096, io::Cursor::new(oversized));
        assert_eq!(
            read_stdin_line(&mut reader).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            read_stdin_line(&mut io::Cursor::new([0xff]))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn initialize_uses_numeric_v2() {
        let params = initialize_params().unwrap();
        assert_eq!(params["protocolVersion"], json!(2));
        assert_eq!(params["info"]["name"], "kit-gateway");
        assert!(params["capabilities"].is_object());
    }

    #[test]
    fn resumed_session_uses_remote_root_and_restores_new_response() {
        let mut frame = TransportFrame::parse_json(
            r#"{"jsonrpc":"2.0","id":7,"method":"session/new","params":{"cwd":"/local","mcpServers":[]}}"#,
        );
        let mut session = Some("remote-session".to_owned());
        let mut initial = true;
        let mut resumed = None;
        rewrite(
            &mut frame,
            std::path::Path::new("/remote"),
            &mut session,
            &mut initial,
            &mut resumed,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&frame.to_json().unwrap()).unwrap();
        assert_eq!(value["method"], "session/resume");
        assert_eq!(value["params"]["cwd"], "/remote");
        assert_eq!(value["params"]["sessionId"], "remote-session");
        assert_eq!(value["params"]["replayFrom"], json!({"type": "start"}));
        assert_eq!(value["id"], 7);
        assert_eq!(session.as_deref(), Some("remote-session"));
        let mut response =
            TransportFrame::parse_json(r#"{"jsonrpc":"2.0","id":7,"result":{"configOptions":[]}}"#);
        restore_new_response(&mut response, &mut resumed);
        let value: Value = serde_json::from_str(&response.to_json().unwrap()).unwrap();
        assert_eq!(value["result"]["sessionId"], "remote-session");
        assert!(resumed.is_none());

        let mut next = TransportFrame::parse_json(
            r#"{"jsonrpc":"2.0","id":8,"method":"session/new","params":{"cwd":"/local"}}"#,
        );
        rewrite(
            &mut next,
            std::path::Path::new("/remote"),
            &mut session,
            &mut initial,
            &mut resumed,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&next.to_json().unwrap()).unwrap();
        assert_eq!(value["method"], "session/new");
        assert_eq!(value["params"]["cwd"], "/remote");
    }
}
