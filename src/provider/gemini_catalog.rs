//! Authenticated, bounded Gemini model discovery with no shared, cross-credential cache.
use futures_util::StreamExt as _;
use serde::Deserialize;
use std::time::Duration;

pub(super) const MODELS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODELS: usize = 2_000;
const MAX_PAGES: usize = 20;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Model {
    pub name: String,
    pub input_token_limit: Option<u64>,
    #[serde(default)]
    pub supported_generation_methods: Vec<String>,
    pub thinking: Option<bool>,
}

impl Model {
    /// Canonical model ID without the leading `models/` resource prefix.
    pub fn id(&self) -> &str {
        self.name.strip_prefix("models/").unwrap_or(&self.name)
    }

    pub fn context_window(&self) -> Option<u64> {
        self.input_token_limit.filter(|value| *value > 0)
    }

    pub fn supports_generate_content(&self) -> bool {
        self.supported_generation_methods.is_empty()
            || self
                .supported_generation_methods
                .iter()
                .any(|method| method == "generateContent")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    #[serde(default)]
    models: Vec<Model>,
    next_page_token: Option<String>,
}

pub(super) async fn discover(url: &str, key: &str) -> Result<Vec<Model>, String> {
    tokio::time::timeout(Duration::from_secs(5), fetch(url, key))
        .await
        .map_err(|_| "Gemini model discovery timed out".to_owned())?
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| "Could not build Gemini discovery client".into())
}

/// Retrieve metadata for a single Gemini model (accepts `gemini-...` or `models/gemini-...`).
pub(super) async fn retrieve(url: &str, key: &str, model: &str) -> Result<Model, String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let normalized = model.strip_prefix("models/").unwrap_or(model);
        let mut url = reqwest::Url::parse(url).map_err(|_| "Invalid Gemini catalog URL")?;
        url.path_segments_mut()
            .map_err(|_| "Invalid Gemini catalog URL")?
            .pop_if_empty()
            .push(normalized);
        let mut bytes_left = MAX_BYTES;
        let bytes = read_model_response(&client()?, url, key, &mut bytes_left).await?;
        serde_json::from_slice(&bytes).map_err(|_| "Invalid Gemini model metadata".into())
    })
    .await
    .map_err(|_| "Gemini model discovery timed out".to_owned())?
}

/// Only a 404 or an explicit method list omitting `generateContent` proves a
/// model cannot be used; any other failure is inconclusive.
#[cfg(test)]
pub(super) async fn confirmed_unsupported(url: &str, key: &str, model: &str) -> bool {
    match retrieve(url, key, model).await {
        Ok(model) => !model.supports_generate_content(),
        Err(error) => error.contains("HTTP 404"),
    }
}

