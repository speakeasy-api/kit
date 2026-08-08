use std::{
    collections::{BTreeMap, VecDeque},
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header::LOCATION};
use kit::{
    domain::{
        egress::{CredentialHandle, Denial, DestinationGrant, EgressPolicy, EgressResolver},
        secret::{SecretHandle, SecretLease},
    },
    protocols::mcp::egress::{
        EgressDialResponse, EgressDialer, HttpCredentialBroker, HttpCredentialError,
        HttpSecretContext, McpEgressConnector, McpEgressLimits, McpEgressRequest,
    },
};

fn ip(value: &str) -> IpAddr {
    value.parse().unwrap()
}

fn limits() -> McpEgressLimits {
    McpEgressLimits {
        max_location_bytes: 1024,
        max_headers: 32,
        max_header_bytes: 4096,
        request_timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(1),
    }
}

fn grant(host: &str, credential: &str) -> DestinationGrant {
    DestinationGrant::new(
        "https",
        host,
        443,
        CredentialHandle::new(credential).unwrap(),
    )
    .unwrap()
}

#[derive(Default)]
struct Resolver {
    calls: AtomicUsize,
    answers: Mutex<BTreeMap<String, VecDeque<Vec<IpAddr>>>>,
}

impl Resolver {
    fn with(self, host: &str, answers: impl IntoIterator<Item = Vec<IpAddr>>) -> Self {
        self.answers
            .lock()
            .unwrap()
            .insert(host.to_owned(), answers.into_iter().collect());
        self
    }
}

#[async_trait::async_trait]
impl EgressResolver for Resolver {
    async fn resolve(&self, host: &str, _port: u16) -> Result<Vec<IpAddr>, Denial> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.answers
            .lock()
            .unwrap()
            .get_mut(host)
            .and_then(|answers| answers.pop_front())
            .ok_or(Denial::ResolverUnavailable)
    }
}

#[derive(Default)]
struct Credentials {
    calls: AtomicUsize,
    contexts: Mutex<Vec<(String, String, usize)>>,
}

#[derive(Default)]
struct RotatingCredentials {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl HttpCredentialBroker for RotatingCredentials {
    async fn authorize_and_resolve(
        &self,
        _handle: &SecretHandle,
        _context: &HttpSecretContext<'_>,
    ) -> Result<Arc<SecretLease>, HttpCredentialError> {
        let value = if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
            b"old-credential".as_slice()
        } else {
            b"rotated-credential".as_slice()
        };
        Ok(Arc::new(SecretLease::new(value.to_vec())))
    }
}

#[async_trait::async_trait]
impl HttpCredentialBroker for Credentials {
    async fn authorize_and_resolve(
        &self,
        handle: &SecretHandle,
        context: &HttpSecretContext<'_>,
    ) -> Result<Arc<SecretLease>, HttpCredentialError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.contexts.lock().unwrap().push((
            handle.identifier().to_owned(),
            context.destination_digest().to_owned(),
            context.hop(),
        ));
        let value = match handle.identifier() {
            "env:A" => b"credential-a".as_slice(),
            "env:B" => b"credential-b".as_slice(),
            _ => return Err(HttpCredentialError::Denied),
        };
        Ok(Arc::new(SecretLease::new(value.to_vec())))
    }
}

struct Reply {
    status: StatusCode,
    location: Option<String>,
    peer: IpAddr,
}

#[derive(Default)]
struct Dialer {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<(String, HeaderMap)>>,
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    delay: Duration,
}

