//! Hook backend authentication through the AHP SDK's managed OAuth.
//!
//! Every hooks.json backend has its own OAuth state: its own grant (the grant
//! partition is the backend's scope, so its event and upload endpoints share one
//! consent) and its own pending logins. As in Kit's MCP sign-in, each login
//! listens on its own loopback port and is completed by the process that
//! prepared it; grants are shared with other Kit processes through the
//! credential store, and the newest completed sign-in wins.
//! Delivery never signs in. A delivery or configuration that needs a login
//! fails under the subscription's failure policy and is recorded; the next
//! turn prepares the login in the background and shows its link to the user.
mod config;
mod login;
mod network;
mod policy;
mod secrets;
mod store;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use agenthooksprotocol::{
    adapters::registered::{AuthChallenge, AuthContext, AuthProvider, BearerCredential},
    auth::{
        AuthBinding, AuthErrorCode, AuthProviders, AuthRecovery, BearerAuth, OAuthFlow,
        managed::{
            ClientSecret, ManagedOAuth, OAuthClientConfig, OperationBudget, Preparation,
            PrepareOptions, RegistrationPolicy, TokenAuthMethod,
        },
    },
    client::{HookError, LocalFuture},
};
use serde_json::Value;

use crate::credentials::CredentialStorage;
pub(crate) use login::{AuthHint, HOOK_AUTH_META, HintSubscription, notices};

/// The credential namespace of Kit's hook grants.
const NAMESPACE: &str = "kit";
const OPERATION: Duration = Duration::from_secs(30);
const CLIENT_NAME: &str = "Kit (Agent Hooks Protocol)";
const SOFTWARE_ID: &str = "dev.kit.ahp";

/// One registered backend, as far as authentication is concerned.
#[derive(Clone)]
struct Backend {
    id: String,
    /// The backend id within the hooks file that declares it: the key of
    /// its grant, its stored records and its hints.
    scope: String,
    /// The HTTP transport URL; stdio backends never authenticate.
    url: Option<String>,
    preset: Option<Value>,
    /// `transport.headers` sets Authorization: the SDK sends only that.
    header_authorization: bool,
    settings: config::Settings,
}

impl Backend {
    fn parse(value: &Value, source: &str) -> (Self, Vec<String>) {
        let (settings, problems) = config::Settings::parse(value);
        let transport = &value["transport"];
        let id = value["id"].as_str().unwrap_or_default().to_owned();
        let backend = Self {
            scope: format!("{source}#{id}"),
            id,
            url: (transport["type"] == "http")
                .then(|| transport["url"].as_str().map(str::to_owned))
                .flatten(),
            preset: value
                .get("authentication")
                .filter(|auth| auth.is_object())
                .cloned(),
            header_authorization: transport["headers"].as_object().is_some_and(|headers| {
                headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("authorization"))
            }),
            settings,
        };
        (backend, problems)
    }

    /// OAuth is expected here: an OAuth preset or an `oauth` object.
    fn expects_oauth(&self) -> bool {
        !self.header_authorization
            && self.url.is_some()
            && (self.settings.configured
                || self
                    .preset
                    .as_ref()
                    .is_some_and(|preset| preset["type"] == "oauth"))
    }

    fn context(&self) -> Option<AuthContext> {
        let url = self.url.as_ref()?;
        Some(login::event_context(&self.id, url, self.preset.clone()))
    }
}

/// Backend scope, whether the flow is PKCE, the preset's client, and the
/// target's sorted scopes.
type ManagerKey = (String, bool, Option<String>, Vec<String>);

/// Where hook credentials live: Kit's credential storage.
#[derive(Clone)]
pub(crate) struct HookAuth(Arc<Inner>);

struct Inner {
    storage: CredentialStorage,
    backends: BTreeMap<String, Backend>,
    /// One SDK provider per backend, flow and preset client.
    managers: Mutex<HashMap<ManagerKey, Arc<ManagedOAuth>>>,
    /// Secret references from `~/.kit/ahp-secrets.json`.
    references: BTreeMap<String, String>,
    /// Starts the HTTP client of each OAuth request.
    client: network::ClientBuilder,
}

fn fail(code: AuthErrorCode) -> HookError {
    HookError::authentication(code)
}

