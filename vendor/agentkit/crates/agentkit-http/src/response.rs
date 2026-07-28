use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream::BoxStream};
use http::{HeaderMap, StatusCode};
use serde::de::DeserializeOwned;

use crate::HttpError;

pub type BodyStream = BoxStream<'static, Result<Bytes, HttpError>>;

pub struct HttpResponse {
    status: StatusCode,
    headers: HeaderMap,
    final_url: String,
    body: BodyStream,
}

impl HttpResponse {
    pub fn new(
        status: StatusCode,
        headers: HeaderMap,
        final_url: String,
        body: BodyStream,
    ) -> Self {
        Self {
            status,
            headers,
            final_url,
            body,
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn url(&self) -> &str {
        &self.final_url
    }

    pub fn bytes_stream(self) -> BodyStream {
        self.body
    }

    pub async fn bytes(self) -> Result<Bytes, HttpError> {
        let mut stream = self.body;
        let mut buf = BytesMut::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(buf.freeze())
    }

    pub async fn text(self) -> Result<String, HttpError> {
        let bytes = self.bytes().await?;
        String::from_utf8(bytes.to_vec()).map_err(|e| HttpError::Body(Box::new(e)))
    }

    pub async fn json<T: DeserializeOwned>(self) -> Result<T, HttpError> {
        let bytes = self.bytes().await?;
        serde_json::from_slice(&bytes).map_err(HttpError::Deserialize)
    }

    pub fn error_for_status(self) -> Result<Self, HttpError> {
        if self.status.is_client_error() || self.status.is_server_error() {
            Err(HttpError::Other(format!(
                "response returned status {}",
                self.status
            )))
        } else {
            Ok(self)
        }
    }
}
