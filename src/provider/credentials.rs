pub(crate) use super::openai_auth::{AuthError as CredentialError, CredentialBinding, TokenRecord};

use std::time::Instant;

pub(crate) fn access_token(
    storage: &crate::credentials::CredentialStorage,
    deadline: Instant,
) -> Result<TokenRecord, CredentialError> {
    super::openai_auth::access_token(storage, deadline)
}

pub(crate) fn refresh_after_unauthorized(
    storage: &crate::credentials::CredentialStorage,
    rejected_access_token: &str,
    deadline: Instant,
) -> Result<TokenRecord, CredentialError> {
    super::openai_auth::refresh_after_unauthorized(storage, rejected_access_token, deadline)
}
