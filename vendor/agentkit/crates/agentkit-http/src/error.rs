use std::error::Error as StdError;

use thiserror::Error;

pub type BoxError = Box<dyn StdError + Send + Sync>;

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("invalid header: {0}")]
    InvalidHeader(String),

    #[error("request body serialization failed: {0}")]
    Serialize(#[source] serde_json::Error),

    #[error("response body deserialization failed: {0}")]
    Deserialize(#[source] serde_json::Error),

    #[error("request failed: {0}")]
    Request(#[source] BoxError),

    #[error("response body read failed: {0}")]
    Body(#[source] BoxError),

    #[error("{0}")]
    Other(String),
}

impl HttpError {
    pub fn request<E>(err: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::Request(Box::new(err))
    }

    pub fn body<E>(err: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::Body(Box::new(err))
    }
}