impl Dialer {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            delay: Duration::ZERO,
        }
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[async_trait::async_trait]
impl EgressDialer for Dialer {
    async fn send(
        &self,
        request: reqwest::Request,
        _authorization: &kit::domain::egress::Authorization,
        _limits: McpEgressLimits,
    ) -> Result<EgressDialResponse, std::io::Error> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_active.fetch_max(active, Ordering::AcqRel);
        tokio::time::sleep(self.delay).await;
        self.active.fetch_sub(1, Ordering::AcqRel);
        self.requests
            .lock()
            .unwrap()
            .push((request.url().to_string(), request.headers().clone()));
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| std::io::Error::other("unexpected dial"))?;
        let mut response = http::Response::builder().status(reply.status);
        if let Some(location) = reply.location {
            response = response.header(LOCATION, location);
        }
        Ok(EgressDialResponse {
            response: response.body(Bytes::new()).unwrap().into(),
            peer: Some(reply.peer),
        })
    }
}

#[tokio::test]
async fn concurrent_requests_keep_connector_authorization_coherent() {
    let resolver =
        Arc::new(Resolver::default().with("a.example", [vec![ip("8.8.8.8")], vec![ip("8.8.8.8")]]));
    let dialer = Arc::new(Dialer::new([
        Reply {
            status: StatusCode::OK,
            location: None,
            peer: ip("8.8.8.8"),
        },
        Reply {
            status: StatusCode::OK,
            location: None,
            peer: ip("8.8.8.8"),
        },
    ]));
    let connector = McpEgressConnector::with_dialer(
        EgressPolicy::new([grant("a.example", "env:A")]).with_resolver(resolver),
        Arc::new(Credentials::default()),
        dialer.clone(),
        limits(),
    );
    let (first, second) = tokio::join!(
        execute(&connector, request("https://a.example/first")),
        execute(&connector, request("https://a.example/second")),
    );
    assert!(first.is_ok());
    assert!(second.is_ok());
    assert_eq!(dialer.max_active.load(Ordering::Acquire), 1);
}

