//! Authenticated, bounded discovery with no shared, cross-credential cache.
use futures_util::StreamExt as _;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

pub(super) const MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODELS: usize = 2_000;
const MAX_PAGES: usize = 20;

#[derive(Clone, Debug, Deserialize)]
pub(super) struct Model {
    pub id: String,
    pub max_input_tokens: Option<u64>,
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub capabilities: Value,
}
impl Model {
    pub fn context_window(&self) -> Option<u64> {
        self.max_input_tokens.filter(|value| *value > 0)
    }
    pub fn supports(&self, pointer: &str) -> bool {
        self.capabilities.pointer(pointer).and_then(Value::as_bool) == Some(true)
    }
}
#[derive(Deserialize)]
struct Page {
    data: Vec<Model>,
    has_more: bool,
    last_id: Option<String>,
}
pub(super) async fn discover(url: &str, key: &str) -> Result<Vec<Model>, String> {
    tokio::time::timeout(Duration::from_secs(5), fetch(url, key))
        .await
        .map_err(|_| "Anthropic model discovery timed out".to_owned())?
}
fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| "Could not build Anthropic discovery client".into())
}

/// Retrieve accepts aliases as well as canonical IDs. Keep the requested completion
/// model unchanged: the returned ID only describes the resolved metadata.
pub(super) async fn retrieve(url: &str, key: &str, model: &str) -> Result<Model, String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut url = reqwest::Url::parse(url).map_err(|_| "Invalid Anthropic catalog URL")?;
        url.path_segments_mut()
            .map_err(|_| "Invalid Anthropic catalog URL")?
            .pop_if_empty()
            .push(model);
        let mut bytes_left = MAX_BYTES;
        let bytes = read_model_response(&client()?, url, key, &mut bytes_left).await?;
        serde_json::from_slice(&bytes).map_err(|_| "Invalid Anthropic model metadata".into())
    })
    .await
    .map_err(|_| "Anthropic model discovery timed out".to_owned())?
}