fn budget() -> OperationBudget {
    OperationBudget::until(Instant::now() + OPERATION)
}

fn with_deadline(context: &AuthContext) -> AuthContext {
    AuthContext {
        deadline: Instant::now() + OPERATION,
        ..context.clone()
    }
}

/// Whether an error asks for the user to sign in to that backend.
fn needs_login(error: &HookError) -> bool {
    error.auth_recovery() == Some(AuthRecovery::Login)
}

impl HookAuth {
    /// Reads every backend of the effective registration. `sources` names
    /// the hooks file that declares each backend id.
    pub(crate) fn new(
        storage: CredentialStorage,
        registration: &Value,
        sources: &BTreeMap<String, String>,
    ) -> Self {
        Self::with_client(storage, registration, sources, reqwest::Client::builder)
    }

    fn with_client(
        storage: CredentialStorage,
        registration: &Value,
        sources: &BTreeMap<String, String>,
        client: network::ClientBuilder,
    ) -> Self {
        let mut backends = BTreeMap::new();
        for value in registration["hooks"].as_array().into_iter().flatten() {
            let id = value["id"].as_str().unwrap_or_default();
            let source = sources.get(id).map_or("", String::as_str);
            let (backend, problems) = Backend::parse(value, source);
            for problem in problems {
                eprintln!("kit: hooks.json: {problem}");
            }
            backends.insert(backend.id.clone(), backend);
        }
        Self(Arc::new(Inner {
            storage,
            backends,
            managers: Mutex::default(),
            references: secrets::references(),
            client,
        }))
    }

    pub(crate) fn may_sign_in(&self) -> bool {
        self.0.backends.values().any(|backend| {
            backend.url.is_some()
                && !backend.header_authorization
                && backend
                    .preset
                    .as_ref()
                    .is_none_or(|preset| preset["type"] != "bearer")
        })
    }

    /// Credential references for bearer `tokenRef` bindings.
    pub(crate) fn references(&self) -> BTreeMap<String, String> {
        self.0.references.clone()
    }

    /// Kit's hook authentication in `options`: bearer references, the
    /// providers, and configuration skips as a source of needed logins.
    pub(crate) fn install(
        &self,
        mut options: agenthooksprotocol::hooks::HooksOptions,
    ) -> agenthooksprotocol::hooks::HooksOptions {
        options.backend.credentials = self.references();
        options.backend.auth_provider = Some(Arc::new(self.providers()));
        // A backend whose hooks/configuration needs a sign-in gets its link at
        // the next turn start; once signed in, the SDK configures it again.
        let auth = self.clone();
        options.on_configuration_skipped(move |diagnostic| {
            if diagnostic.recovery == Some(AuthRecovery::Login) {
                auth.configuration_needs_login(&diagnostic.backend_id);
            }
        })
    }

    /// `backend`'s hooks/configuration needs a sign-in. The need is this
    /// workspace's declaration of it, never another workspace's with the
    /// same id; a more precise target already recorded is kept.
    pub(crate) fn configuration_needs_login(&self, backend: &str) {
        let Some(backend) = self.0.backends.get(backend) else {
            return;
        };
        // A backend authenticated by its Authorization header never signs in.
        if let Some(context) = backend.context()
            && !backend.header_authorization
            && !login::registry().has_needs(&backend.scope)
        {
            login::registry().needs_login(&backend.scope, &context, &[]);
        }
    }

    /// Bearer, OAuth and unbound routes. A route is chosen by the binding, so
    /// a backend that needs sign-in stays registered and its deliveries fail
    /// under its own failure policy.
    pub(crate) fn providers(&self) -> AuthProviders {
        let provider: Arc<dyn AuthProvider> = Arc::new(Provider(self.clone()));
        AuthProviders::builder()
            .bearer(BearerAuth::from_credentials(self.references()))
            .oauth(Arc::clone(&provider))
            .unbound(provider)
            .build()
    }

