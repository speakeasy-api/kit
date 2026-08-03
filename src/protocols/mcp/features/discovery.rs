use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use crate::{
    capabilities::{
        catalog::{
            Availability, CapabilityKind, CatalogAuthority, CatalogEntry, CatalogError,
            CatalogSearch, CatalogSnapshot, CatalogSource, CostStats, ExternalTarget, LatencyStats,
            MAX_CATALOG_ENTRIES, MAX_CATALOG_PAYLOAD_BYTES, MAX_SUMMARY_BYTES, ReliabilityStats,
            SideEffects, SourceKind,
        },
        kernel::{
            grant::EffectClass,
            identity::{
                CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilityVersion, Digest,
                DigestAlgorithm,
            },
            invoke::RetrySafety,
        },
        schema::JSON_SCHEMA_2020_12,
    },
    domain::config::Grant,
    domain::secret::SecretHandle,
};

use super::{
    ConfiguredServerIdentity, FeatureError, FeatureIdentity, FeaturePage, NormalizedFeature,
    PromptDescriptor, ResourceDescriptor, ResourceTemplateDescriptor, ToolDescriptor,
};

pub const MAX_CURSOR_BYTES: usize = 1024;
pub const MAX_DISCOVERY_PAGES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FeatureListKind {
    Tools,
    Resources,
    Prompts,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NegotiatedFeatureKinds {
    available: BTreeSet<FeatureListKind>,
    list_changed: BTreeMap<FeatureListKind, bool>,
}

impl NegotiatedFeatureKinds {
    pub fn new(kinds: impl IntoIterator<Item = FeatureListKind>) -> Self {
        Self {
            available: kinds.into_iter().collect(),
            list_changed: BTreeMap::new(),
        }
    }

    pub fn with_list_changed(
        kinds: impl IntoIterator<Item = FeatureListKind>,
        list_changed: impl IntoIterator<Item = FeatureListKind>,
    ) -> Self {
        let available = kinds.into_iter().collect::<BTreeSet<_>>();
        let list_changed = list_changed
            .into_iter()
            .filter(|kind| available.contains(kind))
            .map(|kind| (kind, true))
            .collect();
        Self {
            available,
            list_changed,
        }
    }

    pub fn with_list_changed_values(
        kinds: impl IntoIterator<Item = FeatureListKind>,
        list_changed: impl IntoIterator<Item = (FeatureListKind, bool)>,
    ) -> Self {
        let available = kinds.into_iter().collect::<BTreeSet<_>>();
        let list_changed = list_changed
            .into_iter()
            .filter(|(kind, _)| available.contains(kind))
            .collect();
        Self {
            available,
            list_changed,
        }
    }

    pub fn contains(&self, kind: FeatureListKind) -> bool {
        self.available.contains(&kind)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = FeatureListKind> + '_ {
        self.available.iter().copied()
    }

    pub fn supports_list_changed(&self, kind: FeatureListKind) -> bool {
        self.list_changed(kind) == Some(true)
    }

    pub fn list_changed(&self, kind: FeatureListKind) -> Option<bool> {
        self.list_changed.get(&kind).copied()
    }

    pub fn list_changed_iter(&self) -> impl Iterator<Item = FeatureListKind> + '_ {
        self.list_changed
            .iter()
            .filter_map(|(&kind, &enabled)| enabled.then_some(kind))
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveredFeatures {
    server: ConfiguredServerIdentity,
    negotiated: NegotiatedFeatureKinds,
    tools: Arc<[ToolDescriptor]>,
    resources: Arc<[ResourceDescriptor]>,
    resource_templates: Arc<[ResourceTemplateDescriptor]>,
    prompts: Arc<[PromptDescriptor]>,
    pages: BTreeMap<FeatureListKind, usize>,
    payload_bytes: BTreeMap<FeatureListKind, usize>,
}

impl DiscoveredFeatures {
    pub(crate) fn combine(
        server: ConfiguredServerIdentity,
        negotiated: NegotiatedFeatureKinds,
        parts: impl IntoIterator<Item = Self>,
    ) -> Result<Self, DiscoveryError> {
        let mut combined = Self {
            server,
            negotiated,
            tools: Arc::from([]),
            resources: Arc::from([]),
            resource_templates: Arc::from([]),
            prompts: Arc::from([]),
            pages: BTreeMap::new(),
            payload_bytes: BTreeMap::new(),
        };
        for part in parts {
            if part.server != combined.server {
                return Err(DiscoveryError::ServerMismatch);
            }
            for kind in part.negotiated.iter() {
                match kind {
                    FeatureListKind::Tools => combined.tools = Arc::clone(&part.tools),
                    FeatureListKind::Resources => {
                        combined.resources = Arc::clone(&part.resources);
                        combined.resource_templates = Arc::clone(&part.resource_templates);
                    }
                    FeatureListKind::Prompts => combined.prompts = Arc::clone(&part.prompts),
                }
                combined.pages.insert(kind, part.pages[&kind]);
                combined
                    .payload_bytes
                    .insert(kind, part.payload_bytes[&kind]);
            }
        }
        combined.validate_aggregate()?;
        Ok(combined)
    }

    pub fn from_pages(
        server: ConfiguredServerIdentity,
        negotiated: NegotiatedFeatureKinds,
        tools: Vec<FeaturePage<ToolDescriptor>>,
        resources: Vec<FeaturePage<ResourceDescriptor>>,
        resource_templates: Vec<FeaturePage<ResourceTemplateDescriptor>>,
        prompts: Vec<FeaturePage<PromptDescriptor>>,
    ) -> Result<Self, DiscoveryError> {
        let mut identities = BTreeSet::new();
        let mut total_pages = 0_usize;
        let mut total_entries = 0_usize;
        let mut total_bytes = 0_usize;
        let (tools, tool_pages, tool_bytes) = collect_pages(
            &server,
            negotiated.contains(FeatureListKind::Tools),
            tools,
            |item| item.identity(),
            |item| item.normalize(),
            &mut identities,
            &mut total_pages,
            &mut total_entries,
            &mut total_bytes,
        )?;
        let (resources, resource_pages, resource_bytes) = collect_pages(
            &server,
            negotiated.contains(FeatureListKind::Resources),
            resources,
            |item| item.identity(),
            |item| item.normalize(),
            &mut identities,
            &mut total_pages,
            &mut total_entries,
            &mut total_bytes,
        )?;
        let (resource_templates, template_pages, template_bytes) = collect_pages(
            &server,
            negotiated.contains(FeatureListKind::Resources),
            resource_templates,
            |item| item.identity(),
            |item| item.normalize(),
            &mut identities,
            &mut total_pages,
            &mut total_entries,
            &mut total_bytes,
        )?;
        let (prompts, prompt_pages, prompt_bytes) = collect_pages(
            &server,
            negotiated.contains(FeatureListKind::Prompts),
            prompts,
            |item| item.identity(),
            |item| item.normalize(),
            &mut identities,
            &mut total_pages,
            &mut total_entries,
            &mut total_bytes,
        )?;
        Ok(Self {
            server,
            negotiated,
            tools: tools.into(),
            resources: resources.into(),
            resource_templates: resource_templates.into(),
            prompts: prompts.into(),
            pages: BTreeMap::from([
                (FeatureListKind::Tools, tool_pages),
                (
                    FeatureListKind::Resources,
                    resource_pages
                        .checked_add(template_pages)
                        .ok_or(DiscoveryError::PageLimit)?,
                ),
                (FeatureListKind::Prompts, prompt_pages),
            ]),
            payload_bytes: BTreeMap::from([
                (FeatureListKind::Tools, tool_bytes),
                (
                    FeatureListKind::Resources,
                    resource_bytes
                        .checked_add(template_bytes)
                        .ok_or(DiscoveryError::PayloadLimit)?,
                ),
                (FeatureListKind::Prompts, prompt_bytes),
            ]),
        })
    }

    pub const fn server(&self) -> &ConfiguredServerIdentity {
        &self.server
    }

    pub const fn negotiated(&self) -> &NegotiatedFeatureKinds {
        &self.negotiated
    }

    pub fn tools(&self) -> &[ToolDescriptor] {
        &self.tools
    }

    pub fn resources(&self) -> &[ResourceDescriptor] {
        &self.resources
    }

    pub fn resource_templates(&self) -> &[ResourceTemplateDescriptor] {
        &self.resource_templates
    }

    pub fn prompts(&self) -> &[PromptDescriptor] {
        &self.prompts
    }

    pub fn page_count(&self) -> Option<usize> {
        self.pages
            .values()
            .try_fold(0_usize, |total, value| total.checked_add(*value))
    }

    pub fn payload_bytes(&self) -> Option<usize> {
        self.payload_bytes
            .values()
            .try_fold(0_usize, |total, value| total.checked_add(*value))
    }

    pub(crate) fn replace_kind(
        &self,
        kind: FeatureListKind,
        replacement: &Self,
    ) -> Result<Self, DiscoveryError> {
        if self.server != replacement.server
            || !self.negotiated.contains(kind)
            || !replacement.negotiated.contains(kind)
        {
            return Err(DiscoveryError::UnnegotiatedKind(kind));
        }
        let mut candidate = self.clone();
        match kind {
            FeatureListKind::Tools => candidate.tools = Arc::clone(&replacement.tools),
            FeatureListKind::Resources => {
                candidate.resources = Arc::clone(&replacement.resources);
                candidate.resource_templates = Arc::clone(&replacement.resource_templates);
            }
            FeatureListKind::Prompts => candidate.prompts = Arc::clone(&replacement.prompts),
        }
        candidate.pages.insert(kind, replacement.pages[&kind]);
        candidate
            .payload_bytes
            .insert(kind, replacement.payload_bytes[&kind]);
        candidate.validate_aggregate()?;
        Ok(candidate)
    }

    fn validate_aggregate(&self) -> Result<(), DiscoveryError> {
        if self
            .page_count()
            .is_none_or(|count| count > MAX_DISCOVERY_PAGES)
        {
            return Err(DiscoveryError::PageLimit);
        }
        if self
            .entry_count()
            .is_none_or(|count| count > MAX_CATALOG_ENTRIES)
        {
            return Err(DiscoveryError::EntryLimit);
        }
        if self
            .payload_bytes()
            .is_none_or(|bytes| bytes > MAX_CATALOG_PAYLOAD_BYTES)
        {
            return Err(DiscoveryError::PayloadLimit);
        }
        let mut identities = BTreeSet::new();
        for identity in self.identities() {
            if !identities.insert(identity) {
                return Err(DiscoveryError::DuplicateIdentity);
            }
        }
        Ok(())
    }

    fn entry_count(&self) -> Option<usize> {
        self.tools
            .len()
            .checked_add(self.resources.len())?
            .checked_add(self.resource_templates.len())?
            .checked_add(self.prompts.len())
    }

    fn identities(&self) -> impl Iterator<Item = FeatureIdentity> + '_ {
        self.tools
            .iter()
            .map(ToolDescriptor::identity)
            .chain(self.resources.iter().map(ResourceDescriptor::identity))
            .chain(
                self.resource_templates
                    .iter()
                    .map(ResourceTemplateDescriptor::identity),
            )
            .chain(self.prompts.iter().map(PromptDescriptor::identity))
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_pages<T>(
    server: &ConfiguredServerIdentity,
    negotiated: bool,
    pages: Vec<FeaturePage<T>>,
    identity: impl Fn(&T) -> FeatureIdentity,
    normalize: impl Fn(&T) -> Result<NormalizedFeature, FeatureError>,
    identities: &mut BTreeSet<FeatureIdentity>,
    total_pages: &mut usize,
    total_entries: &mut usize,
    total_bytes: &mut usize,
) -> Result<(Vec<T>, usize, usize), DiscoveryError> {
    if !negotiated {
        return if pages.is_empty() {
            Ok((Vec::new(), 0, 0))
        } else {
            Err(DiscoveryError::UnnegotiatedPayload)
        };
    }
    if pages.is_empty() {
        return Err(DiscoveryError::IncompletePagination);
    }
    let page_count = pages.len();
    let mut expected = None::<String>;
    let mut seen = BTreeSet::new();
    let mut payload_bytes = 0_usize;
    let mut output = Vec::new();
    let mut terminal = false;
    for page in pages {
        if terminal {
            return Err(DiscoveryError::PageAfterTerminal);
        }
        if page.server() != server {
            return Err(DiscoveryError::ServerMismatch);
        }
        *total_pages = total_pages
            .checked_add(1)
            .ok_or(DiscoveryError::PageLimit)?;
        if *total_pages > MAX_DISCOVERY_PAGES {
            return Err(DiscoveryError::PageLimit);
        }
        let page_bytes = page
            .payload()
            .accounted_bytes()
            .ok_or(DiscoveryError::PayloadLimit)?;
        payload_bytes = payload_bytes
            .checked_add(page_bytes)
            .ok_or(DiscoveryError::PayloadLimit)?;
        *total_bytes = total_bytes
            .checked_add(page_bytes)
            .ok_or(DiscoveryError::PayloadLimit)?;
        if *total_bytes > MAX_CATALOG_PAYLOAD_BYTES {
            return Err(DiscoveryError::PayloadLimit);
        }
        for item in page.items() {
            *total_entries = total_entries
                .checked_add(1)
                .ok_or(DiscoveryError::EntryLimit)?;
            if *total_entries > MAX_CATALOG_ENTRIES {
                return Err(DiscoveryError::EntryLimit);
            }
            normalize(item)?;
            if !identities.insert(identity(item)) {
                return Err(DiscoveryError::DuplicateIdentity);
            }
        }
        let next = validated_next_cursor(expected.as_deref(), page.next_cursor(), &mut seen)?;
        expected = next.clone();
        output.extend(page.into_items());
        if next.is_none() {
            terminal = true;
        }
    }
    if expected.is_some() {
        return Err(DiscoveryError::IncompletePagination);
    }
    Ok((output, page_count, payload_bytes))
}

pub(crate) fn validated_next_cursor(
    current: Option<&str>,
    next: Option<&str>,
    seen: &mut BTreeSet<String>,
) -> Result<Option<String>, DiscoveryError> {
    if let Some(current) = current {
        validate_cursor(current)?;
    }
    let Some(next) = next else {
        return Ok(None);
    };
    validate_cursor(next)?;
    if current == Some(next) || !seen.insert(next.to_owned()) {
        return Err(DiscoveryError::CursorCycle);
    }
    Ok(Some(next.to_owned()))
}

fn validate_cursor(cursor: &str) -> Result<(), DiscoveryError> {
    if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES || cursor.chars().any(char::is_control)
    {
        return Err(DiscoveryError::InvalidCursor);
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct McpCatalogPolicy {
    effect: EffectClass,
    retry_safety: RetrySafety,
    required_grants: BTreeSet<Grant>,
    auth_scopes: BTreeSet<String>,
    credential: Option<SecretHandle>,
    availability: Availability,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct McpCatalogPolicyKey {
    identity: FeatureIdentity,
    kind: CapabilityKind,
    descriptor_digest: Digest,
}

impl McpCatalogPolicyKey {
    pub const fn new(
        identity: FeatureIdentity,
        kind: CapabilityKind,
        descriptor_digest: Digest,
    ) -> Self {
        Self {
            identity,
            kind,
            descriptor_digest,
        }
    }

    pub const fn identity(&self) -> &FeatureIdentity {
        &self.identity
    }

    pub const fn kind(&self) -> CapabilityKind {
        self.kind
    }

    pub const fn descriptor_digest(&self) -> Digest {
        self.descriptor_digest
    }
}

impl McpCatalogPolicy {
    pub fn new<G, S, T>(
        effect: EffectClass,
        retry_safety: RetrySafety,
        required_grants: G,
        auth_scopes: S,
        availability: Availability,
    ) -> Self
    where
        G: IntoIterator<Item = Grant>,
        S: IntoIterator<Item = T>,
        T: Into<String>,
    {
        Self {
            effect,
            retry_safety,
            required_grants: required_grants.into_iter().collect(),
            auth_scopes: auth_scopes.into_iter().map(Into::into).collect(),
            credential: None,
            availability,
        }
    }

    pub fn with_credential(mut self, credential: SecretHandle) -> Self {
        self.credential = Some(credential);
        self
    }
}

#[derive(Clone, Debug)]
pub struct McpCatalogConfig {
    server: ConfiguredServerIdentity,
    source: CatalogSource,
    namespace: CapabilityNamespace,
    version: CapabilityVersion,
    policies: BTreeMap<McpCatalogPolicyKey, McpCatalogPolicy>,
}

impl McpCatalogConfig {
    pub fn new(
        server: ConfiguredServerIdentity,
        source: CatalogSource,
        namespace: CapabilityNamespace,
        version: CapabilityVersion,
        policies: BTreeMap<McpCatalogPolicyKey, McpCatalogPolicy>,
    ) -> Result<Self, DiscoveryError> {
        if source.kind() != SourceKind::Mcp
            || policies.keys().any(|key| {
                feature_server(key.identity()) != &server
                    || feature_kind(key.identity()) != key.kind()
            })
        {
            return Err(DiscoveryError::InvalidConfig);
        }
        Ok(Self {
            server,
            source,
            namespace,
            version,
            policies,
        })
    }

    pub const fn server(&self) -> &ConfiguredServerIdentity {
        &self.server
    }
}

#[derive(Clone, Debug)]
pub struct McpCatalog {
    snapshot: CatalogSnapshot,
    servers: BTreeMap<ConfiguredServerIdentity, (McpCatalogConfig, DiscoveredFeatures)>,
}

impl McpCatalog {
    pub fn new(snapshot: CatalogSnapshot) -> Self {
        Self {
            snapshot,
            servers: BTreeMap::new(),
        }
    }

    pub const fn snapshot(&self) -> &CatalogSnapshot {
        &self.snapshot
    }

    pub fn snapshot_owned(&self) -> CatalogSnapshot {
        self.snapshot.clone()
    }

    pub fn features(&self, server: &ConfiguredServerIdentity) -> Option<&DiscoveredFeatures> {
        self.servers.get(server).map(|(_, features)| features)
    }

    pub fn publish(
        &mut self,
        config: McpCatalogConfig,
        discovered: DiscoveredFeatures,
    ) -> Result<(), DiscoveryError> {
        if config.server != discovered.server {
            return Err(DiscoveryError::ServerMismatch);
        }
        if self
            .servers
            .get(&config.server)
            .is_some_and(|(prior, _)| prior.source != config.source)
            || self.servers.iter().any(|(server, (prior, _))| {
                server != &config.server && prior.source.id() == config.source.id()
            })
        {
            return Err(DiscoveryError::InvalidConfig);
        }
        let entries = catalog_entries(&config, &discovered)?;
        let snapshot = self.snapshot.replace_source(&config.source, entries)?;
        self.servers
            .insert(config.server.clone(), (config, discovered));
        self.snapshot = snapshot;
        Ok(())
    }

    pub fn remove(&mut self, server: &ConfiguredServerIdentity) -> Result<bool, DiscoveryError> {
        let Some((config, _)) = self.servers.get(server) else {
            return Ok(false);
        };
        let snapshot = self.snapshot.replace_source(&config.source, Vec::new())?;
        self.servers.remove(server);
        self.snapshot = snapshot;
        Ok(true)
    }

    pub(crate) fn mark_unavailable(
        &mut self,
        server: &ConfiguredServerIdentity,
    ) -> Result<bool, DiscoveryError> {
        let Some((config, _)) = self.servers.get(server) else {
            return Ok(false);
        };
        let entries = self
            .snapshot
            .entries()
            .iter()
            .filter(|entry| entry.source() == &config.source)
            .map(|entry| entry.with_availability(Availability::Unavailable))
            .collect::<Result<Vec<_>, _>>()?;
        self.snapshot = self.snapshot.replace_source(&config.source, entries)?;
        Ok(true)
    }

    pub fn refresh_kind(
        &mut self,
        server: &ConfiguredServerIdentity,
        kind: FeatureListKind,
        replacement: &DiscoveredFeatures,
    ) -> Result<(), DiscoveryError> {
        self.refresh_kind_until(server, kind, replacement, || false)
            .map(|_| ())
    }

    pub fn refresh_kind_until(
        &mut self,
        server: &ConfiguredServerIdentity,
        kind: FeatureListKind,
        replacement: &DiscoveredFeatures,
        cancelled: impl FnOnce() -> bool,
    ) -> Result<bool, DiscoveryError> {
        let (config, prior) = self
            .servers
            .get(server)
            .ok_or(DiscoveryError::UnknownServer)?;
        let candidate = prior.replace_kind(kind, replacement)?;
        let entries = catalog_entries(config, &candidate)?;
        let snapshot = self.snapshot.replace_source(&config.source, entries)?;
        if cancelled() {
            return Ok(false);
        }
        self.servers
            .insert(server.clone(), (config.clone(), candidate));
        self.snapshot = snapshot;
        Ok(true)
    }
}

fn catalog_entries(
    config: &McpCatalogConfig,
    discovered: &DiscoveredFeatures,
) -> Result<Vec<CatalogEntry>, DiscoveryError> {
    let mut entries =
        Vec::with_capacity(discovered.entry_count().ok_or(DiscoveryError::EntryLimit)?);
    for descriptor in discovered.tools.iter() {
        if let Some(entry) = catalog_entry(
            config,
            descriptor.normalize()?,
            descriptor.name(),
            descriptor.title(),
            descriptor.description(),
        )? {
            entries.push(entry);
        }
    }
    for descriptor in discovered.resources.iter() {
        if let Some(entry) = catalog_entry(
            config,
            descriptor.normalize()?,
            descriptor.name(),
            descriptor.title(),
            descriptor.description(),
        )? {
            entries.push(entry);
        }
    }
    for descriptor in discovered.resource_templates.iter() {
        if let Some(entry) = catalog_entry(
            config,
            descriptor.normalize()?,
            descriptor.name(),
            descriptor.title(),
            descriptor.description(),
        )? {
            entries.push(entry);
        }
    }
    for descriptor in discovered.prompts.iter() {
        if let Some(entry) = catalog_entry(
            config,
            descriptor.normalize()?,
            descriptor.name(),
            descriptor.title(),
            descriptor.description(),
        )? {
            entries.push(entry);
        }
    }
    Ok(entries)
}

fn catalog_entry(
    config: &McpCatalogConfig,
    feature: NormalizedFeature,
    name: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> Result<Option<CatalogEntry>, DiscoveryError> {
    let policy_key = McpCatalogPolicyKey::new(
        feature.identity().clone(),
        feature.kind(),
        feature.descriptor_digest(),
    );
    let Some(policy) = config.policies.get(&policy_key) else {
        return Ok(None);
    };
    let identity_name = capability_name(feature.identity());
    let identity = CapabilityIdentity::new(
        config.source.id().clone(),
        config.namespace.clone(),
        CapabilityName::new(identity_name).map_err(|_| DiscoveryError::InvalidConfig)?,
        config.version.clone(),
        feature.descriptor_digest(),
    );
    let supported = feature.input().source().dialect() == JSON_SCHEMA_2020_12
        && feature
            .output()
            .is_none_or(|schema| schema.source().dialect() == JSON_SCHEMA_2020_12);
    let target = ExternalTarget::mcp(
        feature_server(feature.identity()).as_str(),
        feature.kind(),
        feature_remote(feature.identity()),
        feature.descriptor_digest(),
    )?;
    CatalogEntry::new_external(
        identity,
        config.source.clone(),
        feature.kind(),
        target,
        feature.catalog_schemas(),
        CatalogSearch::new(
            sanitized_summary(description.or(title).unwrap_or(name)),
            [name, config.server.as_str()],
        )?,
        SideEffects::new(policy.effect, policy.retry_safety),
        CatalogAuthority::new_with_credential(
            policy.required_grants.iter().copied(),
            policy.auth_scopes.iter(),
            policy.credential.clone(),
        )?,
        if supported {
            policy.availability
        } else {
            Availability::Unavailable
        },
        ReliabilityStats::default(),
        LatencyStats::Unobserved,
        CostStats::Unobserved,
    )
    .map(Some)
    .map_err(Into::into)
}

fn feature_server(identity: &FeatureIdentity) -> &ConfiguredServerIdentity {
    match identity {
        FeatureIdentity::Tool(server, _)
        | FeatureIdentity::StaticResource(server, _)
        | FeatureIdentity::ResourceTemplate(server, _)
        | FeatureIdentity::Prompt(server, _) => server,
    }
}

fn feature_remote(identity: &FeatureIdentity) -> &str {
    match identity {
        FeatureIdentity::Tool(_, remote)
        | FeatureIdentity::StaticResource(_, remote)
        | FeatureIdentity::ResourceTemplate(_, remote)
        | FeatureIdentity::Prompt(_, remote) => remote,
    }
}

fn feature_kind(identity: &FeatureIdentity) -> CapabilityKind {
    match identity {
        FeatureIdentity::Tool(_, _) => CapabilityKind::Tool,
        FeatureIdentity::StaticResource(_, _) => CapabilityKind::Resource,
        FeatureIdentity::ResourceTemplate(_, _) => CapabilityKind::ResourceTemplate,
        FeatureIdentity::Prompt(_, _) => CapabilityKind::Prompt,
    }
}

fn capability_name(identity: &FeatureIdentity) -> String {
    let mut canonical = Vec::new();
    let kind = match identity {
        FeatureIdentity::Tool(server, value) => (b"tool".as_slice(), server, value),
        FeatureIdentity::StaticResource(server, value) => (b"resource".as_slice(), server, value),
        FeatureIdentity::ResourceTemplate(server, value) => {
            (b"resource-template".as_slice(), server, value)
        }
        FeatureIdentity::Prompt(server, value) => (b"prompt".as_slice(), server, value),
    };
    canonical.extend_from_slice(kind.0);
    canonical.push(0);
    canonical.extend_from_slice(kind.1.as_str().as_bytes());
    canonical.push(0);
    canonical.extend_from_slice(kind.2.as_bytes());
    format!(
        "mcp_{}_{}",
        String::from_utf8_lossy(kind.0),
        Digest::of(DigestAlgorithm::Sha256, &canonical).hex()
    )
}

fn sanitized_summary(value: &str) -> String {
    super::frame_untrusted_metadata(value, MAX_SUMMARY_BYTES)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefreshLimits {
    debounce: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl RefreshLimits {
    pub fn new(
        debounce: Duration,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Result<Self, DiscoveryError> {
        const MAX: Duration = Duration::from_secs(5 * 60);
        if debounce.is_zero()
            || debounce > MAX
            || initial_backoff.is_zero()
            || initial_backoff > max_backoff
            || max_backoff > MAX
        {
            return Err(DiscoveryError::InvalidRefreshLimits);
        }
        Ok(Self {
            debounce,
            initial_backoff,
            max_backoff,
        })
    }
}

impl Default for RefreshLimits {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(50),
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshTicket {
    server: ConfiguredServerIdentity,
    kind: FeatureListKind,
    generation: u64,
}

impl RefreshTicket {
    pub const fn server(&self) -> &ConfiguredServerIdentity {
        &self.server
    }

    pub const fn kind(&self) -> FeatureListKind {
        self.kind
    }
}

#[derive(Clone, Debug, Default)]
struct RefreshState {
    dirty: bool,
    active: bool,
    follow_up: bool,
    due_millis: u64,
    backoff_millis: u64,
    failures: u8,
    generation: u64,
}

#[derive(Clone, Debug)]
pub struct RefreshCoalescer {
    limits: RefreshLimits,
    servers: BTreeMap<ConfiguredServerIdentity, BTreeMap<FeatureListKind, RefreshState>>,
}

impl RefreshCoalescer {
    pub fn new(limits: RefreshLimits) -> Self {
        Self {
            limits,
            servers: BTreeMap::new(),
        }
    }

    pub fn notify(
        &mut self,
        server: ConfiguredServerIdentity,
        kind: FeatureListKind,
        negotiated: &NegotiatedFeatureKinds,
        now_millis: u64,
    ) -> bool {
        if !negotiated.supports_list_changed(kind) {
            return false;
        }
        let state = self
            .servers
            .entry(server)
            .or_default()
            .entry(kind)
            .or_default();
        if state.active {
            state.follow_up = true;
        } else if !state.dirty {
            state.dirty = true;
            state.due_millis = now_millis.saturating_add(duration_millis(self.limits.debounce));
        }
        true
    }

    pub fn mark_lagged(
        &mut self,
        server: ConfiguredServerIdentity,
        negotiated: &NegotiatedFeatureKinds,
        now_millis: u64,
    ) {
        for kind in negotiated.list_changed_iter() {
            self.notify(server.clone(), kind, negotiated, now_millis);
        }
    }

    pub fn take_ready(&mut self, now_millis: u64) -> Vec<RefreshTicket> {
        let mut tickets = Vec::new();
        for (server, kinds) in &mut self.servers {
            for (&kind, state) in kinds {
                if state.dirty && !state.active && state.due_millis <= now_millis {
                    state.dirty = false;
                    state.active = true;
                    state.generation = state.generation.wrapping_add(1);
                    tickets.push(RefreshTicket {
                        server: server.clone(),
                        kind,
                        generation: state.generation,
                    });
                }
            }
        }
        tickets
    }

    pub fn complete(&mut self, ticket: &RefreshTicket, succeeded: bool, now_millis: u64) {
        let Some(state) = self
            .servers
            .get_mut(&ticket.server)
            .and_then(|kinds| kinds.get_mut(&ticket.kind))
        else {
            return;
        };
        if !state.active || state.generation != ticket.generation {
            return;
        }
        state.active = false;
        if succeeded {
            state.backoff_millis = 0;
            state.failures = 0;
            if state.follow_up {
                state.follow_up = false;
                state.dirty = true;
                state.due_millis = now_millis.saturating_add(duration_millis(self.limits.debounce));
            }
        } else {
            state.failures = state.failures.saturating_add(1);
            state.follow_up = false;
            state.dirty = true;
            state.backoff_millis = if state.backoff_millis == 0 {
                duration_millis(self.limits.initial_backoff)
            } else {
                state
                    .backoff_millis
                    .saturating_mul(2)
                    .min(duration_millis(self.limits.max_backoff))
            };
            state.due_millis = now_millis.saturating_add(state.backoff_millis);
        }
    }

    pub fn failures(&self, ticket: &RefreshTicket) -> u8 {
        self.servers
            .get(ticket.server())
            .and_then(|kinds| kinds.get(&ticket.kind()))
            .filter(|state| state.generation == ticket.generation)
            .map_or(0, |state| state.failures)
    }

    pub fn pending_kinds(&self) -> usize {
        self.servers
            .values()
            .flat_map(BTreeMap::values)
            .filter(|state| state.dirty || state.follow_up)
            .count()
    }

    pub fn active_kinds(&self) -> usize {
        self.servers
            .values()
            .flat_map(BTreeMap::values)
            .filter(|state| state.active)
            .count()
    }

    pub fn next_due_millis(&self) -> Option<u64> {
        self.servers
            .values()
            .flat_map(BTreeMap::values)
            .filter(|state| state.dirty && !state.active)
            .map(|state| state.due_millis)
            .min()
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug)]
pub enum DiscoveryError {
    Feature(FeatureError),
    Catalog(CatalogError),
    InvalidCursor,
    CursorCycle,
    IncompletePagination,
    PageAfterTerminal,
    PageLimit,
    EntryLimit,
    PayloadLimit,
    DuplicateIdentity,
    UnnegotiatedPayload,
    UnnegotiatedKind(FeatureListKind),
    ServerMismatch,
    UnknownServer,
    InvalidConfig,
    InvalidRefreshLimits,
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Feature(error) => error.fmt(formatter),
            Self::Catalog(error) => error.fmt(formatter),
            Self::InvalidCursor => formatter.write_str("invalid MCP pagination cursor"),
            Self::CursorCycle => formatter.write_str("MCP pagination cursor did not progress"),
            Self::IncompletePagination => formatter.write_str("MCP pagination is incomplete"),
            Self::PageAfterTerminal => {
                formatter.write_str("MCP pagination continued after its terminal page")
            }
            Self::PageLimit => formatter.write_str("MCP discovery page limit exceeded"),
            Self::EntryLimit => formatter.write_str("MCP discovery entry limit exceeded"),
            Self::PayloadLimit => formatter.write_str("MCP discovery payload limit exceeded"),
            Self::DuplicateIdentity => formatter.write_str("MCP feature identity is duplicated"),
            Self::UnnegotiatedPayload => {
                formatter.write_str("MCP server returned an unnegotiated feature kind")
            }
            Self::UnnegotiatedKind(kind) => write!(formatter, "MCP {kind:?} was not negotiated"),
            Self::ServerMismatch => formatter.write_str("MCP discovery server identity changed"),
            Self::UnknownServer => formatter.write_str("MCP catalog server is unknown"),
            Self::InvalidConfig => formatter.write_str("MCP catalog configuration is invalid"),
            Self::InvalidRefreshLimits => formatter.write_str("MCP refresh limits are invalid"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

impl From<FeatureError> for DiscoveryError {
    fn from(value: FeatureError) -> Self {
        Self::Feature(value)
    }
}

impl From<CatalogError> for DiscoveryError {
    fn from(value: CatalogError) -> Self {
        Self::Catalog(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_features() -> DiscoveredFeatures {
        DiscoveredFeatures {
            server: ConfiguredServerIdentity::new("limit-test").unwrap(),
            negotiated: NegotiatedFeatureKinds::default(),
            tools: Arc::from([]),
            resources: Arc::from([]),
            resource_templates: Arc::from([]),
            prompts: Arc::from([]),
            pages: BTreeMap::new(),
            payload_bytes: BTreeMap::new(),
        }
    }

    #[test]
    fn aggregate_page_payload_and_cursor_bounds_fail_closed() {
        let mut pages = empty_features();
        pages
            .pages
            .insert(FeatureListKind::Tools, MAX_DISCOVERY_PAGES + 1);
        assert!(matches!(
            pages.validate_aggregate(),
            Err(DiscoveryError::PageLimit)
        ));

        let mut payload = empty_features();
        payload
            .payload_bytes
            .insert(FeatureListKind::Tools, MAX_CATALOG_PAYLOAD_BYTES + 1);
        assert!(matches!(
            payload.validate_aggregate(),
            Err(DiscoveryError::PayloadLimit)
        ));
        assert!(matches!(
            validate_cursor(&"x".repeat(MAX_CURSOR_BYTES + 1)),
            Err(DiscoveryError::InvalidCursor)
        ));
        assert!(matches!(
            validate_cursor("cursor\ncontrol"),
            Err(DiscoveryError::InvalidCursor)
        ));
    }
}
