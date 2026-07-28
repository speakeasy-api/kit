use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use serde::Serialize;

use crate::{HttpClient, HttpError, HttpResponse};

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
}

impl HttpRequest {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: HeaderMap::new(),
            body: None,
        }
    }
}

pub struct HttpRequestBuilder {
    client: Arc<dyn HttpClient>,
    request: Result<HttpRequest, HttpError>,
}

impl HttpRequestBuilder {
    pub fn new(client: Arc<dyn HttpClient>, method: Method, url: impl Into<String>) -> Self {
        Self {
            client,
            request: Ok(HttpRequest::new(method, url)),
        }
    }

    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: std::fmt::Display,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: std::fmt::Display,
    {
        if let Ok(req) = self.request.as_mut() {
            match (HeaderName::try_from(key), HeaderValue::try_from(value)) {
                (Ok(name), Ok(val)) => {
                    req.headers.append(name, val);
                }
                (Err(e), _) => {
                    self.request = Err(HttpError::InvalidHeader(format!("header name: {e}")));
                }
                (_, Err(e)) => {
                    self.request = Err(HttpError::InvalidHeader(format!("header value: {e}")));
                }
            }
        }
        self
    }

    pub fn headers(mut self, headers: HeaderMap) -> Self {
        if let Ok(req) = self.request.as_mut() {
            req.headers.extend(headers);
        }
        self
    }

    pub fn bearer_auth(self, token: impl std::fmt::Display) -> Self {
        self.header(http::header::AUTHORIZATION, format!("Bearer {token}"))
    }

    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        if let Ok(req) = self.request.as_mut() {
            req.body = Some(body.into());
        }
        self
    }

    pub fn json<T: Serialize + ?Sized>(mut self, value: &T) -> Self {
        if let Ok(req) = self.request.as_mut() {
            match serde_json::to_vec(value) {
                Ok(bytes) => {
                    if !req.headers.contains_key(http::header::CONTENT_TYPE) {
                        req.headers.insert(
                            http::header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        );
                    }
                    req.body = Some(Bytes::from(bytes));
                }
                Err(e) => {
                    self.request = Err(HttpError::Serialize(e));
                }
            }
        }
        self
    }

    pub fn build(self) -> Result<HttpRequest, HttpError> {
        self.request
    }

    pub async fn send(self) -> Result<HttpResponse, HttpError> {
        let Self { client, request } = self;
        let request = request?;
        client.execute(request).await
    }
}
