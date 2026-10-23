//! Instance-scoped inference authentication without ambient credential fallback.
use agentkit_http::{Authentication, AuthenticationAttempt, AuthenticationProvider, HttpError};
use async_trait::async_trait;
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub endpoint: String,
    pub workload: crate::workload_auth::Config,
}
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceConfig").finish_non_exhaustive()
    }
}
#[derive(Clone, Default)]
pub(crate) struct Instances(Arc<BTreeMap<String, Instance>>);
#[derive(Clone)]
pub(crate) struct Instance {
    pub endpoint: String,
    pub authentication: Authentication,
}
impl Instances {
    pub(crate) fn new(configs: BTreeMap<String, Config>) -> Result<Self, String> {
        let mut instances = BTreeMap::new();
        for (name, config) in configs {
            if !matches!(name.as_str(), "openrouter" | "speakeasy") {
                return Err("workload inference requires a supported completion provider".into());
            }
            let url =
                url::Url::parse(&config.endpoint).map_err(|_| "invalid inference endpoint")?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
                || url.query().is_some()
            {
                return Err(
                    "inference endpoint must be HTTPS without credentials, query or fragment"
                        .into(),
                );
            }
            let session = Arc::new(crate::workload_auth::Session::new(config.workload)?);
            instances.insert(
                name,
                Instance {
                    endpoint: config.endpoint,
                    authentication: Authentication::new(WorkloadAuthentication(session)),
                },
            );
        }
        Ok(Self(Arc::new(instances)))
    }
    pub(crate) fn get(&self, provider: super::ProviderKind) -> Option<&Instance> {
        self.0.get(provider.as_str())
    }
}
struct WorkloadAuthentication(Arc<crate::workload_auth::Session>);
#[async_trait]
impl AuthenticationProvider for WorkloadAuthentication {
    async fn authenticate(
        &self,
        previous: Option<&AuthenticationAttempt>,
    ) -> Result<AuthenticationAttempt, HttpError> {
        // A 401 never replays a potentially billed inference request.
        if previous.is_some() {
            return Err(HttpError::Other(
                "workload inference authentication refused; request not replayed".into(),
            ));
        }
        let token = self.0.acquisition().await.map_err(HttpError::Other)?;
        Authentication::bearer(token.token())
            .authenticate(None)
            .await
    }
}

/// Never surface remote refusal bodies or credential-bearing transport details.
pub(crate) struct Transport(pub reqwest::Client);
#[async_trait]
impl agentkit_http::HttpClient for Transport {
    async fn execute(
        &self,
        request: agentkit_http::HttpRequest,
    ) -> Result<agentkit_http::HttpResponse, HttpError> {
        let response = agentkit_http::HttpClient::execute(&self.0, request)
            .await
            .map_err(|_| HttpError::Other("workload inference transport failed".into()))?;
        if !response.status().is_success() {
            return Err(HttpError::Other(format!(
                "workload inference refused (HTTP {})",
                response.status().as_u16()
            )));
        }
        Ok(response)
    }
}

// HTTP-200 error frames and malformed nonstream responses can echo credentials.
// Typed failures are already sanitized upstream; raw provider errors are not.
pub(crate) fn redact_response_error(error: agentkit_loop::LoopError) -> agentkit_loop::LoopError {
    match error {
        agentkit_loop::LoopError::Provider(_) => {
            agentkit_loop::LoopError::Provider("workload inference response failed".into())
        }
        other => other,
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) fn authentication(
        config: crate::workload_auth::Config,
        client: reqwest::Client,
    ) -> Result<agentkit_http::Authentication, String> {
        Ok(agentkit_http::Authentication::new(
            super::WorkloadAuthentication(std::sync::Arc::new(
                crate::workload_auth::test_support::session(config, client)?,
            )),
        ))
    }
}
