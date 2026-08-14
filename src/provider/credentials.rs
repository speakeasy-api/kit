use std::{env, fs, path::PathBuf, time::Instant};

use serde::Deserialize;
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Clone, Debug, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub(crate) struct CredentialBinding {
    pub(crate) account_id: String,
    pub(crate) generation: String,
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub(crate) struct TokenRecord {
    access_token: String,
    account_id: Option<String>,
    generation: String,
}

impl std::fmt::Debug for TokenRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenRecord")
            .field("access_token", &"[REDACTED]")
            .field("account_id", &self.account_id)
            .finish()
    }
}

impl TokenRecord {
    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }

    pub(crate) fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub(crate) fn binding(&self) -> Result<CredentialBinding, CredentialError> {
        let account_id = self
            .account_id
            .clone()
            .filter(|value| !value.is_empty() && value.len() <= 256 && value.is_ascii())
            .ok_or_else(|| {
                CredentialError("Codex credentials have no ChatGPT account ID".into())
            })?;
        Ok(CredentialBinding {
            account_id,
            generation: self.generation.clone(),
        })
    }

    #[cfg(any())]
    pub(crate) fn for_test(access_token: &str, account_id: &str) -> Self {
        Self::for_test_generation(access_token, account_id, "codex-auth")
    }

    #[cfg(any())]
    pub(crate) fn for_test_generation(
        access_token: &str,
        account_id: &str,
        generation: &str,
    ) -> Self {
        Self {
            access_token: access_token.into(),
            account_id: Some(account_id.into()),
            generation: generation.into(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct CredentialError(String);

impl std::fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CredentialError {}

#[derive(Deserialize)]
struct AuthFile {
    tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: String,
    #[serde(alias = "accountId")]
    account_id: Option<String>,
}

pub(crate) fn access_token(_deadline: Instant) -> Result<TokenRecord, CredentialError> {
    let path = auth_path()?;
    let bytes = fs::read(&path).map_err(|error| {
        CredentialError(format!(
            "could not read {}: {error}; run `codex login` first",
            path.display()
        ))
    })?;
    let auth: AuthFile = serde_json::from_slice(&bytes)
        .map_err(|error| CredentialError(format!("invalid {}: {error}", path.display())))?;
    let tokens = auth.tokens.ok_or_else(|| {
        CredentialError(format!(
            "{} has no ChatGPT subscription tokens; run `codex login`",
            path.display()
        ))
    })?;
    if tokens.access_token.is_empty() {
        return Err(CredentialError("Codex access token is empty".into()));
    }
    Ok(TokenRecord {
        access_token: tokens.access_token,
        account_id: tokens.account_id,
        generation: "codex-auth".into(),
    })
}

pub(crate) fn refresh_after_unauthorized(
    rejected_access_token: &str,
    deadline: Instant,
) -> Result<TokenRecord, CredentialError> {
    let record = access_token(deadline)?;
    if record.access_token == rejected_access_token {
        return Err(CredentialError(
            "ChatGPT rejected the Codex credential; run `codex login` to refresh it".into(),
        ));
    }
    Ok(record)
}

fn auth_path() -> Result<PathBuf, CredentialError> {
    if let Some(path) = env::var_os("KIT_CODEX_AUTH") {
        return Ok(path.into());
    }
    if let Some(home) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home).join("auth.json"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".codex/auth.json"))
        .ok_or_else(|| CredentialError("HOME and CODEX_HOME are unset".into()))
}
