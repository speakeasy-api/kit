//! Quota-consuming subscription image generation. Never retry a submitted request.
use std::{path::PathBuf, time::Duration};

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{
    credentials::CredentialStorage,
    managed_files::{FileReference, FileStore},
    provider::openai_auth as auth,
};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE: usize = 12 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone)]
pub struct ImageGenTool {
    store: FileStore,
    storage: CredentialStorage,
    spec: ToolSpec,
}

impl ImageGenTool {
    pub fn new(root: PathBuf, storage: CredentialStorage) -> Self {
        let file_schema = super::read_file::file_reference_schema();
        // Edit inputs accept historical references without an exported path.
        let mut input_file_schema = file_schema.clone();
        if let Some(required) = input_file_schema["required"].as_array_mut() {
            required.retain(|field| field != "path");
        }
        input_file_schema["properties"]["path"]["type"] =
            Value::Array(vec![Value::from("string"), Value::from("null")]);
        Self {
            store: FileStore::new(&root), storage,
            spec: ToolSpec::new(ToolName::new("image_gen"),
                "Generate one image with the server-selected ChatGPT subscription image capability, or edit optional session-authorized File references. Requires kit auth login openai; consumes subscription image quota, not API-key billing. Returns a durable File reference with a full absolute path to standalone image bytes. Use shell mv on that path to relocate the image; moving or deleting it does not affect the durable reference. Return the reference from compose to deliver pixels. Maximum 4 input images, 16 MiB total; output at most 8 MiB, 8192 pixels per axis, 16 megapixels. Timeout 180 seconds. Never automatically retry: a failed or cancelled submission may still consume quota.",
                object([
                    ("type", Value::from("object")),
                    ("properties", object([
                        ("prompt", object([
                            ("type", Value::from("string")),
                            ("minLength", Value::from(1)),
                            ("maxLength", Value::from(32000)),
                        ])),
                        ("images", object([
                            ("type", Value::from("array")),
                            ("minItems", Value::from(1)),
                            ("maxItems", Value::from(4)),
                            ("items", input_file_schema),
                        ])),
                    ])),
                    ("required", Value::Array(vec![Value::from("prompt")])),
                    ("additionalProperties", Value::from(false)),
                ]))
                .with_output_schema(file_schema)
                .with_annotations(ToolAnnotations::new()),
        }
    }
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(Map::from_iter(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    prompt: String,
    images: Option<Vec<FileReference>>,
}

fn failure(message: impl Into<String>) -> ToolError {
    ToolError::ExecutionFailed(message.into())
}

fn payload(
    input: Input,
    store: &FileStore,
    session: &str,
) -> Result<(Value, &'static str), ToolError> {
    if input.prompt.trim().is_empty() || input.prompt.len() > 32000 {
        return Err(ToolError::InvalidInput(
            "prompt must contain 1 to 32000 UTF-8 bytes".into(),
        ));
    }
    let mut body = object([
        ("model", Value::from("gpt-image-2")),
        ("prompt", Value::from(input.prompt)),
        ("n", Value::from(1)),
        ("quality", Value::from("auto")),
        ("size", Value::from("auto")),
        ("background", Value::from("auto")),
    ]);
    let endpoint = if let Some(images) = input.images {
        if images.is_empty() || images.len() > 4 {
            return Err(ToolError::InvalidInput(
                "images must contain 1 to 4 File references".into(),
            ));
        }
        let mut total = 0;
        let mut encoded = Vec::new();
        for image in images {
            let bytes = store.resolve(session, &image).map_err(failure)?;
            total += bytes.len();
            if total > 2 * MAX_BYTES {
                return Err(failure("input images exceed 16 MiB"));
            }
            encoded.push(object([(
                "image_url",
                Value::from(format!(
                    "data:{};base64,{}",
                    image.mime_type,
                    STANDARD.encode(bytes)
                )),
            )]));
        }
        body["images"] = Value::Array(encoded);
        "edits"
    } else {
        "generations"
    };
    Ok((body, endpoint))
}

// Submissions consume quota even when delivery fails. Disable redirect replay
// and reqwest's default protocol-NACK retries at the transport boundary.
fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(TIMEOUT)
}

async fn submit(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    account: &str,
    body: &Value,
) -> Result<Vec<u8>, ToolError> {
    let mut response = client
        .post(url)
        .bearer_auth(token)
        .header("ChatGPT-Account-ID", account)
        .header("originator", "kit")
        .json(body)
        .send()
        .await
        .map_err(|_| {
            failure("image submission failed; completion unknown, do not automatically retry")
        })?;
    if !response.status().is_success() {
        return Err(failure(format!(
            "subscription image request returned HTTP {}; not retried",
            response.status().as_u16()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE as u64)
    {
        return Err(failure("image response exceeds 12 MiB; not retried"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failure("image response interrupted; not retried"))?
    {
        if bytes.len() + chunk.len() > MAX_RESPONSE {
            return Err(failure("image response exceeds 12 MiB; not retried"));
        }
        bytes.extend_from_slice(&chunk);
    }
    decode_response(&bytes)
}

fn decode_response(bytes: &[u8]) -> Result<Vec<u8>, ToolError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| failure("invalid image response JSON"))?;
    let encoded = value
        .pointer("/data/0/b64_json")
        .and_then(Value::as_str)
        .ok_or_else(|| failure("image response contains no base64 image"))?;
    if encoded.len() > MAX_BYTES.div_ceil(3) * 4 {
        return Err(failure("generated image exceeds 8 MiB"));
    }
    let image = STANDARD
        .decode(encoded)
        .map_err(|_| failure("invalid generated image encoding"))?;
    if image.len() > MAX_BYTES || !image.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(failure("generated image must be PNG and at most 8 MiB"));
    }
    Ok(image)
}

#[async_trait]
impl Tool for ImageGenTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: Input = serde_json::from_value(request.input).map_err(|_| {
            ToolError::InvalidInput("expected prompt and optional File reference images".into())
        })?;
        let cancellation = context.cancellation.clone();
        let work = async {
            let store = self.store.clone();
            let session = request.session_id.0.clone();
            let (body, endpoint) =
                tokio::task::spawn_blocking(move || payload(input, &store, &session))
                    .await
                    .map_err(|_| failure("image input worker failed"))??;
            let storage = self.storage.clone();
            let credentials = tokio::task::spawn_blocking(move || {
                auth::access_token(&storage, auth::checked_deadline(Duration::from_secs(30))?)
            })
            .await
            .map_err(|_| failure("subscription authentication worker failed"))?
            .map_err(|error| failure(error.to_string()))?;
            // Validate the account/generation contract, as the chat subscription route does.
            credentials
                .binding()
                .map_err(|error| failure(error.to_string()))?;
            let account = credentials
                .account_id()
                .ok_or_else(|| failure("subscription account is missing"))?;
            let client = client_builder()
                .build()
                .map_err(|_| failure("could not initialize image client"))?;
            let bytes = submit(
                &client,
                &format!("https://chatgpt.com/backend-api/codex/images/{endpoint}"),
                credentials.access_token(),
                account,
                &body,
            )
            .await?;
            let store = self.store.clone();
            let session = request.session_id.0;
            let cancellation = cancellation.clone();
            let file = tokio::task::spawn_blocking(move || {
                store.import_bytes(&session, "generated.png", &bytes, cancellation.as_ref())
            })
            .await
            .map_err(|_| failure("generated image storage worker failed"))?
            .map_err(failure)?;
            let value = serde_json::to_value(file)
                .map_err(|_| failure("could not serialize generated File"))?;
            Ok(ToolResult::new(ToolResultPart::success(
                request.call_id,
                ToolOutput::structured(value),
            )))
        };
        let cancelled = std::pin::pin!(async {
            match &context.cancellation {
                Some(c) => c.cancelled().await,
                None => std::future::pending().await,
            }
        });
        let work = std::pin::pin!(tokio::time::timeout(TIMEOUT, work));
        // Poll cancellation first, preserving the biased cancellation priority
        // without select!'s generated panic paths.
        match futures_util::future::select(cancelled, work).await {
            futures_util::future::Either::Left(((), _)) => Err(failure(
                "image generation cancelled; submission may still consume quota, do not automatically retry",
            )),
            futures_util::future::Either::Right((result, _)) => result.unwrap_or_else(|_| {
                Err(failure(
                    "image generation timed out; completion unknown, do not automatically retry",
                ))
            }),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};

    fn png() -> Vec<u8> {
        let mut output = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn generated_files_use_real_storage_and_session_authorization() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::new(root.path());
        let bytes = png();
        let response =
            serde_json::to_vec(&json!({"data":[{"b64_json":STANDARD.encode(&bytes)}]})).unwrap();
        let decoded = decode_response(&response).unwrap();
        let file = store
            .import_bytes("session-a", "generated.png", &decoded, None)
            .unwrap();
        let reopened = FileStore::new(root.path());
        assert_eq!(reopened.resolve("session-a", &file).unwrap(), bytes);
        assert!(reopened.resolve("session-b", &file).is_err());
        let (body, endpoint) = payload(
            Input {
                prompt: "edit".into(),
                images: Some(vec![file.clone()]),
            },
            &reopened,
            "session-a",
        )
        .unwrap();
        assert_eq!(endpoint, "edits");
        assert_eq!(
            body["images"][0]["image_url"],
            format!("data:image/png;base64,{}", STANDARD.encode(bytes))
        );
        assert!(
            payload(
                Input {
                    prompt: "edit".into(),
                    images: Some(vec![file])
                },
                &reopened,
                "session-b"
            )
            .is_err()
        );
        assert!(
            store
                .import_bytes("session-a", "generated.png", b"not an image", None)
                .is_err()
        );
    }

    #[test]
    fn file_schemas_accept_legacy_edit_inputs_but_require_paths_on_outputs() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::new(root.path());
        let file = store
            .import_bytes("schema-session", "generated.png", &png(), None)
            .unwrap();
        let current = serde_json::to_value(&file).unwrap();
        let mut legacy = current.clone();
        legacy.as_object_mut().unwrap().remove("path");
        let tool = ImageGenTool::new(root.path().to_owned(), CredentialStorage::default());
        let output =
            jsonschema::validator_for(tool.spec().output_schema.as_ref().unwrap()).unwrap();
        assert!(output.is_valid(&current));
        assert!(!output.is_valid(&legacy));
        let input_schema = jsonschema::validator_for(&tool.spec().input_schema).unwrap();
        for reference in [current.clone(), legacy] {
            let value = json!({"prompt":"edit", "images":[reference]});
            assert!(input_schema.is_valid(&value));
            let input: Input = serde_json::from_value(value).unwrap();
            assert!(payload(input, &store, "schema-session").is_ok());
        }
        let mut malformed = current;
        malformed["path"] = json!(123);
        let value = json!({"prompt":"edit", "images":[malformed]});
        assert!(!input_schema.is_valid(&value));
        assert!(serde_json::from_value::<Input>(value).is_err());
    }

    #[test]
    fn malformed_and_oversized_responses_are_rejected() {
        for response in [
            b"not json".as_slice(),
            br#"{"data":[]}"#,
            br#"{"data":[{"b64_json":"!!!"}]}"#,
        ] {
            assert!(decode_response(response).is_err());
        }
        let response = serde_json::to_vec(
            &json!({"data":[{"b64_json":"A".repeat(MAX_BYTES.div_ceil(3) * 4 + 1)}]}),
        )
        .unwrap();
        assert!(decode_response(&response).is_err());
    }

    async fn serve(
        status: &str,
        body: String,
        declared_length: Option<usize>,
    ) -> Result<Vec<u8>, ToolError> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/images/generations",
            listener.local_addr().unwrap()
        );
        let status = status.to_owned();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let headers = String::from_utf8(request).unwrap().to_lowercase();
            assert!(headers.starts_with("post /images/generations "));
            assert!(headers.contains("authorization: bearer test-token\r\n"));
            assert!(headers.contains("chatgpt-account-id: test-account\r\n"));
            assert!(headers.contains("originator: kit\r\n"));
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                declared_length.unwrap_or(body.len())
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let client = client_builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let result = submit(&client, &url, "test-token", "test-account", &json!({})).await;
        server.join().unwrap();
        result
    }

    // Reqwest's HTTP/2 feature is not enabled in this dependency graph, so
    // REFUSED_STREAM coverage would require dependency changes. Exercise the
    // available HTTP/1 boundary: redirect and disconnect after a complete POST.
    #[tokio::test]
    async fn transport_does_not_replay_submissions() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for redirect in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!(
                "http://{}/images/generations",
                listener.local_addr().unwrap()
            );
            let location = url.clone();
            let (done, finished) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                tokio::pin!(finished);
                let mut requests = Vec::new();
                loop {
                    let (mut stream, _) = tokio::select! {
                        biased;
                        accepted = listener.accept() => accepted.unwrap(),
                        _ = &mut finished => break,
                    };
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(stream.read_u8().await.unwrap());
                    }
                    let headers = String::from_utf8(request).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    let mut body = vec![0; length];
                    stream.read_exact(&mut body).await.unwrap();
                    requests.push((headers, body));
                    if redirect {
                        stream
                            .write_all(
                                format!(
                                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                )
                                .as_bytes(),
                            )
                            .await
                            .unwrap();
                    }
                    stream.shutdown().await.unwrap();
                }
                requests
            });
            let client = client_builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            let body = json!({"prompt": "no replay"});
            let result = submit(&client, &url, "test-token", "test-account", &body).await;
            done.send(()).unwrap();
            let requests = server.await.unwrap();
            assert!(result.is_err());
            assert_eq!(
                requests.len(),
                1,
                "submission replayed (redirect={redirect})"
            );
            assert!(requests[0].0.starts_with("POST /images/generations "));
            assert_eq!(
                serde_json::from_slice::<Value>(&requests[0].1).unwrap(),
                body
            );
            if redirect {
                assert!(result.unwrap_err().to_string().contains("307"));
            }
        }
    }

    #[tokio::test]
    async fn http_boundary_sends_subscription_headers_and_bounds_response() {
        let bytes = png();
        let body = json!({"data":[{"b64_json":STANDARD.encode(&bytes)}]}).to_string();
        assert_eq!(serve("200 OK", body, None).await.unwrap(), bytes);
        let error = serve("429 Too Many Requests", "secret server error".into(), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("429"));
        assert!(!error.contains("secret"));
        assert!(
            serve("200 OK", String::new(), Some(MAX_RESPONSE + 1))
                .await
                .is_err()
        );
        assert!(serve("200 OK", "{".into(), Some(100)).await.is_err());
    }
}
