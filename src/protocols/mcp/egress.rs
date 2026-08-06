use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use http::{
    HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
    header::{AUTHORIZATION, LOCATION},
};

use crate::{
    capabilities::kernel::identity::{Digest, DigestAlgorithm},
    domain::{
        egress::{
            Authorization, Denial, Destination, EgressPolicy, MAX_EGRESS_URL_BYTES, MAX_REDIRECTS,
            MAX_RESOLVED_ADDRESSES, Scheme,
        },
        secret::{SecretHandle, SecretLease},
    },
    telemetry::redact::{CaptureRedactor, SensitiveDataScanner},
};

const MAX_BEARER_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpCredentialError {
    Denied,
    Unavailable,
    Invalid,
}

impl fmt::Display for HttpCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Denied => "MCP credential resolution denied",
            Self::Unavailable => "MCP credential resolver unavailable",
            Self::Invalid => "MCP credential is not a valid bounded bearer value",
        })
    }
}

impl std::error::Error for HttpCredentialError {}

pub struct HttpSecretContext<'a> {
    pub(crate) principal_id: &'a str,
    pub(crate) project_id: &'a str,
    pub(crate) workspace_id: &'a str,
    pub(crate) invocation_id: &'a str,
    pub(crate) decision_digest: &'a str,
    pub(crate) request_digest: &'a str,
    pub(crate) scope: Option<&'a str>,
    pub(crate) operation: &'a str,
    pub(crate) endpoint: &'a str,
    pub(crate) destination_digest: &'a str,
    pub(crate) hop: usize,
}

impl HttpSecretContext<'_> {
    pub fn principal_id(&self) -> &str {
        self.principal_id
    }
    pub fn project_id(&self) -> &str {
        self.project_id
    }
    pub fn workspace_id(&self) -> &str {
        self.workspace_id
    }
    pub fn invocation_id(&self) -> &str {
        self.invocation_id
    }
    pub fn decision_digest(&self) -> &str {
        self.decision_digest
    }
    pub fn request_digest(&self) -> &str {
        self.request_digest
    }
    pub fn scope(&self) -> Option<&str> {
        self.scope
    }
    pub fn operation(&self) -> &str {
        self.operation
    }
    pub fn endpoint(&self) -> &str {
        self.endpoint
    }
    pub fn destination_digest(&self) -> &str {
        self.destination_digest
    }
    pub const fn hop(&self) -> usize {
        self.hop
    }
}

#[async_trait::async_trait]
pub trait HttpCredentialBroker: Send + Sync + 'static {
    async fn authorize_and_resolve(
        &self,
        handle: &SecretHandle,
        context: &HttpSecretContext<'_>,
    ) -> Result<SecretLease, HttpCredentialError>;
}

#[derive(Clone, Copy, Debug)]
pub struct McpEgressLimits {
    pub max_location_bytes: usize,
    pub max_headers: usize,
    pub max_header_bytes: usize,
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
}

pub struct McpEgressRequest {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub struct McpEgressResponse {
    response: reqwest::Response,
    redirects: usize,
    deadline: tokio::time::Instant,
    scanner: Arc<McpResponseScanner>,
}

impl McpEgressResponse {
    pub fn response(&self) -> &reqwest::Response {
        &self.response
    }

    pub const fn redirects(&self) -> usize {
        self.redirects
    }

    pub fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        reqwest::Response,
        tokio::time::Instant,
        Arc<McpResponseScanner>,
    ) {
        (self.response, self.deadline, self.scanner)
    }
}

impl std::ops::Deref for McpEgressResponse {
    type Target = reqwest::Response;

    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

pub(crate) struct McpResponseScanner {
    ingress: Mutex<SensitiveDataScanner>,
    canonical: Mutex<SensitiveDataScanner>,
    callback: Mutex<SensitiveDataScanner>,
}

impl McpResponseScanner {
    pub(crate) fn new(credentials: &[SecretLease]) -> Self {
        let scanner = CaptureRedactor::new(credentials).scanner();
        Self {
            ingress: Mutex::new(scanner.fork()),
            canonical: Mutex::new(scanner.fork()),
            callback: Mutex::new(scanner),
        }
    }

