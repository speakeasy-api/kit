#[path = "../../src/domain/egress/mod.rs"]
#[allow(dead_code)]
mod egress;

use std::net::IpAddr;

use egress::{
    ConnectObservation, CredentialHandle, Denial, DestinationGrant, EgressPolicy, MAX_REDIRECTS,
    ResolverObservation, Scheme,
};

fn ip(value: &str) -> IpAddr {
    value.parse().unwrap()
}

fn observed(values: &[&str]) -> ResolverObservation {
    ResolverObservation::new(values.iter().map(|value| ip(value)))
}

fn handle(value: &str) -> CredentialHandle {
    CredentialHandle::new(value).unwrap()
}

fn grant(scheme: &str, host: &str, port: u16, credential: &str) -> DestinationGrant {
    DestinationGrant::new(scheme, host, port, handle(credential)).unwrap()
}

fn policy() -> EgressPolicy {
    EgressPolicy::new([
        grant("https", "api.example.com", 443, "credential:api"),
        grant("http", "downloads.example.com", 8080, "credential:download"),
        grant("https", "8.8.8.8", 443, "credential:dns"),
        grant(
            "https",
            "2606:4700:4700:0:0:0:0:1111",
            443,
            "credential:dns6",
        ),
    ])
}

#[test]
fn approved_destinations_bind_scheme_host_port_and_credential() {
    let policy = policy();
    let public = observed(&["93.184.216.34", "2606:4700:4700::1111"]);
    assert_eq!(public.addresses().len(), 2);
    let declared = grant("https", "API.EXAMPLE.COM.", 443, "credential:api");
    assert_eq!(declared.destination().host(), "api.example.com");
    assert_eq!(declared.credential().as_str(), "credential:api");
    let credential = handle("credential:api");
    let authorization = policy
        .authorize_initial("HTTPS://API.EXAMPLE.COM./v1?q=ok", &credential, &public)
        .unwrap();
    assert_eq!(authorization.destination().scheme(), Scheme::Https);
    assert_eq!(authorization.destination().host(), "api.example.com");
    assert_eq!(authorization.destination().port(), 443);
    assert_eq!(authorization.credential().as_str(), "credential:api");

    for (url, credential) in [
        ("http://api.example.com/v1", "credential:api"),
        ("https://other.example.com/v1", "credential:api"),
        ("https://api.example.com:8443/v1", "credential:api"),
        ("https://api.example.com/v1", "credential:download"),
    ] {
        assert_eq!(
            policy.authorize_initial(url, &handle(credential), &public),
            Err(Denial::DestinationNotGranted),
            "authority bypass accepted: {url} {credential}"
        );
    }
}

#[test]
fn unsupported_and_ambiguous_urls_fail_closed() {
    let policy = policy();
    let credential = handle("credential:api");
    let public = observed(&["93.184.216.34"]);
    for url in [
        "file:///etc/passwd",
        "ftp://api.example.com/file",
        "gopher://api.example.com/1",
        "data:text/plain,hello",
        "mailto:user@example.com",
        "ws://api.example.com/socket",
        "wss://api.example.com/socket",
        "https:api.example.com/path",
    ] {
        assert_eq!(
            policy.authorize_initial(url, &credential, &public),
            Err(if url == "https:api.example.com/path" {
                Denial::InvalidUrl
            } else {
                Denial::UnsupportedScheme
            }),
            "unsupported URL accepted: {url}"
        );
    }
    for url in [
        "https://user@api.example.com/",
        "https://user:pass@api.example.com/",
        "https://api.example.com@attacker.example/",
    ] {
        assert_eq!(
            policy.authorize_initial(url, &credential, &public),
            Err(Denial::UserInfo)
        );
    }
    assert_eq!(
        policy.authorize_initial(
            "https://api.example.com/path#fragment",
            &credential,
            &public
        ),
        Err(Denial::Fragment)
    );
    for url in [
        "",
        "https://",
        "https://api.example.com\\@127.0.0.1/",
        "https://api.example.com\n/",
        "https://api%2eexample.com/",
        "https://api_example.com/",
        "https://api.example.com../",
        "https://[8.8.8.8]/",
        "https://[2606:4700:4700::1111",
        "https://api.example.com:65536/",
        "https://api.example.com:notaport/",
    ] {
        assert!(
            matches!(
                policy.authorize_initial(url, &credential, &public),
                Err(Denial::InvalidUrl | Denial::InvalidHost | Denial::InvalidPort)
            ),
            "ambiguous URL accepted: {url}"
        );
    }
}

