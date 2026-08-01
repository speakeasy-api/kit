use std::{fmt, sync::Arc};

use crate::{
    api::auth::contract::AuthenticatedPrincipal,
    capabilities::{
        catalog::{CatalogEntry, CatalogSnapshot},
        kernel::{
            grant::{
                self, ArgumentConstraints, CapabilityGrantSnapshot, DelegationSnapshot,
                GrantDecision, GrantRequest,
            },
            grant_ext::RequestExtension,
            identity::{CapabilityIdentity, Digest, DigestAlgorithm, put_digest},
        },
    },
    domain::{
        config::RunConfigSnapshot,
        ids::{ProjectId, WorkspaceId},
    },
};

pub const DISCOVERY_FORMAT_VERSION: u16 = 1;
pub const MAX_SEARCH_QUERY_BYTES: usize = 256;
pub const MAX_SEARCH_RESULTS: usize = 100;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DiscoveryHandle([u8; 32]);

impl DiscoveryHandle {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BindingId([u8; 32]);

impl BindingId {
    pub fn parse(value: &str) -> Result<Self, InvalidBindingId> {
        let hex = value.strip_prefix("binding_v1_").ok_or(InvalidBindingId)?;
        if hex.len() != 64 {
            return Err(InvalidBindingId);
        }
        let mut bytes = [0_u8; 32];
        for (output, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
            *output = (hex_nibble(pair[0]).ok_or(InvalidBindingId)? << 4)
                | hex_nibble(pair[1]).ok_or(InvalidBindingId)?;
        }
        Ok(Self(bytes))
    }

    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidBindingId;

impl fmt::Display for InvalidBindingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid capability binding ID")
    }
}

impl std::error::Error for InvalidBindingId {}

impl fmt::Display for BindingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("binding_v1_")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SearchResult {
    identity: CapabilityIdentity,
    summary: Arc<str>,
    handle: DiscoveryHandle,
}

impl SearchResult {
    pub const fn identity(&self) -> &CapabilityIdentity {
        &self.identity
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub const fn handle(&self) -> DiscoveryHandle {
        self.handle
    }
}

#[derive(Clone, Debug)]
pub struct CapabilityInspection {
    entry: Arc<CatalogEntry>,
    handle: DiscoveryHandle,
}

impl CapabilityInspection {
    pub fn definition(&self) -> &CatalogEntry {
        &self.entry
    }
}

#[derive(Clone, Debug)]
pub struct CapabilityBinding {
    id: BindingId,
    entry: Arc<CatalogEntry>,
    input_schema_digest: Digest,
    entry_digest: Digest,
    catalog_digest: Digest,
    authorization_snapshot_digest: Digest,
}

impl CapabilityBinding {
    pub const fn id(&self) -> BindingId {
        self.id
    }

    pub const fn input_schema_digest(&self) -> Digest {
        self.input_schema_digest
    }

    pub const fn entry_digest(&self) -> Digest {
        self.entry_digest
    }

    pub const fn catalog_digest(&self) -> Digest {
        self.catalog_digest
    }

    pub const fn authorization_snapshot_digest(&self) -> Digest {
        self.authorization_snapshot_digest
    }

    pub const fn pinned_entry(&self) -> &Arc<CatalogEntry> {
        &self.entry
    }

    pub fn validate(
        &self,
        current: &DiscoverySession<'_>,
    ) -> Result<ValidatedBinding, BindingExpired> {
        if current.catalog.digest() != self.catalog_digest {
            return Err(BindingExpired);
        }
        let Some(entry) = current.catalog.get_identity(self.entry.identity()) else {
            return Err(BindingExpired);
        };
        if entry.digest() != self.entry_digest
            || input_schema_digest(entry) != self.input_schema_digest
        {
            return Err(BindingExpired);
        }
        let Some(decision) = current.authorize(entry) else {
            return Err(BindingExpired);
        };
        if decision.snapshot_digest() != self.authorization_snapshot_digest {
            return Err(BindingExpired);
        }
        Ok(ValidatedBinding {
            entry: Arc::clone(&self.entry),
            input_schema_digest: self.input_schema_digest,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedBinding {
    entry: Arc<CatalogEntry>,
    input_schema_digest: Digest,
}

impl ValidatedBinding {
    pub const fn entry(&self) -> &Arc<CatalogEntry> {
        &self.entry
    }

    pub const fn input_schema_digest(&self) -> Digest {
        self.input_schema_digest
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BindingExpired;

impl fmt::Display for BindingExpired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("capability binding expired")
    }
}

impl std::error::Error for BindingExpired {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchError {
    EmptyQuery,
    QueryTooLong,
    InvalidLimit,
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyQuery => "capability search query is empty",
            Self::QueryTooLong => "capability search query exceeds its byte limit",
            Self::InvalidLimit => "capability search result limit is out of range",
        })
    }
}

impl std::error::Error for SearchError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InspectionUnavailable;

impl fmt::Display for InspectionUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("capability inspection is unavailable")
    }
}

impl std::error::Error for InspectionUnavailable {}

pub struct DiscoverySession<'a> {
    catalog: &'a CatalogSnapshot,
    authenticated: &'a AuthenticatedPrincipal,
    config: &'a RunConfigSnapshot,
    grants: &'a CapabilityGrantSnapshot,
    delegation: Option<&'a DelegationSnapshot>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    argument_constraints: &'a ArgumentConstraints,
    extension: RequestExtension,
}

