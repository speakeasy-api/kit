use std::{
    collections::{BTreeSet, HashMap},
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{AlgorithmParameters, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputFormat {
    Human,
    Json,
    Jsonl,
}

#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) stdout: String,
}

#[derive(Clone, Copy, Debug)]
enum ClientErrorKind {
    Invalid,
    Unavailable,
    Timeout,
}

fn render_exec_response(value: Value, format: OutputFormat) -> Result<Output, AuthError> {
    let stdout = match format {
        OutputFormat::Human | OutputFormat::Json => serde_json::to_string_pretty(&value),
        OutputFormat::Jsonl => serde_json::to_string(&value),
    }
    .map_err(|_| AuthError::invalid("output_failed", "could not encode authentication output"))?;
    Ok(human(format!("{stdout}\n")))
}

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REVOKE_URL: &str = "https://auth.openai.com/oauth/revoke";
const JWKS_URL: &str = "https://auth.openai.com/.well-known/jwks.json";
const CALLBACK_PATH: &str = "/auth/callback";
const KEYRING_SERVICE: &str = "dev.kit.openai";
const KEYRING_USER: &str = "subscription";
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const MAX_HTTP_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const CLOCK_SKEW_SECONDS: i64 = 60;
const REFRESH_WINDOW_SECONDS: i64 = 5 * 60;
const JWKS_TTL: Duration = Duration::from_secs(60 * 60);
static REFRESH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static JWKS_CACHE: LazyLock<Mutex<HashMap<String, CachedJwks>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthCommand {
    Login,
    Status,
    Logout { local_only: bool },
}

struct CachedJwks {
    fetched_at: Instant,
    keys: JwkSet,
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct LoginSecrets {
    verifier: String,
    state: String,
    nonce: String,
}

#[derive(Debug)]
pub(crate) struct AuthError {
    code: &'static str,
    detail: String,
    #[allow(dead_code)]
    kind: ClientErrorKind,
}

impl AuthError {
    fn invalid(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            kind: ClientErrorKind::Invalid,
        }
    }

    fn unavailable(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            kind: ClientErrorKind::Unavailable,
        }
    }

    fn timeout(detail: impl Into<String>) -> Self {
        Self {
            code: "openai_auth_timeout",
            detail: detail.into(),
            kind: ClientErrorKind::Timeout,
        }
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl TokenRecord {
    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }
    pub(crate) fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub(crate) fn binding(&self) -> Result<CredentialBinding, AuthError> {
        let account_id = self
            .account_id
            .as_deref()
            .filter(|value| valid_account_id(value))
            .ok_or_else(|| AuthError::invalid("token_invalid", "credential account is missing"))?;
        if !valid_generation(&self.generation) {
            return Err(AuthError::invalid(
                "token_invalid",
                "credential generation is missing or invalid",
            ));
        }
        Ok(CredentialBinding {
            account_id: account_id.to_owned(),
            generation: self.generation.clone(),
        })
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn for_test(access_token: &str, account_id: &str) -> Self {
        Self {
            access_token: access_token.to_owned(),
            refresh_token: "test-refresh-token".to_owned(),
            id_token: "test-id-token".to_owned(),
            expires_at: i64::MAX,
            account_id: Some(account_id.to_owned()),
            email: None,
            plan_type: None,
            generation: format!("test-{account_id}"),
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn for_test_generation(
        access_token: &str,
        account_id: &str,
        generation: &str,
    ) -> Self {
        let mut record = Self::for_test(access_token, account_id);
        record.generation = generation.to_owned();
        record
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub(crate) struct CredentialBinding {
    pub(crate) account_id: String,
    pub(crate) generation: String,
}

#[derive(Clone, Deserialize, Serialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenRecord {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_at: i64,
    account_id: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
    #[serde(default)]
    generation: String,
}

impl std::fmt::Debug for TokenRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenRecord")
            .field("tokens", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("plan_type", &self.plan_type)
            .finish()
    }
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    token_type: Option<String>,
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct JwtClaims {
    iss: String,
    #[zeroize(skip)]
    aud: Value,
    exp: i64,
    iat: i64,
    #[serde(default)]
    nbf: Option<i64>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    organizations: Vec<Organization>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    openai_auth: Option<OpenAiClaims>,
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct Organization {
    id: String,
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct OpenAiClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
}

trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<TokenRecord>, AuthError>;
    fn save(&self, record: &TokenRecord) -> Result<(), AuthError>;
    fn delete(&self) -> Result<bool, AuthError>;
}

struct OsCredentialStore;

impl OsCredentialStore {
    fn entry() -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|_| {
            AuthError::unavailable(
                "credential_backend_unavailable",
                "the OS credential store is unavailable; no plaintext fallback is permitted",
            )
        })
    }
}

impl CredentialStore for OsCredentialStore {
    fn load(&self) -> Result<Option<TokenRecord>, AuthError> {
        let mut bytes = match Self::entry()?.get_secret() {
            Ok(bytes) => bytes,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(_) => {
                return Err(AuthError::unavailable(
                    "credential_read_failed",
                    "could not read OpenAI credentials from the OS credential store",
                ));
            }
        };
        if bytes.len() > MAX_CREDENTIAL_BYTES {
            bytes.zeroize();
            return Err(AuthError::invalid(
                "credential_record_invalid",
                "the credential record exceeds 64 KiB",
            ));
        }
        let record = serde_json::from_slice(&bytes).map_err(|_| {
            AuthError::invalid(
                "credential_record_invalid",
                "the credential record is malformed",
            )
        });
        bytes.zeroize();
        record.map(Some)
    }

    fn save(&self, record: &TokenRecord) -> Result<(), AuthError> {
        let mut bytes = serde_json::to_vec(record).map_err(|_| {
            AuthError::invalid("credential_record_invalid", "could not encode credentials")
        })?;
        if bytes.len() > MAX_CREDENTIAL_BYTES {
            bytes.zeroize();
            return Err(AuthError::invalid(
                "credential_record_invalid",
                "the credential record exceeds 64 KiB",
            ));
        }
        let result = Self::entry()?.set_secret(&bytes).map_err(|_| {
            AuthError::unavailable(
                "credential_write_failed",
                "could not atomically store OpenAI credentials in the OS credential store",
            )
        });
        bytes.zeroize();
        result
    }

    fn delete(&self) -> Result<bool, AuthError> {
        match Self::entry()?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(_) => Err(AuthError::unavailable(
                "credential_delete_failed",
                "could not delete OpenAI credentials from the OS credential store",
            )),
        }
    }
}

pub(crate) fn execute(
    command: AuthCommand,
    format: OutputFormat,
    timeout: Duration,
) -> Result<Output, AuthError> {
    let store = OsCredentialStore;
    let deadline = Instant::now() + timeout.min(Duration::from_secs(300));
    let result = match command {
        AuthCommand::Login => login(&store, format, deadline),
        AuthCommand::Status => status(&store, format, deadline),
        AuthCommand::Logout { local_only } => logout(&store, format, deadline, local_only),
    };
    result
}

fn login(
    store: &dyn CredentialStore,
    format: OutputFormat,
    deadline: Instant,
) -> Result<Output, AuthError> {
    let listener = bind_callback()?;
    let port = listener
        .local_addr()
        .map_err(|_| {
            AuthError::unavailable(
                "callback_bind_failed",
                "could not inspect the callback listener",
            )
        })?
        .port();
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
    let mut secrets = LoginSecrets {
        verifier: random_urlsafe::<64>()?,
        state: random_urlsafe::<32>()?,
        nonce: random_urlsafe::<32>()?,
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(secrets.verifier.as_bytes()));
    let mut auth_url = Zeroizing::new(authorize_url(
        &redirect_uri,
        &challenge,
        &secrets.state,
        &secrets.nonce,
    ));
    emit_auth_url(auth_url.as_str(), format)?;
    open_browser_bounded(auth_url.as_str());
    listener.set_nonblocking(true).map_err(|_| {
        AuthError::unavailable(
            "callback_bind_failed",
            "could not configure callback listener",
        )
    })?;

    let result = 'login: loop {
        if Instant::now() >= deadline {
            break Err(AuthError::timeout(
                "authorization was not completed within five minutes",
            ));
        }
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if !peer.ip().is_loopback() {
                    respond(&mut stream, 403, "Forbidden");
                    continue;
                }
                match callback(&mut stream, port, &secrets.state) {
                    Ok(Callback::Ignore(status, message)) => respond(&mut stream, status, message),
                    Ok(Callback::Error) => {
                        respond(&mut stream, 400, "Authorization failed");
                        break 'login Err(AuthError::invalid(
                            "openai_login_failed",
                            "OpenAI declined the authorization request",
                        ));
                    }
                    Ok(Callback::Code(mut code)) => {
                        let result = exchange_code(
                            &code,
                            &redirect_uri,
                            &secrets.verifier,
                            &secrets.nonce,
                            deadline,
                            TOKEN_URL,
                        )
                        .and_then(|record| {
                            let _lock = process_lock(deadline)?;
                            store.save(&record)?;
                            Ok(record)
                        });
                        code.zeroize();
                        match result {
                            Ok(record) => {
                                respond(
                                    &mut stream,
                                    200,
                                    "Authentication complete. You may close this window.",
                                );
                                break 'login render_status(Some(record), format, true);
                            }
                            Err(error) => {
                                respond(
                                    &mut stream,
                                    500,
                                    "Authentication completed, but credentials could not be saved.",
                                );
                                break 'login Err(error);
                            }
                        }
                    }
                    Err(error) => respond(&mut stream, 400, &error.detail),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                break Err(AuthError::unavailable(
                    "callback_read_failed",
                    "the callback listener failed",
                ));
            }
        }
    };
    auth_url.zeroize();
    secrets.zeroize();
    result
}

