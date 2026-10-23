//! Per-request workload authentication, including catalog reads and SSE reconnects.
use std::{collections::HashMap, sync::Arc};

use agentkit_mcp::{ClientJsonRpcMessage, McpHttpClient, McpSseStream};
use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};

use crate::workload_auth::{Config, Session};

type Error = StreamableHttpError<reqwest::Error>;

pub(super) struct Client {
    destination: String,
    session: Session,
    http: reqwest::Client,
}

impl Client {
    pub(super) fn new(destination: &str, config: Config) -> Result<Self, String> {
        config.validate()?;
        let url = url::Url::parse(destination).map_err(|_| "invalid workload MCP URL")?;
        let resource =
            url::Url::parse(&config.resource).map_err(|_| "invalid workload MCP resource")?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url != resource
        {
            return Err(
                "workload MCP URL must be HTTPS and match the configured resource exactly".into(),
            );
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "could not build workload MCP HTTP client")?;
        Ok(Self {
            destination: destination.to_owned(),
            session: Session::new(config)?,
            http,
        })
    }

    async fn authorization(
        &self,
        uri: &str,
        headers: &HashMap<HeaderName, HeaderValue>,
    ) -> Result<Option<String>, Error> {
        if uri != self.destination
            || headers
                .keys()
                .any(|name| name == reqwest::header::AUTHORIZATION || name == reqwest::header::HOST)
        {
            return Err(Self::error(
                "workload MCP transport destination or authorization override rejected",
            ));
        }
        self.session.token().await.map(Some).map_err(Self::error)
    }

    fn error(message: impl Into<String>) -> Error {
        StreamableHttpError::Io(std::io::Error::other(message.into()))
    }

    async fn finish<T>(&self, result: Result<T, Error>) -> Result<T, Error> {
        let rejected = match &result {
            Err(
                StreamableHttpError::AuthRequired(_) | StreamableHttpError::InsufficientScope(_),
            ) => true,
            Err(StreamableHttpError::Client(error)) => matches!(
                error.status(),
                Some(reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN)
            ),
            _ => false,
        };
        if rejected {
            // Do not replay a possibly side-effecting POST or follow challenge URLs.
            self.session.invalidate().await;
        }
        result
    }
}

#[async_trait]
impl McpHttpClient for Client {
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Error> {
        let authorization = self.authorization(&uri, &custom_headers).await?;
        self.finish(
            self.http
                .post_message(uri, message, session_id, authorization, custom_headers)
                .await,
        )
        .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Error> {
        let authorization = self.authorization(&uri, &custom_headers).await?;
        self.finish(
            self.http
                .delete_session(uri, session_id, authorization, custom_headers)
                .await,
        )
        .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<McpSseStream, Error> {
        let authorization = self.authorization(&uri, &custom_headers).await?;
        self.finish(
            self.http
                .get_stream(
                    uri,
                    session_id,
                    last_event_id,
                    authorization,
                    custom_headers,
                )
                .await,
        )
        .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_json::from_value(serde_json::json!({
            "tokenFile": "/nonexistent/kit-workload-test-token",
            "expectedIssuer": "https://issuer.example",
            "tokenEndpoint": "https://issuer.example/token",
            "resource": "https://mcp.example/mcp"
        }))
        .unwrap()
    }

    #[test]
    fn workload_destination_is_bound_to_resource() {
        for destination in [
            "http://mcp.example/mcp",
            "https://other.example/mcp",
            "https://mcp.example/other",
            "https://user@mcp.example/mcp",
            "https://mcp.example/mcp#fragment",
        ] {
            assert!(Client::new(destination, config()).is_err());
        }
        assert!(Client::new("https://mcp.example/mcp", config()).is_ok());
    }

    #[tokio::test]
    async fn workload_transport_rejects_overrides_before_acquisition() {
        let client = Client::new("https://mcp.example/mcp", config()).unwrap();
        let error = client
            .authorization("https://other.example/mcp", &HashMap::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("override rejected"));
        for name in [reqwest::header::AUTHORIZATION, reqwest::header::HOST] {
            let headers = HashMap::from([(name, HeaderValue::from_static("override"))]);
            assert!(
                client
                    .authorization("https://mcp.example/mcp", &headers)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("override rejected")
            );
        }
        assert!(
            client
                .authorization("https://mcp.example/mcp", &HashMap::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn workload_prepared_transport_requires_credentials_before_http() {
        let value = serde_json::json!({
            "url": "https://mcp.example/mcp",
            "auth": {
                "type": "workload_identity",
                "tokenFile": "/nonexistent/kit-workload-test-token",
                "expectedIssuer": "https://issuer.example",
                "tokenEndpoint": "https://issuer.example/token",
                "resource": "https://mcp.example/mcp"
            }
        });
        let server = serde_json::from_value(value.clone()).unwrap();
        let (binding, _) =
            super::super::prepare_server("workload", server, Vec::new(), None).unwrap();
        let super::super::McpTransportBinding::StreamableHttp(transport) = binding else {
            unreachable!()
        };
        let client = transport.http_client.unwrap();
        assert!(
            client
                .get_stream(
                    Arc::from("https://mcp.example/mcp"),
                    None,
                    None,
                    None,
                    HashMap::new()
                )
                .await
                .is_err()
        );
        for field in ["bearerToken", "headers"] {
            let mut conflict = value.clone();
            conflict[field] = if field == "headers" {
                serde_json::json!({"Authorization": "Bearer static"})
            } else {
                serde_json::json!("static")
            };
            let server = serde_json::from_value(conflict).unwrap();
            assert!(super::super::prepare_server("workload", server, Vec::new(), None).is_err());
        }
    }

    #[tokio::test]
    async fn workload_configuration_never_uses_stored_oauth() {
        let mut value = serde_json::json!({
            "type": "workload_identity",
            "tokenFile": "/nonexistent/kit-workload-test-token",
            "expectedIssuer": "https://issuer.example",
            "tokenEndpoint": "https://issuer.example/token",
            "resource": "https://mcp.example/mcp"
        });
        let config: super::super::auth::Config = serde_json::from_value(value.clone()).unwrap();
        assert!(config.workload().is_some());
        assert!(config.machine().is_none());
        let storage = super::super::CredentialStorage::Memory;
        assert!(
            super::super::auth::restore("https://mcp.example/mcp", &config, &storage)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            super::super::auth::begin("https://mcp.example/mcp", &config, &storage, None, None)
                .await
                .is_err()
        );
        value["clientId"] = serde_json::json!("not-supported");
        assert!(serde_json::from_value::<super::super::auth::Config>(value).is_err());
    }
}