    pub(crate) fn scan_ingress(&self, bytes: &[u8]) -> Result<bool, ()> {
        scan(&self.ingress, bytes)
    }

    pub(crate) fn scan_canonical(&self, bytes: &[u8]) -> Result<bool, ()> {
        scan(&self.canonical, bytes)
    }

    pub(crate) fn scan_callback(&self, bytes: &[u8]) -> Result<bool, ()> {
        let mut scanner = self.callback.lock().map_err(|_| ())?;
        scanner.reset();
        scanner.push(bytes);
        let found = scanner.found();
        scanner.reset();
        Ok(found)
    }
}

fn scan(scanner: &Mutex<SensitiveDataScanner>, bytes: &[u8]) -> Result<bool, ()> {
    let mut scanner = scanner.lock().map_err(|_| ())?;
    scanner.push(bytes);
    Ok(scanner.found())
}

pub struct EgressDialResponse {
    pub response: reqwest::Response,
    pub peer: Option<IpAddr>,
}

#[async_trait::async_trait]
pub trait EgressDialer: Send + Sync + 'static {
    async fn send(
        &self,
        request: reqwest::Request,
        authorization: &Authorization,
        limits: McpEgressLimits,
    ) -> Result<EgressDialResponse, std::io::Error>;
}

#[derive(Default)]
pub struct ProductionEgressDialer;