async fn read_model_response(
    client: &reqwest::Client,
    url: reqwest::Url,
    key: &str,
    bytes_left: &mut usize,
) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .map_err(|_| "Anthropic discovery request failed")?;
    if !response.status().is_success() {
        return Err(format!(
            "Anthropic discovery returned HTTP {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > *bytes_left as u64)
    {
        return Err("Anthropic catalog exceeds byte limit".into());
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "Could not read Anthropic catalog")?;
        if chunk.len() > *bytes_left {
            return Err("Anthropic catalog exceeds byte limit".into());
        }
        *bytes_left -= chunk.len();
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn fetch(url: &str, key: &str) -> Result<Vec<Model>, String> {
    let client = client()?;
    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    let mut model_count = 0;
    let mut cursors = std::collections::HashSet::new();
    let mut bytes_left = MAX_BYTES;
    for _ in 0..MAX_PAGES {
        let mut page_url = reqwest::Url::parse(url).map_err(|_| "Invalid Anthropic catalog URL")?;
        page_url.query_pairs_mut().append_pair("limit", "1000");
        if let Some(cursor) = &cursor {
            page_url.query_pairs_mut().append_pair("after_id", cursor);
        }
        let bytes = read_model_response(&client, page_url, key, &mut bytes_left).await?;
        let page: Page =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid Anthropic model catalog")?;
        model_count += page.data.len();
        if model_count > MAX_MODELS {
            return Err("Anthropic catalog exceeds model limit".into());
        }
        models.extend(
            page.data
                .into_iter()
                .filter(|model| super::adapter::valid_model_id(&model.id)),
        );
        if !page.has_more {
            return Ok(models);
        }
        let next = page
            .last_id
            .filter(|id| !id.is_empty())
            .ok_or("Anthropic catalog missing pagination cursor")?;
        if !cursors.insert(next.clone()) {
            return Err("Anthropic catalog repeated pagination cursor".into());
        }
        cursor = Some(next);
    }
    Err("Anthropic catalog exceeds page limit".into())
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    fn server(responses: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1/models", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            responses.into_iter().map(|(status, body)| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nLocation: http://127.0.0.1:1/leak\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            String::from_utf8(request).unwrap()
        }).collect()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn retrieve_encodes_alias_and_accepts_canonical_identity() {
        let (url, handle) = server(vec![(
            200,
            json!({"id":"canonical-id", "max_input_tokens":77777,
            "max_tokens":8192,"capabilities":{"thinking":{"supported":true}}})
            .to_string(),
        )]);
        let model = retrieve(&url, "alias-key", "alias/with?#space name")
            .await
            .unwrap();
        assert_eq!(model.id, "canonical-id");
        assert_eq!(model.context_window(), Some(77777));
        let requests = handle.join().unwrap();
        assert!(requests[0].starts_with("GET /v1/models/alias%2Fwith%3F%23space%20name HTTP/1.1"));
        assert!(requests[0].contains("x-api-key: alias-key"));
        assert!(requests[0].contains("anthropic-version: 2023-06-01"));
        assert!(!requests[0].contains("limit="));
    }

    #[tokio::test]
    async fn retrieve_rejects_redirects_errors_and_oversized_metadata() {
        for (status, body) in [
            (302, "{}".to_owned()),
            (401, "{}".to_owned()),
            (200, "invalid".to_owned()),
            (200, " ".repeat(MAX_BYTES + 1)),
        ] {
            let (url, handle) = server(vec![(status, body)]);
            assert!(retrieve(&url, "test", "alias").await.is_err());
            let _ = handle.join();
        }
    }

    #[tokio::test]
    async fn paginated_authenticated_capabilities_and_limits() {
        let (url, handle) = server(vec![
            (200, json!({"data":[{"id":"custom-first","max_input_tokens":77777,"max_tokens":2048,
                "capabilities":{"thinking":{"supported":true,"types":{"enabled":{"supported":true}}}},"future_field":true}],
                "has_more":true,"last_id":"cursor /+"}).to_string()),
            (200, json!({"data":[{"id":"custom-second","max_input_tokens":null,"max_tokens":null}],"has_more":false,"last_id":null}).to_string()),
        ]);
        let models = discover(&url, "secret-one").await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].context_window(), Some(77777));
        assert_eq!(models[0].max_tokens, Some(2048));
        assert!(models[0].supports("/thinking/types/enabled/supported"));
        assert!(!models[1].supports("/thinking/supported"));
        assert_eq!(models[1].context_window(), None);
        let requests = handle.join().unwrap();
        for request in &requests {
            let request = request.to_ascii_lowercase();
            assert!(request.contains("x-api-key: secret-one\r\n"));
            assert!(request.contains("anthropic-version: 2023-06-01\r\n"));
            assert!(request.contains("limit=1000"));
        }
        assert!(requests[1].contains("after_id=cursor+%2F%2B"));
    }

    #[tokio::test]
    async fn failures_are_bounded_and_do_not_follow_redirects() {
        for (status, body) in [
            (401, "{}".to_owned()),
            (302, "{}".to_owned()),
            (200, "not json".to_owned()),
            (
                200,
                json!({"data":[],"has_more":true,"last_id":null}).to_string(),
            ),
            (200, " ".repeat(MAX_BYTES + 1)),
            (
                200,
                json!({"data":vec![json!({"id":"x"}); MAX_MODELS + 1],"has_more":false})
                    .to_string(),
            ),
        ] {
            let (url, handle) = server(vec![(status, body)]);
            assert!(discover(&url, "test").await.is_err());
            // An over-limit response can close while the fixture is still writing.
            let _ = handle.join();
        }
        let body = json!({"data":[],"has_more":true,"last_id":"same"}).to_string();
        let (url, handle) = server(vec![(200, body.clone()), (200, body)]);
        assert!(
            discover(&url, "test")
                .await
                .unwrap_err()
                .contains("repeated")
        );
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn discovery_is_not_cached_across_credentials() {
        let body = json!({"data":[],"has_more":false}).to_string();
        let (url, handle) = server(vec![(200, body.clone()), (200, body)]);
        discover(&url, "key-one").await.unwrap();
        discover(&url, "key-two").await.unwrap();
        let requests = handle.join().unwrap();
        assert!(requests[0].contains("key-one"));
        assert!(requests[1].contains("key-two"));
    }
}