#[test]
fn private_special_and_alternate_numeric_targets_are_denied() {
    let public = observed(&["93.184.216.34"]);
    for host in [
        "0.0.0.0",
        "10.0.0.1",
        "100.64.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "192.0.0.1",
        "192.168.1.1",
        "198.18.0.1",
        "224.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "::127.0.0.1",
        "::ffff:127.0.0.1",
        "64:ff9b::7f00:1",
        "100::1",
        "2001:db8::1",
        "2002:7f00:1::",
        "fd00:ec2::254",
        "fe80::1",
        "ff02::1",
    ] {
        assert_eq!(
            DestinationGrant::new("https", host, 443, handle("credential:blocked")),
            Err(Denial::PrivateAddress),
            "private or special address grant accepted: {host}"
        );
    }

    let policy = EgressPolicy::new([]);
    let credential = handle("credential:none");
    for url in [
        "http://2130706433/",
        "http://0x7f000001/",
        "http://017700000001/",
        "http://127.1/",
        "http://127.0.1/",
        "http://0177.0.0.1/",
        "http://0x7f.0.0.1/",
    ] {
        assert_eq!(
            policy.authorize_initial(url, &credential, &public),
            Err(Denial::AlternateNumericHost),
            "alternate numeric address accepted: {url}"
        );
    }
}

#[test]
fn local_names_and_dangerous_ports_are_denied_even_as_grants() {
    for host in [
        "localhost",
        "service.localhost",
        "printer.local",
        "router.lan",
        "service.internal",
        "host.localdomain",
        "metadata",
        "instance-data",
        "metadata.google.internal",
        "1.0.0.127.in-addr.arpa",
        "singlelabel",
    ] {
        assert_eq!(
            DestinationGrant::new("https", host, 443, handle("credential:local")),
            Err(Denial::LocalHostname),
            "local hostname grant accepted: {host}"
        );
    }
    for port in [
        0, 21, 22, 25, 53, 111, 135, 445, 631, 2049, 2375, 3306, 5432, 6379, 6443, 9200, 11211,
        27017,
    ] {
        assert_eq!(
            DestinationGrant::new("https", "api.example.com", port, handle("credential:port")),
            Err(Denial::DangerousPort),
            "dangerous port grant accepted: {port}"
        );
    }
}

#[test]
fn resolutions_are_public_nonempty_and_rechecked_at_connect() {
    let policy = policy();
    let credential = handle("credential:api");
    let url = "https://api.example.com/resource";
    assert_eq!(
        policy.authorize_initial(url, &credential, &observed(&[])),
        Err(Denial::EmptyResolution)
    );
    for addresses in [
        vec!["127.0.0.1"],
        vec!["93.184.216.34", "10.0.0.1"],
        vec!["2606:4700:4700::1111", "fd00::1"],
    ] {
        assert_eq!(
            policy.authorize_initial(url, &credential, &observed(&addresses)),
            Err(Denial::PrivateAddress)
        );
    }

    let authorized = policy
        .authorize_initial(url, &credential, &observed(&["8.8.8.8", "1.1.1.1"]))
        .unwrap();
    assert_eq!(
        policy.authorize_connect(
            &authorized,
            &ConnectObservation::new(observed(&["1.1.1.1", "8.8.8.8"]), ip("8.8.8.8")),
        ),
        Ok(())
    );
    assert_eq!(
        policy.authorize_connect(
            &authorized,
            &ConnectObservation::new(observed(&["9.9.9.9"]), ip("9.9.9.9")),
        ),
        Err(Denial::DnsRebinding)
    );
    assert_eq!(
        policy.authorize_connect(
            &authorized,
            &ConnectObservation::new(observed(&["8.8.8.8", "1.1.1.1"]), ip("127.0.0.1")),
        ),
        Err(Denial::PrivateAddress)
    );
    assert_eq!(
        policy.authorize_connect(
            &authorized,
            &ConnectObservation::new(observed(&["8.8.8.8", "1.1.1.1"]), ip("9.9.9.9")),
        ),
        Err(Denial::ConnectedAddressMismatch)
    );
}

