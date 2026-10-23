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
        let token = self.0.acquisition().await.map_err(|error| {
            let _ = REQUEST
                .try_with(|state| state.borrow_mut().authentication_error = Some(error.clone()));
            HttpError::Other(error)
        })?;
        let _ = REQUEST.try_with(|state| {
            state.borrow_mut().acquisition = Some((self.0.clone(), token.clone()));
        });
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
        if matches!(response.status().as_u16(), 401 | 403) {
            let rejected = REQUEST
                .try_with(|state| state.borrow().acquisition.clone())
                .ok()
                .flatten();
            if let Some((session, acquisition)) = rejected {
                session.invalidate_acquisition(&acquisition).await;
            }
        }
        if !response.status().is_success() {
            return Err(HttpError::Other(format!(
                "workload inference refused (HTTP {})",
                response.status().as_u16()
            )));
        }
        Ok(response)
    }
}

// Authentication and transport run in the same begin-turn future. Task-scoped
// state preserves the exact acquisition and trusted local error without a token
// lookup, cross-session mutable state, or exposing credentials in request headers.
#[derive(Default)]
struct RequestState {
    acquisition: Option<(
        Arc<crate::workload_auth::Session>,
        crate::workload_auth::Acquisition,
    )>,
    authentication_error: Option<String>,
}
tokio::task_local! {
    static REQUEST: std::cell::RefCell<RequestState>;
}

pub(crate) async fn request<T>(
    future: impl std::future::Future<Output = Result<T, agentkit_loop::LoopError>>,
) -> Result<T, agentkit_loop::LoopError> {
    REQUEST
        .scope(Default::default(), async {
            future.await.map_err(|error| {
                match REQUEST.with(|state| state.borrow_mut().authentication_error.take()) {
                    Some(message) => agentkit_loop::LoopError::Provider(message),
                    None => redact_response_error(error),
                }
            })
        })
        .await
}

pub(crate) fn resilience(
    budget: crate::request_budget::RequestBudget,
) -> agentkit_http::ResilienceConfig {
    let mut config = budget.resilience();
    config.max_retries = 0;
    config
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