async fn read_model_response(
    client: &reqwest::Client,
    url: reqwest::Url,
    key: &str,
    bytes_left: &mut usize,
) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .header("x-goog-api-key", key)
        .send()
        .await
        .map_err(|_| "Gemini discovery request failed")?;
    if !response.status().is_success() {
        return Err(format!(
            "Gemini discovery returned HTTP {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > *bytes_left as u64)
    {
        return Err("Gemini catalog exceeds byte limit".into());
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "Could not read Gemini catalog")?;
        if chunk.len() > *bytes_left {
            return Err("Gemini catalog exceeds byte limit".into());
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
        let mut page_url = reqwest::Url::parse(url).map_err(|_| "Invalid Gemini catalog URL")?;
        page_url.query_pairs_mut().append_pair("pageSize", "1000");
        if let Some(cursor) = &cursor {
            page_url.query_pairs_mut().append_pair("pageToken", cursor);
        }
        let bytes = read_model_response(&client, page_url, key, &mut bytes_left).await?;
        let page: Page =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid Gemini model catalog")?;
        model_count += page.models.len();
        if model_count > MAX_MODELS {
            return Err("Gemini catalog exceeds model limit".into());
        }
        models.extend(
            page.models
                .into_iter()
                .filter(|model| super::adapter::valid_model_id(model.id())),
        );
        let Some(next) = page.next_page_token.filter(|token| !token.is_empty()) else {
            return Ok(models);
        };
        if !cursors.insert(next.clone()) {
            return Err("Gemini catalog repeated pagination cursor".into());
        }
        cursor = Some(next);
    }
    Err("Gemini catalog exceeds page limit".into())
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
        let url = format!("http://{}/v1beta/models", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            responses
                .into_iter()
                .map(|(status, body)| {
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    let (mut stream, _) = loop {
                        match listener.accept() {
                            Ok(connection) => break connection,
                            Err(error)
                                if error.kind() == std::io::ErrorKind::WouldBlock
                                    && std::time::Instant::now() < deadline =>
                            {
                                std::thread::sleep(Duration::from_millis(5))
                            }
                            Err(error) => panic!("fixture accept failed: {error}"),
                        }
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        stream.read_exact(&mut byte).unwrap();
                        request.push(byte[0]);
                    }
                    write!(
                        stream,
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nLocation: http://127.0.0.1:1/leak\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    String::from_utf8(request).unwrap()
                })
                .collect()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn retrieve_normalizes_prefix_and_reads_limits() {
        let (url, handle) = server(vec![(
            200,
            json!({
                "name": "models/gemini-2.5-pro",
                "inputTokenLimit": 1048576,
                "outputTokenLimit": 65536,
                "supportedGenerationMethods": ["generateContent", "countTokens"],
                "thinking": true
            })
            .to_string(),
        )]);
        let model = retrieve(&url, "gemini-key", "models/gemini-2.5-pro")
            .await
            .unwrap();
        assert_eq!(model.id(), "gemini-2.5-pro");
        assert_eq!(model.context_window(), Some(1048576));
        assert_eq!(model.thinking, Some(true));
        let requests = handle.join().unwrap();
        assert!(requests[0].starts_with("GET /v1beta/models/gemini-2.5-pro HTTP/1.1"));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("x-goog-api-key: gemini-key\r\n")
        );
        assert!(!requests[0].contains("key="));
    }

    #[tokio::test]
    async fn only_not_found_or_non_generative_confirms_model_is_unsupported() {
        for (status, body, unsupported) in [
            (404, "{}".to_owned(), true),
            (
                200,
                json!({
                    "name": "models/text-embedding-004",
                    "supportedGenerationMethods": ["embedContent"]
                })
                .to_string(),
                true,
            ),
            (401, "{}".to_owned(), false),
            (500, "{}".to_owned(), false),
            (
                200,
                json!({
                    "name": "models/gemini-2.5-pro",
                    "supportedGenerationMethods": ["generateContent"]
                })
                .to_string(),
                false,
            ),
            (
                200,
                json!({"name": "models/gemini-2.5-pro"}).to_string(),
                false,
            ),
        ] {
            let (url, handle) = server(vec![(status, body)]);
            assert_eq!(
                confirmed_unsupported(&url, "test", "gemini-2.5-pro").await,
                unsupported
            );
            let _ = handle.join();
        }
    }

    #[tokio::test]
    async fn paginated_discovery_filters_non_generative_models() {
        let (url, handle) = server(vec![
            (
                200,
                json!({
                    "models": [
                        {
                            "name": "models/gemini-2.5-pro",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "supportedGenerationMethods": ["generateContent", "countTokens"],
                            "thinking": true
                        },
                        {
                            "name": "models/text-embedding-004",
                            "inputTokenLimit": 2048,
                            "supportedGenerationMethods": ["embedContent"]
                        }
                    ],
                    "nextPageToken": "page /+"
                })
                .to_string(),
            ),
            (
                200,
                json!({
                    "models": [
                        {
                            "name": "models/gemini-2.5-flash",
                            "inputTokenLimit": 0,
                            "supportedGenerationMethods": ["generateContent"]
                        }
                    ]
                })
                .to_string(),
            ),
        ]);
        let models = discover(&url, "secret-one").await.unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id(), "gemini-2.5-pro");
        assert!(models[0].supports_generate_content());
        assert_eq!(models[0].context_window(), Some(1048576));
        assert_eq!(models[1].id(), "text-embedding-004");
        assert!(!models[1].supports_generate_content());
        assert_eq!(models[2].id(), "gemini-2.5-flash");
        assert!(models[2].supports_generate_content());
        assert_eq!(models[2].context_window(), None);
        let requests = handle.join().unwrap();
        for request in &requests {
            let lower = request.to_ascii_lowercase();
            assert!(lower.contains("x-goog-api-key: secret-one\r\n"));
            assert!(request.contains("pageSize=1000"));
        }
        assert!(requests[1].contains("pageToken=page+%2F%2B"));
    }

    #[tokio::test]
    async fn failures_are_bounded_and_do_not_follow_redirects() {
        for (status, body) in [
            (401, "{}".to_owned()),
            (302, "{}".to_owned()),
            (200, "not json".to_owned()),
            (200, " ".repeat(MAX_BYTES + 1)),
            (
                200,
                json!({"models": vec![json!({"name": "models/x"}); MAX_MODELS + 1]}).to_string(),
            ),
        ] {
            let (url, handle) = server(vec![(status, body)]);
            assert!(discover(&url, "test").await.is_err());
            let _ = handle.join();
        }
        let body = json!({"models": [], "nextPageToken": "same"}).to_string();
        let (url, handle) = server(vec![(200, body.clone()), (200, body)]);
        assert!(
            discover(&url, "test")
                .await
                .unwrap_err()
                .contains("repeated")
        );
        handle.join().unwrap();
    }
}
