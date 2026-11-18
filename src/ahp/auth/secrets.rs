//! Client secrets issued by dynamic client registration.
use agenthooksprotocol::{
    auth::{
        AuthErrorCode,
        managed::{ClientSecret, ClientSecretStorage, OperationBudget},
    },
    client::{HookError, LocalFuture},
};
use zeroize::Zeroizing;

use super::store::{SECRETS, Store};

fn unavailable() -> HookError {
    HookError::authentication(AuthErrorCode::CredentialUnavailable)
}

/// Kept in Kit's credential storage beside the backend's records, under the
/// SDK-chosen reference, and removed with them on logout.
pub(super) struct Secrets(pub(super) Store);

impl ClientSecretStorage for Secrets {
    fn store(
        &self,
        reference: String,
        secret: ClientSecret,
        budget: OperationBudget,
    ) -> LocalFuture<'_, Result<(), HookError>> {
        Box::pin(async move {
            self.0.remember(SECRETS, &reference, budget).await?;
            let _lock = self.0.lock(SECRETS, &reference, budget).await?;
            let entry = self.0.storage().entry(SECRETS, &reference);
            let bytes = Zeroizing::new(secret.secret().as_bytes().to_vec());
            tokio::task::spawn_blocking(move || entry.save(&bytes))
                .await
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())
        })
    }

    fn resolve(
        &self,
        reference: String,
        budget: OperationBudget,
    ) -> LocalFuture<'_, Result<Option<ClientSecret>, HookError>> {
        Box::pin(async move {
            let _lock = self.0.lock(SECRETS, &reference, budget).await?;
            let entry = self.0.storage().entry(SECRETS, &reference);
            let bytes = tokio::task::spawn_blocking(move || entry.load())
                .await
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())?;
            Ok(bytes
                .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
                .map(ClientSecret::new))
        })
    }

    fn delete(
        &self,
        reference: String,
        budget: OperationBudget,
    ) -> LocalFuture<'_, Result<(), HookError>> {
        Box::pin(async move {
            let _lock = self.0.lock(SECRETS, &reference, budget).await?;
            let entry = self.0.storage().entry(SECRETS, &reference);
            tokio::task::spawn_blocking(move || entry.delete())
                .await
                .map_err(|_| unavailable())?
                .map(|_| ())
                .map_err(|_| unavailable())
        })
    }
}

/// Secrets the user names by reference in hooks.json (`clientSecretRef`,
/// `initialAccessTokenRef`, a bearer `tokenRef`): `~/.kit/ahp-secrets.json`,
/// `{ "version": 1, "secrets": { "<ref>": "<secret>" } }`, readable only by
/// its owner. The same layout other AHP adapters keep in their own directory.
pub(crate) fn references() -> std::collections::BTreeMap<String, String> {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return Default::default();
    };
    let path = std::path::PathBuf::from(home).join(".kit/ahp-secrets.json");
    let Ok(bytes) = crate::config_files::read(&path) else {
        return Default::default();
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.permissions().mode() & 0o077 != 0)
        {
            eprintln!(
                "kit: ignoring {}: it is readable by other users; chmod 600 it",
                path.display()
            );
            return Default::default();
        }
    }
    #[derive(serde::Deserialize)]
    struct File {
        version: u32,
        #[serde(default)]
        secrets: std::collections::BTreeMap<String, String>,
    }
    match serde_json::from_slice::<File>(&bytes) {
        Ok(file) if file.version == 1 => file.secrets,
        _ => {
            eprintln!(
                "kit: ignoring {}: not a version 1 secrets file",
                path.display()
            );
            Default::default()
        }
    }
}
