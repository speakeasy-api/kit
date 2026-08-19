pub(crate) use super::openai_auth::{AuthError as CredentialError, CredentialBinding, TokenRecord};

use std::time::Instant;

pub(crate) fn access_token(deadline: Instant) -> Result<TokenRecord, CredentialError> {
    super::openai_auth::access_token(deadline)
}

pub(crate) fn refresh_after_unauthorized(
    rejected_access_token: &str,
    deadline: Instant,
) -> Result<TokenRecord, CredentialError> {
    super::openai_auth::refresh_after_unauthorized(rejected_access_token, deadline)
}
