use std::{
    collections::BTreeSet,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

pub const MAX_REDIRECTS: usize = 5;
pub const MAX_RESOLVED_ADDRESSES: usize = 64;
pub const MAX_EGRESS_URL_BYTES: usize = 16 * 1024;
const DNS_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(10);

#[async_trait::async_trait]
pub trait EgressResolver: Send + Sync + 'static {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, Denial>;
}

#[derive(Default)]
pub struct SystemEgressResolver;

#[async_trait::async_trait]
impl EgressResolver for SystemEgressResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, Denial> {
        let resolved = tokio::time::timeout(
            DNS_RESOLUTION_TIMEOUT,
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| Denial::ResolverUnavailable)?
        .map_err(|_| Denial::ResolverUnavailable)?;
        let addresses = resolved
            .take(MAX_RESOLVED_ADDRESSES + 1)
            .map(|address| address.ip())
            .collect::<Vec<_>>();
        if addresses.len() > MAX_RESOLVED_ADDRESSES {
            return Err(Denial::ResolutionLimit);
        }
        Ok(addresses)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    const fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

impl FromStr for Scheme {
    type Err = Denial;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("http") {
            Ok(Self::Http)
        } else if value.eq_ignore_ascii_case("https") {
            Ok(Self::Https)
        } else {
            Err(Denial::UnsupportedScheme)
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CredentialHandle(String);

impl CredentialHandle {
    pub fn new(value: impl Into<String>) -> Result<Self, Denial> {
        let value = value.into();
        if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(Denial::InvalidCredentialHandle);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Host {
    Domain(String),
    Ip(IpAddr),
}

impl Host {
    fn canonical(&self) -> String {
        match self {
            Self::Domain(host) => host.clone(),
            Self::Ip(address) => address.to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Destination {
    scheme: Scheme,
    host: Host,
    port: u16,
}

impl Destination {
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    pub fn host(&self) -> String {
        self.host.canonical()
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DestinationGrant {
    destination: Destination,
    credential: CredentialHandle,
}

impl DestinationGrant {
    pub fn new(
        scheme: &str,
        host: &str,
        port: u16,
        credential: CredentialHandle,
    ) -> Result<Self, Denial> {
        let scheme = scheme.parse()?;
        validate_port(port)?;
        let host = parse_host(host)?;
        Ok(Self {
            destination: Destination { scheme, host, port },
            credential,
        })
    }

    pub fn destination(&self) -> &Destination {
        &self.destination
    }

    pub fn credential(&self) -> &CredentialHandle {
        &self.credential
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolverObservation {
    addresses: Vec<IpAddr>,
}

impl ResolverObservation {
    pub(crate) fn new(addresses: impl IntoIterator<Item = IpAddr>) -> Self {
        Self {
            addresses: addresses.into_iter().collect(),
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }
}

#[cfg(test)]
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConnectObservation {
    resolution: ResolverObservation,
    connected: IpAddr,
}

#[cfg(test)]
#[allow(dead_code)]
impl ConnectObservation {
    pub(crate) fn new(resolution: ResolverObservation, connected: IpAddr) -> Self {
        Self {
            resolution,
            connected,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Authorization {
    destination: Destination,
    credential: CredentialHandle,
    resolved: BTreeSet<IpAddr>,
    redirects: usize,
}

impl Authorization {
    pub fn destination(&self) -> &Destination {
        &self.destination
    }

    pub fn credential(&self) -> &CredentialHandle {
        &self.credential
    }

    pub fn redirect_count(&self) -> usize {
        self.redirects
    }

    pub fn resolved_addresses(&self) -> impl Iterator<Item = IpAddr> + '_ {
        self.resolved.iter().copied()
    }

    pub fn authorizes_peer(&self, address: IpAddr) -> bool {
        self.resolved.contains(&address)
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        scheme: Scheme,
        host: &str,
        port: u16,
        credential: CredentialHandle,
        addresses: impl IntoIterator<Item = IpAddr>,
    ) -> Self {
        Self {
            destination: Destination {
                scheme,
                host: Host::Domain(host.to_owned()),
                port,
            },
            credential,
            resolved: addresses.into_iter().collect(),
            redirects: 0,
        }
    }
}

#[derive(Clone)]
pub struct EgressPolicy {
    grants: BTreeSet<DestinationGrant>,
    resolver: Arc<dyn EgressResolver>,
}

impl fmt::Debug for EgressPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EgressPolicy")
            .field("grants", &self.grants)
            .finish_non_exhaustive()
    }
}

impl EgressPolicy {
    pub fn new(grants: impl IntoIterator<Item = DestinationGrant>) -> Self {
        Self {
            grants: grants.into_iter().collect(),
            resolver: Arc::new(SystemEgressResolver),
        }
    }

    pub fn with_resolver(mut self, resolver: Arc<dyn EgressResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    pub(crate) fn configured_destinations(&self) -> impl Iterator<Item = &Destination> {
        self.grants.iter().map(DestinationGrant::destination)
    }

    /// Selects an exact configured destination and its opaque credential
    /// before any resolver or credential storage is consulted.
    pub fn grant_for_url(&self, url: &str) -> Result<&DestinationGrant, Denial> {
        let destination = parse_url(url)?;
        let mut matches = self
            .grants
            .iter()
            .filter(|grant| grant.destination == destination);
        let grant = matches.next().ok_or(Denial::DestinationNotGranted)?;
        if matches.next().is_some() {
            return Err(Denial::AmbiguousGrant);
        }
        Ok(grant)
    }

    pub fn canonical_url(url: &str) -> Result<url::Url, Denial> {
        let destination = parse_url(url)?;
        let mut parsed = url::Url::parse(url).map_err(|_| Denial::InvalidUrl)?;
        let parsed_host = parsed.host_str().ok_or(Denial::InvalidHost)?;
        if !parsed_host.is_ascii() {
            return Err(Denial::InvalidHost);
        }
        if matches!(&destination.host, Host::Domain(host) if !parsed_host.trim_end_matches('.').eq_ignore_ascii_case(host))
        {
            return Err(Denial::InvalidHost);
        }
        let canonical_host = match &destination.host {
            Host::Ip(IpAddr::V6(address)) => format!("[{address}]"),
            host => host.canonical(),
        };
        parsed
            .set_host(Some(&canonical_host))
            .map_err(|_| Denial::InvalidHost)?;
        if parse_url(parsed.as_str())? != destination {
            return Err(Denial::InvalidUrl);
        }
        Ok(parsed)
    }

    /// Resolves through the policy-owned system resolver and pins the complete
    /// validated address set into the returned authorization.
    pub async fn resolve_initial(
        &self,
        url: &str,
        credential: &CredentialHandle,
    ) -> Result<Authorization, Denial> {
        let destination = parse_url(url)?;
        self.authorize_destination(&destination, credential)?;
        let resolution = self.resolve_destination(&destination).await?;
        self.authorize(url, credential, &resolution, 0)
    }

    pub async fn resolve_initial_granted(&self, url: &str) -> Result<Authorization, Denial> {
        let credential = self.grant_for_url(url)?.credential.clone();
        self.resolve_initial(url, &credential).await
    }

    pub async fn resolve_redirect(
        &self,
        previous: &Authorization,
        url: &str,
        credential: &CredentialHandle,
    ) -> Result<Authorization, Denial> {
        if previous.redirects >= MAX_REDIRECTS {
            return Err(Denial::RedirectLimit);
        }
        let destination = parse_url(url)?;
        self.authorize_destination(&destination, credential)?;
        let resolution = self.resolve_destination(&destination).await?;
        self.authorize(url, credential, &resolution, previous.redirects + 1)
    }

    pub async fn resolve_redirect_granted(
        &self,
        previous: &Authorization,
        url: &str,
    ) -> Result<Authorization, Denial> {
        let credential = self.grant_for_url(url)?.credential.clone();
        self.resolve_redirect(previous, url, &credential).await
    }

    pub fn validate_peer(
        &self,
        authorization: &Authorization,
        connected: IpAddr,
    ) -> Result<(), Denial> {
        if !public_ip(connected) {
            return Err(Denial::PrivateAddress);
        }
        if !authorization.resolved.contains(&connected) {
            return Err(Denial::ConnectedAddressMismatch);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn authorize_initial(
        &self,
        url: &str,
        credential: &CredentialHandle,
        resolution: &ResolverObservation,
    ) -> Result<Authorization, Denial> {
        self.authorize(url, credential, resolution, 0)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn authorize_redirect(
        &self,
        previous: &Authorization,
        url: &str,
        credential: &CredentialHandle,
        resolution: &ResolverObservation,
    ) -> Result<Authorization, Denial> {
        if previous.redirects >= MAX_REDIRECTS {
            return Err(Denial::RedirectLimit);
        }
        self.authorize(url, credential, resolution, previous.redirects + 1)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn authorize_connect(
        &self,
        authorization: &Authorization,
        observation: &ConnectObservation,
    ) -> Result<(), Denial> {
        let current =
            validated_addresses(&authorization.destination.host, &observation.resolution)?;
        if current != authorization.resolved {
            return Err(Denial::DnsRebinding);
        }
        if !public_ip(observation.connected) {
            return Err(Denial::PrivateAddress);
        }
        if !current.contains(&observation.connected) {
            return Err(Denial::ConnectedAddressMismatch);
        }
        Ok(())
    }

    fn authorize(
        &self,
        url: &str,
        credential: &CredentialHandle,
        resolution: &ResolverObservation,
        redirects: usize,
    ) -> Result<Authorization, Denial> {
        let destination = parse_url(url)?;
        self.authorize_destination(&destination, credential)?;
        let resolved = validated_addresses(&destination.host, resolution)?;
        Ok(Authorization {
            destination,
            credential: credential.clone(),
            resolved,
            redirects,
        })
    }

    fn authorize_destination(
        &self,
        destination: &Destination,
        credential: &CredentialHandle,
    ) -> Result<(), Denial> {
        let grant = DestinationGrant {
            destination: destination.clone(),
            credential: credential.clone(),
        };
        if !self.grants.contains(&grant) {
            return Err(Denial::DestinationNotGranted);
        }
        Ok(())
    }

    async fn resolve_destination(
        &self,
        destination: &Destination,
    ) -> Result<ResolverObservation, Denial> {
        let host = destination.host.canonical();
        let addresses = self.resolver.resolve(&host, destination.port).await?;
        if addresses.len() > MAX_RESOLVED_ADDRESSES {
            return Err(Denial::ResolutionLimit);
        }
        Ok(ResolverObservation::new(addresses))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Denial {
    InvalidUrl,
    UnsupportedScheme,
    UserInfo,
    Fragment,
    InvalidHost,
    LocalHostname,
    AlternateNumericHost,
    PrivateAddress,
    InvalidPort,
    DangerousPort,
    InvalidCredentialHandle,
    DestinationNotGranted,
    AmbiguousGrant,
    EmptyResolution,
    ResolverUnavailable,
    ResolutionLimit,
    DnsRebinding,
    ConnectedAddressMismatch,
    RedirectLimit,
}

impl fmt::Display for Denial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "egress denied: {self:?}")
    }
}

impl std::error::Error for Denial {}

fn parse_url(value: &str) -> Result<Destination, Denial> {
    if value.is_empty()
        || value.len() > MAX_EGRESS_URL_BYTES
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ' || byte == b'\\')
    {
        return Err(Denial::InvalidUrl);
    }
    if value.contains('#') {
        return Err(Denial::Fragment);
    }
    let (scheme, remainder) = value.split_once(':').ok_or(Denial::InvalidUrl)?;
    let scheme: Scheme = scheme.parse()?;
    let remainder = remainder.strip_prefix("//").ok_or(Denial::InvalidUrl)?;
    let authority_end = remainder.find(['/', '?']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if authority.is_empty() {
        return Err(Denial::InvalidHost);
    }
    if authority.contains('@') {
        return Err(Denial::UserInfo);
    }

    let (host, port) = if authority.starts_with('[') {
        let close = authority.find(']').ok_or(Denial::InvalidHost)?;
        let host = &authority[1..close];
        host.parse::<Ipv6Addr>().map_err(|_| Denial::InvalidHost)?;
        let suffix = &authority[close + 1..];
        if suffix.is_empty() {
            (host, scheme.default_port())
        } else {
            let port = suffix.strip_prefix(':').ok_or(Denial::InvalidHost)?;
            (host, parse_port(port)?)
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (host, parse_port(port)?),
            Some(_) => return Err(Denial::InvalidHost),
            None => (authority, scheme.default_port()),
        }
    };
    validate_port(port)?;
    Ok(Destination {
        scheme,
        host: parse_host(host)?,
        port,
    })
}

fn parse_port(value: &str) -> Result<u16, Denial> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Denial::InvalidPort);
    }
    value.parse().map_err(|_| Denial::InvalidPort)
}

fn validate_port(port: u16) -> Result<(), Denial> {
    const DANGEROUS: &[u16] = &[
        1080, 1099, 1433, 1521, 2049, 2181, 2375, 2376, 3306, 3389, 4369, 5432, 5900, 6379, 6443,
        9200, 9300, 11211, 27017,
    ];
    if (port < 1024 && !matches!(port, 80 | 443)) || DANGEROUS.contains(&port) {
        Err(Denial::DangerousPort)
    } else {
        Ok(())
    }
}

fn parse_host(value: &str) -> Result<Host, Denial> {
    if value.is_empty() || !value.is_ascii() || value.contains(['%', '[', ']']) {
        return Err(Denial::InvalidHost);
    }
    if value.contains(':') {
        let address = value.parse::<Ipv6Addr>().map_err(|_| Denial::InvalidHost)?;
        let address = IpAddr::V6(address);
        return public_ip(address)
            .then_some(Host::Ip(address))
            .ok_or(Denial::PrivateAddress);
    }

    if value.ends_with("..") {
        return Err(Denial::InvalidHost);
    }
    let host = value.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.len() > 253 {
        return Err(Denial::InvalidHost);
    }
    let labels = host.split('.').collect::<Vec<_>>();
    if labels.iter().any(|label| {
        label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return Err(Denial::InvalidHost);
    }

    if labels.len() == 4
        && labels
            .iter()
            .all(|label| label.bytes().all(|b| b.is_ascii_digit()))
    {
        if labels
            .iter()
            .any(|label| label.len() > 1 && label.starts_with('0'))
        {
            return Err(Denial::AlternateNumericHost);
        }
        let octets = labels
            .iter()
            .map(|label| label.parse::<u8>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| Denial::AlternateNumericHost)?;
        let address = IpAddr::V4(Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]));
        return public_ip(address)
            .then_some(Host::Ip(address))
            .ok_or(Denial::PrivateAddress);
    }
    if alternate_numeric_host(&labels) {
        return Err(Denial::AlternateNumericHost);
    }
    if local_hostname(&host, &labels) {
        return Err(Denial::LocalHostname);
    }
    Ok(Host::Domain(host))
}

fn alternate_numeric_host(labels: &[&str]) -> bool {
    labels
        .iter()
        .all(|label| label.bytes().all(|byte| byte.is_ascii_digit()))
        || labels.iter().all(|label| {
            label.bytes().all(|byte| byte.is_ascii_digit())
                || label
                    .strip_prefix("0x")
                    .or_else(|| label.strip_prefix("0X"))
                    .is_some_and(|digits| {
                        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
        })
}

fn local_hostname(host: &str, labels: &[&str]) -> bool {
    labels.len() == 1
        || [
            "localhost",
            "local",
            "localdomain",
            "internal",
            "lan",
            "home",
            "in-addr.arpa",
            "ip6.arpa",
        ]
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        || matches!(
            host,
            "metadata" | "instance-data" | "metadata.google.internal" | "metadata.aws.internal"
        )
}

fn validated_addresses(
    host: &Host,
    observation: &ResolverObservation,
) -> Result<BTreeSet<IpAddr>, Denial> {
    if observation.addresses.is_empty() {
        return Err(Denial::EmptyResolution);
    }
    let addresses = observation
        .addresses
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if addresses.iter().any(|address| !public_ip(*address)) {
        return Err(Denial::PrivateAddress);
    }
    if let Host::Ip(literal) = host
        && (addresses.len() != 1 || !addresses.contains(literal))
    {
        return Err(Denial::DnsRebinding);
    }
    Ok(addresses)
}

fn public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => public_ipv4(address),
        IpAddr::V6(address) => public_ipv6(address),
    }
}

fn public_ipv4(address: Ipv4Addr) -> bool {
    let value = u32::from(address);
    ![
        (0x0000_0000, 8),
        (0x0a00_0000, 8),
        (0x6440_0000, 10),
        (0x7f00_0000, 8),
        (0xa9fe_0000, 16),
        (0xac10_0000, 12),
        (0xc000_0000, 24),
        (0xc000_0200, 24),
        (0xc058_6300, 24),
        (0xc0a8_0000, 16),
        (0xc612_0000, 15),
        (0xc633_6400, 24),
        (0xcb00_7100, 24),
        (0xe000_0000, 4),
        (0xf000_0000, 4),
    ]
    .iter()
    .any(|(network, prefix)| in_v4_network(value, *network, *prefix))
}

fn in_v4_network(address: u32, network: u32, prefix: u32) -> bool {
    let mask = u32::MAX << (32 - prefix);
    address & mask == network
}

fn public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return public_ipv4(mapped);
    }
    let value = u128::from(address);
    ![
        (0_u128, 96),
        (0x0064_ff9b_0000_0000_0000_0000_0000_0000, 96),
        (0x0064_ff9b_0001_0000_0000_0000_0000_0000, 48),
        (0x0100_0000_0000_0000_0000_0000_0000_0000, 64),
        (0x2001_0000_0000_0000_0000_0000_0000_0000, 23),
        (0x2001_0db8_0000_0000_0000_0000_0000_0000, 32),
        (0x2002_0000_0000_0000_0000_0000_0000_0000, 16),
        (0x3fff_0000_0000_0000_0000_0000_0000_0000, 20),
        (0x5f00_0000_0000_0000_0000_0000_0000_0000, 16),
        (0xfc00_0000_0000_0000_0000_0000_0000_0000, 7),
        (0xfe80_0000_0000_0000_0000_0000_0000_0000, 10),
        (0xfec0_0000_0000_0000_0000_0000_0000_0000, 10),
        (0xff00_0000_0000_0000_0000_0000_0000_0000, 8),
    ]
    .iter()
    .any(|(network, prefix)| in_v6_network(value, *network, *prefix))
}

fn in_v6_network(address: u128, network: u128, prefix: u32) -> bool {
    let mask = u128::MAX << (128 - prefix);
    address & mask == network
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ungranted_destination_is_denied_before_dns() {
        let policy = EgressPolicy::new([]);
        let credential = CredentialHandle::new("test:credential").unwrap();
        assert_eq!(
            policy
                .resolve_initial("https://must-not-resolve.invalid/mcp", &credential)
                .await,
            Err(Denial::DestinationNotGranted)
        );
    }

    #[test]
    fn canonical_url_strips_one_root_dot_and_rejects_ambiguous_hosts() {
        assert_eq!(
            EgressPolicy::canonical_url("https://EXAMPLE.com./mcp?x=1")
                .unwrap()
                .as_str(),
            "https://example.com/mcp?x=1"
        );
        for denied in [
            "https://example.com../mcp",
            "https://exämple.com/mcp",
            "https://xn--exmple-cua.com../mcp",
        ] {
            assert!(
                EgressPolicy::canonical_url(denied).is_err(),
                "accepted {denied}"
            );
        }
    }

    #[test]
    fn current_iana_non_global_ipv6_ranges_are_shared_denials() {
        for address in [
            "::",
            "::1",
            "64:ff9b::1",
            "64:ff9b:1::1",
            "100::1",
            "2001::1",
            "2001:db8::1",
            "2002::1",
            "3fff::1",
            "5f00::1",
            "fc00::1",
            "fe80::1",
            "fec0::1",
            "ff00::1",
        ] {
            assert!(!public_ip(address.parse().unwrap()), "accepted {address}");
        }
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn public_ipv6_literal_is_canonicalized_without_losing_url_brackets() {
        let credential = CredentialHandle::new("test:credential").unwrap();
        let grant =
            DestinationGrant::new("https", "2606:4700:4700::1111", 443, credential).unwrap();
        let policy = EgressPolicy::new([grant]);
        let url = EgressPolicy::canonical_url(
            "https://[2606:4700:4700:0:0:0:0:1111]/authorize?client=kit",
        )
        .unwrap();

        assert_eq!(
            url.as_str(),
            "https://[2606:4700:4700::1111]/authorize?client=kit"
        );
        assert_eq!(
            policy
                .grant_for_url(url.as_str())
                .unwrap()
                .destination()
                .host(),
            "2606:4700:4700::1111"
        );
    }
}