fn status(
    store: &dyn CredentialStore,
    format: OutputFormat,
    deadline: Instant,
) -> Result<Output, AuthError> {
    let record = match store.load()? {
        Some(record) => Some(refresh_if_needed(store, record, deadline)?),
        None => None,
    };
    render_status(record, format, false)
}

fn logout(
    store: &dyn CredentialStore,
    format: OutputFormat,
    deadline: Instant,
    local_only: bool,
) -> Result<Output, AuthError> {
    logout_at(store, format, deadline, local_only, REVOKE_URL)
}

fn logout_at(
    store: &dyn CredentialStore,
    format: OutputFormat,
    deadline: Instant,
    local_only: bool,
    revoke_url: &str,
) -> Result<Output, AuthError> {
    let _lock = process_lock(deadline)?;
    if let Some(record) = store.load()?
        && !local_only
    {
        revoke_at(&record, deadline, revoke_url)?;
    }
    let removed = store.delete()?;
    if format == OutputFormat::Human {
        Ok(human(if removed && local_only {
            "WARNING: local OpenAI credentials removed without remote revocation.\n"
        } else if removed {
            "OpenAI credentials revoked and removed.\n"
        } else {
            "OpenAI is not authenticated.\n"
        }))
    } else {
        render_exec_response(
            json!({"provider":"openai","authenticated":false,"removed":removed,"local_only":local_only}),
            format,
        )
        .map_err(|error| AuthError::invalid("output_failed", error.to_string()))
    }
}

fn render_status(
    record: Option<TokenRecord>,
    format: OutputFormat,
    login: bool,
) -> Result<Output, AuthError> {
    if format == OutputFormat::Human {
        return Ok(match record {
            Some(record) => human(format!(
                "{} with ChatGPT{}{}.\n",
                if login {
                    "Authenticated"
                } else {
                    "OpenAI: authenticated"
                },
                record
                    .email
                    .as_deref()
                    .map(|value| format!(" as {value}"))
                    .unwrap_or_default(),
                record
                    .plan_type
                    .as_deref()
                    .map(|value| format!(" (plan: {value})"))
                    .unwrap_or_default(),
            )),
            None => human("OpenAI: not authenticated with ChatGPT.\n"),
        });
    }
    let value = json!({
        "provider": "openai",
        "authenticated": record.is_some(),
        "account": record.as_ref().map(|record| json!({
            "id": record.account_id,
            "email": record.email,
            "plan_type": record.plan_type,
        })),
    });
    render_exec_response(value, format)
        .map_err(|error| AuthError::invalid("output_failed", error.to_string()))
}

pub(crate) fn access_token(deadline: Instant) -> Result<TokenRecord, AuthError> {
    let store = OsCredentialStore;
    let mut record = store.load()?.ok_or_else(|| {
        AuthError::invalid(
            "openai_auth_required",
            "run `kit auth login openai` before using openai-subscription",
        )
    })?;
    if !valid_generation(&record.generation) {
        let _thread = refresh_guard(deadline)?;
        let _process = process_lock(deadline)?;
        record = store.load()?.ok_or_else(|| {
            AuthError::invalid(
                "openai_auth_required",
                "OpenAI subscription credentials are missing",
            )
        })?;
        if !valid_generation(&record.generation) {
            record.generation = random_urlsafe::<32>()?;
            store.save(&record)?;
        }
    }
    refresh_if_needed(&store, record, deadline)
}

pub(crate) fn refresh_after_unauthorized(
    rejected_access_token: &str,
    deadline: Instant,
) -> Result<TokenRecord, AuthError> {
    let store = OsCredentialStore;
    refresh_locked(&store, deadline, Some(rejected_access_token), TOKEN_URL)
}

fn refresh_if_needed(
    store: &dyn CredentialStore,
    record: TokenRecord,
    deadline: Instant,
) -> Result<TokenRecord, AuthError> {
    if record.expires_at > unix_seconds() + REFRESH_WINDOW_SECONDS {
        Ok(record)
    } else {
        refresh_locked(store, deadline, None, TOKEN_URL)
    }
}

fn refresh_locked(
    store: &dyn CredentialStore,
    deadline: Instant,
    rejected_access_token: Option<&str>,
    token_url: &str,
) -> Result<TokenRecord, AuthError> {
    let _thread = refresh_guard(deadline)?;
    let _process = process_lock(deadline)?;
    refresh_current(store, deadline, rejected_access_token, token_url)
}

fn refresh_guard(deadline: Instant) -> Result<std::sync::MutexGuard<'static, ()>, AuthError> {
    loop {
        match REFRESH_LOCK.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(AuthError::timeout(
                    "timed out waiting for credential refresh",
                ));
            }
        }
    }
}

fn refresh_current(
    store: &dyn CredentialStore,
    deadline: Instant,
    rejected_access_token: Option<&str>,
    token_url: &str,
) -> Result<TokenRecord, AuthError> {
    refresh_current_at(store, deadline, rejected_access_token, token_url, JWKS_URL)
}