impl<'a> DiscoverySession<'a> {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        catalog: &'a CatalogSnapshot,
        authenticated: &'a AuthenticatedPrincipal,
        config: &'a RunConfigSnapshot,
        grants: &'a CapabilityGrantSnapshot,
        delegation: Option<&'a DelegationSnapshot>,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        argument_constraints: &'a ArgumentConstraints,
        extension: RequestExtension,
    ) -> Self {
        Self {
            catalog,
            authenticated,
            config,
            grants,
            delegation,
            workspace_id,
            project_id,
            argument_constraints,
            extension,
        }
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, SearchError> {
        if query.is_empty() {
            return Err(SearchError::EmptyQuery);
        }
        if query.len() > MAX_SEARCH_QUERY_BYTES {
            return Err(SearchError::QueryTooLong);
        }
        if !(1..=MAX_SEARCH_RESULTS).contains(&limit) {
            return Err(SearchError::InvalidLimit);
        }

        let query = query.to_ascii_lowercase();
        let mut results = Vec::new();
        for entry in self.catalog.entries() {
            let Some(decision) = self.authorize(entry) else {
                continue;
            };
            if !matches_entry(entry, &query) {
                continue;
            }
            results.push(SearchResult {
                identity: entry.identity().clone(),
                summary: Arc::from(entry.search().summary()),
                handle: discovery_handle(entry.digest(), &decision),
            });
            if results.len() == limit {
                break;
            }
        }
        Ok(results)
    }

    pub fn inspect(&self, handle: DiscoveryHandle) -> Option<CapabilityInspection> {
        self.catalog.entries().iter().find_map(|entry| {
            let decision = self.authorize(entry)?;
            (discovery_handle(entry.digest(), &decision) == handle).then(|| CapabilityInspection {
                entry: Arc::clone(entry),
                handle,
            })
        })
    }

    pub fn bind(
        &self,
        inspection: &CapabilityInspection,
    ) -> Result<CapabilityBinding, InspectionUnavailable> {
        let inspected = self
            .inspect(inspection.handle)
            .filter(|current| current.entry.digest() == inspection.entry.digest())
            .ok_or(InspectionUnavailable)?;
        let decision = self
            .authorize(&inspected.entry)
            .ok_or(InspectionUnavailable)?;
        let input_schema_digest = input_schema_digest(&inspected.entry);
        let entry_digest = inspected.entry.digest();
        let catalog_digest = self.catalog.digest();
        let authorization_snapshot_digest = decision.snapshot_digest();
        let id = binding_id(
            &inspected.entry,
            input_schema_digest,
            authorization_snapshot_digest,
        );
        Ok(CapabilityBinding {
            id,
            entry: inspected.entry,
            input_schema_digest,
            entry_digest,
            catalog_digest,
            authorization_snapshot_digest,
        })
    }

    fn authorize(&self, entry: &CatalogEntry) -> Option<GrantDecision> {
        let required = entry.authority().required_grants();
        if !entry.authority().auth_scopes().is_empty()
            || !required.is_subset(self.authenticated.grant_snapshot().grants())
            || !required.is_subset(self.config.effective_authority())
        {
            return None;
        }
        let decision = grant::decide(GrantRequest {
            authenticated: self.authenticated,
            capability: entry.identity(),
            schema_digest: input_schema_digest(entry),
            effect: entry.side_effects().effect(),
            argument_constraints: self.argument_constraints,
            workspace_id: self.workspace_id,
            project_id: self.project_id,
            config: self.config,
            grants: self.grants,
            delegation: self.delegation,
            extension: self.extension.clone(),
        });
        decision.is_allowed().then_some(decision)
    }
}

fn input_schema_digest(entry: &CatalogEntry) -> Digest {
    entry
        .schemas()
        .input()
        .schema()
        .source()
        .normalized_digest()
}

fn discovery_handle(entry_digest: Digest, decision: &GrantDecision) -> DiscoveryHandle {
    let mut canonical = Vec::new();
    canonical.extend_from_slice(b"KIT-DISCOVERY-HANDLE\0");
    canonical.extend_from_slice(&DISCOVERY_FORMAT_VERSION.to_be_bytes());
    put_digest(&mut canonical, entry_digest);
    put_digest(&mut canonical, decision.snapshot_digest());
    DiscoveryHandle(Digest::of(DigestAlgorithm::Sha256, &canonical).as_bytes())
}

fn binding_id(
    entry: &CatalogEntry,
    schema_digest: Digest,
    authorization_snapshot_digest: Digest,
) -> BindingId {
    let mut canonical = Vec::new();
    canonical.extend_from_slice(b"KIT-CAPABILITY-BINDING\0");
    canonical.extend_from_slice(&DISCOVERY_FORMAT_VERSION.to_be_bytes());
    entry.identity().write_canonical(&mut canonical);
    put_digest(&mut canonical, schema_digest);
    put_digest(&mut canonical, authorization_snapshot_digest);
    BindingId(Digest::of(DigestAlgorithm::Sha256, &canonical).as_bytes())
}

fn matches_entry(entry: &CatalogEntry, lowercase_query: &str) -> bool {
    [
        entry.identity().namespace().as_str(),
        entry.identity().name().as_str(),
        entry.search().summary(),
    ]
    .into_iter()
    .chain(entry.search().terms().iter().map(AsRef::as_ref))
    .any(|value| value.to_ascii_lowercase().contains(lowercase_query))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