    fn manager(&self, context: &AuthContext) -> Result<Arc<ManagedOAuth>, HookError> {
        let target = context.target()?;
        let backend = self
            .0
            .backends
            .get(target.backend_id())
            .ok_or_else(|| fail(AuthErrorCode::InvalidConfiguration))?;
        let settings = &backend.settings;
        let (flow, preset_client) = match target.binding() {
            AuthBinding::OAuth { flow, .. } => (*flow, target.client_id().map(str::to_owned)),
            AuthBinding::Unbound => (
                settings.flow.unwrap_or(OAuthFlow::AuthorizationCodePkce),
                None,
            ),
            _ => return Err(fail(AuthErrorCode::UnsupportedBinding)),
        };
        let pkce = flow == OAuthFlow::AuthorizationCodePkce;
        let policy = policy::Policy {
            settings: settings.clone(),
        };
        // The target's own scopes set the manager's request and ceiling, so
        // targets with different scopes (an event and an upload binding,
        // say) never share one. They still share the backend's grant.
        let mut scopes = policy.requested(&target);
        scopes.sort();
        let key = (
            backend.scope.clone(),
            pkce,
            preset_client.clone(),
            scopes.clone(),
        );
        let mut managers = self
            .0
            .managers
            .lock()
            .map_err(|_| fail(AuthErrorCode::ProviderFailed))?;
        if let Some(manager) = managers.get(&key) {
            return Ok(Arc::clone(manager));
        }
        let store = store::Store::new(self.0.storage.clone(), &backend.scope);
        let client = settings.client_id.clone().or(preset_client);
        let mut config = match &client {
            Some(client) => OAuthClientConfig::new(NAMESPACE, client, flow),
            None => {
                let mut config = OAuthClientConfig::unregistered(NAMESPACE, flow);
                if let Some((url, redirects)) = &settings.client_metadata_document {
                    config = config.client_id_metadata_document(url, redirects.clone());
                }
                if let (Some(registration), Some(issuer)) =
                    (&settings.registration, &settings.issuer)
                {
                    let mut policy = RegistrationPolicy::new(
                        issuer,
                        registration.client_name.as_deref().unwrap_or(CLIENT_NAME),
                    )
                    .software_id(SOFTWARE_ID)
                    .software_version(env!("CARGO_PKG_VERSION"));
                    if let Some(scopes) = &registration.scopes {
                        policy = policy.scope(scopes.clone());
                    }
                    if let Some(reference) = &registration.initial_access_token_ref {
                        let token = self
                            .0
                            .references
                            .get(reference)
                            .ok_or_else(|| fail(AuthErrorCode::CredentialUnavailable))?;
                        policy = policy
                            .initial_access_token(reference, ClientSecret::new(token.clone()));
                    }
                    config = config.dynamic_registration(policy);
                }
                config
            }
        };
        // Each login passes its own loopback redirect when it is prepared.
        config = config
            .grant_partition(&backend.scope)
            .login_lifetime(settings.login_lifetime)
            .scopes(scopes.clone(), scopes)
            .client_secret_storage(Arc::new(secrets::Secrets(store.clone())));
        if !pkce && client.is_some() {
            let reference = settings
                .client_secret_ref
                .as_deref()
                .or(target.client_secret_ref())
                .ok_or_else(|| fail(AuthErrorCode::CredentialUnavailable))?;
            let secret = self
                .0
                .references
                .get(reference)
                .ok_or_else(|| fail(AuthErrorCode::CredentialUnavailable))?;
            config = config.client_secret(
                reference,
                ClientSecret::new(secret.clone()),
                TokenAuthMethod::ClientSecretBasic,
            );
        }
        let network = network::Network::new(settings.allow_private_network, self.0.client);
        let manager = Arc::new(ManagedOAuth::new(
            config,
            Arc::new(network),
            Arc::new(policy),
            Arc::new(store),
        )?);
        managers.insert(key, Arc::clone(&manager));
        Ok(manager)
    }

    /// Records what an operation learned about its backend's sign-in.
    fn observe<T>(&self, context: &AuthContext, result: &Result<Option<T>, HookError>) {
        match result {
            Ok(Some(_)) => {
                if let Some(scope) = self.scope(context) {
                    login::registry().usable(scope, context);
                }
            }
            Err(error) if needs_login(error) => {
                if let Some(scope) = self.scope(context) {
                    login::registry().needs_login(scope, context, error.required_scopes());
                }
            }
            Err(error) => self.explain_ceiling(context, error),
            _ => {}
        }
    }