fn refresh_current_at(
    store: &dyn CredentialStore,
    deadline: Instant,
    rejected_access_token: Option<&str>,
    token_url: &str,
    jwks_url: &str,
) -> Result<TokenRecord, AuthError> {
    let current = store.load()?.ok_or_else(|| {
        AuthError::invalid(
            "openai_auth_required",
            "OpenAI subscription credentials are missing",
        )
    })?;
    if rejected_access_token
        .is_some_and(|rejected| !constant_time_eq(current.access_token.as_str(), rejected))
        || rejected_access_token.is_none()
            && current.expires_at > unix_seconds() + REFRESH_WINDOW_SECONDS
    {
        return Ok(current);
    }
    let client = http_client(deadline)?;
    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", current.refresh_token.as_str()),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .map_err(|_| {
            AuthError::unavailable("token_refresh_failed", "token refresh transport failed")
        })?;
    let status = response.status();
    let body = bounded_body(response)?;
    if !status.is_success() {
        return Err(AuthError::unavailable(
            "token_refresh_failed",
            format!("token endpoint returned {status}"),
        ));
    }
    let response: TokenResponse = serde_json::from_slice(body.as_slice()).map_err(|_| {
        AuthError::invalid(
            "token_refresh_failed",
            "token endpoint returned malformed JSON",
        )
    })?;
    let next = token_record(response, None, Some(&current), deadline, jwks_url)?;
    store.save(&next)?;
    Ok(next)
}

fn exchange_code(
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    nonce: &str,
    deadline: Instant,
    token_url: &str,
) -> Result<TokenRecord, AuthError> {
    exchange_code_at(
        code,
        redirect_uri,
        verifier,
        nonce,
        deadline,
        token_url,
        JWKS_URL,
    )
}

fn exchange_code_at(
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    nonce: &str,
    deadline: Instant,
    token_url: &str,
    jwks_url: &str,
) -> Result<TokenRecord, AuthError> {
    if !matches!(
        redirect_uri,
        "http://localhost:1455/auth/callback" | "http://localhost:1457/auth/callback"
    ) {
        return Err(AuthError::invalid(
            "token_exchange_failed",
            "redirect URI is not a registered OpenAI loopback callback",
        ));
    }
    let response = http_client(deadline)?
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .map_err(|_| {
            AuthError::unavailable("token_exchange_failed", "token exchange transport failed")
        })?;
    let status = response.status();
    let body = bounded_body(response)?;
    if !status.is_success() {
        return Err(AuthError::invalid(
            "token_exchange_failed",
            format!("token endpoint returned {status}"),
        ));
    }
    let response: TokenResponse = serde_json::from_slice(body.as_slice()).map_err(|_| {
        AuthError::invalid(
            "token_exchange_failed",
            "token endpoint returned malformed JSON",
        )
    })?;
    token_record(response, Some(nonce), None, deadline, jwks_url)
}

fn token_record(
    mut response: TokenResponse,
    nonce: Option<&str>,
    previous: Option<&TokenRecord>,
    deadline: Instant,
    jwks_url: &str,
) -> Result<TokenRecord, AuthError> {
    if response.access_token.is_empty()
        || response
            .token_type
            .as_deref()
            .is_some_and(|value| !value.eq_ignore_ascii_case("bearer"))
    {
        return Err(AuthError::invalid(
            "token_invalid",
            "token response omitted a bearer access token",
        ));
    }
    let access_claims = verify_claims(
        &response.access_token,
        "https://api.openai.com/v1",
        None,
        deadline,
        jwks_url,
    )?;
    let access_account = extract_account_id(&access_claims)?;
    let (id_token, id_claims) = match response.id_token.take() {
        Some(id_token) => {
            let claims = verify_claims(&id_token, CLIENT_ID, nonce, deadline, jwks_url)?;
            (id_token, Some(claims))
        }
        None if nonce.is_some() => {
            return Err(AuthError::invalid(
                "token_invalid",
                "initial token response omitted the ID token",
            ));
        }
        None => (
            previous.map_or_else(String::new, |value| value.id_token.clone()),
            None,
        ),
    };
    let id_account = id_claims
        .as_ref()
        .map(extract_account_id)
        .transpose()?
        .flatten();
    if let (Some(left), Some(right)) = (&access_account, &id_account)
        && left != right
    {
        return Err(AuthError::invalid(
            "token_invalid",
            "ID and access token account claims disagree",
        ));
    }
    let prior_account = previous.and_then(|value| value.account_id.clone());
    let account_id = access_account.or(id_account).or(prior_account);
    if account_id.is_none() {
        return Err(AuthError::invalid(
            "token_invalid",
            "verified tokens omitted a valid account claim",
        ));
    }
    if let (Some(previous), Some(current)) = (
        previous.and_then(|value| value.account_id.as_ref()),
        account_id.as_ref(),
    ) && previous != current
    {
        return Err(AuthError::invalid(
            "token_invalid",
            "refreshed token changed the authenticated account",
        ));
    }
    let refresh_token = response
        .refresh_token
        .take()
        .or_else(|| previous.map(|value| value.refresh_token.clone()))
        .ok_or_else(|| {
            AuthError::invalid("token_invalid", "token response omitted the refresh token")
        })?;
    if refresh_token.is_empty() {
        return Err(AuthError::invalid(
            "token_invalid",
            "token response returned an empty refresh token",
        ));
    }
    let expires_at = access_claims
        .exp
        .min(unix_seconds().saturating_add(response.expires_in.unwrap_or(3600).min(86_400) as i64));
    let generation = match previous.map(|value| value.generation.as_str()) {
        Some(value) if valid_generation(value) => value.to_owned(),
        _ => random_urlsafe::<32>()?,
    };
    Ok(TokenRecord {
        access_token: std::mem::take(&mut response.access_token),
        refresh_token,
        id_token,
        expires_at,
        account_id,
        email: id_claims
            .as_ref()
            .and_then(|claims| claims.email.clone())
            .filter(|value| valid_email(value))
            .or_else(|| previous.and_then(|value| value.email.clone())),
        plan_type: id_claims
            .as_ref()
            .and_then(|claims| claims.openai_auth.as_ref())
            .and_then(|claims| claims.chatgpt_plan_type.clone())
            .filter(|value| !value.is_empty() && value.len() <= 64 && value.is_ascii())
            .or_else(|| previous.and_then(|value| value.plan_type.clone())),
        generation,
    })
}

fn valid_generation(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn verify_claims(
    token: &str,
    audience: &str,
    nonce: Option<&str>,
    deadline: Instant,
    jwks_url: &str,
) -> Result<JwtClaims, AuthError> {
    if token.len() > MAX_CREDENTIAL_BYTES {
        return Err(AuthError::invalid("token_invalid", "JWT exceeds 64 KiB"));
    }
    let header = decode_header(token)
        .map_err(|_| AuthError::invalid("token_invalid", "JWT header is malformed"))?;
    if header.alg != Algorithm::RS256 {
        return Err(AuthError::invalid(
            "token_invalid",
            "JWT uses an unexpected signing algorithm",
        ));
    }
    let kid = header
        .kid
        .filter(|value| !value.is_empty() && value.len() <= 256 && value.is_ascii())
        .ok_or_else(|| AuthError::invalid("token_invalid", "JWT omitted a valid key ID"))?;
    let key = verification_key(&kid, deadline, jwks_url)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.leeway = CLOCK_SKEW_SECONDS as u64;
    validation.validate_nbf = true;
    validation.set_required_spec_claims(&["iss", "aud", "exp"]);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[audience]);
    let claims = decode::<JwtClaims>(token, &key, &validation)
        .map_err(|_| AuthError::invalid("token_invalid", "JWT signature or claims are invalid"))?
        .claims;
    let now = unix_seconds();
    let audience_matches = match &claims.aud {
        Value::String(value) => value == audience,
        Value::Array(values) => values.len() == 1 && values[0].as_str() == Some(audience),
        _ => false,
    };
    if claims.iss != ISSUER
        || !audience_matches
        || claims.exp <= now - CLOCK_SKEW_SECONDS
        || claims.iat > now + CLOCK_SKEW_SECONDS
        || claims
            .nbf
            .is_some_and(|not_before| not_before > now + CLOCK_SKEW_SECONDS)
        || claims.iat > claims.exp
    {
        return Err(AuthError::invalid(
            "token_invalid",
            "JWT issuer, audience, or time claims are invalid",
        ));
    }
    if let Some(expected) = nonce
        && claims
            .nonce
            .as_deref()
            .is_none_or(|actual| !constant_time_eq(actual, expected))
    {
        return Err(AuthError::invalid(
            "token_invalid",
            "ID token nonce mismatch",
        ));
    }
    extract_account_id(&claims)?;
    Ok(claims)
}

