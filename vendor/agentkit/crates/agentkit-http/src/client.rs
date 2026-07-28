use std::sync::Arc;

use async_trait::async_trait;
use http::Method;

use crate::{HttpError, HttpRequest, HttpRequestBuilder, HttpResponse};

/// Send a request, return a streaming response.
///
/// Contract:
/// - [`HttpResponse::url`] must be the post-redirect URL — streamable-HTTP
///   transports resolve relative endpoints against it.
/// - The body must be surfaced as a stream, not buffered — SSE consumers
///   pull chunks incrementally.
/// - Use [`HttpError::Request`] for connect/transport failures,
///   [`HttpError::Body`] for errors that surface mid-stream.
#[async_trait]
pub trait HttpClient: Send + Sync + 'static {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, HttpError>;
}

/// Clone-cheap handle over an [`HttpClient`]. Methods mirror `reqwest::Client`.
#[derive(Clone)]
pub struct Http {
    inner: Arc<dyn HttpClient>,
}

impl Http {
    pub fn new<C: HttpClient>(client: C) -> Self {
        Self {
            inner: Arc::new(client),
        }
    }

    pub fn from_arc(inner: Arc<dyn HttpClient>) -> Self {
        Self { inner }
    }

    pub fn as_arc(&self) -> &Arc<dyn HttpClient> {
        &self.inner
    }

    pub fn request(&self, method: Method, url: impl Into<String>) -> HttpRequestBuilder {
        HttpRequestBuilder::new(self.inner.clone(), method, url)
    }

    pub fn get(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request(Method::GET, url)
    }

    pub fn post(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request(Method::POST, url)
    }

    pub fn put(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request(Method::PUT, url)
    }

    pub fn delete(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request(Method::DELETE, url)
    }

    pub fn patch(&self, url: impl Into<String>) -> HttpRequestBuilder {
        self.request(Method::PATCH, url)
    }

    pub async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.inner.execute(request).await
    }
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http").finish_non_exhaustive()
    }
}
