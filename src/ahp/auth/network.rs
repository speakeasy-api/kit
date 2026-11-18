//! The SDK's OAuth network boundary over reqwest.
use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use agenthooksprotocol::{
    auth::{
        AuthErrorCode,
        managed::{EndpointUse, OAuthRequest, OAuthResponse, OperationBudget, RequestWritten},
    },
    client::{HookError, LocalFuture},
};
use futures_util::StreamExt as _;
use url::Url;

/// OAuth responses are small JSON documents; anything larger is refused.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

fn failed() -> HookError {
    HookError::authentication(AuthErrorCode::ProviderFailed)
}

fn untrusted() -> HookError {
    HookError::authentication(AuthErrorCode::UntrustedDestination)
}

/// HTTPS only, no redirects, bounded reads, and connections only to the
/// addresses checked at send time: a name is resolved once per request, the
/// result checked, and the connection pinned to it, so a rebinding DNS answer
/// cannot redirect a token request. Requests and responses carry tokens and
/// secrets, so nothing here is ever logged.
pub(super) struct Network {
    allow_private: bool,
    roots: Vec<reqwest::Certificate>,
}

impl Network {
    /// `roots` replaces the system's trusted certificates when not empty.
    pub(super) fn new(allow_private: bool, roots: Vec<reqwest::Certificate>) -> Self {
        Self {
            allow_private,
            roots,
        }
    }

    /// Resolves `url`'s host to the addresses Kit may connect to.
    async fn addresses(
        &self,
        url: &Url,
        budget: OperationBudget,
    ) -> Result<Vec<SocketAddr>, HookError> {
        if !https(url) {
            return Err(untrusted());
        }
        let port = url.port_or_known_default().ok_or_else(untrusted)?;
        let host = url.host_str().ok_or_else(untrusted)?;
        let literal = host.trim_matches(['[', ']']).parse::<IpAddr>().ok();
        let addresses: Vec<SocketAddr> = match literal {
            Some(ip) => vec![SocketAddr::new(ip, port)],
            None => tokio::time::timeout_at(
                budget.deadline().into(),
                tokio::net::lookup_host((host, port)),
            )
            .await
            .map_err(|_| failed())?
            .map_err(|_| failed().request_not_sent())?
            .collect(),
        };
        if addresses.is_empty()
            || (!self.allow_private && addresses.iter().any(|address| private(address.ip())))
        {
            return Err(untrusted());
        }
        Ok(addresses)
    }

    fn client(&self, url: &Url, addresses: &[SocketAddr]) -> Result<reqwest::Client, HookError> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10));
        if !self.roots.is_empty() {
            builder = builder.tls_certs_only(self.roots.clone());
        }
        if let Some(host) = url.host_str()
            && host.trim_matches(['[', ']']).parse::<IpAddr>().is_err()
        {
            builder = builder.resolve_to_addrs(host, addresses);
        }
        builder.build().map_err(|_| failed())
    }
}

fn https(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

/// Loopback, private, link-local, shared and other non-public addresses.
fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (18..20).contains(&b))
                || a >= 240
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return private(IpAddr::V4(mapped));
            }
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || first == 0x2001 && ip.segments()[1] == 0x0db8
        }
    }
}

impl agenthooksprotocol::auth::managed::OAuthNetwork for Network {
    fn check(
        &self,
        endpoint: Url,
        _usage: EndpointUse,
        budget: OperationBudget,
    ) -> LocalFuture<'_, Result<(), HookError>> {
        Box::pin(async move { self.addresses(&endpoint, budget).await.map(|_| ()) })
    }

    fn send(
        &self,
        request: OAuthRequest,
        _usage: EndpointUse,
        budget: OperationBudget,
    ) -> LocalFuture<'_, Result<OAuthResponse, HookError>> {
        Box::pin(async move {
            let url = Url::parse(&request.uri().to_string()).map_err(|_| untrusted())?;
            let addresses = self.addresses(&url, budget).await?;
            let client = self.client(&url, &addresses)?;
            let written = request.extensions().get::<RequestWritten>().cloned();
            let remaining = budget.deadline().saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(failed());
            }
            let (parts, body) = request.into_parts();
            let outgoing = client
                .request(parts.method, url)
                .headers(parts.headers)
                .timeout(remaining)
                .body(body)
                .build()
                .map_err(|_| failed())?;
            // reqwest cannot report when the request bytes are written. As the
            // SDK documents for such clients, mark when the send is first
            // polled. Marking early only lets the SDK finish an exchange the
            // caller abandoned; it never loses a rotated refresh token.
            if let Some(written) = &written {
                written.mark();
            }
            let response = client.execute(outgoing).await.map_err(|error| {
                if error.is_connect() {
                    // DNS, TCP and TLS failures: the endpoint never saw it.
                    failed().request_not_sent()
                } else {
                    failed()
                }
            })?;
            let status = response.status();
            let headers = response.headers().clone();
            if response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
            {
                return Err(failed());
            }
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| failed())?;
                if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(failed());
                }
                body.extend_from_slice(&chunk);
            }
            let mut built = http::Response::new(body);
            *built.status_mut() = status;
            *built.headers_mut() = headers;
            Ok(built)
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::private;

    #[test]
    fn non_public_addresses_are_private() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["93.184.216.34", "2606:4700::1111"] {
            assert!(!private(ip.parse().unwrap()), "{ip}");
        }
    }
}