fn extract_account_id(claims: &JwtClaims) -> Result<Option<String>, AuthError> {
    if claims.organizations.len() > 100 {
        return Err(AuthError::invalid(
            "token_invalid",
            "JWT contains too many organizations",
        ));
    }
    let nested = claims
        .openai_auth
        .as_ref()
        .and_then(|value| value.chatgpt_account_id.as_ref());
    if claims
        .chatgpt_account_id
        .iter()
        .chain(nested)
        .chain(claims.organizations.iter().map(|value| &value.id))
        .any(|value| !valid_account_id(value))
        || claims
            .chatgpt_account_id
            .as_ref()
            .zip(nested)
            .is_some_and(|(left, right)| left != right)
    {
        return Err(AuthError::invalid(
            "token_invalid",
            "JWT account claims are invalid or ambiguous",
        ));
    }
    Ok(claims
        .chatgpt_account_id
        .clone()
        .or_else(|| nested.cloned())
        .or_else(|| claims.organizations.first().map(|value| value.id.clone())))
}

fn valid_account_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.is_ascii()
}

fn valid_email(value: &str) -> bool {
    !value.is_empty() && value.len() <= 320 && value.is_ascii() && value.contains('@')
}

fn verification_key(
    kid: &str,
    deadline: Instant,
    jwks_url: &str,
) -> Result<DecodingKey, AuthError> {
    for refresh in [false, true] {
        let keys = jwks(deadline, jwks_url, refresh)?;
        if let Some(jwk) = keys.find(kid) {
            if jwk.common.key_algorithm != Some(KeyAlgorithm::RS256)
                || !matches!(jwk.algorithm, AlgorithmParameters::RSA(_))
                || matches!(
                    jwk.common.public_key_use,
                    Some(PublicKeyUse::Encryption | PublicKeyUse::Other(_))
                )
                || jwk
                    .common
                    .key_operations
                    .as_ref()
                    .is_some_and(|operations| !operations.contains(&KeyOperations::Verify))
            {
                return Err(AuthError::invalid(
                    "token_invalid",
                    "JWT key is not valid for RS256 signature verification",
                ));
            }
            return DecodingKey::from_jwk(jwk).map_err(|_| {
                AuthError::invalid("token_invalid", "JWT verification key is malformed")
            });
        }
    }
    Err(AuthError::invalid(
        "token_invalid",
        "JWT key ID is unknown after one JWKS refresh",
    ))
}

fn jwks(deadline: Instant, jwks_url: &str, refresh: bool) -> Result<JwkSet, AuthError> {
    if !cfg!(test) && jwks_url != JWKS_URL {
        return Err(AuthError::invalid(
            "jwks_invalid",
            "only the pinned OpenAI JWKS endpoint is permitted",
        ));
    }
    if !refresh {
        let cache = JWKS_CACHE.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(cached) = cache.get(jwks_url)
            && cached.fetched_at.elapsed() < JWKS_TTL
        {
            return Ok(cached.keys.clone());
        }
    }
    let response = http_client(deadline)?
        .get(jwks_url)
        .send()
        .map_err(|_| AuthError::unavailable("jwks_fetch_failed", "JWKS fetch failed"))?;
    if !response.status().is_success() {
        return Err(AuthError::unavailable(
            "jwks_fetch_failed",
            "JWKS endpoint rejected the request",
        ));
    }
    let body = bounded_body(response)?;
    let keys: JwkSet = serde_json::from_slice(body.as_slice())
        .map_err(|_| AuthError::invalid("jwks_invalid", "JWKS response is malformed"))?;
    if keys.keys.is_empty() || keys.keys.len() > 64 {
        return Err(AuthError::invalid(
            "jwks_invalid",
            "JWKS key count is outside bounds",
        ));
    }
    let mut kids = BTreeSet::new();
    for key in &keys.keys {
        if let Some(kid) = &key.common.key_id
            && (!valid_account_id(kid) || !kids.insert(kid.clone()))
        {
            return Err(AuthError::invalid(
                "jwks_invalid",
                "JWKS contains an invalid or duplicate key ID",
            ));
        }
    }
    JWKS_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            jwks_url.to_owned(),
            CachedJwks {
                fetched_at: Instant::now(),
                keys: keys.clone(),
            },
        );
    Ok(keys)
}

enum Callback {
    Ignore(u16, &'static str),
    Error,
    Code(String),
}

fn callback(stream: &mut TcpStream, port: u16, state: &str) -> Result<Callback, AuthError> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let mut request = Vec::new();
    let mut chunk = [0_u8; 2048];
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let count = stream.read(&mut chunk).map_err(|_| {
            AuthError::invalid("callback_invalid", "could not read callback request")
        })?;
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        if request.len() > MAX_HTTP_BYTES {
            break;
        }
    }
    if request.len() > MAX_HTTP_BYTES || request.windows(2).any(|bytes| bytes == b"\n\n") {
        return Err(AuthError::invalid(
            "callback_invalid",
            "callback request framing is invalid",
        ));
    }
    let text = std::str::from_utf8(&request)
        .map_err(|_| AuthError::invalid("callback_invalid", "callback request is not UTF-8"))?;
    let header_end = text
        .find("\r\n\r\n")
        .ok_or_else(|| AuthError::invalid("callback_invalid", "callback headers are incomplete"))?;
    if header_end + 4 != text.len() {
        return Err(AuthError::invalid(
            "callback_invalid",
            "callback request has a body",
        ));
    }
    let mut lines = text[..header_end].split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut request_parts = request_line.split(' ');
    let (Some("GET"), Some(target), Some("HTTP/1.1"), None) = (
        request_parts.next(),
        request_parts.next(),
        request_parts.next(),
        request_parts.next(),
    ) else {
        return Err(AuthError::invalid(
            "callback_invalid",
            "only an exact HTTP/1.1 GET is accepted",
        ));
    };
    if target.contains('#') || target.len() > 8192 {
        return Err(AuthError::invalid(
            "callback_invalid",
            "callback target is invalid",
        ));
    }
    let mut host = None;
    for line in lines {
        if line.starts_with(' ') || line.starts_with('\t') || !line.is_ascii() {
            return Err(AuthError::invalid(
                "callback_invalid",
                "callback header folding is rejected",
            ));
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            AuthError::invalid("callback_invalid", "callback header is malformed")
        })?;
        if name.eq_ignore_ascii_case("host") && host.replace(value.trim()).is_some() {
            return Err(AuthError::invalid(
                "callback_invalid",
                "duplicate Host header",
            ));
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("content-length")
        {
            return Err(AuthError::invalid(
                "callback_invalid",
                "callback body framing is rejected",
            ));
        }
    }
    let expected_host = format!("localhost:{port}");
    if host != Some(expected_host.as_str()) {
        return Err(AuthError::invalid(
            "callback_invalid",
            "callback Host does not match the listener",
        ));
    }
    let url = url::Url::parse(&format!("http://localhost:{port}{target}"))
        .map_err(|_| AuthError::invalid("callback_invalid", "callback URL is malformed"))?;
    if url.path() != CALLBACK_PATH {
        return Ok(Callback::Ignore(404, "Not Found"));
    }
    let mut code = None;
    let mut actual_state = None;
    let mut oauth_error = None;
    let mut description = None;
    for (name, value) in url.query_pairs() {
        let slot = match name.as_ref() {
            "code" => &mut code,
            "error" => &mut oauth_error,
            "error_description" => &mut description,
            "state" => {
                if actual_state
                    .replace(Zeroizing::new(value.into_owned()))
                    .is_some()
                {
                    return Err(AuthError::invalid(
                        "callback_invalid",
                        "callback contains duplicate parameters",
                    ));
                }
                continue;
            }
            // RFC 6749 §4.1.2: clients MUST ignore unrecognized response parameters.
            _ => continue,
        };
        if slot.replace(value.into_owned()).is_some() {
            return Err(AuthError::invalid(
                "callback_invalid",
                "callback contains duplicate parameters",
            ));
        }
    }
    if actual_state
        .as_ref()
        .map(|value| value.as_str())
        .is_none_or(|actual| !constant_time_eq(actual, state))
    {
        return Ok(Callback::Ignore(400, "State mismatch"));
    }
    if let Some(mut error) = oauth_error {
        if code.is_some() {
            error.zeroize();
            if let Some(mut description) = description {
                description.zeroize();
            }
            return Err(AuthError::invalid(
                "callback_invalid",
                "callback contains both code and error",
            ));
        }
        error.zeroize();
        if let Some(mut description) = description {
            description.zeroize();
        }
        return Ok(Callback::Error);
    }
    let code = code
        .filter(|value| !value.is_empty() && value.len() <= 4096)
        .ok_or_else(|| {
            AuthError::invalid(
                "callback_invalid",
                "callback omitted the authorization code",
            )
        })?;
    Ok(Callback::Code(code))
}

