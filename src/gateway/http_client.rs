//! Stdio relay for the SDK's ACP v2 HTTP transport.
use super::object;
use agent_client_protocol::schema::v1::{RequestId, Response};
use agent_client_protocol::{Channel, Client, ConnectTo, RawJsonRpcMessage, TransportFrame};
use agent_client_protocol_http::HttpClient as AcpHttpClient;
use futures_util::{
    StreamExt,
    future::{Either, select},
};
use serde_json::Value;
use std::{io, path::PathBuf};

fn client(remote: &super::Remote) -> Result<AcpHttpClient, Box<dyn std::error::Error>> {
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
    Ok(AcpHttpClient::with_endpoint_and_client(
        endpoint.as_str(),
        http,
    )?)
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
            if let Some(id) = session.take() {
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

pub(super) async fn bridge(
    remote: super::Remote,
    root: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut channel, mut transport) =
        ConnectTo::<Client>::into_channel_and_future(client(&remote)?);
    // A dedicated reader avoids Tokio's uncancellable blocking stdin read keeping
    // the runtime alive after a remote disconnect.
    let (send, mut lines) = tokio::sync::mpsc::channel(16);
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in io::stdin().lock().lines() {
            if send.blocking_send(line).is_err() {
                break;
            }
        }
    });
    let mut session = remote.session;
    let mut initial = true;
    let mut resumed = None;
    let mut input_open = true;
    enum Event {
        Frame(Option<TransportFrame>),
        Transport(Result<(), agent_client_protocol::Error>),
        Line(Option<io::Result<String>>),
    }
    loop {
        let event = {
            let frame = std::pin::pin!(channel.rx.next());
            let line = std::pin::pin!(async {
                if input_open {
                    lines.recv().await
                } else {
                    std::future::pending().await
                }
            });
            match select(frame, select(&mut transport, line)).await {
                Either::Left((frame, _)) => Event::Frame(frame),
                Either::Right((Either::Left((result, _)), _)) => Event::Transport(result),
                Either::Right((Either::Right((line, _)), _)) => Event::Line(line),
            }
        };
        match event {
            Event::Frame(Some(mut frame)) => {
                restore_new_response(&mut frame, &mut resumed);
                print_frame(&frame)?;
            }
            Event::Frame(None) => return transport.await.map_err(transport_error),
            Event::Transport(result) => return result.map_err(transport_error),
            Event::Line(Some(line)) => {
                let mut frame = TransportFrame::parse_json(&line?);
                rewrite(&mut frame, &root, &mut session, &mut initial, &mut resumed)?;
                channel
                    .tx
                    .unbounded_send(frame)
                    .map_err(|_| io::Error::other("gateway transport closed"))?;
            }
            Event::Line(None) => {
                input_open = false;
                // Keep polling the driver after EOF: it drains accepted POSTs
                // and awaits HTTP DELETE before returning. Dropping it here
                // would race process shutdown against best-effort cleanup.
                channel.tx.close_channel();
            }
        }
    }
}

fn transport_error(error: agent_client_protocol::Error) -> Box<dyn std::error::Error> {
    format!("gateway transport failed; submission outcome may be unknown. Reconnect and inspect before resubmitting: {error}").into()
}

fn print_frame(frame: &TransportFrame) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{}", frame.to_json()?)?;
    stdout.flush()?;
    Ok(())
}

async fn request(
    channel: &mut Channel,
    id: i64,
    method: &str,
    params: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let frame = TransportFrame::parse_json(
        &object([
            ("jsonrpc", Value::String("2.0".into())),
            ("id", Value::from(id)),
            ("method", Value::String(method.to_owned())),
            ("params", params),
        ])
        .to_string(),
    );
    channel
        .tx
        .unbounded_send(frame)
        .map_err(|_| io::Error::other("gateway transport closed"))?;
    while let Some(frame) = channel.rx.next().await {
        let value: Value = serde_json::from_str(&frame.to_json()?)?;
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
                    .ok_or_else(|| "gateway response omitted result".into());
            }
        }
    }
    Err("gateway transport closed before response".into())
}

pub(super) async fn list(remote: super::Remote) -> Result<(), Box<dyn std::error::Error>> {
    let (mut channel, mut transport) =
        ConnectTo::<Client>::into_channel_and_future(client(&remote)?);
    let result = {
        let requests = async {
            request(&mut channel, 1, "initialize", initialize_params()?).await?;
            let response = request(&mut channel, 2, "session/list", object([])).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
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
