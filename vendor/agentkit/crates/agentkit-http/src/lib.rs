//! HTTP transport trait and request/response types.
//!
//! Implementors satisfy [`HttpClient::execute`]; [`HttpRequestBuilder`] and
//! [`HttpResponse`] carry the ergonomics (body encoding, header helpers,
//! streaming). Enable the `reqwest-client` feature (default) for an
//! `impl HttpClient for reqwest::Client`; disable it to compile trait-only.

mod client;
mod error;
mod request;
mod response;

#[cfg(feature = "reqwest-client")]
mod reqwest_impl;

#[cfg(feature = "reqwest-middleware-client")]
mod reqwest_middleware_impl;

pub use client::{Http, HttpClient};
pub use error::{BoxError, HttpError};
pub use request::{HttpRequest, HttpRequestBuilder};
pub use response::{BodyStream, HttpResponse};

pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_util::stream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubClient {
        calls: AtomicUsize,
        status: StatusCode,
        body: Bytes,
        expected_body: Option<Bytes>,
    }

    #[async_trait]
    impl HttpClient for StubClient {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(expected) = &self.expected_body {
                assert_eq!(request.body.as_deref(), Some(expected.as_ref()));
            }
            let body = self.body.clone();
            let stream = stream::once(async move { Ok::<_, HttpError>(body) });
            Ok(HttpResponse::new(
                self.status,
                request.headers.clone(),
                request.url.clone(),
                Box::pin(stream),
            ))
        }
    }

    #[tokio::test]
    async fn builder_sends_json_body_and_decodes_response() {
        #[derive(serde::Serialize)]
        struct Req {
            name: &'static str,
        }
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Resp {
            ok: bool,
        }

        let stub = StubClient {
            calls: AtomicUsize::new(0),
            status: StatusCode::OK,
            body: Bytes::from_static(br#"{"ok":true}"#),
            expected_body: Some(Bytes::from_static(br#"{"name":"agentkit"}"#)),
        };
        let http = Http::from_arc(Arc::new(stub));

        let resp = http
            .post("https://example.test/echo")
            .bearer_auth("tok")
            .json(&Req { name: "agentkit" })
            .send()
            .await
            .expect("send");

        assert_eq!(resp.status(), StatusCode::OK);
        let auth = resp.headers().get(http::header::AUTHORIZATION).unwrap();
        assert_eq!(auth, "Bearer tok");
        let ct = resp.headers().get(http::header::CONTENT_TYPE).unwrap();
        assert_eq!(ct, "application/json");

        let decoded: Resp = resp.json().await.expect("json");
        assert_eq!(decoded, Resp { ok: true });
    }

    #[tokio::test]
    async fn error_for_status_flags_4xx() {
        let stub = StubClient {
            calls: AtomicUsize::new(0),
            status: StatusCode::BAD_REQUEST,
            body: Bytes::from_static(b"nope"),
            expected_body: None,
        };
        let http = Http::from_arc(Arc::new(stub));

        let resp = http.get("https://example.test").send().await.unwrap();
        assert!(resp.error_for_status().is_err());
    }
}