fn bind_callback() -> Result<TcpListener, AuthError> {
    for port in [1455, 1457] {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(_) => break,
        }
    }
    Err(AuthError::unavailable(
        "callback_bind_failed",
        "registered callback ports 1455 and 1457 are unavailable",
    ))
}

fn authorize_url(redirect_uri: &str, challenge: &str, state: &str, nonce: &str) -> String {
    let mut url = url::Url::parse("https://auth.openai.com/oauth/authorize").expect("fixed URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair(
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        )
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("nonce", nonce)
        .append_pair("originator", "kit");
    url.into()
}

fn http_client(deadline: Instant) -> Result<reqwest::blocking::Client, AuthError> {
    let timeout = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(30));
    if timeout.is_zero() {
        return Err(AuthError::timeout(
            "authentication request deadline expired",
        ));
    }
    reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .timeout(timeout)
        .user_agent(concat!("kit/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| {
            AuthError::unavailable("auth_transport_unavailable", "could not build TLS client")
        })
}

fn bounded_body(response: reqwest::blocking::Response) -> Result<Zeroizing<Vec<u8>>, AuthError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(AuthError::invalid(
            "auth_response_oversize",
            "authentication response exceeds 64 KiB",
        ));
    }
    let mut body = Zeroizing::new(Vec::new());
    response
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| {
            AuthError::unavailable(
                "auth_transport_failed",
                "could not read authentication response",
            )
        })?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(AuthError::invalid(
            "auth_response_oversize",
            "authentication response exceeds 64 KiB",
        ));
    }
    Ok(body)
}

fn revoke_at(record: &TokenRecord, deadline: Instant, revoke_url: &str) -> Result<(), AuthError> {
    let response = http_client(deadline)?
        .post(revoke_url)
        .form(&[
            ("token", record.refresh_token.as_str()),
            ("token_type_hint", "refresh_token"),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .map_err(|_| {
            AuthError::unavailable("token_revoke_failed", "token revocation transport failed")
        })?;
    let status = response.status();
    let _body = bounded_body(response)?;
    if status.is_success() {
        Ok(())
    } else {
        Err(AuthError::unavailable(
            "token_revoke_failed",
            "token revocation was rejected",
        ))
    }
}

struct ProcessLock(fs::File);

impl Drop for ProcessLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn process_lock(deadline: Instant) -> Result<ProcessLock, AuthError> {
    let path = auth_lock_path()?;
    let parent = path.parent().expect("auth lock path has a parent");
    fs::create_dir_all(parent).map_err(|_| {
        AuthError::unavailable("auth_lock_failed", "could not create the state directory")
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = fs::symlink_metadata(parent).map_err(|_| {
            AuthError::unavailable(
                "auth_lock_failed",
                "could not inspect the auth lock directory",
            )
        })?;
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(AuthError::unavailable(
                "auth_lock_failed",
                "the auth lock directory is not owned by the current OS user",
            ));
        }
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|_| {
            AuthError::unavailable(
                "auth_lock_failed",
                "could not secure the auth lock directory",
            )
        })?;
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(&path).map_err(|_| {
        AuthError::unavailable("auth_lock_failed", "could not open the authentication lock")
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| {
                AuthError::unavailable(
                    "auth_lock_failed",
                    "could not secure the authentication lock",
                )
            })?;
        let metadata = file.metadata().map_err(|_| {
            AuthError::unavailable(
                "auth_lock_failed",
                "could not inspect the authentication lock",
            )
        })?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(AuthError::unavailable(
                "auth_lock_failed",
                "the authentication lock is not a secure user-owned regular file",
            ));
        }
    }
    #[cfg(not(unix))]
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(AuthError::unavailable(
            "auth_lock_failed",
            "the authentication lock is not a regular file",
        ));
    }
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(ProcessLock(file)),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                return Err(AuthError::timeout(
                    "timed out waiting for the authentication lock",
                ));
            }
        }
    }
}

fn auth_lock_path() -> Result<std::path::PathBuf, AuthError> {
    os_user_data_dir()
        .map(|path| {
            #[cfg(unix)]
            return path.join(".kit-auth/openai-auth.lock");
            #[cfg(windows)]
            return path.join("kit/openai-auth.lock");
        })
        .ok_or_else(|| {
            AuthError::unavailable("auth_lock_failed", "the OS user directory is unavailable")
        })
}

#[cfg(unix)]
fn os_user_data_dir() -> Option<PathBuf> {
    use std::ffi::CStr;
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0_u8; 16 * 1024];
    // SAFETY: all pointers refer to live writable storage for the duration of getpwuid_r.
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    // SAFETY: a successful getpwuid_r initializes entry and its pw_dir points into buffer.
    let home = unsafe { CStr::from_ptr(entry.assume_init().pw_dir) };
    (!home.to_bytes().is_empty())
        .then(|| PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes())))
}

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;

#[cfg(windows)]
fn os_user_data_dir() -> Option<PathBuf> {
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath},
    };
    let mut raw = std::ptr::null_mut();
    // SAFETY: SHGetKnownFolderPath initializes raw on success; CoTaskMemFree releases it.
    let status =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, std::ptr::null_mut(), &mut raw) };
    if status < 0 || raw.is_null() {
        return None;
    }
    let mut length = 0;
    // SAFETY: raw is a NUL-terminated string returned by SHGetKnownFolderPath.
    while unsafe { *raw.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: the preceding scan established this initialized UTF-16 slice.
    let path = PathBuf::from(std::ffi::OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, length)
    }));
    // SAFETY: raw was allocated by SHGetKnownFolderPath.
    unsafe { CoTaskMemFree(raw.cast()) };
    Some(path)
}

#[cfg(windows)]
use std::os::windows::ffi::OsStringExt as _;