    /// A backend that requires scopes outside its allowed set is denied, not
    /// signed in again; the user is told which scopes and where to allow them.
    fn explain_ceiling(&self, context: &AuthContext, error: &HookError) {
        if error.auth_code() != Some(AuthErrorCode::AccessDenied)
            || error.required_scopes().is_empty()
        {
            return;
        }
        if let Some(backend) = self.0.backends.get(&context.backend_id) {
            login::registry().problem(
                &backend.scope,
                &backend.id,
                format!(
                    "access_denied. The backend requires scopes ({}) outside the ones Kit allows for it. Review oauth.scopes in hooks.json.",
                    error.required_scopes().join(" ")
                ),
            );
        }
    }

    /// The ids of backends whose sign-in hints a session of these hooks shows.
    fn scope(&self, context: &AuthContext) -> Option<&str> {
        self.0
            .backends
            .get(&context.backend_id)
            .map(|backend| backend.scope.as_str())
    }

    /// The scopes whose sign-in hints a session of these hooks shows.
    fn scopes(&self) -> HashSet<String> {
        self.0
            .backends
            .values()
            .map(|backend| backend.scope.clone())
            .collect()
    }

    pub(crate) fn subscribe(
        &self,
        sink: impl Fn(&AuthHint) + Send + Sync + 'static,
    ) -> HintSubscription {
        login::registry().subscribe(self.scopes(), sink)
    }

    /// At a turn boundary: for each backend that needs a login, prepare it
    /// in the background and show its link. Never blocks the turn and never
    /// touches a backend that has a usable grant.
    pub(crate) fn turn_started(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        for backend in self.0.backends.values() {
            let known = login::registry().has_needs(&backend.scope);
            if !known && !backend.expects_oauth() {
                continue;
            }
            let Some(claim) = login::registry().begin_preparing(&backend.scope) else {
                continue;
            };
            let auth = self.clone();
            let backend = backend.clone();
            tokio::spawn(async move {
                let _claim = claim;
                auth.prepare_backend(&backend).await;
            });
        }
    }

    async fn prepare_backend(&self, backend: &Backend) {
        let mut needs = login::registry().needs(&backend.scope);
        if needs.is_empty() {
            // Not yet seen failing: a known grant needs nothing; ask the store.
            let Some(context) = backend.context() else {
                return;
            };
            let result = Provider(self.clone()).credential(context).await;
            if !matches!(&result, Err(error) if needs_login(error)) {
                return;
            }
            needs = login::registry().needs(&backend.scope);
        }
        for need in needs {
            if login::registry().has_live_login(&backend.scope, &need.context) {
                // The link shown stays until it expires, unless the grant
                // arrived meanwhile (signed in from another Kit process).
                let _ = Provider(self.clone())
                    .credential(with_deadline(&need.context))
                    .await;
                continue;
            }
            if let Err(error) = self.prepare(&need.context, &need.required).await {
                let code = error.auth_code().map_or("failed", AuthErrorCode::as_str);
                tracing::warn!(backend = %backend.id, code, "hook sign-in could not be prepared");
                // The user sees why, once, beside where a link would be.
                login::registry().problem(&backend.scope, &backend.id, guidance(&error));
            }
        }
    }

    /// Prepares one target: a usable grant clears its need; otherwise a new
    /// login is shown, and its own listener awaits the callback.
    async fn prepare(
        &self,
        context: &AuthContext,
        required: &[String],
    ) -> Result<Option<url::Url>, HookError> {
        let manager = self.manager(context)?;
        let context = with_deadline(context);
        let (listener, redirect) = login::bind()
            .await
            .map_err(|_| fail(AuthErrorCode::ProviderFailed))?;
        let options = PrepareOptions::new()
            .required(required.to_vec())
            .redirect_uri(redirect);
        match manager
            .prepare_with(context.clone(), options, budget())
            .await?
        {
            Preparation::Ready(_) => {
                if let Some(scope) = self.scope(&context) {
                    login::registry().usable(scope, &context);
                }
                Ok(None)
            }
            Preparation::InteractionRequired(pending) => {
                let scope = self
                    .scope(&context)
                    .ok_or_else(|| fail(AuthErrorCode::InvalidConfiguration))?
                    .to_owned();
                let url = pending.authorization_url().clone();
                let login = login::Login::new(&scope, manager, &context, pending);
                login::registry().offer(login.clone());
                // Listening already: the link works as soon as it is shown.
                tokio::spawn(async move {
                    match login::serve(&listener, &login).await {
                        login::Outcome::Completed => login::registry().completed(&login),
                        login::Outcome::Ended => login::registry().ended(&login),
                        login::Outcome::Stopped => {}
                    }
                });
                login::registry().show(&scope);
                Ok(Some(url))
            }
            _ => Err(fail(AuthErrorCode::ProviderFailed)),
        }
    }

