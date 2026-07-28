use async_trait::async_trait;
use futures_util::TryStreamExt;

use crate::{HttpClient, HttpError, HttpRequest, HttpResponse};

#[async_trait]
impl HttpClient for reqwest_middleware::ClientWithMiddleware {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let HttpRequest {
            method,
            url,
            headers,
            body,
        } = request;

        let mut builder = self.request(method, &url).headers(headers);
        if let Some(body) = body {
            builder = builder.body(body);
        }

        let response = builder.send().await.map_err(HttpError::request)?;

        let status = response.status();
        let headers = response.headers().clone();
        let final_url = response.url().to_string();
        let body = Box::pin(response.bytes_stream().map_err(HttpError::body));

        Ok(HttpResponse::new(status, headers, final_url, body))
    }
}
