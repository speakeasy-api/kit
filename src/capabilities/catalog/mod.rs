use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use crate::{
    agent::accounting::MoneyMicros,
    capabilities::{
        kernel::{
            grant::EffectClass,
            identity::{
                CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                CapabilityVersion, Digest, DigestAlgorithm, put_bytes, put_digest,
            },
            invoke::RetrySafety,
        },
        native::NativeToolDescriptor,
        schema::{JSON_SCHEMA_2020_12, NormalizedSchema, SchemaProjectionSet},
    },
    domain::{config::Grant, secret::SecretHandle},
};

pub const CATALOG_FORMAT_VERSION: u16 = 4;
pub const MAX_CATALOG_ENTRIES: usize = 4096;
pub const MAX_CATALOG_SOURCES: usize = 512;
pub const MAX_SUMMARY_BYTES: usize = 1024;
pub const MAX_SEARCH_TERMS: usize = 64;
pub const MAX_AUTH_SCOPES: usize = 64;
pub const MAX_CATALOG_TEXT_BYTES: usize = 256;
pub const MAX_CATALOG_ENTRY_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CATALOG_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SourceKind {
    Native,
    ProjectPlugin,
    Mcp,
    Acp,
    A2a,
    ProviderNative,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Tool,
    Resource,
    ResourceTemplate,
    Prompt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalTarget {
    configured_server: Arc<str>,
    kind: CapabilityKind,
    remote: Arc<str>,
    descriptor_digest: Digest,
}

impl ExternalTarget {
    pub(crate) fn mcp(
        configured_server: impl AsRef<str>,
        kind: CapabilityKind,
        remote: impl AsRef<str>,
        descriptor_digest: Digest,
    ) -> Result<Self, CatalogError> {
        let configured_server = configured_server.as_ref();
        let remote = remote.as_ref();
        validate_text(
            "configured server",
            configured_server,
            MAX_CATALOG_TEXT_BYTES,
        )?;
        if remote.is_empty() || remote.len() > 16 * 1024 || remote.chars().any(char::is_control) {
            return Err(CatalogError::InvalidText("external target"));
        }
        Ok(Self {
            configured_server: Arc::from(configured_server),
            kind,
            remote: Arc::from(remote),
            descriptor_digest,
        })
    }

    pub fn configured_server(&self) -> &str {
        &self.configured_server
    }

    pub const fn kind(&self) -> CapabilityKind {
        self.kind
    }

    pub fn remote(&self) -> &str {
        &self.remote
    }

    pub const fn descriptor_digest(&self) -> Digest {
        self.descriptor_digest
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TrustDomain(Arc<str>);

impl TrustDomain {
    pub fn new(value: impl AsRef<str>) -> Result<Self, CatalogError> {
        let value = value.as_ref();
        validate_text("trust domain", value, MAX_CATALOG_TEXT_BYTES)?;
        Ok(Self(Arc::from(value)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CatalogSource {
    kind: SourceKind,
    id: CapabilitySource,
    trust_domain: TrustDomain,
}

impl CatalogSource {
    pub fn new(
        kind: SourceKind,
        id: CapabilitySource,
        trust_domain: TrustDomain,
    ) -> Result<Self, CatalogError> {
        validate_text("source", id.as_str(), MAX_CATALOG_TEXT_BYTES)?;
        Ok(Self {
            kind,
            id,
            trust_domain,
        })
    }

    pub const fn kind(&self) -> SourceKind {
        self.kind
    }

    pub const fn id(&self) -> &CapabilitySource {
        &self.id
    }

    pub const fn trust_domain(&self) -> &TrustDomain {
        &self.trust_domain
    }
}

#[derive(Clone, Debug)]
pub struct CatalogSchemas {
    input: SchemaProjectionSet,
    output: Option<SchemaProjectionSet>,
    digest: Digest,
}

impl CatalogSchemas {
    pub fn new(input: SchemaProjectionSet, output: Option<SchemaProjectionSet>) -> Self {
        let algorithm = input.schema().source().normalized_digest().algorithm();
        let mut canonical = Vec::new();
        canonical.extend_from_slice(b"KIT-CATALOG-SCHEMAS\0");
        canonical.extend_from_slice(&CATALOG_FORMAT_VERSION.to_be_bytes());
        write_projection_set(&mut canonical, &input);
        match &output {
            Some(output) => {
                canonical.push(1);
                write_projection_set(&mut canonical, output);
            }
            None => canonical.push(0),
        }
        let digest = Digest::of(algorithm, &canonical);
        Self {
            input,
            output,
            digest,
        }
    }

    pub const fn input(&self) -> &SchemaProjectionSet {
        &self.input
    }

    pub const fn output(&self) -> Option<&SchemaProjectionSet> {
        self.output.as_ref()
    }

    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSearch {
    summary: Arc<str>,
    terms: BTreeSet<Arc<str>>,
}

impl CatalogSearch {
    pub fn new<I, S>(summary: impl AsRef<str>, terms: I) -> Result<Self, CatalogError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let summary = summary.as_ref();
        validate_text("summary", summary, MAX_SUMMARY_BYTES)?;
        let terms = collect_bounded_text(
            terms,
            MAX_SEARCH_TERMS,
            "search term",
            CatalogLimit::SearchTerms,
        )?;
        Ok(Self {
            summary: Arc::from(summary),
            terms,
        })
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub const fn terms(&self) -> &BTreeSet<Arc<str>> {
        &self.terms
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogDigests {
    schema: Digest,
    implementation: Digest,
}

impl CatalogDigests {
    pub const fn schema(&self) -> Digest {
        self.schema
    }

    pub const fn implementation(&self) -> Digest {
        self.implementation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SideEffects {
    effect: EffectClass,
    retry_safety: RetrySafety,
}

impl SideEffects {
    pub const fn new(effect: EffectClass, retry_safety: RetrySafety) -> Self {
        Self {
            effect,
            retry_safety,
        }
    }

    pub const fn effect(&self) -> EffectClass {
        self.effect
    }

    pub const fn retry_safety(&self) -> RetrySafety {
        self.retry_safety
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogAuthority {
    required_grants: BTreeSet<Grant>,
    /// External invocation metadata learned after authenticated discovery.
    /// These scopes never grant or suppress Kit-local discovery authority.
    auth_scopes: BTreeSet<Arc<str>>,
    credential: Option<SecretHandle>,
}

impl CatalogAuthority {
    pub fn new<G, I, S>(required_grants: G, auth_scopes: I) -> Result<Self, CatalogError>
    where
        G: IntoIterator<Item = Grant>,
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::new_with_credential(required_grants, auth_scopes, None)
    }

    pub fn new_with_credential<G, I, S>(
        required_grants: G,
        auth_scopes: I,
        credential: Option<SecretHandle>,
    ) -> Result<Self, CatalogError>
    where
        G: IntoIterator<Item = Grant>,
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut grants = BTreeSet::new();
        let mut grant_count = 0_usize;
        for grant in required_grants {
            grant_count = grant_count
                .checked_add(1)
                .ok_or(CatalogError::LimitExceeded(CatalogLimit::RequiredGrants))?;
            if grant_count > 8 {
                return Err(CatalogError::LimitExceeded(CatalogLimit::RequiredGrants));
            }
            grants.insert(grant);
        }
        Ok(Self {
            required_grants: grants,
            auth_scopes: collect_bounded_text(
                auth_scopes,
                MAX_AUTH_SCOPES,
                "auth scope",
                CatalogLimit::AuthScopes,
            )?,
            credential,
        })
    }

    pub const fn required_grants(&self) -> &BTreeSet<Grant> {
        &self.required_grants
    }

    pub const fn auth_scopes(&self) -> &BTreeSet<Arc<str>> {
        &self.auth_scopes
    }

    pub const fn credential(&self) -> Option<&SecretHandle> {
        self.credential.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    Degraded,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReliabilityStats {
    attempts: u64,
    succeeded: u64,
    failed: u64,
    cancelled: u64,
    outcome_unknown: u64,
}

impl ReliabilityStats {
    pub fn new(
        attempts: u64,
        succeeded: u64,
        failed: u64,
        cancelled: u64,
        outcome_unknown: u64,
    ) -> Result<Self, CatalogError> {
        validate_reliability(Self {
            attempts,
            succeeded,
            failed,
            cancelled,
            outcome_unknown,
        })?;
        Ok(Self {
            attempts,
            succeeded,
            failed,
            cancelled,
            outcome_unknown,
        })
    }

    pub const fn attempts(&self) -> u64 {
        self.attempts
    }

    pub const fn succeeded(&self) -> u64 {
        self.succeeded
    }

    pub const fn failed(&self) -> u64 {
        self.failed
    }

    pub const fn cancelled(&self) -> u64 {
        self.cancelled
    }

    pub const fn outcome_unknown(&self) -> u64 {
        self.outcome_unknown
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LatencyStats {
    Unobserved,
    Measured {
        samples: u64,
        minimum_micros: u64,
        maximum_micros: u64,
        total_micros: u64,
    },
}

impl LatencyStats {
    pub fn measured(
        samples: u64,
        minimum_micros: u64,
        maximum_micros: u64,
        total_micros: u64,
    ) -> Result<Self, CatalogError> {
        validate_measurement(samples, minimum_micros, maximum_micros, total_micros)?;
        Ok(Self::Measured {
            samples,
            minimum_micros,
            maximum_micros,
            total_micros,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CostStats {
    Unobserved,
    Measured {
        samples: u64,
        minimum: MoneyMicros,
        maximum: MoneyMicros,
        total: MoneyMicros,
    },
}

impl CostStats {
    pub fn measured(
        samples: u64,
        minimum: MoneyMicros,
        maximum: MoneyMicros,
        total: MoneyMicros,
    ) -> Result<Self, CatalogError> {
        let measured = Self::Measured {
            samples,
            minimum,
            maximum,
            total,
        };
        validate_cost(&measured)?;
        Ok(measured)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CollisionKey {
    namespace: CapabilityNamespace,
    name: CapabilityName,
    version: CapabilityVersion,
}

impl CollisionKey {
    pub fn new(
        namespace: CapabilityNamespace,
        name: CapabilityName,
        version: CapabilityVersion,
    ) -> Self {
        Self {
            namespace,
            name,
            version,
        }
    }

    pub fn from_identity(identity: &CapabilityIdentity) -> Self {
        Self::new(
            identity.namespace().clone(),
            identity.name().clone(),
            identity.version().clone(),
        )
    }

    pub const fn namespace(&self) -> &CapabilityNamespace {
        &self.namespace
    }

    pub const fn name(&self) -> &CapabilityName {
        &self.name
    }

    pub const fn version(&self) -> &CapabilityVersion {
        &self.version
    }
}

#[derive(Clone, Debug)]
pub struct CatalogEntry {
    identity: CapabilityIdentity,
    source: CatalogSource,
    kind: CapabilityKind,
    external_target: Option<ExternalTarget>,
    schemas: CatalogSchemas,
    search: CatalogSearch,
    digests: CatalogDigests,
    side_effects: SideEffects,
    authority: CatalogAuthority,
    availability: Availability,
    reliability: ReliabilityStats,
    latency: LatencyStats,
    cost: CostStats,
    payload_bytes: usize,
    digest: Digest,
}

impl CatalogEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identity: CapabilityIdentity,
        source: CatalogSource,
        kind: CapabilityKind,
        schemas: CatalogSchemas,
        search: CatalogSearch,
        side_effects: SideEffects,
        authority: CatalogAuthority,
        availability: Availability,
        reliability: ReliabilityStats,
        latency: LatencyStats,
        cost: CostStats,
    ) -> Result<Self, CatalogError> {
        Self::build(
            identity,
            source,
            kind,
            None,
            schemas,
            search,
            side_effects,
            authority,
            availability,
            reliability,
            latency,
            cost,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_external(
        identity: CapabilityIdentity,
        source: CatalogSource,
        kind: CapabilityKind,
        external_target: ExternalTarget,
        schemas: CatalogSchemas,
        search: CatalogSearch,
        side_effects: SideEffects,
        authority: CatalogAuthority,
        availability: Availability,
        reliability: ReliabilityStats,
        latency: LatencyStats,
        cost: CostStats,
    ) -> Result<Self, CatalogError> {
        Self::build(
            identity,
            source,
            kind,
            Some(external_target),
            schemas,
            search,
            side_effects,
            authority,
            availability,
            reliability,
            latency,
            cost,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        identity: CapabilityIdentity,
        source: CatalogSource,
        kind: CapabilityKind,
        external_target: Option<ExternalTarget>,
        schemas: CatalogSchemas,
        search: CatalogSearch,
        side_effects: SideEffects,
        authority: CatalogAuthority,
        availability: Availability,
        reliability: ReliabilityStats,
        latency: LatencyStats,
        cost: CostStats,
    ) -> Result<Self, CatalogError> {
        validate_identity(&identity)?;
        if identity.source() != source.id() {
            return Err(CatalogError::SourceMismatch);
        }
        validate_statistics(reliability, latency, &cost)?;
        if !authority
            .required_grants
            .contains(&side_effects.effect.required_grant())
        {
            return Err(CatalogError::AuthorityMismatch);
        }
        if external_target.as_ref().is_some_and(|target| {
            source.kind != SourceKind::Mcp
                || target.kind != kind
                || target.descriptor_digest != identity.implementation_digest()
        }) {
            return Err(CatalogError::AuthorityMismatch);
        }
        let digests = CatalogDigests {
            schema: schemas.digest(),
            implementation: identity.implementation_digest(),
        };
        let payload_bytes = catalog_entry_payload_bytes(
            &identity,
            &source,
            external_target.as_ref(),
            &schemas,
            &search,
            &authority,
            &cost,
        )?;
        let mut entry = Self {
            identity,
            source,
            kind,
            external_target,
            schemas,
            search,
            digests,
            side_effects,
            authority,
            availability,
            reliability,
            latency,
            cost,
            payload_bytes,
            digest: Digest::of(digests.schema.algorithm(), &[]),
        };
        let mut canonical = Vec::new();
        canonical.extend_from_slice(b"KIT-CATALOG-ENTRY\0");
        canonical.extend_from_slice(&CATALOG_FORMAT_VERSION.to_be_bytes());
        entry.write_canonical(&mut canonical);
        entry.digest = Digest::of(entry.digests.schema.algorithm(), &canonical);
        Ok(entry)
    }

    pub fn from_native(descriptor: &NativeToolDescriptor) -> Result<Self, CatalogError> {
        let identity = descriptor.identity().clone();
        let source = CatalogSource::new(
            SourceKind::Native,
            identity.source().clone(),
            TrustDomain::new("kit.native")?,
        )?;
        let output_value = descriptor
            .spec()
            .output_schema
            .as_ref()
            .ok_or(CatalogError::MissingOutputSchema)?;
        let output_source = serde_json::to_vec(output_value).map_err(|_| CatalogError::Schema)?;
        let output_docs = format!("Output schema for {}.", descriptor.canonical_name());
        let output = NormalizedSchema::ingest(
            output_source,
            JSON_SCHEMA_2020_12,
            output_docs.as_bytes(),
            DigestAlgorithm::Sha256,
        )
        .map_err(|_| CatalogError::Schema)?;
        let schemas = CatalogSchemas::new(
            SchemaProjectionSet::new(descriptor.normalized_schema().clone()),
            Some(SchemaProjectionSet::new(output)),
        );
        let search = native_search(descriptor)?;
        Self::new(
            identity,
            source,
            CapabilityKind::Tool,
            schemas,
            search,
            SideEffects::new(descriptor.effect(), descriptor.retry_safety()),
            CatalogAuthority::new(
                descriptor.required_grants().iter().copied(),
                Vec::<String>::new(),
            )?,
            Availability::Available,
            ReliabilityStats::default(),
            LatencyStats::Unobserved,
            CostStats::Unobserved,
        )
    }

    pub const fn identity(&self) -> &CapabilityIdentity {
        &self.identity
    }

    pub const fn source(&self) -> &CatalogSource {
        &self.source
    }

    pub const fn kind(&self) -> CapabilityKind {
        self.kind
    }

    pub const fn external_target(&self) -> Option<&ExternalTarget> {
        self.external_target.as_ref()
    }

    pub const fn schemas(&self) -> &CatalogSchemas {
        &self.schemas
    }

    pub const fn search(&self) -> &CatalogSearch {
        &self.search
    }

    pub const fn digests(&self) -> CatalogDigests {
        self.digests
    }

    pub const fn side_effects(&self) -> SideEffects {
        self.side_effects
    }

    pub const fn authority(&self) -> &CatalogAuthority {
        &self.authority
    }

    pub const fn availability(&self) -> Availability {
        self.availability
    }

    pub(crate) fn with_availability(
        &self,
        availability: Availability,
    ) -> Result<Self, CatalogError> {
        Self::build(
            self.identity.clone(),
            self.source.clone(),
            self.kind,
            self.external_target.clone(),
            self.schemas.clone(),
            self.search.clone(),
            self.side_effects,
            self.authority.clone(),
            availability,
            self.reliability,
            self.latency,
            self.cost.clone(),
        )
    }

    pub const fn reliability(&self) -> ReliabilityStats {
        self.reliability
    }

    pub const fn latency(&self) -> LatencyStats {
        self.latency
    }

    pub const fn cost(&self) -> &CostStats {
        &self.cost
    }

    pub const fn version(&self) -> &CapabilityVersion {
        self.identity.version()
    }

    pub const fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }

    pub const fn digest(&self) -> Digest {
        self.digest
    }

    pub fn collision_key(&self) -> CollisionKey {
        CollisionKey::from_identity(&self.identity)
    }

    fn write_canonical(&self, output: &mut Vec<u8>) {
        self.identity.write_canonical(output);
        output.push(source_kind_tag(self.source.kind));
        output.push(capability_kind_tag(self.kind));
        match &self.external_target {
            Some(target) => {
                output.push(1);
                put_bytes(output, target.configured_server.as_bytes());
                output.push(capability_kind_tag(target.kind));
                put_bytes(output, target.remote.as_bytes());
                put_digest(output, target.descriptor_digest);
            }
            None => output.push(0),
        }
        put_bytes(output, self.source.id.as_str().as_bytes());
        put_bytes(output, self.source.trust_domain.as_str().as_bytes());
        put_digest(output, self.schemas.digest);
        put_bytes(output, self.search.summary.as_bytes());
        write_text_set(output, &self.search.terms);
        output.push(self.side_effects.effect.tag());
        output.push(self.side_effects.retry_safety.tag());
        output.extend_from_slice(&(self.authority.required_grants.len() as u64).to_be_bytes());
        for grant in &self.authority.required_grants {
            output.push(grant.tag());
        }
        write_text_set(output, &self.authority.auth_scopes);
        match &self.authority.credential {
            Some(credential) => {
                output.push(1);
                put_bytes(output, credential.identifier().as_bytes());
            }
            None => output.push(0),
        }
        output.push(availability_tag(self.availability));
        output.extend_from_slice(&self.reliability.attempts.to_be_bytes());
        output.extend_from_slice(&self.reliability.succeeded.to_be_bytes());
        output.extend_from_slice(&self.reliability.failed.to_be_bytes());
        output.extend_from_slice(&self.reliability.cancelled.to_be_bytes());
        output.extend_from_slice(&self.reliability.outcome_unknown.to_be_bytes());
        write_latency(output, self.latency);
        write_cost(output, &self.cost);
    }
}

#[derive(Clone, Debug)]
pub struct CatalogSnapshot {
    entries: Arc<[Arc<CatalogEntry>]>,
    sources: Arc<BTreeMap<CapabilitySource, CatalogSource>>,
    digest: Digest,
}

impl CatalogSnapshot {
    pub fn new<I>(entries: I, algorithm: DigestAlgorithm) -> Result<Self, CatalogError>
    where
        I: IntoIterator<Item = CatalogEntry>,
    {
        Self::from_shared(
            entries.into_iter().map(Arc::new),
            algorithm,
            BTreeMap::new(),
        )
    }

    fn from_shared<I>(
        entries: I,
        algorithm: DigestAlgorithm,
        mut sources: BTreeMap<CapabilitySource, CatalogSource>,
    ) -> Result<Self, CatalogError>
    where
        I: IntoIterator<Item = Arc<CatalogEntry>>,
    {
        if sources.len() > MAX_CATALOG_SOURCES {
            return Err(CatalogError::LimitExceeded(CatalogLimit::Sources));
        }
        let mut by_key = BTreeMap::<CollisionKey, Arc<CatalogEntry>>::new();
        let mut count = 0_usize;
        let mut payload_bytes = 0_usize;
        for entry in entries {
            count = count
                .checked_add(1)
                .ok_or(CatalogError::LimitExceeded(CatalogLimit::Entries))?;
            if count > MAX_CATALOG_ENTRIES {
                return Err(CatalogError::LimitExceeded(CatalogLimit::Entries));
            }
            payload_bytes = payload_bytes
                .checked_add(entry.payload_bytes)
                .ok_or(CatalogError::LimitExceeded(CatalogLimit::PayloadBytes))?;
            if payload_bytes > MAX_CATALOG_PAYLOAD_BYTES {
                return Err(CatalogError::LimitExceeded(CatalogLimit::PayloadBytes));
            }
            if let Some(existing) = sources.get(entry.source.id())
                && existing != entry.source()
            {
                return Err(CatalogError::SourceConflict);
            }
            sources.insert(entry.source.id.clone(), entry.source.clone());
            if sources.len() > MAX_CATALOG_SOURCES {
                return Err(CatalogError::LimitExceeded(CatalogLimit::Sources));
            }
            let key = entry.collision_key();
            if let Some(existing) = by_key.get(&key) {
                return if existing.identity.source() == entry.identity.source() {
                    Err(CatalogError::DuplicateIdentity(key))
                } else {
                    Err(CatalogError::IdentityCollision(key))
                };
            }
            by_key.insert(key, entry);
        }
        let entries = by_key.into_values().collect::<Vec<_>>();
        let mut canonical = Vec::new();
        canonical.extend_from_slice(b"KIT-CATALOG-SNAPSHOT\0");
        canonical.extend_from_slice(&CATALOG_FORMAT_VERSION.to_be_bytes());
        canonical.extend_from_slice(&(sources.len() as u64).to_be_bytes());
        for source in sources.values() {
            canonical.push(source_kind_tag(source.kind));
            put_bytes(&mut canonical, source.id.as_str().as_bytes());
            put_bytes(&mut canonical, source.trust_domain.as_str().as_bytes());
        }
        canonical.extend_from_slice(&(entries.len() as u64).to_be_bytes());
        for entry in &entries {
            write_collision_key(&mut canonical, &entry.collision_key());
            put_digest(&mut canonical, entry.digest());
        }
        Ok(Self {
            entries: entries.into(),
            sources: Arc::new(sources),
            digest: Digest::of(algorithm, &canonical),
        })
    }

    pub fn from_native(algorithm: DigestAlgorithm) -> Result<Self, CatalogError> {
        Self::new(
            crate::capabilities::native::NativeCatalog::all()
                .iter()
                .map(CatalogEntry::from_native)
                .collect::<Result<Vec<_>, _>>()?,
            algorithm,
        )
    }

    pub fn entries(&self) -> &[Arc<CatalogEntry>] {
        &self.entries
    }

    pub(crate) fn filtered(
        &self,
        include: impl Fn(&CatalogEntry) -> bool,
    ) -> Result<Self, CatalogError> {
        Self::from_shared(
            self.entries.iter().filter(|entry| include(entry)).cloned(),
            self.digest.algorithm(),
            BTreeMap::new(),
        )
    }

    pub const fn digest(&self) -> Digest {
        self.digest
    }

    pub fn get(&self, key: &CollisionKey) -> Option<&CatalogEntry> {
        self.entries
            .binary_search_by(|entry| {
                entry
                    .identity
                    .namespace()
                    .cmp(key.namespace())
                    .then_with(|| entry.identity.name().cmp(key.name()))
                    .then_with(|| entry.identity.version().cmp(key.version()))
            })
            .ok()
            .map(|index| self.entries[index].as_ref())
    }

    pub fn get_identity(&self, identity: &CapabilityIdentity) -> Option<&CatalogEntry> {
        self.get(&CollisionKey::from_identity(identity))
            .filter(|entry| entry.identity() == identity)
    }

    pub fn replace_source<I>(
        &self,
        source: &CatalogSource,
        replacements: I,
    ) -> Result<Self, CatalogError>
    where
        I: IntoIterator<Item = CatalogEntry>,
    {
        let algorithm = self.digest.algorithm();
        if self
            .sources
            .get(source.id())
            .is_some_and(|bound| bound != source)
        {
            return Err(CatalogError::SourceConflict);
        }
        let mut sources = self.sources.as_ref().clone();
        sources.insert(source.id.clone(), source.clone());
        let mut entries = Vec::new();
        let mut payload_bytes = 0_usize;
        for entry in self.entries.iter().filter(|entry| entry.source() != source) {
            add_snapshot_capacity(&mut payload_bytes, entries.len(), entry.payload_bytes)?;
            entries.push(Arc::clone(entry));
        }
        for entry in replacements {
            if entry.source() != source {
                return Err(CatalogError::SourceMismatch);
            }
            add_snapshot_capacity(&mut payload_bytes, entries.len(), entry.payload_bytes)?;
            entries.push(Arc::new(entry));
        }
        Self::from_shared(entries, algorithm, sources)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogLimit {
    Entries,
    Sources,
    SearchTerms,
    AuthScopes,
    RequiredGrants,
    EntryPayloadBytes,
    PayloadBytes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogError {
    IdentityCollision(CollisionKey),
    DuplicateIdentity(CollisionKey),
    SourceMismatch,
    SourceConflict,
    AuthorityMismatch,
    InvalidText(&'static str),
    TextTooLong(&'static str),
    LimitExceeded(CatalogLimit),
    InvalidStatistics,
    InvalidCurrency,
    CurrencyMismatch,
    MissingOutputSchema,
    Schema,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentityCollision(_) => {
                formatter.write_str("capability identity collides across sources")
            }
            Self::DuplicateIdentity(_) => {
                formatter.write_str("capability identity is duplicated within a source")
            }
            Self::SourceMismatch => {
                formatter.write_str("entry identity source does not match catalog source")
            }
            Self::SourceConflict => {
                formatter.write_str("catalog source id has conflicting metadata")
            }
            Self::AuthorityMismatch => {
                formatter.write_str("catalog authority does not grant its side effect")
            }
            Self::InvalidText(field) => write!(
                formatter,
                "catalog {field} is empty or contains control characters"
            ),
            Self::TextTooLong(field) => write!(formatter, "catalog {field} exceeds its byte limit"),
            Self::LimitExceeded(limit) => write!(formatter, "catalog {limit:?} limit exceeded"),
            Self::InvalidStatistics => {
                formatter.write_str("catalog statistics are inconsistent or overflowed")
            }
            Self::InvalidCurrency => formatter.write_str("catalog cost currency is invalid"),
            Self::CurrencyMismatch => formatter.write_str("catalog cost currencies differ"),
            Self::MissingOutputSchema => {
                formatter.write_str("native capability has no output schema")
            }
            Self::Schema => formatter.write_str("catalog schema conversion failed"),
        }
    }
}

impl std::error::Error for CatalogError {}

fn collect_bounded_text<I, S>(
    values: I,
    limit: usize,
    field: &'static str,
    bound: CatalogLimit,
) -> Result<BTreeSet<Arc<str>>, CatalogError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut output = BTreeSet::new();
    let mut count = 0_usize;
    for value in values {
        count = count
            .checked_add(1)
            .ok_or(CatalogError::LimitExceeded(bound))?;
        if count > limit {
            return Err(CatalogError::LimitExceeded(bound));
        }
        let value = value.as_ref();
        validate_text(field, value, MAX_CATALOG_TEXT_BYTES)?;
        output.insert(Arc::from(value));
    }
    Ok(output)
}

fn catalog_entry_payload_bytes(
    identity: &CapabilityIdentity,
    source: &CatalogSource,
    external_target: Option<&ExternalTarget>,
    schemas: &CatalogSchemas,
    search: &CatalogSearch,
    authority: &CatalogAuthority,
    cost: &CostStats,
) -> Result<usize, CatalogError> {
    let mut total = 0_usize;
    for text in [
        identity.source().as_str(),
        identity.namespace().as_str(),
        identity.name().as_str(),
        identity.version().as_str(),
        source.id().as_str(),
        source.trust_domain().as_str(),
        search.summary(),
    ] {
        add_entry_bytes(&mut total, text.len())?;
    }
    for text in search.terms().iter().chain(authority.auth_scopes().iter()) {
        add_entry_bytes(&mut total, text.len())?;
    }
    if let Some(credential) = authority.credential() {
        add_entry_bytes(&mut total, credential.identifier().len())?;
    }
    if let Some(target) = external_target {
        add_entry_bytes(&mut total, target.configured_server.len())?;
        add_entry_bytes(&mut total, target.remote.len())?;
    }
    add_entry_bytes(&mut total, authority.required_grants().len())?;
    add_schema_set_bytes(&mut total, schemas.input())?;
    if let Some(output) = schemas.output() {
        add_schema_set_bytes(&mut total, output)?;
    }
    if let CostStats::Measured {
        minimum,
        maximum,
        total: cost_total,
        ..
    } = cost
    {
        for money in [minimum, maximum, cost_total] {
            add_entry_bytes(&mut total, money.currency.len())?;
        }
    }
    Ok(total)
}

fn add_schema_set_bytes(total: &mut usize, set: &SchemaProjectionSet) -> Result<(), CatalogError> {
    let schema = set.schema();
    let source = schema.source();
    for bytes in [
        source.source_bytes(),
        source.dialect().as_bytes(),
        source.documentation(),
        source.normalized_bytes(),
    ] {
        add_entry_bytes(total, bytes.len())?;
    }
    add_json_value_bytes(total, schema.value())?;
    for (target, _) in set.projections() {
        for text in [target.provider(), target.model(), target.adapter()] {
            add_entry_bytes(total, text.len())?;
        }
        add_entry_bytes(total, 2 + 2 * 33)?;
    }
    Ok(())
}

fn add_json_value_bytes(total: &mut usize, value: &serde_json::Value) -> Result<(), CatalogError> {
    add_entry_bytes(total, std::mem::size_of::<serde_json::Value>())?;
    match value {
        serde_json::Value::String(value) => add_entry_bytes(total, value.len()),
        serde_json::Value::Array(values) => {
            for value in values {
                add_json_value_bytes(total, value)?;
            }
            Ok(())
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                add_entry_bytes(total, key.len())?;
                add_json_value_bytes(total, value)?;
            }
            Ok(())
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            Ok(())
        }
    }
}

fn add_entry_bytes(total: &mut usize, amount: usize) -> Result<(), CatalogError> {
    *total = total
        .checked_add(amount)
        .ok_or(CatalogError::LimitExceeded(CatalogLimit::EntryPayloadBytes))?;
    if *total > MAX_CATALOG_ENTRY_PAYLOAD_BYTES {
        return Err(CatalogError::LimitExceeded(CatalogLimit::EntryPayloadBytes));
    }
    Ok(())
}

fn add_snapshot_capacity(
    payload_bytes: &mut usize,
    current_entries: usize,
    entry_bytes: usize,
) -> Result<(), CatalogError> {
    if current_entries >= MAX_CATALOG_ENTRIES {
        return Err(CatalogError::LimitExceeded(CatalogLimit::Entries));
    }
    *payload_bytes = payload_bytes
        .checked_add(entry_bytes)
        .ok_or(CatalogError::LimitExceeded(CatalogLimit::PayloadBytes))?;
    if *payload_bytes > MAX_CATALOG_PAYLOAD_BYTES {
        return Err(CatalogError::LimitExceeded(CatalogLimit::PayloadBytes));
    }
    Ok(())
}

fn validate_text(field: &'static str, value: &str, max: usize) -> Result<(), CatalogError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(CatalogError::InvalidText(field));
    }
    if value.len() > max {
        return Err(CatalogError::TextTooLong(field));
    }
    Ok(())
}

fn validate_identity(identity: &CapabilityIdentity) -> Result<(), CatalogError> {
    validate_text("source", identity.source().as_str(), MAX_CATALOG_TEXT_BYTES)?;
    validate_text(
        "namespace",
        identity.namespace().as_str(),
        MAX_CATALOG_TEXT_BYTES,
    )?;
    validate_text("name", identity.name().as_str(), MAX_CATALOG_TEXT_BYTES)?;
    validate_text(
        "version",
        identity.version().as_str(),
        MAX_CATALOG_TEXT_BYTES,
    )
}

fn validate_measurement(
    samples: u64,
    minimum: u64,
    maximum: u64,
    total: u64,
) -> Result<(), CatalogError> {
    let minimum_total = minimum
        .checked_mul(samples)
        .ok_or(CatalogError::InvalidStatistics)?;
    let maximum_total = maximum
        .checked_mul(samples)
        .ok_or(CatalogError::InvalidStatistics)?;
    if samples == 0
        || minimum > maximum
        || maximum > total
        || total < minimum_total
        || total > maximum_total
    {
        return Err(CatalogError::InvalidStatistics);
    }
    Ok(())
}

fn validate_reliability(reliability: ReliabilityStats) -> Result<(), CatalogError> {
    let terminal = reliability
        .succeeded
        .checked_add(reliability.failed)
        .and_then(|value| value.checked_add(reliability.cancelled))
        .and_then(|value| value.checked_add(reliability.outcome_unknown))
        .ok_or(CatalogError::InvalidStatistics)?;
    if reliability.attempts != terminal {
        return Err(CatalogError::InvalidStatistics);
    }
    Ok(())
}

fn validate_statistics(
    reliability: ReliabilityStats,
    latency: LatencyStats,
    cost: &CostStats,
) -> Result<(), CatalogError> {
    validate_reliability(reliability)?;
    if let LatencyStats::Measured {
        samples,
        minimum_micros,
        maximum_micros,
        total_micros,
    } = latency
    {
        validate_measurement(samples, minimum_micros, maximum_micros, total_micros)?;
        if samples > reliability.attempts {
            return Err(CatalogError::InvalidStatistics);
        }
    }
    validate_cost(cost)?;
    if let CostStats::Measured { samples, .. } = cost
        && *samples > reliability.attempts
    {
        return Err(CatalogError::InvalidStatistics);
    }
    Ok(())
}

fn validate_cost(cost: &CostStats) -> Result<(), CatalogError> {
    let CostStats::Measured {
        samples,
        minimum,
        maximum,
        total,
    } = cost
    else {
        return Ok(());
    };
    if [&minimum, &maximum, &total]
        .into_iter()
        .any(|money| !money.is_canonical())
    {
        return Err(CatalogError::InvalidCurrency);
    }
    if minimum.currency != maximum.currency || minimum.currency != total.currency {
        return Err(CatalogError::CurrencyMismatch);
    }
    validate_measurement(*samples, minimum.micros, maximum.micros, total.micros)
}

fn write_projection_set(output: &mut Vec<u8>, set: &SchemaProjectionSet) {
    let schema = set.schema();
    let source = schema.source();
    put_digest(output, source.source_digest());
    put_digest(output, schema.dialect_digest());
    put_digest(output, schema.documentation_digest());
    put_digest(output, source.normalized_digest());
    output.extend_from_slice(&(set.len() as u64).to_be_bytes());
    for (target, projection) in set.projections() {
        put_bytes(output, target.provider().as_bytes());
        put_bytes(output, target.model().as_bytes());
        put_bytes(output, target.adapter().as_bytes());
        output.extend_from_slice(&target.profile_version().to_be_bytes());
        put_digest(output, projection.profile_digest());
        put_digest(output, projection.digest());
    }
}

fn write_collision_key(output: &mut Vec<u8>, key: &CollisionKey) {
    put_bytes(output, key.namespace.as_str().as_bytes());
    put_bytes(output, key.name.as_str().as_bytes());
    put_bytes(output, key.version.as_str().as_bytes());
}

fn write_text_set(output: &mut Vec<u8>, values: &BTreeSet<Arc<str>>) {
    output.extend_from_slice(&(values.len() as u64).to_be_bytes());
    for value in values {
        put_bytes(output, value.as_bytes());
    }
}

fn write_latency(output: &mut Vec<u8>, latency: LatencyStats) {
    match latency {
        LatencyStats::Unobserved => output.push(0),
        LatencyStats::Measured {
            samples,
            minimum_micros,
            maximum_micros,
            total_micros,
        } => {
            output.push(1);
            output.extend_from_slice(&samples.to_be_bytes());
            output.extend_from_slice(&minimum_micros.to_be_bytes());
            output.extend_from_slice(&maximum_micros.to_be_bytes());
            output.extend_from_slice(&total_micros.to_be_bytes());
        }
    }
}

fn write_cost(output: &mut Vec<u8>, cost: &CostStats) {
    match cost {
        CostStats::Unobserved => output.push(0),
        CostStats::Measured {
            samples,
            minimum,
            maximum,
            total,
        } => {
            output.push(1);
            output.extend_from_slice(&samples.to_be_bytes());
            put_bytes(output, minimum.currency.as_bytes());
            output.extend_from_slice(&minimum.micros.to_be_bytes());
            output.extend_from_slice(&maximum.micros.to_be_bytes());
            output.extend_from_slice(&total.micros.to_be_bytes());
        }
    }
}

fn native_search(descriptor: &NativeToolDescriptor) -> Result<CatalogSearch, CatalogError> {
    let (summary, terms): (&str, &[&str]) = match descriptor.tool() {
        crate::capabilities::native::NativeTool::Discover => (
            "Discover ranked repository structure and relationships.",
            &["repository", "symbols", "relationships", "map"],
        ),
        crate::capabilities::native::NativeTool::Search => (
            "Search exact text or structural code patterns.",
            &["search", "text", "structural", "preview"],
        ),
        crate::capabilities::native::NativeTool::Read => (
            "Read a bounded file or range at a revision.",
            &["read", "file", "range", "revision"],
        ),
        crate::capabilities::native::NativeTool::Edit => (
            "Apply a transactional structured workspace patch.",
            &["edit", "patch", "workspace", "transaction"],
        ),
        crate::capabilities::native::NativeTool::Run => (
            "Run an explicit bounded process.",
            &["run", "process", "command", "argv"],
        ),
        crate::capabilities::native::NativeTool::Check => (
            "Run trusted verification profiles.",
            &["check", "test", "lint", "verification"],
        ),
    };
    CatalogSearch::new(summary, terms.iter().copied())
}

const fn source_kind_tag(kind: SourceKind) -> u8 {
    match kind {
        SourceKind::Native => 0,
        SourceKind::ProjectPlugin => 1,
        SourceKind::Mcp => 2,
        SourceKind::Acp => 3,
        SourceKind::A2a => 4,
        SourceKind::ProviderNative => 5,
    }
}

const fn capability_kind_tag(kind: CapabilityKind) -> u8 {
    match kind {
        CapabilityKind::Tool => 0,
        CapabilityKind::Resource => 1,
        CapabilityKind::ResourceTemplate => 2,
        CapabilityKind::Prompt => 3,
    }
}

const fn availability_tag(availability: Availability) -> u8 {
    match availability {
        Availability::Available => 0,
        Availability::Degraded => 1,
        Availability::Unavailable => 2,
    }
}