fn random_urlsafe<const N: usize>() -> Result<String, AuthError> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|_| {
        AuthError::unavailable(
            "secure_random_unavailable",
            "cryptographic randomness is unavailable",
        )
    })?;
    let value = URL_SAFE_NO_PAD.encode(bytes);
    bytes.zeroize();
    Ok(value)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    left.len() == right.len() && left.as_bytes().ct_eq(right.as_bytes()).into()
}

fn emit_auth_url(url: &str, format: OutputFormat) -> Result<(), AuthError> {
    let line = match format {
        OutputFormat::Human => format!("Open this URL to authenticate:\n{url}\n"),
        OutputFormat::Json => format!("Open this URL to authenticate: {url}\n"),
        OutputFormat::Jsonl => format!(
            "{}\n",
            json!({"type":"authorization_url","provider":"openai","url":url})
        ),
    };
    let result = if format == OutputFormat::Json {
        let mut stderr = std::io::stderr().lock();
        stderr
            .write_all(line.as_bytes())
            .and_then(|_| stderr.flush())
    } else {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(line.as_bytes())
            .and_then(|_| stdout.flush())
    };
    result.map_err(|_| {
        AuthError::unavailable(
            "auth_output_failed",
            "could not print the authorization URL",
        )
    })
}

#[cfg(not(windows))]
fn open_browser_bounded(url: &str) {
    let mut command = Command::new(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    });
    command.arg(url);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = command.spawn() {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(windows)]
