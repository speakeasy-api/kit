pub mod a2a;
pub mod acp;
pub mod mcp;

use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use url::{Host, Url};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessGrant {
    pub principal_id: String,
    pub origin: String,
    pub host: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiRequest {
    pub principal: Principal,
    pub origin: String,
    pub host: String,
    pub idempotency_key: String,
    pub request_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiResponse {
    Unauthorized,
    Accepted { trace_id: u64 },
    Replay,
    Conflict,
}

#[derive(Clone, Debug)]
pub struct ApiIngressSimulator {
    seed: u64,
    grants: Vec<AccessGrant>,
}

impl ApiIngressSimulator {
    pub fn new(seed: u64, grants: Vec<AccessGrant>) -> Self {
        Self { seed, grants }
    }

    pub fn replay(&self, requests: &[ApiRequest]) -> (Vec<ApiResponse>, usize) {
        let mut accepted = BTreeMap::<String, String>::new();
        let mut dispatches = 0usize;
        let responses = requests
            .iter()
            .enumerate()
            .map(|(index, request)| {
                if !self.grants.iter().any(|grant| {
                    grant.principal_id == request.principal.id
                        && grant.origin == request.origin
                        && grant.host == request.host
                }) {
                    return ApiResponse::Unauthorized;
                }
                match accepted.get(&request.idempotency_key) {
                    Some(digest) if digest == &request.request_digest => ApiResponse::Replay,
                    Some(_) => ApiResponse::Conflict,
                    None => {
                        accepted.insert(
                            request.idempotency_key.clone(),
                            request.request_digest.clone(),
                        );
                        dispatches += 1;
                        ApiResponse::Accepted {
                            trace_id: self.seed.rotate_left(11) ^ index as u64,
                        }
                    }
                }
            })
            .collect();
        (responses, dispatches)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedirectOutcome {
    pub seed: u64,
    pub allowed_hops: usize,
    pub denied_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedirectHop {
    pub url: String,
    pub resolved: IpAddr,
    pub connected: IpAddr,
}

pub fn replay_redirects(seed: u64, hops: &[RedirectHop]) -> RedirectOutcome {
    let denied = hops.iter().position(|hop| {
        !allowed_url(&hop.url) || !public_ip(hop.resolved) || hop.resolved != hop.connected
    });
    RedirectOutcome {
        seed,
        allowed_hops: denied.unwrap_or(hops.len()),
        denied_url: denied.map(|index| hops[index].url.clone()),
    }
}

fn allowed_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    match url.host() {
        Some(Host::Domain(host)) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host != "localhost" && !host.ends_with(".localhost")
        }
        Some(Host::Ipv4(address)) => public_ipv4(address),
        Some(Host::Ipv6(address)) => public_ipv6(address),
        None => false,
    }
}

fn public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => public_ipv4(address),
        IpAddr::V6(address) => public_ipv6(address),
    }
}

fn public_ipv4(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_documentation()
        || first == 0
        || first >= 240
        || (first == 100 && (64..=127).contains(&second))
        || (first == 198 && (18..=19).contains(&second)))
}

fn public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return public_ipv4(mapped);
    }
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || (address.segments()[0] & 0xfe00) == 0xfc00
        || (address.segments()[0] & 0xffc0) == 0xfe80
        || (address.segments()[0] == 0x2001 && address.segments()[1] == 0x0db8))
}