    /// One line per backend for `kit auth status hooks`, or for `only`.
    /// An unknown `only` is refused before any credential is checked.
    pub(crate) async fn status(&self, only: Option<&str>) -> Result<Vec<(String, String)>, String> {
        if let Some(id) = only
            && !self.0.backends.contains_key(id)
        {
            return Err(format!("no hook backend `{id}` is configured"));
        }
        let mut lines = Vec::new();
        for backend in self
            .0
            .backends
            .values()
            .filter(|backend| only.is_none_or(|id| id == backend.id))
        {
            let status = match backend.context() {
                None => "no authentication (stdio)".to_owned(),
                Some(_) if backend.header_authorization => {
                    "authorization header from hooks.json".to_owned()
                }
                Some(context) => match self.manager(&context) {
                    Err(error)
                        if context.authentication.is_some()
                            && error.auth_code() == Some(AuthErrorCode::UnsupportedBinding) =>
                    {
                        "bearer token".to_owned()
                    }
                    Err(error) => describe(&error),
                    Ok(manager) => match manager.credential(context).await {
                        Ok(Some(_)) => "authorized".to_owned(),
                        Ok(None) if backend.expects_oauth() => {
                            "needs-login (not signed in yet)".to_owned()
                        }
                        Ok(None) => "anonymous".to_owned(),
                        Err(error) => describe(&error),
                    },
                },
            };
            lines.push((backend.id.clone(), status));
        }
        Ok(lines)
    }

    /// Signs in to `only`, or to each backend that needs it. `present` is
    /// shown each link; a backend with a usable grant is never re-prompted.
    pub(crate) async fn login(
        &self,
        only: Option<&str>,
        mut present: impl FnMut(&str, &url::Url),
    ) -> Result<Vec<String>, String> {
        if let Some(id) = only
            && !self.0.backends.contains_key(id)
        {
            return Err(format!("no hook backend `{id}` is configured"));
        }
        let mut signed_in = Vec::new();
        for backend in self.0.backends.values() {
            if only.is_some_and(|id| id != backend.id) || backend.header_authorization {
                continue;
            }
            let Some(context) = backend.context() else {
                continue;
            };
            let manager = match self.manager(&context) {
                Ok(manager) => manager,
                Err(_) if only.is_none() => continue,
                Err(error) => return Err(format!("{}: {}", backend.id, describe(&error))),
            };
            if let Ok(Some(_)) = manager.credential(with_deadline(&context)).await {
                continue;
            }
            // Listen before showing the link, so an immediate approval lands.
            let (listener, redirect) = login::bind().await.map_err(|error| {
                format!("{}: could not listen for sign-in: {error}", backend.id)
            })?;
            let prepared = manager
                .prepare_with(
                    with_deadline(&context),
                    PrepareOptions::new().redirect_uri(redirect),
                    budget(),
                )
                .await;
            let pending = match prepared {
                Ok(Preparation::Ready(_)) => continue,
                Ok(Preparation::InteractionRequired(pending)) => pending,
                Ok(_) => continue,
                // Without a preset or settings, a backend that publishes no
                // protected resource metadata does not use OAuth.
                Err(error)
                    if only.is_none()
                        && !backend.expects_oauth()
                        && error.auth_code() == Some(AuthErrorCode::DiscoveryFailed) =>
                {
                    continue;
                }
                Err(error) => return Err(format!("{}: {}", backend.id, describe(&error))),
            };
            present(&backend.id, pending.authorization_url());
            let candidate =
                login::Login::new(&backend.scope, Arc::clone(&manager), &context, pending);
            if login::serve(&listener, &candidate).await != login::Outcome::Completed {
                return Err(format!(
                    "{}: the sign-in link expired or was not approved",
                    backend.id
                ));
            }
            login::registry().usable(&backend.scope, &context);
            signed_in.push(backend.id.clone());
        }
        Ok(signed_in)
    }

