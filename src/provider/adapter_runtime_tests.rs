use super::*;
use agentkit_core::{Item, ItemKind, MetadataMap, SessionId, TurnId};
use serde_json::json;
use std::io::{Read, Write};

fn server(responses: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        responses.into_iter().map(|(status, response)| {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let count = stream.read(&mut chunk).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&chunk[..count]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length = headers.lines().find_map(|line| line.strip_prefix("content-length:")).map(|v| v.trim().parse::<usize>().unwrap()).unwrap_or(0);
                    if request.len() >= end + 4 + length { break; }
                }
            }
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nLocation: http://127.0.0.1:1/leak\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            String::from_utf8(request).unwrap()
        }).collect()
    });
    (format!("http://{address}"), handle)
}

#[tokio::test]
async fn runtime_catalog_and_multiple_sessions_use_bearer_and_canonical_chats() {
    let completion = json!({"id":"test","object":"chat.completion","created":1,"model":"private/offered","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}).to_string();
    let (origin, server) = server(vec![
        (
            200,
            json!({"data":[{"id":"private/offered","context_length":123456}]}).to_string(),
        ),
        (200, completion.clone()),
        (200, completion),
    ]);
    let auth =
        speakeasy_runtime::Authentication::Runtime(zeroize::Zeroizing::new("sai_test".into()));
    let catalog = fetch_authenticated_model_ids(&format!("{origin}/chat/models"), Some(&auth))
        .await
        .unwrap();
    assert_eq!(catalog.models, ["private/offered"]);
    assert_eq!(catalog.context_windows["private/offered"], 123456);
    let adapter = KitAdapter::Speakeasy(Box::new(SpeakeasyKitAdapter {
        provider: SpeakeasyProvider {
            openrouter: OpenRouterProvider::from(
                OpenRouterConfig::new("unused", "private/offered")
                    .with_base_url(format!("{origin}/chat/completions"))
                    .with_streaming(false),
            ),
            auth,
            chat_id: None,
        },
        client: agentkit_http::Http::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        ),
    }));
    assert!(
        adapter
            .start_session(SessionConfig::new("no-binding"))
            .await
            .is_err()
    );
    let chats = [
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    ];
    for (i, chat) in chats.iter().enumerate() {
        let id = format!("acp-session-{i}");
        let mut metadata = MetadataMap::new();
        metadata.insert(speakeasy_runtime::CHAT_ID_KEY.into(), json!(chat));
        let mut session = adapter
            .start_session(SessionConfig::new(id.clone()).with_metadata(metadata))
            .await
            .unwrap();
        session
            .begin_turn(
                TurnRequest {
                    session_id: SessionId::new(id),
                    turn_id: TurnId::new("turn"),
                    transcript: vec![Item::text(ItemKind::User, "hello")],
                    available_tools: Vec::new(),
                    cache: None,
                    metadata: MetadataMap::new(),
                },
                None,
            )
            .await
            .unwrap();
    }
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /chat/models "));
    for request in &requests {
        let headers = request
            .split_once("\r\n\r\n")
            .unwrap()
            .0
            .to_ascii_lowercase();
        assert!(headers.contains("authorization: bearer sai_test"));
        assert!(!headers.contains("gram-key:"));
        assert!(!headers.contains("gram-project:"));
    }
    assert!(!requests[0].to_ascii_lowercase().contains("gram-chat-id:"));
    for (request, chat) in requests[1..].iter().zip(chats) {
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("gram-chat-id: {chat}"))
        );
    }
}

#[tokio::test]
async fn authenticated_catalog_does_not_follow_redirects_or_accept_errors() {
    for status in [302, 401, 403, 500] {
        let (origin, server) = server(vec![(status, "{}".into())]);
        let auth =
            speakeasy_runtime::Authentication::Runtime(zeroize::Zeroizing::new("sai_test".into()));
        let error = fetch_authenticated_model_ids(&format!("{origin}/chat/models"), Some(&auth))
            .await
            .err()
            .unwrap();
        assert!(error.contains(&status.to_string()));
        assert!(!error.contains("sai_test"));
        server.join().unwrap();
    }
}