#[test]
fn ip_literals_are_normalized_and_must_match_observations() {
    let policy = policy();
    let dns4 = handle("credential:dns");
    let authorized4 = policy
        .authorize_initial("https://8.8.8.8/dns-query", &dns4, &observed(&["8.8.8.8"]))
        .unwrap();
    assert_eq!(authorized4.destination().host(), "8.8.8.8");
    assert_eq!(
        policy.authorize_initial("https://8.8.8.8/dns-query", &dns4, &observed(&["1.1.1.1"])),
        Err(Denial::DnsRebinding)
    );

    let dns6 = handle("credential:dns6");
    let authorized6 = policy
        .authorize_initial(
            "https://[2606:4700:4700::1111]/dns-query",
            &dns6,
            &observed(&["2606:4700:4700::1111"]),
        )
        .unwrap();
    assert_eq!(authorized6.destination().host(), "2606:4700:4700::1111");
}

#[test]
fn every_redirect_hop_is_reauthorized_and_the_chain_is_bounded() {
    let policy = policy();
    let mut authorization = policy
        .authorize_initial(
            "https://api.example.com/start",
            &handle("credential:api"),
            &observed(&["93.184.216.34"]),
        )
        .unwrap();
    authorization = policy
        .authorize_redirect(
            &authorization,
            "http://downloads.example.com:8080/file",
            &handle("credential:download"),
            &observed(&["1.1.1.1"]),
        )
        .unwrap();
    assert_eq!(authorization.redirect_count(), 1);

    assert_eq!(
        policy.authorize_redirect(
            &authorization,
            "https://ungranted.example.net/",
            &handle("credential:download"),
            &observed(&["1.1.1.1"]),
        ),
        Err(Denial::DestinationNotGranted)
    );
    assert_eq!(
        policy.authorize_redirect(
            &authorization,
            "http://169.254.169.254/latest/meta-data",
            &handle("credential:download"),
            &observed(&["169.254.169.254"]),
        ),
        Err(Denial::PrivateAddress)
    );

    while authorization.redirect_count() < MAX_REDIRECTS {
        authorization = policy
            .authorize_redirect(
                &authorization,
                "https://api.example.com/next",
                &handle("credential:api"),
                &observed(&["93.184.216.34"]),
            )
            .unwrap();
    }
    assert_eq!(
        policy.authorize_redirect(
            &authorization,
            "https://api.example.com/overflow",
            &handle("credential:api"),
            &observed(&["93.184.216.34"]),
        ),
        Err(Denial::RedirectLimit)
    );
}

#[test]
fn credential_handles_and_grants_are_validated() {
    assert_eq!(
        CredentialHandle::new(""),
        Err(Denial::InvalidCredentialHandle)
    );
    assert_eq!(
        CredentialHandle::new("credential:\nsecret"),
        Err(Denial::InvalidCredentialHandle)
    );
    assert_eq!(
        DestinationGrant::new("ftp", "api.example.com", 443, handle("credential:x")),
        Err(Denial::UnsupportedScheme)
    );
}