    /// Forgets `backend`'s tokens, pending login and registered client, and
    /// nothing of any other backend.
    pub(crate) async fn logout(&self, backend: &str) -> Result<bool, String> {
        let Some(scope) = self.0.backends.get(backend).map(|b| b.scope.clone()) else {
            return Err(format!("no hook backend `{backend}` is configured"));
        };
        let removed = store::Store::new(self.0.storage.clone(), &scope)
            .forget(budget())
            .await
            .map_err(|error| describe(&error))?;
        login::registry().reset(&scope, backend);
        Ok(removed > 0)
    }
}

/// What to tell the user when a backend's sign-in cannot be prepared: the
/// safe error code and the setting to check, never a provider message.
fn guidance(error: &HookError) -> String {
    match error.auth_code() {
        Some(AuthErrorCode::UntrustedDestination) => "untrusted_destination. Check oauth.issuer, oauth.trustAdvertisedIssuer and oauth.allowPrivateNetwork in hooks.json.".into(),
        Some(
            code @ (AuthErrorCode::CredentialUnavailable
            | AuthErrorCode::InvalidConfiguration
            | AuthErrorCode::UnsupportedBinding),
        ) => format!(
            "{}. Check the backend's authentication and oauth settings in hooks.json.",
            code.as_str()
        ),
        Some(code) => format!("{}. Kit tries again at the next turn.", code.as_str()),
        None => "failed. Kit tries again at the next turn.".into(),
    }
}

fn describe(error: &HookError) -> String {
    match error.auth_code() {
        Some(AuthErrorCode::InteractionRequired | AuthErrorCode::RefreshRejected) => {
            "needs-login".into()
        }
        Some(code) => format!("failing ({})", code.as_str()),
        None => "failing".into(),
    }
}

/// Removes `backend`'s state in every workspace, including workspaces whose
/// hooks file has since moved. Returns how many workspaces had state.
pub(crate) async fn logout_everywhere(
    storage: &CredentialStorage,
    backend: &str,
) -> Result<usize, String> {
    let scopes = store::scopes(storage)
        .await
        .map_err(|error| describe(&error))?;
    let suffix = format!("#{backend}");
    let mut removed = 0;
    for scope in scopes.iter().filter(|scope| scope.ends_with(&suffix)) {
        if store::Store::new(storage.clone(), scope)
            .forget(budget())
            .await
            .map_err(|error| describe(&error))?
            > 0
        {
            removed += 1;
        }
        login::registry().reset(scope, backend);
    }
    Ok(removed)
}

/// Routes each operation to its backend's managed OAuth, and records which
/// backends need a login.
struct Provider(HookAuth);

impl AuthProvider for Provider {
    /// Forwarded, so a completed sign-in lets the SDK retry the backend's
    /// configuration within the session.
    fn preparations(&self, context: &AuthContext) -> u64 {
        self.0
            .manager(context)
            .map_or(0, |manager| manager.preparations(context))
    }

    fn credential(
        &self,
        context: AuthContext,
    ) -> LocalFuture<'_, Result<Option<BearerCredential>, HookError>> {
        Box::pin(async move {
            let observed = context.clone();
            let result = match self.0.manager(&context) {
                Ok(manager) => manager.credential(context).await,
                Err(error) => Err(error),
            };
            self.0.observe(&observed, &result);
            result
        })
    }

    fn challenge(
        &self,
        context: AuthContext,
        challenge: AuthChallenge,
    ) -> LocalFuture<'_, Result<(), HookError>> {
        Box::pin(async move {
            let observed = context.clone();
            let result = match self.0.manager(&context) {
                Ok(manager) => manager.challenge(context, challenge).await,
                Err(error) => Err(error),
            };
            if let Err(error) = &result {
                if needs_login(error) {
                    if let Some(scope) = self.0.scope(&observed) {
                        login::registry().needs_login(scope, &observed, error.required_scopes());
                    }
                } else {
                    self.0.explain_ceiling(&observed, error);
                }
            }
            result
        })
    }
}

