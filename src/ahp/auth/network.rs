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
    client: ClientBuilder,
}

/// Starts each request's HTTP client: TLS roots and other transport
/// settings. Redirects, proxies, retries and address pinning are added here.
pub(super) type ClientBuilder = fn() -> reqwest::ClientBuilder;

impl Network {
    pub(super) fn new(allow_private: bool, client: ClientBuilder) -> Self {
        Self {
            allow_private,
            client,
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

    fn client(
        &self,
        url: &Url,
        addresses: &[SocketAddr],
        written: Option<RequestWritten>,
    ) -> Result<reqwest::Client, HookError> {
        // One client per request, so its connection belongs to this request.
        let mut builder = (self.client)()
            .connector_layer(MarkOnConnect(written))
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10));
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

/// Whether `ip` is anything but a globally routable unicast address.
/// IPv6 forms that carry an IPv4 address are judged by that address
/// (IPv4-mapped, IPv4-compatible) or refused outright (NAT64 and 6to4,
/// whose destination a gateway chooses).
fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..128).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..32).contains(&b))
                || (a == 192 && b == 0 && (c == 0 || c == 2))
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (18..20).contains(&b))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            if let Some(v4) = ip.to_ipv4_mapped() {
                return private(IpAddr::V4(v4));
            }
            // IPv4-compatible ::a.b.c.d (deprecated, but still routed by some stacks).
            if segments[..6].iter().all(|segment| *segment == 0) && !ip.is_loopback() {
                let [.., a, b, c, d] = ip.octets();
                return ip.is_unspecified() || private(IpAddr::V4([a, b, c, d].into()));
            }
            // Only 2000::/3 is global unicast; within it, refuse the
            // translated, documentation and IETF special-purpose blocks.
            (segments[0] & 0xe000) != 0x2000
                || segments[0] == 0x2002
                || (segments[0] == 0x2001 && segments[1] < 0x0200)
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || (segments[0] == 0x0064 && segments[1] == 0xff9b)
        }
    }
}

/// Marks a token or registration request written once its connection is
/// established (TCP and TLS), the closest reqwest lets Kit observe.
#[derive(Clone)]
struct MarkOnConnect(Option<RequestWritten>);

impl<S> tower::Layer<S> for MarkOnConnect {
    type Service = MarkOnConnectService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        MarkOnConnectService {
            inner,
            written: self.0.clone(),
        }
    }
}

#[derive(Clone)]
struct MarkOnConnectService<S> {
    inner: S,
    written: Option<RequestWritten>,
}

impl<S, R> tower::Service<R> for MarkOnConnectService<S>
where
    S: tower::Service<R>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: R) -> Self::Future {
        let connecting = self.inner.call(request);
        let written = self.written.clone();
        Box::pin(async move {
            let connection = connecting.await?;
            if let Some(written) = written {
                written.mark();
            }
            Ok(connection)
        })
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
            let written = request.extensions().get::<RequestWritten>().cloned();
            let client = self.client(&url, &addresses, written)?;
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
            // reqwest cannot report when the request bytes are written; the
            // connector marks the request once connected, as the SDK
            // documents for such clients. Before that, cancellation aborts.
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
    use super::*;

    #[test]
    fn non_public_addresses_are_private() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "198.18.0.1",
            "::",
            "::1",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::808:808",
            "2002:7f00:1::1",
            "2001:db8::1",
            "2001::1",
        ] {
            assert!(private(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "93.184.216.34",
            "8.8.8.8",
            "::ffff:8.8.8.8",
            "2606:4700::1111",
        ] {
            assert!(!private(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn ip_literal_endpoints_are_checked_like_resolved_ones() {
        use agenthooksprotocol::auth::managed::{EndpointUse, OAuthNetwork as _};
        let network = Network::new(false, reqwest::Client::builder);
        let budget = OperationBudget::until(Instant::now() + Duration::from_secs(5));
        for url in [
            "https://127.0.0.1/token",
            "https://[::1]/token",
            "https://[::ffff:127.0.0.1]/token",
            "https://[::ffff:7f00:1]/token",
            "https://[64:ff9b::7f00:1]/token",
            "https://localhost/token",
        ] {
            let result = network
                .check(url.parse().unwrap(), EndpointUse::Token, budget)
                .await;
            assert!(result.is_err(), "{url}");
        }
        // Allowed on request, as for a local authorization server.
        let local = Network::new(true, reqwest::Client::builder);
        assert!(
            local
                .check(
                    "https://[::ffff:127.0.0.1]/token".parse().unwrap(),
                    EndpointUse::Token,
                    budget
                )
                .await
                .is_ok()
        );
    }
}