fn request(url: &str) -> McpEgressRequest {
    let mut headers = HeaderMap::new();
    headers.insert("cookie", HeaderValue::from_static("cookie-secret"));
    headers.insert("mcp-session-id", HeaderValue::from_static("session-secret"));
    headers.insert("last-event-id", HeaderValue::from_static("event-secret"));
    headers.insert("origin", HeaderValue::from_static("https://origin.invalid"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    McpEgressRequest {
        method: Method::POST,
        url: url.to_owned(),
        headers,
        body: Bytes::from_static(b"{}"),
    }
}

async fn execute(
    connector: &McpEgressConnector,
    request: McpEgressRequest,
) -> Result<
    kit::protocols::mcp::egress::McpEgressResponse,
    kit::protocols::mcp::egress::McpEgressError,
> {
    connector
        .execute(
            request,
            "principal",
            "project",
            "workspace",
            "invocation",
            "sha256:decision",
            "sha256:request",
            None,
            "tools/call",
        )
        .await
}

#[tokio::test]
async fn caller_supplied_authorization_headers_are_rejected_before_resolution_or_dial() {
    for name in ["authorization", "proxy-authorization"] {
        let resolver = Arc::new(Resolver::default().with("a.example", [vec![ip("8.8.8.8")]]));
        let credentials = Arc::new(Credentials::default());
        let dialer = Arc::new(Dialer::default());
        let connector = McpEgressConnector::with_dialer(
            EgressPolicy::new([grant("a.example", "env:A")]).with_resolver(resolver.clone()),
            credentials.clone(),
            dialer.clone(),
            limits(),
        );
        let mut outbound = request("https://a.example/start");
        outbound
            .headers
            .insert(name, HeaderValue::from_static("attacker"));

        assert!(matches!(
            execute(&connector, outbound).await,
            Err(kit::protocols::mcp::egress::McpEgressError::InvalidHeader)
        ));
        assert_eq!(resolver.calls.load(Ordering::Acquire), 0);
        assert_eq!(credentials.calls.load(Ordering::Acquire), 0);
        assert_eq!(dialer.calls.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
async fn authorized_relative_and_cross_origin_redirects_reauthorize_every_hop() {
    let resolver = Arc::new(
        Resolver::default()
            .with("a.example", [vec![ip("8.8.8.8")], vec![ip("8.8.8.8")]])
            .with("b.example", [vec![ip("1.1.1.1")]]),
    );
    let credentials = Arc::new(Credentials::default());
    let dialer = Arc::new(Dialer::new([
        Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: Some("next?cursor=a%2Fb".to_owned()),
            peer: ip("8.8.8.8"),
        },
        Reply {
            status: StatusCode::PERMANENT_REDIRECT,
            location: Some("https://b.example/final".to_owned()),
            peer: ip("8.8.8.8"),
        },
        Reply {
            status: StatusCode::OK,
            location: None,
            peer: ip("1.1.1.1"),
        },
    ]));
    let policy = EgressPolicy::new([grant("a.example", "env:A"), grant("b.example", "env:B")])
        .with_resolver(resolver.clone());
    let connector =
        McpEgressConnector::with_dialer(policy, credentials.clone(), dialer.clone(), limits());
    if let Err(error) = execute(&connector, request("https://a.example/start")).await {
        panic!("valid redirect chain failed: {error:?}");
    }
    assert_eq!(resolver.calls.load(Ordering::Acquire), 3);
    assert_eq!(credentials.calls.load(Ordering::Acquire), 3);
    let requests = dialer.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].1["authorization"], "Bearer credential-a");
    assert_eq!(requests[1].0, "https://a.example/next?cursor=a%2Fb");
    assert_eq!(requests[1].1["authorization"], "Bearer credential-a");
    assert_eq!(requests[2].1["authorization"], "Bearer credential-b");
    for (_, headers) in requests.iter().skip(1) {
        for stripped in [
            "proxy-authorization",
            "cookie",
            "mcp-session-id",
            "last-event-id",
            "origin",
        ] {
            assert!(
                !headers.contains_key(stripped),
                "redirect inherited {stripped}"
            );
        }
    }
}

#[tokio::test]
async fn hop_zero_headers_are_scanned_against_the_rotated_outbound_credential() {
    for name in [
        "cookie",
        "mcp-session-id",
        "last-event-id",
        "x-custom-header",
    ] {
        let resolver = Arc::new(
            Resolver::default().with("a.example", [vec![ip("8.8.8.8")], vec![ip("8.8.8.8")]]),
        );
        let credentials = Arc::new(RotatingCredentials::default());
        let dialer = Arc::new(Dialer::new([Reply {
            status: StatusCode::OK,
            location: None,
            peer: ip("8.8.8.8"),
        }]));
        let connector = McpEgressConnector::with_dialer(
            EgressPolicy::new([grant("a.example", "env:A")]).with_resolver(resolver),
            credentials.clone(),
            dialer.clone(),
            limits(),
        );

        assert!(
            execute(&connector, request("https://a.example/first"))
                .await
                .is_ok()
        );
        let mut reflected = request("https://a.example/second");
        reflected
            .headers
            .insert(name, HeaderValue::from_static("rotated-credential"));
        assert!(matches!(
            execute(&connector, reflected).await,
            Err(kit::protocols::mcp::egress::McpEgressError::InvalidRequest)
        ));
        assert_eq!(credentials.calls.load(Ordering::Acquire), 2);
        assert_eq!(dialer.calls.load(Ordering::Acquire), 1);
    }
}

#[tokio::test]
async fn callback_post_is_scanned_against_the_rotated_outbound_credential() {
    let resolver =
        Arc::new(Resolver::default().with("a.example", (0..3).map(|_| vec![ip("8.8.8.8")])));
    let credentials = Arc::new(RotatingCredentials::default());
    let dialer = Arc::new(Dialer::new([
        Reply {
            status: StatusCode::OK,
            location: None,
            peer: ip("8.8.8.8"),
        },
        Reply {
            status: StatusCode::OK,
            location: None,
            peer: ip("8.8.8.8"),
        },
    ]));
    let connector = McpEgressConnector::with_dialer(
        EgressPolicy::new([grant("a.example", "env:A")]).with_resolver(resolver),
        credentials.clone(),
        dialer.clone(),
        limits(),
    );

    assert!(
        execute(&connector, request("https://a.example/callback-request"))
            .await
            .is_ok()
    );
    let mut reflected = request("https://a.example/callback-response");
    reflected.body = Bytes::from_static(br#"{"result":"Bearer rotated-credential"}"#);
    assert!(matches!(
        execute(&connector, reflected).await,
        Err(kit::protocols::mcp::egress::McpEgressError::InvalidRequest)
    ));
    assert_eq!(dialer.calls.load(Ordering::Acquire), 1);

    let mut control = request("https://a.example/callback-response-control");
    control.body = Bytes::from_static(br#"{"result":"public callback value"}"#);
    assert!(execute(&connector, control).await.is_ok());
    assert_eq!(credentials.calls.load(Ordering::Acquire), 3);
    assert_eq!(dialer.calls.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn canonical_url_and_new_redirect_credential_are_scanned_before_wire() {
    let initial_dialer = Arc::new(Dialer::default());
    let initial = McpEgressConnector::with_dialer(
        EgressPolicy::new([grant("a.example", "env:A")]).with_resolver(Arc::new(
            Resolver::default().with("a.example", [vec![ip("8.8.8.8")]]),
        )),
        Arc::new(Credentials::default()),
        initial_dialer.clone(),
        limits(),
    );
    assert!(matches!(
        execute(&initial, request("https://a.example/credential-a")).await,
        Err(kit::protocols::mcp::egress::McpEgressError::InvalidRequest)
    ));
    assert_eq!(initial_dialer.calls.load(Ordering::Acquire), 0);

    let credentials = Arc::new(Credentials::default());
    let redirect_dialer = Arc::new(Dialer::new([Reply {
        status: StatusCode::TEMPORARY_REDIRECT,
        location: Some("https://b.example/credential-b".to_owned()),
        peer: ip("8.8.8.8"),
    }]));
    let redirect = McpEgressConnector::with_dialer(
        EgressPolicy::new([grant("a.example", "env:A"), grant("b.example", "env:B")])
            .with_resolver(Arc::new(
                Resolver::default()
                    .with("a.example", [vec![ip("8.8.8.8")]])
                    .with("b.example", [vec![ip("1.1.1.1")]]),
            )),
        credentials.clone(),
        redirect_dialer.clone(),
        limits(),
    );
    assert!(matches!(
        execute(&redirect, request("https://a.example/start")).await,
        Err(kit::protocols::mcp::egress::McpEgressError::InvalidRequest)
    ));
    assert_eq!(credentials.calls.load(Ordering::Acquire), 2);
    assert_eq!(redirect_dialer.calls.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn redirect_destination_dns_pin_persists_for_connector_lifetime() {
    let resolver = Arc::new(
        Resolver::default()
            .with("a.example", [vec![ip("8.8.8.8")], vec![ip("8.8.8.8")]])
            .with(
                "b.example",
                [vec![ip("1.1.1.1")], vec![ip("1.1.1.1"), ip("127.0.0.1")]],
            ),
    );
    let dialer = Arc::new(Dialer::new([
        Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: Some("https://b.example/final".to_owned()),
            peer: ip("8.8.8.8"),
        },
        Reply {
            status: StatusCode::OK,
            location: None,
            peer: ip("1.1.1.1"),
        },
        Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: Some("https://b.example/final".to_owned()),
            peer: ip("8.8.8.8"),
        },
    ]));
    let connector = McpEgressConnector::with_dialer(
        EgressPolicy::new([grant("a.example", "env:A"), grant("b.example", "env:B")])
            .with_resolver(resolver),
        Arc::new(Credentials::default()),
        dialer.clone(),
        limits(),
    );

    assert!(
        execute(&connector, request("https://a.example/first"))
            .await
            .is_ok()
    );
    assert!(matches!(
        execute(&connector, request("https://a.example/second")).await,
        Err(kit::protocols::mcp::egress::McpEgressError::Denied(
            Denial::DnsRebinding
        ))
    ));
    assert_eq!(dialer.requests.lock().unwrap().len(), 3);
    assert!(matches!(
        execute(&connector, request("https://a.example/third")).await,
        Err(kit::protocols::mcp::egress::McpEgressError::Denied(
            Denial::DnsRebinding
        ))
    ));
}

#[tokio::test]
async fn one_hundred_granted_private_resolutions_are_denied_before_credentials_or_connect() {
    let mut resolver = Resolver::default();
    let mut grants = Vec::new();
    let mut cases = Vec::new();
    for index in 0..100 {
        let host = format!("private-{index}.example");
        resolver = resolver.with(&host, [vec![ip("10.0.0.1")]]);
        grants.push(grant(&host, "env:A"));
        cases.push(format!("https://{host}/path"));
    }
    let resolver = Arc::new(resolver);
    let credentials = Arc::new(Credentials::default());
    let connector = McpEgressConnector::new(
        EgressPolicy::new(grants).with_resolver(resolver.clone()),
        credentials.clone(),
        limits(),
    );
    let fixed = [
        "http://169.254.169.254/latest/meta-data",
        "https://127.0.0.1/",
        "https://10.0.0.1/",
        "https://192.168.1.1/",
        "https://172.16.0.1/",
        "https://[::1]/",
        "https://[fe80::1]/",
        "https://localhost/",
        "https://service.local/",
        "https://2130706433/",
        "https://0x7f000001/",
        "file:///etc/passwd",
        "gopher://example.com/",
        "https://user:pass@example.com/",
        "https://example.com/#fragment",
        "https://example.com:22/",
    ];
    cases.extend(fixed.iter().map(|value| (*value).to_owned()));
    for malicious in &cases {
        assert!(
            execute(&connector, request(malicious)).await.is_err(),
            "malicious URL was accepted: {malicious}"
        );
    }
    assert!(cases.len() >= 100);
    assert_eq!(resolver.calls.load(Ordering::Acquire), 100);
    assert_eq!(credentials.calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn public_ipv6_literal_uses_one_normalized_address_and_non_global_is_denied() {
    let public = ip("2606:4700:4700::1111");
    let resolver = Arc::new(Resolver::default().with("2606:4700:4700::1111", [vec![public]]));
    let dialer = Arc::new(Dialer::new([Reply {
        status: StatusCode::OK,
        location: None,
        peer: public,
    }]));
    let connector = McpEgressConnector::with_dialer(
        EgressPolicy::new([grant("2606:4700:4700::1111", "env:A")]).with_resolver(resolver.clone()),
        Arc::new(Credentials::default()),
        dialer.clone(),
        limits(),
    );

    assert!(
        execute(
            &connector,
            request("https://[2606:4700:4700:0:0:0:0:1111]/authorize"),
        )
        .await
        .is_ok()
    );
    assert_eq!(resolver.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        dialer.requests.lock().unwrap()[0].0,
        "https://[2606:4700:4700::1111]/authorize"
    );

    for address in ["2001:db8::1", "fc00::1", "fe80::1"] {
        let denied = DestinationGrant::new(
            "https",
            address,
            443,
            CredentialHandle::new("env:A").unwrap(),
        );
        assert_eq!(denied.unwrap_err(), Denial::PrivateAddress);
    }
}

#[tokio::test]
async fn encoded_credentials_in_locations_are_denied_before_parse_or_follow() {
    for location in [
        "https://a.example/%2563%2572%2565%2564%2565%256E%2574%2569%2561%256C%252D%2561",
        "https://a.example/Y3JlZGVudGlhbC1h",
        "https://a.example/WTNKbFpHVnVkR2xoYkMxaA==",
    ] {
        let resolver = Arc::new(Resolver::default().with("a.example", [vec![ip("8.8.8.8")]]));
        let credentials = Arc::new(Credentials::default());
        let dialer = Arc::new(Dialer::new([Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: Some(location.to_owned()),
            peer: ip("8.8.8.8"),
        }]));
        let connector = McpEgressConnector::with_dialer(
            EgressPolicy::new([grant("a.example", "env:A")]).with_resolver(resolver.clone()),
            credentials.clone(),
            dialer.clone(),
            limits(),
        );
        assert!(
            execute(&connector, request("https://a.example/start"))
                .await
                .is_err(),
            "followed encoded credential location {location}"
        );
        assert_eq!(resolver.calls.load(Ordering::Acquire), 1);
        assert_eq!(credentials.calls.load(Ordering::Acquire), 1);
        assert_eq!(dialer.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn mixed_dns_peer_mismatch_method_rewrite_and_credential_reflection_fail_closed() {
    let mixed =
        Arc::new(Resolver::default().with("a.example", [vec![ip("8.8.8.8"), ip("127.0.0.1")]]));
    let credentials = Arc::new(Credentials::default());
    let dialer = Arc::new(Dialer::default());
    let connector = McpEgressConnector::with_dialer(
        EgressPolicy::new([grant("a.example", "env:A")]).with_resolver(mixed),
        credentials.clone(),
        dialer.clone(),
        limits(),
    );
    assert!(
        execute(&connector, request("https://a.example/start"))
            .await
            .is_err()
    );
    assert_eq!(credentials.calls.load(Ordering::Acquire), 0);
    assert!(dialer.requests.lock().unwrap().is_empty());

    for reply in [
        Reply {
            status: StatusCode::OK,
            location: None,
            peer: ip("9.9.9.9"),
        },
        Reply {
            status: StatusCode::FOUND,
            location: Some("/rewritten".to_owned()),
            peer: ip("8.8.8.8"),
        },
        Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: Some("https://a.example/credential-a".to_owned()),
            peer: ip("8.8.8.8"),
        },
    ] {
        let peer_mismatch = reply.peer == ip("9.9.9.9");
        let resolver = Arc::new(Resolver::default().with("a.example", [vec![ip("8.8.8.8")]]));
        let connector = McpEgressConnector::with_dialer(
            EgressPolicy::new([grant("a.example", "env:A")]).with_resolver(resolver),
            Arc::new(Credentials::default()),
            Arc::new(Dialer::new([reply])),
            limits(),
        );
        let result = execute(&connector, request("https://a.example/start")).await;
        if peer_mismatch {
            assert!(matches!(
                result,
                Err(kit::protocols::mcp::egress::McpEgressError::Denied(
                    Denial::DnsRebinding
                ))
            ));
        } else {
            assert!(result.is_err());
        }
    }
}

#[tokio::test]
async fn redirect_loops_hop_overflow_location_bounds_downgrade_and_dns_change_are_denied() {
    let cases = [
        vec![Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: Some("/start".to_owned()),
            peer: ip("8.8.8.8"),
        }],
        vec![Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: Some(format!("/{}", "x".repeat(1024))),
            peer: ip("8.8.8.8"),
        }],
        vec![Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: Some("http://a.example/plaintext".to_owned()),
            peer: ip("8.8.8.8"),
        }],
    ];
    for replies in cases {
        let resolver = Arc::new(Resolver::default().with("a.example", [vec![ip("8.8.8.8")]]));
        let connector = McpEgressConnector::with_dialer(
            EgressPolicy::new([grant("a.example", "env:A")]).with_resolver(resolver),
            Arc::new(Credentials::default()),
            Arc::new(Dialer::new(replies)),
            limits(),
        );
        assert!(
            execute(&connector, request("https://a.example/start"))
                .await
                .is_err()
        );
    }

    let replies = (0..=kit::domain::egress::MAX_REDIRECTS)
        .map(|index| Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: Some(format!("/hop-{}", index + 1)),
            peer: ip("8.8.8.8"),
        })
        .collect::<Vec<_>>();
    let answers = (0..=kit::domain::egress::MAX_REDIRECTS)
        .map(|_| vec![ip("8.8.8.8")])
        .collect::<Vec<_>>();
    let connector = McpEgressConnector::with_dialer(
        EgressPolicy::new([grant("a.example", "env:A")])
            .with_resolver(Arc::new(Resolver::default().with("a.example", answers))),
        Arc::new(Credentials::default()),
        Arc::new(Dialer::new(replies)),
        limits(),
    );
    assert!(
        execute(&connector, request("https://a.example/start"))
            .await
            .is_err()
    );

    let resolver = Arc::new(Resolver::default().with(
        "a.example",
        [
            vec![ip("8.8.8.8")],
            vec![ip("8.8.8.8")],
            vec![ip("1.1.1.1")],
        ],
    ));
    let dialer = Arc::new(Dialer::new([
        Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: Some("/first".to_owned()),
            peer: ip("8.8.8.8"),
        },
        Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: Some("/changed-dns".to_owned()),
            peer: ip("8.8.8.8"),
        },
    ]));
    let connector = McpEgressConnector::with_dialer(
        EgressPolicy::new([grant("a.example", "env:A")]).with_resolver(resolver),
        Arc::new(Credentials::default()),
        dialer.clone(),
        limits(),
    );
    assert!(matches!(
        execute(&connector, request("https://a.example/start")).await,
        Err(kit::protocols::mcp::egress::McpEgressError::Denied(
            Denial::DnsRebinding
        ))
    ));
    assert_eq!(dialer.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn cumulative_slow_redirect_hops_share_one_request_deadline() {
    let resolver =
        Arc::new(Resolver::default().with("a.example", (0..3).map(|_| vec![ip("8.8.8.8")])));
    let dialer = Arc::new(
        Dialer::new([
            Reply {
                status: StatusCode::TEMPORARY_REDIRECT,
                location: Some("/first".to_owned()),
                peer: ip("8.8.8.8"),
            },
            Reply {
                status: StatusCode::TEMPORARY_REDIRECT,
                location: Some("/second".to_owned()),
                peer: ip("8.8.8.8"),
            },
            Reply {
                status: StatusCode::OK,
                location: None,
                peer: ip("8.8.8.8"),
            },
        ])
        .with_delay(Duration::from_millis(300)),
    );
    let connector = McpEgressConnector::with_dialer(
        EgressPolicy::new([grant("a.example", "env:A")]).with_resolver(resolver),
        Arc::new(Credentials::default()),
        dialer.clone(),
        McpEgressLimits {
            request_timeout: Duration::from_millis(750),
            ..limits()
        },
    );
    assert!(matches!(
        execute(&connector, request("https://a.example/start")).await,
        Err(kit::protocols::mcp::egress::McpEgressError::Timeout)
    ));
    assert!(dialer.calls.load(Ordering::Acquire) >= 2);
}

#[test]
fn server_url_surfaces_remain_inert_typed_data() {
    let dialer = Dialer::default();
    let result: rmcp::model::CallToolResult = serde_json::from_value(serde_json::json!({
        "content": [{
            "type": "resource_link",
            "uri": "https://169.254.169.254/latest/meta-data",
            "name": "inert"
        }],
        "structuredContent": {
            "result_link": "file:///etc/passwd",
            "url": "gopher://127.0.0.1/"
        }
    }))
    .unwrap();
    let descriptors: rmcp::model::ListResourcesResult = serde_json::from_value(serde_json::json!({
        "resources": [{
            "uri": "https://127.0.0.1/private",
            "name": "descriptor"
        }]
    }))
    .unwrap();
    assert_eq!(result.content.len(), 1);
    assert_eq!(descriptors.resources.len(), 1);
    assert!(dialer.requests.lock().unwrap().is_empty());
}