/// `kit auth … hooks` for a workspace, without starting a runtime.
pub mod cli {
    use std::path::Path;

    use crate::credentials::CredentialStorage;

    fn auth(root: &Path, storage: &CredentialStorage) -> Result<super::HookAuth, String> {
        let (registration, sources) = super::super::effective_registration_sources(root)
            .ok_or_else(|| "no hook backends are configured for this workspace".to_owned())?;
        let registration =
            serde_json::to_value(registration).map_err(|_| "invalid hook configuration")?;
        Ok(super::HookAuth::new(
            storage.clone(),
            &registration,
            &sources,
        ))
    }

    /// `backend: status` lines, for `backend` or every backend.
    pub async fn status(
        root: &Path,
        storage: &CredentialStorage,
        backend: Option<&str>,
    ) -> Result<String, String> {
        let lines = auth(root, storage)?.status(backend).await?;
        Ok(lines
            .into_iter()
            .map(|(backend, status)| format!("{backend}: {status}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Signs in to `backend`, or each backend that needs it, showing each
    /// link through `present`. Dropping the future cancels the wait.
    pub async fn login(
        root: &Path,
        storage: &CredentialStorage,
        backend: Option<&str>,
        mut present: impl FnMut(&str, &str),
    ) -> Result<Vec<String>, String> {
        if !storage.is_persistent() {
            return Err("hook sign-in requires persistent credential storage".into());
        }
        auth(root, storage)?
            .login(backend, |backend, url| present(backend, url.as_str()))
            .await
    }

    pub async fn logout(
        root: &Path,
        storage: &CredentialStorage,
        backend: &str,
    ) -> Result<bool, String> {
        auth(root, storage)?.logout(backend).await
    }

    /// Signs out of `backend` in every workspace, including ones whose hooks
    /// file moved. Returns how many workspaces had stored state.
    pub async fn logout_all_workspaces(
        storage: &CredentialStorage,
        backend: &str,
    ) -> Result<usize, String> {
        super::logout_everywhere(storage, backend).await
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
pub(crate) mod test_support {
    /// A pending sign-in hint for `backend` of `hooks`.
    pub(crate) fn hint(hooks: &crate::ahp::Hooks, backend: &str, url: &str) -> super::AuthHint {
        super::AuthHint {
            scope: hooks.auth.as_ref().unwrap().0.backends[backend]
                .scope
                .clone(),
            backend: backend.into(),
            url: Some(url.into()),
            expires_at_ms: Some(u64::MAX),
            cleared: false,
            signed_in: false,
            problem: None,
        }
    }

    /// Hook auth whose OAuth requests start from `client`, as a test that
    /// trusts its own authorization server's certificate needs.
    pub(crate) fn hook_auth(
        storage: crate::credentials::CredentialStorage,
        registration: &serde_json::Value,
        sources: &std::collections::BTreeMap<String, String>,
        client: fn() -> reqwest::ClientBuilder,
    ) -> super::HookAuth {
        super::HookAuth::with_client(storage, registration, sources, client)
    }

    /// Shows `hint` as a turn start would.
    pub(crate) fn publish(hint: super::AuthHint) {
        super::login::registry().publish(hint);
    }

    /// Hooks for one HTTP backend `id` that may need sign-in.
    pub(crate) fn hooks(id: &str) -> (tempfile::TempDir, std::sync::Arc<crate::ahp::Hooks>) {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let agents = root.path().join(".agents");
        std::fs::create_dir_all(&agents).unwrap_or_else(|error| panic!("{error}"));
        std::fs::write(
            agents.join("hooks.json"),
            serde_json::json!({"protocolVersion": "draft", "hooks": [{
                "id": id,
                "transport": {"type": "http", "url": "https://hooks.example/ahp"},
                "subscriptions": [crate::ahp::test_support::observe(&["session.start"])],
            }]})
            .to_string(),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let hooks =
            crate::ahp::Hooks::load(root.path(), &crate::credentials::CredentialStorage::Memory)
                .unwrap_or_else(|| panic!("hooks load"));
        (root, hooks)
    }
}

#[cfg(test)]
mod tests;