fn open_browser_bounded(url: &str) {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    let operation = std::ffi::OsStr::new("open")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let url = std::ffi::OsStr::new(url)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both arguments are NUL-terminated UTF-16 strings and remain live for the call.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = if status == 200 { "OK" } else { "Error" };
    let body = message.as_bytes();
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn human(stdout: impl Into<String>) -> Output {
    Output {
        stdout: stdout.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode, jwk::Jwk};

    const TEST_RSA_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC3UQBTeVjOtSY4\nHaZHjSpQPlIIUXSiIq+WRInLoWwYEXmloR41HMmwsCQVV1WFZ7z0wUj14vD3/Bl6\nJG2JTU8ur+RvJojm1gXxg/etp4DG2HVtXong4QE7BKqJufHITMVuEhojkTulHIbW\nXfQQjaxQpGsOIuWcRz3YVB7zpAL7yoeHhvFd7RV+IqG9i4fjN4pzlCTv/TQig+s7\n539MsNx1ZakBfeBhx62JUPhFe6pXdPS2hXVUiTQRPMBm3GimDzyuA3WkVKzPyNMB\n2h+BALRFLslqPaFpul7NIifX36KgUPaimntpvFRxahqyDvJ9ATtq6oMeHaRUMZf5\nkRxjLIXLAgMBAAECggEAAIIV+SVDTMINyrwHo6J4NTlnACTm/jK7FTSNbpC8/E1t\nbpBwGqpAw4pJdKcFqAADSGkSFbRnrJhN+HEKE1uxK3+gp3o43kLw80bFX1Lb4DE7\nahkyp/qXsUfbB9S0dIoEm2srbWElWYN8ZYhkeSNGEKx+q3mx9JPx+kaJa2159flh\nis34maBeEr97gwjAvMjLbdVEpoaEIRC/hmem2ckT5jsDd4HS7RKNXwk/S8O7/PQW\n42xKAvL0APk5J53CDoW4DT78y7t4Rj/dVeRZAhdjDUFP+idZ1r9k6PM8vs5tl1P0\njzcOMzUBFmhnb5MKFvBLc4MKJQYzTT06/qdfAV0M2QKBgQDsoUF+pNQuERUKCI9T\nZey7rFgsbBkK2t0XvgpLwwMbF548HgL+QJhAAONaLe5+2GlZSgb5OgoYYspiuzQT\noz2mqeN2MSMnUtntyUt+Y6IzPEEPg6bVGdoCP3FSvz1L/JDJuJq4cqb3OGPe4yEt\nZDymqUJCDTO52vT0GLdZ6S70twKBgQDGUoYrbButBHX5nwE5XnrjgENGT4RauI76\nQ158MuFmRmpgaWlc37ByVyzMG7x9qxcad4Ry19hsG5KYnL/PNs31a2i/BdfLZyFF\nY0dfNExz6tKf4PWxZhhFhX94f7qseSzXLx8eMQqdds4WQsA13JI9qQJ0pVSNyb/V\nXM/n9XMrjQKBgDsKgSz4M3jLClTWjexhIhAxkE6FKjprIX8rC6abocrAudqGInkN\n5O8TSabWjwtXM/HzZooI0TwEajr4OqYrtNZAzWBQIlVNdtK9xvhiI7Zk8lbMonPJ\nX3vwGHZtAP5Upkuuo+whr0c/6qtSQJTyza9HzCBu6tkUqMm+4QCuDelBAoGAax8S\nF4w6WrcJHj7Tg3BUAmQ6clTrEbGUkPsoov88nmi0dsUZQzAT93681La6lkp+nS4n\nXXzXCnXONh6cwElC8CgHGP8H83cOEpOwbm0qSoZxJCh3rU2PGKYmFyku5JBDNyvd\nrAojSLBuWrnNZopwd1u91tGinT93HcEXD5yVi9UCgYBq1sjl5jlliyHzPWMeV3dn\nkJWDLMpCwrpmQzrhkA02PaZO1BB7QgZeIKTYkzECHT44wHflalVOEEsVZpEn2Ivd\nJz6j2JwX7Ke23MA0MDaV6+7syAwPKx3+pOGwdun2uZNgvS74IWeBEfdMhGrGncX0\nQegKxe+skNhLjXJ5SUTdZg==\n-----END PRIVATE KEY-----\n";

    #[derive(Default)]
    struct MemoryStore(Mutex<Option<Vec<u8>>>);

    impl CredentialStore for MemoryStore {
        fn load(&self) -> Result<Option<TokenRecord>, AuthError> {
            self.0
                .lock()
                .unwrap()
                .as_deref()
                .map(serde_json::from_slice)
                .transpose()
                .map_err(|_| AuthError::invalid("memory", "invalid record"))
        }

        fn save(&self, record: &TokenRecord) -> Result<(), AuthError> {
            *self.0.lock().unwrap() = Some(serde_json::to_vec(record).unwrap());
            Ok(())
        }

        fn delete(&self) -> Result<bool, AuthError> {
            Ok(self.0.lock().unwrap().take().is_some())
        }
    }

    struct FailingStore;

    impl CredentialStore for FailingStore {
        fn load(&self) -> Result<Option<TokenRecord>, AuthError> {
            Err(AuthError::unavailable(
                "credential_backend_unavailable",
                "synthetic keyring failure",
            ))
        }
        fn save(&self, _: &TokenRecord) -> Result<(), AuthError> {
            self.load().map(|_| ())
        }
        fn delete(&self) -> Result<bool, AuthError> {
            self.load().map(|_| false)
        }
    }

    fn callback_request(target: &str, extra: &str) -> (Callback, u16) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let target = target.to_owned();
        let extra = extra.to_owned();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                stream,
                "GET {target} HTTP/1.1\r\nHost: localhost:{port}\r\n{extra}\r\n"
            )
            .unwrap();
        });
        let (mut stream, _) = listener.accept().unwrap();
        let result = callback(&mut stream, port, "expected").unwrap();
        client.join().unwrap();
        (result, port)
    }

    fn jwt(
        audience: &str,
        nonce: Option<&str>,
        kid: &str,
        expires_in: i64,
        not_before_in: Option<i64>,
    ) -> String {
        let now = unix_seconds();
        let mut claims = json!({
            "iss": ISSUER,
            "aud": audience,
            "exp": now + expires_in,
            "iat": now,
            "chatgpt_account_id": "account-one",
        });
        if let Some(nonce) = nonce {
            claims["nonce"] = json!(nonce);
        }
        if let Some(not_before_in) = not_before_in {
            claims["nbf"] = json!(now + not_before_in);
        }
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_owned());
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(TEST_RSA_KEY.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    fn test_jwks(kid: &str) -> String {
        let key = EncodingKey::from_rsa_pem(TEST_RSA_KEY.as_bytes()).unwrap();
        let mut jwk = Jwk::from_encoding_key(&key, Algorithm::RS256).unwrap();
        jwk.common.key_id = Some(kid.to_owned());
        jwk.common.public_key_use = Some(PublicKeyUse::Signature);
        jwk.common.key_operations = Some(vec![KeyOperations::Verify]);
        serde_json::to_string(&JwkSet { keys: vec![jwk] }).unwrap()
    }

    fn serve_once(body: String) -> (String, std::thread::JoinHandle<String>) {
        serve_once_status(body, 200)
    }

    fn serve_once_status(body: String, status: u16) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 2048];
            loop {
                let count = stream.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..count]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}/oauth/token"), handle)
    }

    fn serve_jwks(bodies: Vec<String>) -> (String, std::thread::JoinHandle<usize>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let count = bodies.len();
        let handle = std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let read = stream.read(&mut chunk).unwrap();
                    request.extend_from_slice(&chunk[..read]);
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .unwrap();
            }
            count
        });
        (format!("http://{address}/.well-known/jwks.json"), handle)
    }

    #[test]
    fn authorization_url_has_exact_native_flow_parameters() {
        let url = url::Url::parse(&authorize_url(
            "http://localhost:1455/auth/callback",
            "c",
            "s",
            "n",
        ))
        .unwrap();
        let params = url
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(url.origin().ascii_serialization(), ISSUER);
        assert_eq!(params["client_id"], CLIENT_ID);
        assert_eq!(
            params["redirect_uri"],
            "http://localhost:1455/auth/callback"
        );
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["nonce"], "n");
        assert_eq!(params["originator"], "kit");
        assert!(params["scope"].contains("api.connectors.invoke"));
    }

    #[test]
    fn login_secrets_are_explicitly_zeroizable() {
        let mut secrets = LoginSecrets {
            verifier: "verifier".into(),
            state: "state".into(),
            nonce: "nonce".into(),
        };
        secrets.zeroize();
        assert!(secrets.verifier.is_empty());
        assert!(secrets.state.is_empty());
        assert!(secrets.nonce.is_empty());
    }

    #[test]
    fn token_debug_never_contains_secrets() {
        let record = TokenRecord {
            access_token: "ACCESS_CANARY".into(),
            refresh_token: "REFRESH_CANARY".into(),
            id_token: "ID_CANARY".into(),
            expires_at: 1,
            account_id: None,
            email: None,
            plan_type: None,
            generation: "generation-one".into(),
        };
        let debug = format!("{record:?}");
        assert!(!debug.contains("CANARY"));
    }

    #[test]
    fn memory_backend_rotates_one_record_and_keyring_failure_is_typed() {
        let store = MemoryStore::default();
        let first = TokenRecord {
            generation: "generation-one".into(),
            access_token: "one".into(),
            refresh_token: "refresh-one".into(),
            id_token: "id-one".into(),
            expires_at: 1,
            account_id: Some("account-one".into()),
            email: None,
            plan_type: None,
        };
        let mut second = first.clone();
        second.access_token = "two".into();
        second.account_id = Some("account-two".into());
        store.save(&first).unwrap();
        store.save(&second).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.access_token(), "two");
        assert_eq!(loaded.account_id(), Some("account-two"));
        assert!(store.delete().unwrap());
        assert!(store.load().unwrap().is_none());
        let error = FailingStore.load().unwrap_err();
        assert_eq!(error.code, "credential_backend_unavailable");
    }

    #[test]
    fn rejected_token_generation_refreshes_once_past_old_id_expiry_and_logout_wins() {
        let kid = "refresh-key";
        let store = std::sync::Arc::new(MemoryStore::default());
        store
            .save(&TokenRecord {
                access_token: "rejected-access-token".into(),
                refresh_token: "refresh-token".into(),
                id_token: jwt(CLIENT_ID, None, kid, -3600, None),
                expires_at: unix_seconds() + 3600,
                account_id: Some("account-one".into()),
                email: None,
                plan_type: None,
                generation: "generation-one".into(),
            })
            .unwrap();
        let old_id_token = store.load().unwrap().unwrap().id_token.clone();
        let body = json!({
            "access_token": jwt("https://api.openai.com/v1", None, kid, 3600, None),
            "expires_in": 3600,
            "token_type": "Bearer",
        })
        .to_string();
        let (token_url, server) = serve_once(body);
        let (jwks_url, jwks_server) = serve_once(test_jwks(kid));
        let jwks_url = jwks_url.replace("/oauth/token", "/.well-known/jwks.json");
        let mut workers = Vec::new();
        for _ in 0..8 {
            let store = std::sync::Arc::clone(&store);
            let token_url = token_url.clone();
            let jwks_url = jwks_url.clone();
            workers.push(std::thread::spawn(move || {
                let _guard = REFRESH_LOCK
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                refresh_current_at(
                    store.as_ref(),
                    Instant::now() + Duration::from_secs(5),
                    Some("rejected-access-token"),
                    &token_url,
                    &jwks_url,
                )
                .unwrap()
                .access_token()
                .to_owned()
            }));
        }
        let tokens = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert!(tokens.windows(2).all(|pair| pair[0] == pair[1]));
        let refreshed = store.load().unwrap().unwrap();
        assert_eq!(refreshed.refresh_token, "refresh-token");
        assert_eq!(refreshed.account_id.as_deref(), Some("account-one"));
        assert_eq!(refreshed.id_token, old_id_token);
        let request = server.join().unwrap();
        jwks_server.join().unwrap();
        assert!(request.starts_with("POST /oauth/token HTTP/1.1\r\n"));
        assert!(request.contains("content-type: application/x-www-form-urlencoded"));
        assert!(request.contains("grant_type=refresh_token"));
        assert!(request.contains("refresh_token=refresh-token"));

        store.delete().unwrap();
        let error = refresh_current(
            store.as_ref(),
            Instant::now() + Duration::from_secs(1),
            Some(tokens[0].as_str()),
            "http://127.0.0.1:1/oauth/token",
        )
        .unwrap_err();
        assert_eq!(error.code, "openai_auth_required");
    }

    #[test]
    fn loopback_code_exchange_and_revoke_use_native_protocols() {
        let kid = "exchange-key";
        let body = json!({
            "access_token": jwt("https://api.openai.com/v1", None, kid, 3600, None),
            "refresh_token": "refresh-token",
            "id_token": jwt(CLIENT_ID, Some("expected-nonce"), kid, 3600, None),
            "expires_in": 3600,
            "token_type": "Bearer",
        })
        .to_string();
        let (token_url, token_server) = serve_once(body);
        let (jwks_url, jwks_server) = serve_once(test_jwks(kid));
        let record = exchange_code_at(
            "authorization-code",
            "http://localhost:1455/auth/callback",
            "pkce-verifier",
            "expected-nonce",
            Instant::now() + Duration::from_secs(5),
            &token_url,
            &jwks_url.replace("/oauth/token", "/.well-known/jwks.json"),
        )
        .unwrap();
        let request = token_server.join().unwrap();
        jwks_server.join().unwrap();
        assert!(request.contains("content-type: application/x-www-form-urlencoded"));
        assert!(request.contains("grant_type=authorization_code"));
        assert!(request.contains("code=authorization-code"));
        assert!(request.contains("code_verifier=pkce-verifier"));

        let (revoke_url, revoke_server) = serve_once(String::new());
        revoke_at(
            &record,
            Instant::now() + Duration::from_secs(5),
            &revoke_url.replace("/oauth/token", "/oauth/revoke"),
        )
        .unwrap();
        let request = revoke_server.join().unwrap();
        assert!(request.starts_with("POST /oauth/revoke HTTP/1.1\r\n"));
        assert!(request.contains("content-type: application/x-www-form-urlencoded"));
        assert!(request.contains("token=refresh-token"));
        assert!(request.contains("token_type_hint=refresh_token"));
        assert!(request.contains(&format!("client_id={CLIENT_ID}")));
    }

    #[test]
    fn signed_jwt_rejects_bad_signatures_and_refreshes_unknown_kid_once() {
        JWKS_CACHE.lock().unwrap().clear();
        let token = jwt("https://api.openai.com/v1", None, "rotated-key", 3600, None);
        let (jwks_url, server) = serve_jwks(vec![test_jwks("old-key"), test_jwks("rotated-key")]);
        let claims = verify_claims(
            &token,
            "https://api.openai.com/v1",
            None,
            Instant::now() + Duration::from_secs(5),
            &jwks_url,
        )
        .unwrap();
        assert_eq!(
            extract_account_id(&claims).unwrap().as_deref(),
            Some("account-one")
        );
        assert_eq!(server.join().unwrap(), 2);

        let mut bad = token.into_bytes();
        let signature = bad.iter().rposition(|byte| *byte == b'.').unwrap() + 1;
        bad[signature] = if bad[signature] == b'A' { b'B' } else { b'A' };
        let bad = String::from_utf8(bad).unwrap();
        let error = verify_claims(
            &bad,
            "https://api.openai.com/v1",
            None,
            Instant::now() + Duration::from_secs(1),
            &jwks_url,
        )
        .err()
        .unwrap();
        assert_eq!(error.code, "token_invalid");

        let now = unix_seconds();
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("rotated-key".into());
        let unexpected = encode(
            &header,
            &json!({
                "iss": ISSUER,
                "aud": "https://api.openai.com/v1",
                "exp": now + 3600,
                "iat": now,
                "chatgpt_account_id": "account-one",
            }),
            &EncodingKey::from_secret(b"01234567890123456789012345678901"),
        )
        .unwrap();
        assert!(
            verify_claims(
                &unexpected,
                "https://api.openai.com/v1",
                None,
                Instant::now() + Duration::from_secs(1),
                &jwks_url,
            )
            .is_err()
        );
        let none = format!(
            "{}.{}.\n",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
            URL_SAFE_NO_PAD.encode(br#"{}"#),
        );
        assert!(
            verify_claims(
                none.trim(),
                "https://api.openai.com/v1",
                None,
                Instant::now() + Duration::from_secs(1),
                &jwks_url,
            )
            .is_err()
        );
    }

    #[test]
    fn jwt_not_before_uses_the_bounded_clock_skew() {
        for (not_before_in, accepted) in [(30, true), (120, false)] {
            let kid = format!("nbf-key-{not_before_in}");
            let token = jwt(
                "https://api.openai.com/v1",
                None,
                &kid,
                3600,
                Some(not_before_in),
            );
            let (jwks_url, server) = serve_once(test_jwks(&kid));
            let result = verify_claims(
                &token,
                "https://api.openai.com/v1",
                None,
                Instant::now() + Duration::from_secs(5),
                &jwks_url.replace("/oauth/token", "/.well-known/jwks.json"),
            );
            assert_eq!(result.is_ok(), accepted);
            server.join().unwrap();
        }
    }

    #[test]
    fn revoke_failure_retains_credentials_and_local_only_is_explicit() {
        let store = MemoryStore::default();
        store
            .save(&TokenRecord {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                id_token: "id".into(),
                expires_at: 1,
                account_id: Some("account-one".into()),
                email: None,
                plan_type: None,
                generation: "generation-one".into(),
            })
            .unwrap();
        let (revoke_url, server) = serve_once_status(String::new(), 503);
        let error = logout_at(
            &store,
            OutputFormat::Human,
            Instant::now() + Duration::from_secs(5),
            false,
            &revoke_url.replace("/oauth/token", "/oauth/revoke"),
        )
        .unwrap_err();
        server.join().unwrap();
        assert_eq!(error.code, "token_revoke_failed");
        assert!(store.load().unwrap().is_some());

        let output = logout_at(
            &store,
            OutputFormat::Human,
            Instant::now() + Duration::from_secs(5),
            true,
            "http://127.0.0.1:1/unused",
        )
        .unwrap();
        assert!(output.stdout.starts_with("WARNING:"));
        assert!(store.load().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn auth_lock_path_ignores_environment_and_contends_across_processes() {
        let expected = auth_lock_path().unwrap();
        let marker = std::env::temp_dir().join(format!(
            "kit-auth-lock-test-{}-{}",
            std::process::id(),
            unix_seconds()
        ));
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "provider::openai_auth::tests::auth_lock_environment_child",
            ])
            .env("HOME", "/tmp/not-the-os-home")
            .env("XDG_CONFIG_HOME", "/tmp/not-the-os-xdg")
            .env("KIT_EXPECTED_AUTH_LOCK", &expected)
            .env("KIT_AUTH_LOCK_MARKER", &marker)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < ready_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            marker.exists(),
            "child did not acquire the canonical auth lock"
        );
        let error = process_lock(Instant::now() + Duration::from_millis(100))
            .err()
            .unwrap();
        assert_eq!(error.code, "openai_auth_timeout");
        assert!(child.wait().unwrap().success());
        fs::remove_file(marker).unwrap();
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess helper for auth lock environment test"]
    fn auth_lock_environment_child() {
        assert_eq!(
            auth_lock_path().unwrap(),
            PathBuf::from(std::env::var_os("KIT_EXPECTED_AUTH_LOCK").unwrap())
        );
        let _lock = process_lock(Instant::now() + Duration::from_secs(1)).unwrap();
        fs::write(std::env::var_os("KIT_AUTH_LOCK_MARKER").unwrap(), b"ready").unwrap();
        std::thread::sleep(Duration::from_millis(500));
    }

    #[test]
    fn callback_rejects_csrf_duplicates_and_request_smuggling() {
        let (result, _) = callback_request("/auth/callback?code=x&state=wrong", "");
        assert!(matches!(result, Callback::Ignore(400, _)));

        let (result, _) = callback_request(
            "/auth/callback?code=x&scope=openid+profile&state=expected&unknown=y",
            "",
        );
        assert!(matches!(result, Callback::Code(code) if code == "x"));

        for target in [
            "/auth/callback?code=x&code=y&state=expected",
            "/auth/callback?code=x&state=expected#error",
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let target = target.to_owned();
            std::thread::spawn(move || {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(
                    stream,
                    "GET {target} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n"
                )
                .unwrap();
            });
            let (mut stream, _) = listener.accept().unwrap();
            assert!(callback(&mut stream, port, "expected").is_err());
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(stream, "GET /auth/callback?code=x&state=expected HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Length: 0\r\n\r\n").unwrap();
        });
        let (mut stream, _) = listener.accept().unwrap();
        assert!(callback(&mut stream, port, "expected").is_err());
    }

    #[test]
    #[ignore = "requires an interactive OS keyring entry created by kit auth login openai"]
    fn real_openai_auth_smoke() {
        let output = status(
            &OsCredentialStore,
            OutputFormat::Json,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
        let value: Value = serde_json::from_str(output.stdout.trim()).unwrap();
        assert_eq!(value["authenticated"], true);
    }
}