#[async_trait::async_trait]
impl EgressDialer for ProductionEgressDialer {
    async fn send(
        &self,
        request: reqwest::Request,
        authorization: &Authorization,
        limits: McpEgressLimits,
    ) -> Result<EgressDialResponse, std::io::Error> {
        let host = authorization.destination().host();
        let port = authorization.destination().port();
        let addresses = authorization
            .resolved_addresses()
            .map(|address| SocketAddr::new(address, port))
            .collect::<Vec<_>>();
        if addresses.is_empty() || addresses.len() > MAX_RESOLVED_ADDRESSES {
            return Err(std::io::Error::other("invalid MCP egress address set"));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .resolve_to_addrs(&host, &addresses)
            .connect_timeout(limits.connect_timeout)
            .timeout(limits.request_timeout)
            .pool_max_idle_per_host(0)
            .build()
            .map_err(std::io::Error::other)?;
        let response = client.execute(request).await.map_err(|error| {
            if error.is_timeout() {
                std::io::Error::new(std::io::ErrorKind::TimedOut, error)
            } else {
                std::io::Error::other(error)
            }
        })?;
        let peer = response.remote_addr().map(|address| address.ip());
        Ok(EgressDialResponse { response, peer })
    }
}

#[derive(Debug)]
pub enum McpEgressError {
    Denied(Denial),
    InvalidRequest,
    InvalidHeader,
    RedirectLocation,
    RedirectLoop,
    HttpsDowngrade,
    AmbiguousMethodRewrite,
    PeerUnavailable,
    Timeout,
    Credential(HttpCredentialError),
    Io(std::io::Error),
}

impl fmt::Display for McpEgressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied(error) => error.fmt(formatter),
            Self::InvalidRequest => formatter.write_str("invalid bounded MCP egress request"),
            Self::InvalidHeader => formatter.write_str("invalid bounded MCP egress headers"),
            Self::RedirectLocation => formatter.write_str("invalid MCP redirect location"),
            Self::RedirectLoop => formatter.write_str("MCP redirect loop denied"),
            Self::HttpsDowngrade => formatter.write_str("MCP HTTPS downgrade denied"),
            Self::AmbiguousMethodRewrite => {
                formatter.write_str("ambiguous MCP redirect method rewrite denied")
            }
            Self::PeerUnavailable => formatter.write_str("MCP egress peer address unavailable"),
            Self::Timeout => formatter.write_str("MCP egress request timed out"),
            Self::Credential(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for McpEgressError {}

impl From<Denial> for McpEgressError {
    fn from(value: Denial) -> Self {
        Self::Denied(value)
    }
}

pub struct McpEgressConnector {
    policy: EgressPolicy,
    initial_authorization: Option<Authorization>,
    credentials: Arc<dyn HttpCredentialBroker>,
    dialer: Arc<dyn EgressDialer>,
    limits: McpEgressLimits,
    execution: tokio::sync::Mutex<ConnectorState>,
}

struct ConnectorState {
    used: bool,
    dns_rebinding: bool,
    authorized_destinations: BTreeMap<Destination, Option<BTreeSet<IpAddr>>>,
}

impl McpEgressConnector {
    pub fn new(
        policy: EgressPolicy,
        credentials: Arc<dyn HttpCredentialBroker>,
        limits: McpEgressLimits,
    ) -> Self {
        Self::with_dialer(
            policy,
            credentials,
            Arc::new(ProductionEgressDialer),
            limits,
        )
    }

    pub(crate) fn with_initial_authorization(mut self, authorization: Authorization) -> Self {
        if let Some(addresses) = self
            .execution
            .get_mut()
            .authorized_destinations
            .get_mut(authorization.destination())
        {
            *addresses = Some(authorization.resolved_addresses().collect());
        }
        self.initial_authorization = Some(authorization);
        self
    }

    pub fn with_dialer(
        policy: EgressPolicy,
        credentials: Arc<dyn HttpCredentialBroker>,
        dialer: Arc<dyn EgressDialer>,
        limits: McpEgressLimits,
    ) -> Self {
        let authorized_destinations = policy
            .configured_destinations()
            .cloned()
            .map(|destination| (destination, None))
            .collect();
        Self {
            policy,
            initial_authorization: None,
            credentials,
            dialer,
            limits,
            execution: tokio::sync::Mutex::new(ConnectorState {
                used: false,
                dns_rebinding: false,
                authorized_destinations,
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        request: McpEgressRequest,
        principal_id: &str,
        project_id: &str,
        workspace_id: &str,
        invocation_id: &str,
        decision_digest: &str,
        request_digest: &str,
        scope: Option<&str>,
        operation: &str,
    ) -> Result<McpEgressResponse, McpEgressError> {
        self.execute_before(
            request,
            principal_id,
            project_id,
            workspace_id,
            invocation_id,
            decision_digest,
            request_digest,
            scope,
            operation,
            tokio::time::Instant::now() + self.limits.request_timeout,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_before(
        &self,
        mut request: McpEgressRequest,
        principal_id: &str,
        project_id: &str,
        workspace_id: &str,
        invocation_id: &str,
        decision_digest: &str,
        request_digest: &str,
        scope: Option<&str>,
        operation: &str,
        deadline: tokio::time::Instant,
    ) -> Result<McpEgressResponse, McpEgressError> {
        let mut execution = tokio::time::timeout_at(deadline, self.execution.lock())
            .await
            .map_err(|_| McpEgressError::Timeout)?;
        if execution.dns_rebinding {
            return Err(McpEgressError::Denied(Denial::DnsRebinding));
        }
        if request.url.is_empty()
            || request.url.len() > MAX_EGRESS_URL_BYTES
            || request.method == Method::CONNECT
            || request.method == Method::TRACE
        {
            return Err(McpEgressError::InvalidRequest);
        }
        check_headers(&request.headers, self.limits)?;
        if request.headers.contains_key(AUTHORIZATION)
            || request
                .headers
                .contains_key(http::header::PROXY_AUTHORIZATION)
        {
            return Err(McpEgressError::InvalidHeader);
        }
        let mut current = canonical_connector_url(&request.url)?;
        let first_request = !execution.used;
        execution.used = true;
        let mut authorization = match (&self.initial_authorization, first_request) {
            (Some(authorization), _) if insecure_test_url(&current) => authorization.clone(),
            (Some(authorization), true) => {
                let grant = self.policy.grant_for_url(current.as_str())?;
                if grant.destination() != authorization.destination()
                    || grant.credential() != authorization.credential()
                {
                    return Err(McpEgressError::Denied(Denial::DestinationNotGranted));
                }
                authorization.clone()
            }
            (Some(initial), false) => {
                let destination = self.policy.grant_for_url(current.as_str())?.destination();
                let current = tokio::time::timeout_at(
                    deadline,
                    self.policy.resolve_initial_granted(current.as_str()),
                )
                .await
                .map_err(|_| McpEgressError::Timeout)?
                .map_err(|error| resolution_error(&mut execution, destination, error))?;
                if current.destination() != initial.destination()
                    || current.credential() != initial.credential()
                    || current.resolved_addresses().collect::<BTreeSet<_>>()
                        != initial.resolved_addresses().collect::<BTreeSet<_>>()
                {
                    execution.dns_rebinding = true;
                    return Err(McpEgressError::Denied(Denial::DnsRebinding));
                }
                current
            }
            (None, _) => {
                let destination = self.policy.grant_for_url(current.as_str())?.destination();
                tokio::time::timeout_at(
                    deadline,
                    self.policy.resolve_initial_granted(current.as_str()),
                )
                .await
                .map_err(|_| McpEgressError::Timeout)?
                .map_err(|error| resolution_error(&mut execution, destination, error))?
            }
        };
        let mut seen = BTreeSet::from([current.as_str().to_owned()]);
        let mut credentials = Vec::new();
        let mut outbound_scanner = None;

        loop {
            let hop = authorization.redirect_count();
            let destination_digest = destination_digest(&authorization);
            let destination = authorization.destination();
            let addresses = authorization.resolved_addresses().collect::<BTreeSet<_>>();
            match execution.authorized_destinations.get_mut(destination) {
                Some(Some(authorized)) if *authorized != addresses => {
                    execution.dns_rebinding = true;
                    return Err(McpEgressError::Denied(Denial::DnsRebinding));
                }
                Some(slot @ None) => *slot = Some(addresses),
                Some(Some(_)) => {}
                None if insecure_test_url(&current) => {}
                None => return Err(McpEgressError::Denied(Denial::DestinationNotGranted)),
            }
            let handle = SecretHandle::parse(authorization.credential().as_str())
                .map_err(|_| McpEgressError::Credential(HttpCredentialError::Invalid))?;
            let lease = tokio::time::timeout_at(
                deadline,
                self.credentials.authorize_and_resolve(
                    &handle,
                    &HttpSecretContext {
                        principal_id,
                        project_id,
                        workspace_id,
                        invocation_id,
                        decision_digest,
                        request_digest,
                        scope,
                        operation,
                        endpoint: current.as_str(),
                        destination_digest: &destination_digest,
                        hop,
                    },
                ),
            )
            .await
            .map_err(|_| McpEgressError::Timeout)?
            .map_err(McpEgressError::Credential)?;
            let credential_changed = credentials
                .last()
                .is_none_or(|previous: &SecretLease| previous.expose() != lease.expose());
            if credential_changed {
                outbound_scanner =
                    Some(CaptureRedactor::new(std::slice::from_ref(&lease)).scanner());
            }
            let mut headers = if hop == 0 {
                request.headers.clone()
            } else {
                redirect_headers(&request.headers)
            };
            let authorization_header = bearer_header(&lease)?;
            headers.insert(AUTHORIZATION, authorization_header);
            if outbound_reflects_credential(
                current.as_str().as_bytes(),
                &request.body,
                &headers,
                outbound_scanner
                    .as_mut()
                    .expect("the active credential has a scanner"),
            ) {
                scrub_bytes(&mut request.body);
                return Err(McpEgressError::InvalidRequest);
            }
            let mut outbound = reqwest::Request::new(request.method.clone(), current.clone());
            *outbound.headers_mut() = headers;
            *outbound.body_mut() = Some(reqwest::Body::from(request.body.clone()));
            if credential_changed {
                credentials.push(lease);
            }
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(McpEgressError::Timeout)?;
            let limits = McpEgressLimits {
                request_timeout: remaining,
                connect_timeout: self.limits.connect_timeout.min(remaining),
                ..self.limits
            };
            let dialed = tokio::time::timeout_at(
                deadline,
                self.dialer.send(outbound, &authorization, limits),
            )
            .await
            .map_err(|_| McpEgressError::Timeout)?
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::TimedOut {
                    McpEgressError::Timeout
                } else {
                    McpEgressError::Io(error)
                }
            })?;
            let peer = dialed.peer.ok_or(McpEgressError::PeerUnavailable)?;
            if !insecure_test_peer(&current, &authorization, peer) {
                if !authorization.authorizes_peer(peer) {
                    execution.dns_rebinding = true;
                    return Err(McpEgressError::Denied(Denial::DnsRebinding));
                }
                self.policy.validate_peer(&authorization, peer)?;
            }
            let mut response_scanner = CaptureRedactor::new(&credentials).scanner();
            check_response_metadata(
                current.as_str().as_bytes(),
                dialed.response.headers(),
                self.limits,
                &mut response_scanner,
            )?;

            if !is_redirect(dialed.response.status()) {
                return Ok(McpEgressResponse {
                    response: dialed.response,
                    redirects: hop,
                    deadline,
                    scanner: Arc::new(McpResponseScanner::new(&credentials)),
                });
            }
            if matches!(
                dialed.response.status(),
                StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER
            ) && request.method != Method::GET
                && request.method != Method::HEAD
            {
                return Err(McpEgressError::AmbiguousMethodRewrite);
            }
            let location = exact_location(dialed.response.headers(), self.limits)?;
            scan_secret_bytes(location.as_bytes(), &mut response_scanner)?;
            validate_location_reference(location)?;
            let next = current
                .join(location)
                .map_err(|_| McpEgressError::RedirectLocation)?;
            let next = EgressPolicy::canonical_url(next.as_str())?;
            if next.as_str().len() > MAX_EGRESS_URL_BYTES {
                return Err(McpEgressError::RedirectLocation);
            }
            scan_secret_bytes(next.as_str().as_bytes(), &mut response_scanner)?;
            if current.scheme() == "https" && next.scheme() != "https" {
                return Err(McpEgressError::HttpsDowngrade);
            }
            if !seen.insert(next.as_str().to_owned()) {
                return Err(McpEgressError::RedirectLoop);
            }
            if hop >= MAX_REDIRECTS {
                return Err(McpEgressError::Denied(Denial::RedirectLimit));
            }
            let destination = self.policy.grant_for_url(next.as_str())?.destination();
            authorization = tokio::time::timeout_at(
                deadline,
                self.policy
                    .resolve_redirect_granted(&authorization, next.as_str()),
            )
            .await
            .map_err(|_| McpEgressError::Timeout)?
            .map_err(|error| resolution_error(&mut execution, destination, error))?;
            current = next;
        }
    }
}

fn resolution_error(
    state: &mut ConnectorState,
    destination: &Destination,
    error: Denial,
) -> McpEgressError {
    if matches!(
        error,
        Denial::EmptyResolution
            | Denial::PrivateAddress
            | Denial::ResolutionLimit
            | Denial::DnsRebinding
    ) && matches!(
        state.authorized_destinations.get(destination),
        Some(Some(_))
    ) {
        state.dns_rebinding = true;
        McpEgressError::Denied(Denial::DnsRebinding)
    } else {
        McpEgressError::Denied(error)
    }
}

#[cfg(test)]
fn insecure_test_peer(url: &url::Url, authorization: &Authorization, peer: IpAddr) -> bool {
    url.scheme() == "http" && peer.is_loopback() && authorization.authorizes_peer(peer)
}

#[cfg(test)]
fn insecure_test_url(url: &url::Url) -> bool {
    url.scheme() == "http"
        && url
            .host()
            .is_some_and(|host| matches!(host, url::Host::Ipv4(address) if address.is_loopback()))
}

#[cfg(not(test))]
const fn insecure_test_peer(_: &url::Url, _: &Authorization, _: IpAddr) -> bool {
    false
}

#[cfg(not(test))]
const fn insecure_test_url(_: &url::Url) -> bool {
    false
}

fn digest(bytes: &[u8]) -> String {
    Digest::of(DigestAlgorithm::Sha256, bytes).to_string()
}

fn destination_digest(authorization: &Authorization) -> String {
    let destination = authorization.destination();
    let scheme = match destination.scheme() {
        Scheme::Http => "http",
        Scheme::Https => "https",
    };
    digest(
        format!(
            "{scheme}://{}:{}\n{}",
            destination.host(),
            destination.port(),
            authorization.credential().as_str()
        )
        .as_bytes(),
    )
}

fn bearer_header(lease: &SecretLease) -> Result<HeaderValue, McpEgressError> {
    let secret = lease.expose();
    if secret.is_empty()
        || secret.len() > MAX_BEARER_BYTES
        || secret.iter().any(|byte| !byte.is_ascii_graphic())
    {
        return Err(McpEgressError::Credential(HttpCredentialError::Invalid));
    }
    let mut bearer = Vec::with_capacity("Bearer ".len() + secret.len());
    bearer.extend_from_slice(b"Bearer ");
    bearer.extend_from_slice(secret);
    let mut value = HeaderValue::from_bytes(&bearer)
        .map_err(|_| McpEgressError::Credential(HttpCredentialError::Invalid))?;
    bearer.fill(0);
    value.set_sensitive(true);
    Ok(value)
}

pub(crate) fn check_headers(
    headers: &HeaderMap,
    limits: McpEgressLimits,
) -> Result<(), McpEgressError> {
    if headers.len() > limits.max_headers {
        return Err(McpEgressError::InvalidHeader);
    }
    let bytes = headers.iter().try_fold(0usize, |total, (name, value)| {
        total
            .checked_add(name.as_str().len())?
            .checked_add(value.as_bytes().len())
    });
    if bytes.is_none_or(|bytes| bytes > limits.max_header_bytes) {
        return Err(McpEgressError::InvalidHeader);
    }
    Ok(())
}

fn check_response_metadata(
    url: &[u8],
    headers: &HeaderMap,
    limits: McpEgressLimits,
    scanner: &mut SensitiveDataScanner,
) -> Result<(), McpEgressError> {
    check_headers(headers, limits)?;
    if scan_field(scanner, url) {
        return Err(McpEgressError::InvalidHeader);
    }
    for (name, value) in headers {
        if scan_field(scanner, name.as_str().as_bytes()) || scan_field(scanner, value.as_bytes()) {
            return Err(McpEgressError::InvalidHeader);
        }
    }
    Ok(())
}

fn scan_field(scanner: &mut SensitiveDataScanner, bytes: &[u8]) -> bool {
    scanner.reset();
    scanner.push(bytes);
    scanner.found()
}

fn scan_secret_bytes(
    bytes: &[u8],
    scanner: &mut SensitiveDataScanner,
) -> Result<(), McpEgressError> {
    if scan_field(scanner, bytes) {
        Err(McpEgressError::InvalidHeader)
    } else {
        Ok(())
    }
}

fn outbound_reflects_credential(
    url: &[u8],
    body: &[u8],
    headers: &HeaderMap,
    scanner: &mut SensitiveDataScanner,
) -> bool {
    scanner.reset();
    scanner.push(url);
    if scanner.found() {
        return true;
    }
    scanner.reset();
    scanner.push(body);
    if scanner.found() {
        return true;
    }
    for (name, value) in headers {
        scanner.reset();
        scanner.push(name.as_str().as_bytes());
        if scanner.found() {
            return true;
        }
        if name == AUTHORIZATION {
            continue;
        }
        scanner.reset();
        scanner.push(value.as_bytes());
        if scanner.found() {
            return true;
        }
    }
    false
}

fn scrub_bytes(bytes: &mut Bytes) {
    let owned = std::mem::take(bytes);
    if let Ok(mut owned) = owned.try_into_mut() {
        owned.fill(0);
        std::hint::black_box(&mut owned);
    }
}

fn redirect_headers(headers: &HeaderMap) -> HeaderMap {
    headers
        .iter()
        .filter(|(name, _)| !origin_sensitive(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn origin_sensitive(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "cookie2"
            | "mcp-session-id"
            | "last-event-id"
            | "origin"
            | "referer"
            | "host"
            | "forwarded"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-proto"
            | "sec-fetch-dest"
            | "sec-fetch-mode"
            | "sec-fetch-site"
            | "sec-fetch-user"
    )
}

fn exact_location(headers: &HeaderMap, limits: McpEgressLimits) -> Result<&str, McpEgressError> {
    let mut locations = headers.get_all(LOCATION).iter();
    let location = locations.next().ok_or(McpEgressError::RedirectLocation)?;
    if locations.next().is_some() {
        return Err(McpEgressError::RedirectLocation);
    }
    let location = location
        .to_str()
        .map_err(|_| McpEgressError::RedirectLocation)?;
    if location.is_empty()
        || location.len() > limits.max_location_bytes
        || location.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(McpEgressError::RedirectLocation);
    }
    Ok(location)
}

fn validate_location_reference(location: &str) -> Result<(), McpEgressError> {
    let invalid = || McpEgressError::RedirectLocation;
    if location
        .chars()
        .any(|character| character.is_control() || character.is_whitespace() || character == '\\')
        || location.contains('#')
        || location.starts_with("//")
    {
        return Err(invalid());
    }

    let bytes = location.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if bytes
                .get(index + 1..index + 3)
                .is_none_or(|escape| !escape.iter().all(u8::is_ascii_hexdigit))
            {
                return Err(invalid());
            }
            index += 3;
        } else {
            index += 1;
        }
    }

    let first_segment_end = location.find(['/', '?']).unwrap_or(location.len());
    let Some(scheme_end) = location[..first_segment_end].find(':') else {
        return Ok(());
    };
    let remainder = location[scheme_end + 1..]
        .strip_prefix("//")
        .ok_or_else(invalid)?;
    let authority_end = remainder.find(['/', '?']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if authority.is_empty() || authority.contains(['%', '@']) {
        return Err(invalid());
    }

    let parsed = url::Url::parse(location).map_err(|_| invalid())?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid());
    }
    let raw_host = if authority.starts_with('[') {
        let close = authority.find(']').ok_or_else(invalid)?;
        &authority[..=close]
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    let canonical_host = match parsed.host().ok_or_else(invalid)? {
        url::Host::Domain(host) => host.to_owned(),
        url::Host::Ipv4(address) => address.to_string(),
        url::Host::Ipv6(address) => format!("[{address}]"),
    };
    if raw_host.ends_with('.') || raw_host != canonical_host {
        return Err(invalid());
    }
    Ok(())
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn canonical_connector_url(value: &str) -> Result<url::Url, McpEgressError> {
    match EgressPolicy::canonical_url(value) {
        Ok(url) => Ok(url),
        #[cfg(test)]
        Err(_) => {
            let url = url::Url::parse(value).map_err(|_| McpEgressError::InvalidRequest)?;
            if insecure_test_url(&url) {
                Ok(url)
            } else {
                Err(McpEgressError::InvalidRequest)
            }
        }
        #[cfg(not(test))]
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_hop_scanner_is_not_replaced_by_a_later_response() {
        for reflected in [
            b"first-hop-credential".as_slice(),
            b"final-hop-credential".as_slice(),
        ] {
            let redirected = McpResponseScanner::new(&[
                SecretLease::new(b"first-hop-credential".to_vec()),
                SecretLease::new(b"final-hop-credential".to_vec()),
            ]);
            let later =
                McpResponseScanner::new(&[SecretLease::new(b"later-response-credential".to_vec())]);

            assert!(later.scan_ingress(b"later-response-credential").unwrap());
            assert!(redirected.scan_ingress(reflected).unwrap());
        }
    }

    #[test]
    fn response_scanner_does_not_continue_outbound_residual_state() {
        let lease = SecretLease::new(b"cross-direction-secret".to_vec());
        let mut outbound = CaptureRedactor::new(std::slice::from_ref(&lease)).scanner();
        outbound.push(b"cross-direction-");

        let response = McpResponseScanner::new(&[lease]);
        assert!(!response.scan_ingress(b"secret").unwrap());
    }

    #[test]
    fn response_scanner_matches_across_chunks_in_one_stream() {
        let response =
            McpResponseScanner::new(&[SecretLease::new(b"split-response-secret".to_vec())]);

        assert!(!response.scan_ingress(b"split-response-").unwrap());
        assert!(response.scan_ingress(b"secret").unwrap());
    }

    #[test]
    fn valid_header_names_are_scanned_on_request_and_response() {
        let lease = SecretLease::new(b"credential-canary".to_vec());
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("credential-canary"),
            HeaderValue::from_static("public"),
        );
        let mut scanner = CaptureRedactor::new(std::slice::from_ref(&lease)).scanner();

        assert!(outbound_reflects_credential(
            b"https://example.com/",
            b"{}",
            &headers,
            &mut scanner,
        ));
        assert!(matches!(
            check_response_metadata(
                b"https://example.com/",
                &headers,
                McpEgressLimits {
                    max_location_bytes: 1024,
                    max_headers: 8,
                    max_header_bytes: 1024,
                    request_timeout: Duration::from_secs(1),
                    connect_timeout: Duration::from_secs(1),
                },
                &mut scanner,
            ),
            Err(McpEgressError::InvalidHeader)
        ));
    }

    #[test]
    fn raw_redirect_locations_reject_parser_ambiguities() {
        for location in [
            "https://%65xample.com/path",
            "https://EXAMPLE.com/path",
            "https://example.com./path",
            "https://user@example.com/path",
            "https://2130706433/path",
            "https://0x7f000001/path",
            "//example.com/path",
            "/back\\slash",
            "/has space",
            "/bad%2",
            "/path#fragment",
        ] {
            assert!(
                matches!(
                    validate_location_reference(location),
                    Err(McpEgressError::RedirectLocation)
                ),
                "accepted {location}"
            );
        }
    }

    #[test]
    fn raw_redirect_locations_allow_relative_paths_queries_and_escaped_data() {
        let current = url::Url::parse("https://example.com/mcp/start").unwrap();
        for (location, expected) in [
            ("next", "https://example.com/mcp/next"),
            (
                "../next?cursor=a%2Fb",
                "https://example.com/next?cursor=a%2Fb",
            ),
            (
                "?cursor=%25done",
                "https://example.com/mcp/start?cursor=%25done",
            ),
            ("/a%2Fb", "https://example.com/a%2Fb"),
        ] {
            validate_location_reference(location).unwrap();
            assert_eq!(current.join(location).unwrap().as_str(), expected);
        }
    }
}
