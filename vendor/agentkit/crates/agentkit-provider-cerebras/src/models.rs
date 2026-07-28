//! Thin typed wrapper over `GET /v1/models` and `GET /v1/models/{id}`.
//!
//! Useful to validate a configured model at startup, or to populate a UI with
//! the list of available Cerebras models.

use std::sync::Arc;

use agentkit_http::Http;
use serde::{Deserialize, Serialize};

use crate::config::CerebrasConfig;
use crate::error::CerebrasError;

/// Client over `/v1/models*` bound to an adapter's `Http` + `CerebrasConfig`.
pub struct ModelsClient<'a> {
    http: &'a Http,
    config: Arc<CerebrasConfig>,
}

impl<'a> ModelsClient<'a> {
    /// Builds a new client. Normally constructed via
    /// [`crate::CerebrasAdapter::models`].
    pub fn new(http: &'a Http, config: Arc<CerebrasConfig>) -> Self {
        Self { http, config }
    }

    /// `GET /v1/models` — list every model visible to the API key.
    pub async fn list(&self) -> Result<Vec<ModelObject>, CerebrasError> {
        let url = format!("{}/models", self.config.base_url);
        let response = self
            .http
            .get(&url)
            .bearer_auth(&self.config.api_key)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(CerebrasError::Status { status, body });
        }
        let body: ModelList = response.json().await?;
        Ok(body.data)
    }

    /// `GET /v1/models/{id}` — fetch a single model object.
    pub async fn retrieve(&self, id: &str) -> Result<ModelObject, CerebrasError> {
        let url = format!("{}/models/{id}", self.config.base_url);
        let response = self
            .http
            .get(&url)
            .bearer_auth(&self.config.api_key)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(CerebrasError::Status { status, body });
        }
        Ok(response.json().await?)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ModelList {
    data: Vec<ModelObject>,
}

/// One model entry as returned by `/v1/models`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelObject {
    /// Model identifier.
    pub id: String,
    /// Object kind (always `"model"`).
    #[serde(default)]
    pub object: String,
    /// Creation timestamp (seconds).
    #[serde(default)]
    pub created: u64,
    /// Owner string (e.g. `"cerebras"`).
    #[serde(default)]
    pub owned_by: String,
}
