use std::{
    collections::BTreeSet,
    net::{Ipv4Addr, Ipv6Addr},
};

use url::{Host, Url};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceDenial {
    UnsupportedSource,
    InvalidUrl,
    SchemeMismatch,
    UserInfo,
    PrivateTarget,
    InvalidFixture,
    FixtureGrantRequired,
}

#[derive(Clone, Debug, Default)]
pub struct RepositorySourcePolicy {
    fixture_grants: BTreeSet<String>,
}

impl RepositorySourcePolicy {
    pub fn new(grants: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            fixture_grants: grants.into_iter().map(Into::into).collect(),
        }
    }

    pub fn authorize(
        &self,
        source: &str,
        location: &str,
        fixture_grant: Option<&str>,
    ) -> Result<(), SourceDenial> {
        if source == "local_fixture" {
            if !valid_fixture_name(location) {
                return Err(SourceDenial::InvalidFixture);
            }
            return fixture_grant
                .is_some_and(|grant| self.fixture_grants.contains(grant))
                .then_some(())
                .ok_or(SourceDenial::FixtureGrantRequired);
        }
        if !matches!(source, "https" | "ssh") {
            return Err(SourceDenial::UnsupportedSource);
        }

        let url = Url::parse(location).map_err(|_| SourceDenial::InvalidUrl)?;
        if url.scheme() != source {
            return Err(SourceDenial::SchemeMismatch);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(SourceDenial::UserInfo);
        }
        let host = url.host().ok_or(SourceDenial::InvalidUrl)?;
        let private = match host {
            Host::Domain(host) => {
                let host = host.trim_end_matches('.');
                host.eq_ignore_ascii_case("localhost")
                    || host.to_ascii_lowercase().ends_with(".localhost")
            }
            Host::Ipv4(address) => !public_ipv4(address),
            Host::Ipv6(address) => !public_ipv6(address),
        };
        if private {
            return Err(SourceDenial::PrivateTarget);
        }
        Ok(())
    }
}

fn valid_fixture_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && name.len() <= 128
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoEntryKind {
    File(Vec<u8>),
    Symlink(String),
    ExecutableHook(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoEntry {
    pub path: String,
    pub kind: RepoEntryKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoFixture {
    pub seed: u64,
    pub base_revision: String,
    pub entries: Vec<RepoEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DenialReason {
    AbsolutePath,
    Traversal,
    SymlinkDenied,
    HooksDisabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeniedEntry {
    pub path: String,
    pub reason: DenialReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInspection {
    pub accepted_paths: Vec<String>,
    pub denied: Vec<DeniedEntry>,
}

impl RepoFixture {
    pub fn malicious(seed: u64) -> Self {
        let suffix = mix(seed);
        Self {
            seed,
            base_revision: format!("fixture-{suffix:016x}"),
            entries: vec![
                RepoEntry {
                    path: "README.md".to_owned(),
                    kind: RepoEntryKind::File(b"fixture repository".to_vec()),
                },
                RepoEntry {
                    path: format!("../outside-{suffix:016x}"),
                    kind: RepoEntryKind::File(b"escape".to_vec()),
                },
                RepoEntry {
                    path: "/host/absolute".to_owned(),
                    kind: RepoEntryKind::File(b"absolute escape".to_vec()),
                },
                RepoEntry {
                    path: "C:\\host\\absolute".to_owned(),
                    kind: RepoEntryKind::File(b"windows absolute escape".to_vec()),
                },
                RepoEntry {
                    path: "linked-secret".to_owned(),
                    kind: RepoEntryKind::Symlink("../../host-secret".to_owned()),
                },
                RepoEntry {
                    path: "nested\\..\\..\\outside".to_owned(),
                    kind: RepoEntryKind::File(b"windows escape".to_vec()),
                },
                RepoEntry {
                    path: "safe-link".to_owned(),
                    kind: RepoEntryKind::Symlink("README.md".to_owned()),
                },
                RepoEntry {
                    path: ".git/hooks/post-checkout".to_owned(),
                    kind: RepoEntryKind::ExecutableHook(b"write outside workspace".to_vec()),
                },
            ],
        }
    }

    pub fn inspect_default_policy(&self) -> RepoInspection {
        let mut accepted_paths = Vec::new();
        let mut denied = Vec::new();

        for entry in &self.entries {
            let reason = if is_absolute(&entry.path) {
                Some(DenialReason::AbsolutePath)
            } else if !stays_within_root(&entry.path) {
                Some(DenialReason::Traversal)
            } else {
                match &entry.kind {
                    RepoEntryKind::Symlink(_) => Some(DenialReason::SymlinkDenied),
                    RepoEntryKind::ExecutableHook(_) => Some(DenialReason::HooksDisabled),
                    _ => None,
                }
            };

            if let Some(reason) = reason {
                denied.push(DeniedEntry {
                    path: entry.path.clone(),
                    reason,
                });
            } else {
                accepted_paths.push(entry.path.clone());
            }
        }

        RepoInspection {
            accepted_paths,
            denied,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterArbiter {
    pub seed: u64,
    owner: Option<String>,
}

impl WriterArbiter {
    pub fn new(seed: u64) -> Self {
        Self { seed, owner: None }
    }

    pub fn claim(&mut self, writer: &str) -> Result<(), String> {
        match &self.owner {
            None => {
                self.owner = Some(writer.to_owned());
                Ok(())
            }
            Some(owner) if owner == writer => Ok(()),
            Some(owner) => Err(format!("workspace already claimed by {owner}")),
        }
    }
}

fn is_absolute(path: &str) -> bool {
    path.starts_with(['/', '\\'])
        || path
            .as_bytes()
            .get(1)
            .is_some_and(|separator| *separator == b':')
}

fn stays_within_root(path: &str) -> bool {
    let mut depth = 0usize;
    for component in path.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." if depth > 0 => depth -= 1,
            ".." => return false,
            _ => depth += 1,
        }
    }
    depth > 0
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

fn mix(value: u64) -> u64 {
    value.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(17)
}
