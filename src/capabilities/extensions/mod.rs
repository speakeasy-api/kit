use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    agent::extensions::{
        CompatibilityRange, ContentDigest, ContractVersion, ExtensionIdentity, ExtensionReference,
        ExtensionVersion, TrustedExtensionToken,
    },
    api::auth::contract::{AuthenticatedPrincipal, Authorizer, ResourceScope, ScopedAuthorizer},
    capabilities::{
        broker::transport_auth::TransportAuthorization,
        kernel::identity::{Digest, DigestAlgorithm, put_bytes},
    },
    domain::{
        config::Grant,
        ids::{PrincipalId, ProjectId},
    },
    executor::profile::{ExecutionLabel, ExecutorProfile, MountAccess, MountRole},
};

pub const REGISTRY_FORMAT_VERSION: u16 = 1;
pub const CAPABILITY_EXTENSION_HOST_VERSION: ContractVersion = ContractVersion::new(1, 0, 0);
pub const MAX_EXTENSION_ENTRIES: usize = 4096;
pub const MAX_EXTENSION_ENTRIES_PER_PROJECT: usize = 256;
pub const MAX_EXTENSION_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_EXTENSION_PROJECT_SNAPSHOT_BYTES: usize = 512 * 1024;
pub const MAX_EXTENSION_TEXT_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionKind {
    NativeProvider,
    McpServer,
    SchemaProjectionAdapter,
}

impl ExtensionKind {
    pub const ALL: [Self; 3] = [
        Self::NativeProvider,
        Self::McpServer,
        Self::SchemaProjectionAdapter,
    ];
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustClassification {
    Trusted,
    Untrusted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionProtocol {
    Mcp,
    Acp,
    A2a,
    KitPluginV1,
}

impl ExtensionProtocol {
    pub const ALL: [Self; 4] = [Self::Mcp, Self::Acp, Self::A2a, Self::KitPluginV1];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Acp => "acp",
            Self::A2a => "a2a",
            Self::KitPluginV1 => "kit_plugin_v1",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionRoute {
    InProcess,
    OutOfProcess {
        protocol: ExtensionProtocol,
        route_id: String,
        sandbox_profile_digest: String,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionMetadata {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub vendor: Option<String>,
}

impl ExtensionMetadata {
    fn validate(&self) -> Result<(), RegistryError> {
        for value in [
            self.display_name.as_deref(),
            self.description.as_deref(),
            self.vendor.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_text(value)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionContract {
    format_version: u16,
    kind: ExtensionKind,
    identity: ExtensionIdentity,
    version: ExtensionVersion,
    schema_digest: ContentDigest,
    implementation_digest: ContentDigest,
    compatibility: CompatibilityRange,
    trust: TrustClassification,
    route: ExtensionRoute,
    metadata: ExtensionMetadata,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtensionContractSnapshot {
    format_version: u16,
    kind: ExtensionKind,
    identity: ExtensionIdentity,
    version: ExtensionVersion,
    schema_digest: ContentDigest,
    implementation_digest: ContentDigest,
    compatibility: CompatibilityRange,
    trust: TrustClassification,
    route: ExtensionRoute,
    metadata: ExtensionMetadata,
}

impl<'de> Deserialize<'de> for ExtensionContract {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ExtensionContractSnapshot::deserialize(deserializer)?;
        let contract = Self {
            format_version: value.format_version,
            kind: value.kind,
            identity: value.identity,
            version: value.version,
            schema_digest: value.schema_digest,
            implementation_digest: value.implementation_digest,
            compatibility: value.compatibility,
            trust: value.trust,
            route: value.route,
            metadata: value.metadata,
        };
        contract.validate().map_err(serde::de::Error::custom)?;
        Ok(contract)
    }
}

impl ExtensionContract {
    #[allow(clippy::too_many_arguments)]
    pub fn untrusted(
        kind: ExtensionKind,
        identity: ExtensionIdentity,
        version: ExtensionVersion,
        schema_digest: ContentDigest,
        implementation_digest: ContentDigest,
        compatibility: CompatibilityRange,
        protocol: ExtensionProtocol,
        route_id: impl Into<String>,
        sandbox_profile_digest: impl Into<String>,
        metadata: ExtensionMetadata,
    ) -> Result<Self, RegistryError> {
        Self::new(
            kind,
            identity,
            version,
            schema_digest,
            implementation_digest,
            compatibility,
            TrustClassification::Untrusted,
            ExtensionRoute::OutOfProcess {
                protocol,
                route_id: route_id.into(),
                sandbox_profile_digest: sandbox_profile_digest.into(),
            },
            metadata,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn trusted(
        _token: &TrustedExtensionToken,
        kind: ExtensionKind,
        identity: ExtensionIdentity,
        version: ExtensionVersion,
        schema_digest: ContentDigest,
        implementation_digest: ContentDigest,
        compatibility: CompatibilityRange,
        metadata: ExtensionMetadata,
    ) -> Result<Self, RegistryError> {
        Self::new(
            kind,
            identity,
            version,
            schema_digest,
            implementation_digest,
            compatibility,
            TrustClassification::Trusted,
            ExtensionRoute::InProcess,
            metadata,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        kind: ExtensionKind,
        identity: ExtensionIdentity,
        version: ExtensionVersion,
        schema_digest: ContentDigest,
        implementation_digest: ContentDigest,
        compatibility: CompatibilityRange,
        trust: TrustClassification,
        route: ExtensionRoute,
        metadata: ExtensionMetadata,
    ) -> Result<Self, RegistryError> {
        let contract = Self {
            format_version: REGISTRY_FORMAT_VERSION,
            kind,
            identity,
            version,
            schema_digest,
            implementation_digest,
            compatibility,
            trust,
            route,
            metadata,
        };
        contract.validate()?;
        Ok(contract)
    }

    pub const fn kind(&self) -> ExtensionKind {
        self.kind
    }
    pub fn identity(&self) -> &ExtensionIdentity {
        &self.identity
    }
    pub fn version(&self) -> &ExtensionVersion {
        &self.version
    }
    pub fn schema_digest(&self) -> &ContentDigest {
        &self.schema_digest
    }
    pub fn implementation_digest(&self) -> &ContentDigest {
        &self.implementation_digest
    }
    pub const fn compatibility(&self) -> CompatibilityRange {
        self.compatibility
    }
    pub const fn trust(&self) -> TrustClassification {
        self.trust
    }
    pub const fn route(&self) -> &ExtensionRoute {
        &self.route
    }
    pub const fn metadata(&self) -> &ExtensionMetadata {
        &self.metadata
    }

    pub fn reference(&self) -> ExtensionReference {
        ExtensionReference::new(self.identity.clone(), self.version.clone())
    }

    pub fn canonical_identity(&self) -> ContentDigest {
        ContentDigest::sha256(&self.canonical_bytes())
    }

    pub(crate) fn schema_kernel_digest(&self) -> Digest {
        self.schema_digest
            .as_str()
            .parse()
            .expect("validated SHA-256 digest")
    }

    pub(crate) fn implementation_kernel_digest(&self) -> Digest {
        self.implementation_digest
            .as_str()
            .parse()
            .expect("validated SHA-256 digest")
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"KIT-CAPABILITY-EXTENSION\0");
        bytes.extend_from_slice(&self.format_version.to_be_bytes());
        bytes.push(kind_tag(self.kind));
        put_bytes(&mut bytes, self.identity.as_str().as_bytes());
        put_bytes(&mut bytes, self.version.as_str().as_bytes());
        put_bytes(&mut bytes, self.schema_digest.as_str().as_bytes());
        put_bytes(&mut bytes, self.implementation_digest.as_str().as_bytes());
        put_version(&mut bytes, self.compatibility.minimum);
        put_version(&mut bytes, self.compatibility.maximum_exclusive);
        bytes.push(match self.trust {
            TrustClassification::Trusted => 0,
            TrustClassification::Untrusted => 1,
        });
        match &self.route {
            ExtensionRoute::InProcess => bytes.push(0),
            ExtensionRoute::OutOfProcess {
                protocol,
                route_id,
                sandbox_profile_digest,
            } => {
                bytes.push(1);
                bytes.push(protocol_tag(*protocol));
                put_bytes(&mut bytes, route_id.as_bytes());
                put_bytes(&mut bytes, sandbox_profile_digest.as_bytes());
            }
        }
        for value in [
            self.metadata.display_name.as_deref(),
            self.metadata.description.as_deref(),
            self.metadata.vendor.as_deref(),
        ] {
            match value {
                Some(value) => {
                    bytes.push(1);
                    put_bytes(&mut bytes, value.as_bytes());
                }
                None => bytes.push(0),
            }
        }
        bytes
    }

    fn validate(&self) -> Result<(), RegistryError> {
        if self.format_version != REGISTRY_FORMAT_VERSION {
            return Err(RegistryError::UnsupportedFormatVersion(self.format_version));
        }
        if self.compatibility.minimum >= self.compatibility.maximum_exclusive
            || !self
                .compatibility
                .contains(CAPABILITY_EXTENSION_HOST_VERSION)
        {
            return Err(RegistryError::IncompatibleContract(self.reference()));
        }
        self.metadata.validate()?;
        match (&self.trust, &self.route) {
            (TrustClassification::Trusted, ExtensionRoute::InProcess) => Ok(()),
            (
                TrustClassification::Untrusted,
                ExtensionRoute::OutOfProcess {
                    route_id,
                    sandbox_profile_digest,
                    ..
                },
            ) => {
                validate_text(route_id)?;
                validate_digest(sandbox_profile_digest)
            }
            (TrustClassification::Untrusted, ExtensionRoute::InProcess) => {
                Err(RegistryError::OutOfProcessRequired(self.reference()))
            }
            (TrustClassification::Trusted, ExtensionRoute::OutOfProcess { .. }) => {
                Err(RegistryError::TrustRouteMismatch)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionScope {
    principal_id: PrincipalId,
    project_id: ProjectId,
}

impl ExtensionScope {
    pub const fn new(principal_id: PrincipalId, project_id: ProjectId) -> Self {
        Self {
            principal_id,
            project_id,
        }
    }
    pub const fn principal_id(self) -> PrincipalId {
        self.principal_id
    }
    pub const fn project_id(self) -> ProjectId {
        self.project_id
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExtensionState {
    Active,
    Revoked,
    Superseded { by: ExtensionReference },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryEntry {
    scope: ExtensionScope,
    contract: ExtensionContract,
    state: ExtensionState,
}

impl RegistryEntry {
    pub const fn scope(&self) -> ExtensionScope {
        self.scope
    }
    pub const fn contract(&self) -> &ExtensionContract {
        &self.contract
    }
    pub const fn state(&self) -> &ExtensionState {
        &self.state
    }

    fn validate(&self) -> Result<(), RegistryError> {
        self.contract.validate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationOutcome {
    Inserted,
    Existing,
    /// A trusted daemon-bootstrap registration replaced a stored entry whose
    /// contract digests no longer match the current build (in-place upgrade of
    /// a built-in extension). Everything else keeps `ContractConflict`.
    Superseded,
}

/// Durable audit record for one in-place upgrade of a built-in extension
/// contract (`capability.extension.upgraded`). The registry revision bump
/// persists the new contract; this record carries the digest transition for
/// the structured audit log emitted by the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionUpgradeAudit {
    pub reference: ExtensionReference,
    pub old_schema_digest: ContentDigest,
    pub new_schema_digest: ContentDigest,
    pub old_implementation_digest: ContentDigest,
    pub new_implementation_digest: ContentDigest,
}

type RegistryKey = (ExtensionScope, ExtensionReference);

pub type SharedCapabilityExtensionRegistry = Arc<RwLock<CapabilityExtensionRegistry>>;

#[derive(Debug)]
struct ExtensionLifecycle {
    active: AtomicBool,
    changed: tokio::sync::watch::Sender<bool>,
}

impl ExtensionLifecycle {
    fn active() -> Arc<Self> {
        let (changed, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            active: AtomicBool::new(true),
            changed,
        })
    }

    fn revoke(&self) {
        self.active.store(false, Ordering::Release);
        self.changed.send_replace(true);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ExtensionLifecycleGuard {
    registry: SharedCapabilityExtensionRegistry,
    scope: ExtensionScope,
    reference: ExtensionReference,
    kind: ExtensionKind,
    trust: TrustClassification,
    lifecycle: Arc<ExtensionLifecycle>,
}

impl ExtensionLifecycleGuard {
    pub(crate) fn ensure_current(&self) -> Result<(), RegistryError> {
        if !self.lifecycle.active.load(Ordering::Acquire) {
            return Err(RegistryError::Revoked(self.reference.clone()));
        }
        let registry = self
            .registry
            .read()
            .map_err(|_| RegistryError::Unavailable)?;
        let entry = registry.active_entry(
            self.scope,
            &self.reference,
            CAPABILITY_EXTENSION_HOST_VERSION,
        )?;
        if entry.contract.kind != self.kind || entry.contract.trust != self.trust {
            return Err(RegistryError::ContractConflict(self.reference.clone()));
        }
        Ok(())
    }

    pub(crate) fn cancellation(&self) -> tokio::sync::watch::Receiver<bool> {
        self.lifecycle.changed.subscribe()
    }
}

#[derive(Clone)]
pub(crate) struct NativeExtensionGuard {
    registry: SharedCapabilityExtensionRegistry,
    scope: ExtensionScope,
    reference: ExtensionReference,
}

impl NativeExtensionGuard {
    pub(crate) fn new(
        registry: SharedCapabilityExtensionRegistry,
        scope: ExtensionScope,
    ) -> Result<Self, RegistryError> {
        let reference = built_in_contracts()
            .into_iter()
            .find(|contract| contract.kind == ExtensionKind::NativeProvider)
            .expect("native provider contract exists")
            .reference();
        registry
            .read()
            .map_err(|_| RegistryError::Unavailable)?
            .resolve_trusted(
                &TrustedExtensionToken::daemon_bootstrap(),
                scope,
                &reference,
                CAPABILITY_EXTENSION_HOST_VERSION,
            )?;
        Ok(Self {
            registry,
            scope,
            reference,
        })
    }

    pub(crate) fn ensure_current(&self) -> Result<(), RegistryError> {
        self.registry
            .read()
            .map_err(|_| RegistryError::Unavailable)?
            .resolve_trusted(
                &TrustedExtensionToken::daemon_bootstrap(),
                self.scope,
                &self.reference,
                CAPABILITY_EXTENSION_HOST_VERSION,
            )?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn attest_native_extension(
    registry: &SharedCapabilityExtensionRegistry,
    scope: ExtensionScope,
) -> Result<NativeExtensionGuard, RegistryError> {
    let token = TrustedExtensionToken::daemon_bootstrap();
    {
        let mut current = registry.write().map_err(|_| RegistryError::Unavailable)?;
        for contract in built_in_contracts() {
            current.register_trusted(&token, scope, contract)?;
        }
    }
    NativeExtensionGuard::new(Arc::clone(registry), scope)
}

pub(crate) fn attest_native_extension_durable(
    registry: &SharedCapabilityExtensionRegistry,
    scope: ExtensionScope,
    store: &mut crate::store::sqlite::append::SqliteStore,
) -> Result<(NativeExtensionGuard, Vec<ExtensionUpgradeAudit>), RegistryError> {
    let token = TrustedExtensionToken::daemon_bootstrap();
    let mut current = registry.write().map_err(|_| RegistryError::Unavailable)?;
    let upgrades = loop {
        current.reconcile(store)?;
        let mut candidate = current.clone();
        let mut mutated = false;
        let mut upgrades = Vec::new();
        for contract in built_in_contracts() {
            let key = (scope, contract.reference());
            let previous = candidate
                .entries
                .get(&key)
                .map(|entry| entry.contract.clone());
            match candidate.register_trusted(&token, scope, contract)? {
                RegistrationOutcome::Inserted => mutated = true,
                RegistrationOutcome::Superseded => {
                    mutated = true;
                    let old = previous.expect("superseded entry existed");
                    let new = candidate
                        .entries
                        .get(&key)
                        .expect("superseding entry is stored")
                        .contract
                        .clone();
                    upgrades.push(ExtensionUpgradeAudit {
                        reference: key.1,
                        old_schema_digest: old.schema_digest().clone(),
                        new_schema_digest: new.schema_digest().clone(),
                        old_implementation_digest: old.implementation_digest().clone(),
                        new_implementation_digest: new.implementation_digest().clone(),
                    });
                }
                RegistrationOutcome::Existing => {}
            }
        }
        if !mutated {
            current.commit_candidate(candidate);
            break Vec::new();
        }
        if CapabilityExtensionRegistry::persist_candidate(&mut current, candidate, scope, store)? {
            break upgrades;
        }
    };
    drop(current);
    Ok((
        NativeExtensionGuard::new(Arc::clone(registry), scope)?,
        upgrades,
    ))
}

#[derive(Debug, Default)]
pub struct CapabilityExtensionRegistry {
    entries: BTreeMap<RegistryKey, RegistryEntry>,
    revisions: BTreeMap<ExtensionScope, u64>,
    revision: u64,
    // Runtime attestation is deliberately excluded from every persisted form.
    trusted_active: BTreeSet<RegistryKey>,
    // Runtime lifecycle signals are deliberately excluded from every persisted form.
    lifecycle: BTreeMap<RegistryKey, Arc<ExtensionLifecycle>>,
}

impl Clone for CapabilityExtensionRegistry {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            revisions: self.revisions.clone(),
            revision: self.revision,
            trusted_active: self.trusted_active.clone(),
            lifecycle: self
                .entries
                .iter()
                .filter(|(_, entry)| matches!(entry.state, ExtensionState::Active))
                .map(|(key, _)| (key.clone(), ExtensionLifecycle::active()))
                .collect(),
        }
    }
}

impl CapabilityExtensionRegistry {
    pub fn register_untrusted(
        &mut self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
        contract: ExtensionContract,
    ) -> Result<RegistrationOutcome, RegistryError> {
        authorize_project_mutation(authenticated, project_id)?;
        if contract.trust != TrustClassification::Untrusted {
            return Err(RegistryError::TrustedAttestationRequired);
        }
        let scope = ExtensionScope::new(authenticated.principal_id(), project_id);
        self.insert(
            RegistryEntry {
                scope,
                contract,
                state: ExtensionState::Active,
            },
            false,
        )
    }

    pub(crate) fn register_trusted(
        &mut self,
        token: &TrustedExtensionToken,
        scope: ExtensionScope,
        contract: ExtensionContract,
    ) -> Result<RegistrationOutcome, RegistryError> {
        if contract.trust != TrustClassification::Trusted
            || !trusted_build_allowlist(token).contains(&contract.canonical_identity())
        {
            return Err(RegistryError::TrustedAttestationRequired);
        }
        self.insert(
            RegistryEntry {
                scope,
                contract,
                state: ExtensionState::Active,
            },
            true,
        )
    }

    pub fn contract(
        &self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
        reference: &ExtensionReference,
    ) -> Result<&RegistryEntry, RegistryError> {
        authorize_project(authenticated, project_id)?;
        self.entries
            .get(&(
                ExtensionScope::new(authenticated.principal_id(), project_id),
                reference.clone(),
            ))
            .ok_or_else(|| RegistryError::UnknownExtension(reference.clone()))
    }

    pub fn entries_for_project<'a>(
        &'a self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
    ) -> Result<impl ExactSizeIterator<Item = &'a RegistryEntry>, RegistryError> {
        authorize_project(authenticated, project_id)?;
        Ok(self
            .entries
            .values()
            .filter(move |entry| {
                entry.scope == ExtensionScope::new(authenticated.principal_id(), project_id)
            })
            .collect::<Vec<_>>()
            .into_iter())
    }

    pub fn trusted_attested(
        &self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
        reference: &ExtensionReference,
    ) -> Result<bool, RegistryError> {
        authorize_project(authenticated, project_id)?;
        Ok(self.trusted_active.contains(&(
            ExtensionScope::new(authenticated.principal_id(), project_id),
            reference.clone(),
        )))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn authorize_mcp_route(
        &self,
        scope: ExtensionScope,
        reference: &ExtensionReference,
        request: &crate::capabilities::broker::BrokerInvocation<'_>,
        transport: &str,
        endpoint: &str,
        sandbox_profile_digest: &str,
        profile: Option<&ExecutorProfile>,
        store: &mut crate::store::sqlite::append::SqliteStore,
    ) -> Result<(), RegistryError> {
        let entry = self.active_entry(scope, reference, CAPABILITY_EXTENSION_HOST_VERSION)?;
        let ExtensionRoute::OutOfProcess { route_id, .. } = &entry.contract.route else {
            return Err(RegistryError::OutOfProcessRequired(reference.clone()));
        };
        let operation =
            crate::capabilities::broker::transport_auth::TransportOperation::parse("initialize")
                .map_err(|_| RegistryError::RouteMismatch)?;
        let binding = crate::capabilities::broker::transport_auth::TransportBinding::new(
            request, route_id, transport, endpoint, None,
        );
        let authorization = crate::capabilities::broker::transport_auth::authorize(
            request, &operation, &binding, store,
        )
        .map_err(|_| RegistryError::RouteNotAuthorized)?;
        self.resolve_authorization(
            scope,
            reference,
            CAPABILITY_EXTENSION_HOST_VERSION,
            &authorization,
            sandbox_profile_digest,
            profile,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn authorize_mcp_route_durable(
        shared: &SharedCapabilityExtensionRegistry,
        scope: ExtensionScope,
        reference: &ExtensionReference,
        request: &crate::capabilities::broker::BrokerInvocation<'_>,
        transport: &str,
        endpoint: &str,
        sandbox_profile_digest: &str,
        profile: Option<&ExecutorProfile>,
        store: &mut crate::store::sqlite::append::SqliteStore,
    ) -> Result<(), RegistryError> {
        let mut registry = shared.write().map_err(|_| RegistryError::Unavailable)?;
        registry.reconcile(store)?;
        registry.authorize_mcp_route(
            scope,
            reference,
            request,
            transport,
            endpoint,
            sandbox_profile_digest,
            profile,
            store,
        )
    }

    pub fn ensure_untrusted_active(
        &self,
        scope: ExtensionScope,
        reference: &ExtensionReference,
    ) -> Result<(), RegistryError> {
        let entry = self.active_entry(scope, reference, CAPABILITY_EXTENSION_HOST_VERSION)?;
        if entry.contract.trust != TrustClassification::Untrusted {
            return Err(RegistryError::OutOfProcessRequired(reference.clone()));
        }
        Ok(())
    }

    pub fn ensure_untrusted_active_durable(
        shared: &SharedCapabilityExtensionRegistry,
        scope: ExtensionScope,
        reference: &ExtensionReference,
        store: &mut crate::store::sqlite::append::SqliteStore,
    ) -> Result<(), RegistryError> {
        let mut registry = shared.write().map_err(|_| RegistryError::Unavailable)?;
        registry.reconcile(store)?;
        registry.ensure_untrusted_active(scope, reference)
    }

    pub(crate) fn untrusted_lifecycle_guard(
        shared: &SharedCapabilityExtensionRegistry,
        scope: ExtensionScope,
        reference: &ExtensionReference,
    ) -> Result<ExtensionLifecycleGuard, RegistryError> {
        Self::lifecycle_guard(
            shared,
            scope,
            reference,
            ExtensionKind::McpServer,
            TrustClassification::Untrusted,
        )
    }

    pub(crate) fn untrusted_lifecycle_guard_durable(
        shared: &SharedCapabilityExtensionRegistry,
        scope: ExtensionScope,
        reference: &ExtensionReference,
        store: &mut crate::store::sqlite::append::SqliteStore,
    ) -> Result<ExtensionLifecycleGuard, RegistryError> {
        {
            let mut registry = shared.write().map_err(|_| RegistryError::Unavailable)?;
            registry.reconcile(store)?;
        }
        Self::untrusted_lifecycle_guard(shared, scope, reference)
    }

    pub(crate) fn schema_projection_lifecycle_guard(
        shared: &SharedCapabilityExtensionRegistry,
        scope: ExtensionScope,
    ) -> Result<ExtensionLifecycleGuard, RegistryError> {
        let reference = built_in_contracts()
            .into_iter()
            .find(|contract| contract.kind == ExtensionKind::SchemaProjectionAdapter)
            .expect("schema projection contract exists")
            .reference();
        Self::lifecycle_guard(
            shared,
            scope,
            &reference,
            ExtensionKind::SchemaProjectionAdapter,
            TrustClassification::Trusted,
        )
    }

    fn lifecycle_guard(
        shared: &SharedCapabilityExtensionRegistry,
        scope: ExtensionScope,
        reference: &ExtensionReference,
        kind: ExtensionKind,
        trust: TrustClassification,
    ) -> Result<ExtensionLifecycleGuard, RegistryError> {
        let mut registry = shared.write().map_err(|_| RegistryError::Unavailable)?;
        let entry = registry.active_entry(scope, reference, CAPABILITY_EXTENSION_HOST_VERSION)?;
        if entry.contract.kind != kind || entry.contract.trust != trust {
            return Err(RegistryError::ContractConflict(reference.clone()));
        }
        if trust == TrustClassification::Trusted
            && !registry
                .trusted_active
                .contains(&(scope, reference.clone()))
        {
            return Err(RegistryError::TrustedAttestationRequired);
        }
        let lifecycle = registry
            .lifecycle
            .entry((scope, reference.clone()))
            .or_insert_with(ExtensionLifecycle::active)
            .clone();
        Ok(ExtensionLifecycleGuard {
            registry: Arc::clone(shared),
            scope,
            reference: reference.clone(),
            kind,
            trust,
            lifecycle,
        })
    }

    fn resolve_authorization<'a>(
        &'a self,
        scope: ExtensionScope,
        reference: &ExtensionReference,
        host_version: ContractVersion,
        authorization: &TransportAuthorization,
        sandbox_profile_digest: &str,
        profile: Option<&ExecutorProfile>,
    ) -> Result<&'a RegistryEntry, RegistryError> {
        let entry = self.active_entry(scope, reference, host_version)?;
        let ExtensionRoute::OutOfProcess {
            protocol,
            route_id,
            sandbox_profile_digest: expected_profile,
        } = &entry.contract.route
        else {
            return Err(RegistryError::OutOfProcessRequired(reference.clone()));
        };
        if entry.contract.trust != TrustClassification::Untrusted
            || expected_profile != sandbox_profile_digest
            || !authorization.matches_extension_route(
                scope.principal_id.to_string().as_str(),
                scope.project_id.to_string().as_str(),
                protocol.as_str(),
                route_id,
            )
            || !authorization.matches_contract_digests(
                entry.contract.schema_digest.as_str(),
                entry.contract.implementation_digest.as_str(),
            )
        {
            return Err(RegistryError::RouteMismatch);
        }
        if let Some(profile) = profile {
            validate_sandbox(profile, authorization)?;
        } else if !authorization.is_brokered_egress_only() {
            return Err(RegistryError::SandboxRequired);
        }
        Ok(entry)
    }

    pub(crate) fn resolve_trusted<'a>(
        &'a self,
        _token: &TrustedExtensionToken,
        scope: ExtensionScope,
        reference: &ExtensionReference,
        host_version: ContractVersion,
    ) -> Result<&'a RegistryEntry, RegistryError> {
        let entry = self.active_entry(scope, reference, host_version)?;
        if entry.contract.trust != TrustClassification::Trusted
            || !self.trusted_active.contains(&(scope, reference.clone()))
        {
            return Err(RegistryError::TrustedAttestationRequired);
        }
        Ok(entry)
    }

    pub fn revoke(
        &mut self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
        reference: &ExtensionReference,
    ) -> Result<(), RegistryError> {
        authorize_project_mutation(authenticated, project_id)?;
        let scope = ExtensionScope::new(authenticated.principal_id(), project_id);
        let key = (scope, reference.clone());
        let mut candidate = self.clone();
        let entry = candidate
            .entries
            .get_mut(&key)
            .ok_or_else(|| RegistryError::UnknownExtension(reference.clone()))?;
        entry.state = ExtensionState::Revoked;
        candidate.trusted_active.remove(&key);
        candidate.validate_all()?;
        self.commit_candidate(candidate);
        Ok(())
    }

    pub fn supersede(
        &mut self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
        extension: &ExtensionReference,
        replacement: &ExtensionReference,
    ) -> Result<(), RegistryError> {
        authorize_project_mutation(authenticated, project_id)?;
        let scope = ExtensionScope::new(authenticated.principal_id(), project_id);
        if extension == replacement {
            return Err(RegistryError::SelfSupersede);
        }
        let source_key = (scope, extension.clone());
        let target_key = (scope, replacement.clone());
        if matches!(self.entries.get(&source_key).map(|entry| &entry.state), Some(ExtensionState::Superseded { by }) if by == replacement)
        {
            return Ok(());
        }
        let replacement_entry = self
            .entries
            .get(&target_key)
            .ok_or_else(|| RegistryError::UnknownExtension(replacement.clone()))?;
        if !matches!(replacement_entry.state, ExtensionState::Active) {
            return Err(RegistryError::InactiveReplacement(replacement.clone()));
        }
        let source = self
            .entries
            .get(&source_key)
            .ok_or_else(|| RegistryError::UnknownExtension(extension.clone()))?;
        if source.contract.kind != replacement_entry.contract.kind
            || source.scope.project_id != replacement_entry.scope.project_id
            || source.contract.compatibility != replacement_entry.contract.compatibility
        {
            return Err(RegistryError::IncompatibleReplacement);
        }
        let mut candidate = self.clone();
        candidate
            .entries
            .get_mut(&source_key)
            .expect("checked above")
            .state = ExtensionState::Superseded {
            by: replacement.clone(),
        };
        candidate.trusted_active.remove(&source_key);
        candidate.validate_all()?;
        self.commit_candidate(candidate);
        Ok(())
    }

    pub fn canonical_project_bytes(
        &self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
    ) -> Result<Vec<u8>, RegistryError> {
        authorize_project(authenticated, project_id)?;
        self.scope_bytes(ExtensionScope::new(
            authenticated.principal_id(),
            project_id,
        ))
    }

    pub fn project_digest(
        &self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
    ) -> Result<Digest, RegistryError> {
        Ok(Digest::of(
            DigestAlgorithm::Sha256,
            &self.canonical_project_bytes(authenticated, project_id)?,
        ))
    }

    pub fn from_project_bytes(
        authenticated: &AuthenticatedPrincipal,
        bytes: &[u8],
    ) -> Result<Self, RegistryError> {
        if bytes.len() > MAX_EXTENSION_PROJECT_SNAPSHOT_BYTES {
            return Err(RegistryError::LimitExceeded);
        }
        let snapshot: RegistrySnapshot =
            serde_json::from_slice(bytes).map_err(|_| RegistryError::InvalidSnapshot)?;
        if snapshot.format_version != REGISTRY_FORMAT_VERSION {
            return Err(RegistryError::InvalidSnapshot);
        }
        if snapshot.principal_id != authenticated.principal_id() {
            return Err(RegistryError::ProjectUnauthorized);
        }
        authorize_project_mutation(authenticated, snapshot.project_id)?;
        let mut registry = Self::default();
        let scope = ExtensionScope::new(snapshot.principal_id, snapshot.project_id);
        registry.revisions.insert(scope, snapshot.revision);
        registry.revision = snapshot.revision;
        for entry in snapshot.entries {
            if entry.scope != scope {
                return Err(RegistryError::InvalidSnapshot);
            }
            entry.validate()?;
            let key = (entry.scope, entry.contract.reference());
            if registry.entries.insert(key, entry).is_some() {
                return Err(RegistryError::InvalidSnapshot);
            }
        }
        registry.validate_all()?;
        if registry.scope_bytes(scope)? != bytes {
            return Err(RegistryError::NonCanonicalSnapshot);
        }
        Ok(registry)
    }

    pub(crate) fn from_repository_snapshots(
        snapshots: impl IntoIterator<Item = (u64, Vec<u8>)>,
    ) -> Result<Self, RegistryError> {
        let mut registry = Self::default();
        for (stored_revision, bytes) in snapshots {
            if bytes.len() > MAX_EXTENSION_PROJECT_SNAPSHOT_BYTES {
                return Err(RegistryError::LimitExceeded);
            }
            let snapshot: RegistrySnapshot =
                serde_json::from_slice(&bytes).map_err(|_| RegistryError::InvalidSnapshot)?;
            let scope = ExtensionScope::new(snapshot.principal_id, snapshot.project_id);
            if snapshot.format_version != REGISTRY_FORMAT_VERSION
                || snapshot.revision == 0
                || snapshot.revision != stored_revision
                || snapshot.entries.iter().any(|entry| entry.scope != scope)
            {
                return Err(RegistryError::InvalidSnapshot);
            }
            if registry
                .revisions
                .insert(scope, snapshot.revision)
                .is_some()
            {
                return Err(RegistryError::InvalidSnapshot);
            }
            registry.revision = registry.revision.max(snapshot.revision);
            for entry in snapshot.entries {
                entry.validate()?;
                let key = (entry.scope, entry.contract.reference());
                if registry.entries.insert(key, entry).is_some() {
                    return Err(RegistryError::InvalidSnapshot);
                }
            }
            registry.validate_all()?;
            if registry.scope_bytes(scope)? != bytes {
                return Err(RegistryError::NonCanonicalSnapshot);
            }
        }
        Ok(registry)
    }

    pub fn register_untrusted_durable(
        shared: &SharedCapabilityExtensionRegistry,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
        contract: ExtensionContract,
        store: &mut crate::store::sqlite::append::SqliteStore,
    ) -> Result<RegistrationOutcome, RegistryError> {
        let mut current = shared.write().map_err(|_| RegistryError::Unavailable)?;
        let scope = ExtensionScope::new(authenticated.principal_id(), project_id);
        loop {
            current.reconcile(store)?;
            let mut candidate = current.clone();
            let outcome =
                candidate.register_untrusted(authenticated, project_id, contract.clone())?;
            if outcome == RegistrationOutcome::Existing {
                return Ok(outcome);
            }
            if Self::persist_candidate(&mut current, candidate, scope, store)? {
                return Ok(outcome);
            }
        }
    }

    pub fn revoke_durable(
        shared: &SharedCapabilityExtensionRegistry,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
        reference: &ExtensionReference,
        store: &mut crate::store::sqlite::append::SqliteStore,
    ) -> Result<(), RegistryError> {
        let mut current = shared.write().map_err(|_| RegistryError::Unavailable)?;
        let scope = ExtensionScope::new(authenticated.principal_id(), project_id);
        loop {
            current.reconcile(store)?;
            let before = current.scope_entries(scope);
            let mut candidate = current.clone();
            candidate.revoke(authenticated, project_id, reference)?;
            if candidate.scope_entries(scope) == before
                || Self::persist_candidate(&mut current, candidate, scope, store)?
            {
                return Ok(());
            }
        }
    }

    pub fn supersede_durable(
        shared: &SharedCapabilityExtensionRegistry,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
        extension: &ExtensionReference,
        replacement: &ExtensionReference,
        store: &mut crate::store::sqlite::append::SqliteStore,
    ) -> Result<(), RegistryError> {
        let mut current = shared.write().map_err(|_| RegistryError::Unavailable)?;
        let scope = ExtensionScope::new(authenticated.principal_id(), project_id);
        loop {
            current.reconcile(store)?;
            let before = current.scope_entries(scope);
            let mut candidate = current.clone();
            candidate.supersede(authenticated, project_id, extension, replacement)?;
            if candidate.scope_entries(scope) == before
                || Self::persist_candidate(&mut current, candidate, scope, store)?
            {
                return Ok(());
            }
        }
    }

    fn persist_candidate(
        current: &mut Self,
        mut candidate: Self,
        scope: ExtensionScope,
        store: &mut crate::store::sqlite::append::SqliteStore,
    ) -> Result<bool, RegistryError> {
        let expected = current.revision;
        let revision = expected
            .checked_add(1)
            .ok_or(RegistryError::LimitExceeded)?;
        candidate.revision = revision;
        candidate.revisions.insert(scope, revision);
        let bytes = candidate.scope_bytes(scope)?;
        let entry_count = candidate
            .entries
            .values()
            .filter(|entry| entry.scope == scope)
            .count();
        match store
            .persist_extension_registry_snapshot(
                scope.principal_id,
                scope.project_id,
                expected,
                revision,
                &bytes,
                entry_count,
                MAX_EXTENSION_ENTRIES,
                MAX_EXTENSION_SNAPSHOT_BYTES,
            )
            .map_err(|error| RegistryError::Persistence(error.to_string()))?
        {
            crate::store::sqlite::append::ExtensionRegistryCommit::Committed => {
                current.commit_candidate(candidate);
                Ok(true)
            }
            crate::store::sqlite::append::ExtensionRegistryCommit::Stale => {
                current.reconcile(store)?;
                Ok(false)
            }
            crate::store::sqlite::append::ExtensionRegistryCommit::LimitExceeded => {
                Err(RegistryError::LimitExceeded)
            }
        }
    }

    fn reconcile(
        &mut self,
        store: &mut crate::store::sqlite::append::SqliteStore,
    ) -> Result<(), RegistryError> {
        let (revision, snapshots) = store
            .extension_registry_state()
            .map_err(|error| RegistryError::Persistence(error.to_string()))?;
        if revision == self.revision {
            return Ok(());
        }
        let mut restored = Self::from_repository_snapshots(snapshots)?;
        if restored.revision != revision {
            return Err(RegistryError::InvalidSnapshot);
        }
        restored.trusted_active = self
            .trusted_active
            .iter()
            .filter(|key| {
                self.entries.get(*key) == restored.entries.get(*key)
                    && restored
                        .entries
                        .get(*key)
                        .is_some_and(|entry| matches!(entry.state, ExtensionState::Active))
            })
            .cloned()
            .collect();
        self.commit_candidate(restored);
        Ok(())
    }

    fn scope_entries(&self, scope: ExtensionScope) -> Vec<RegistryEntry> {
        self.entries
            .values()
            .filter(|entry| entry.scope == scope)
            .cloned()
            .collect()
    }

    fn active_entry(
        &self,
        scope: ExtensionScope,
        reference: &ExtensionReference,
        host_version: ContractVersion,
    ) -> Result<&RegistryEntry, RegistryError> {
        let entry = self
            .entries
            .get(&(scope, reference.clone()))
            .ok_or_else(|| RegistryError::UnknownExtension(reference.clone()))?;
        match &entry.state {
            ExtensionState::Active => {}
            ExtensionState::Revoked => return Err(RegistryError::Revoked(reference.clone())),
            ExtensionState::Superseded { by } => {
                return Err(RegistryError::Superseded {
                    extension: reference.clone(),
                    by: by.clone(),
                });
            }
        }
        if !entry.contract.compatibility.contains(host_version) {
            return Err(RegistryError::IncompatibleContract(reference.clone()));
        }
        Ok(entry)
    }

    fn insert(
        &mut self,
        entry: RegistryEntry,
        activate_trusted: bool,
    ) -> Result<RegistrationOutcome, RegistryError> {
        entry.validate()?;
        let key = (entry.scope, entry.contract.reference());
        if let Some(existing) = self.entries.get(&key) {
            if existing != &entry {
                // A rebuild of a built-in extension changes its schema or
                // implementation digest without changing its reference. Only
                // the trusted daemon-bootstrap path (`activate_trusted` is set
                // exclusively by token-gated, build-allowlisted
                // `register_trusted`) may supersede the stored entry, and only
                // when the contract stays the same kind and trust
                // classification and does not downgrade. Everything else keeps
                // the hard conflict.
                if activate_trusted && built_in_upgrade_allowed(existing, &entry) {
                    let mut candidate = self.clone();
                    candidate.entries.insert(key.clone(), entry);
                    candidate.trusted_active.insert(key);
                    candidate.validate_all()?;
                    // `commit_candidate` revokes the superseded entry's
                    // lifecycle, so in-flight guards minted under the old
                    // contract are cancelled, never mixed with the new one.
                    self.commit_candidate(candidate);
                    return Ok(RegistrationOutcome::Superseded);
                }
                return Err(RegistryError::ContractConflict(entry.contract.reference()));
            }
            if activate_trusted {
                self.trusted_active.insert(key);
            }
            return Ok(RegistrationOutcome::Existing);
        }
        // A trusted registration must not register below an already stored
        // newer version of the same built-in (an old binary against a state
        // root touched by a newer kit refuses to boot instead of downgrading).
        if activate_trusted
            && self.entries.iter().any(|((scope, reference), existing)| {
                *scope == entry.scope
                    && reference.identity == key.1.identity
                    && reference.version > key.1.version
                    && existing.contract.kind == entry.contract.kind
                    && existing.contract.trust == entry.contract.trust
                    && matches!(existing.state, ExtensionState::Active)
            })
        {
            return Err(RegistryError::ContractConflict(entry.contract.reference()));
        }
        let mut candidate = self.clone();
        candidate.entries.insert(key.clone(), entry);
        if activate_trusted {
            candidate.trusted_active.insert(key);
        }
        candidate.validate_all()?;
        self.commit_candidate(candidate);
        Ok(RegistrationOutcome::Inserted)
    }

    fn commit_candidate(&mut self, mut candidate: Self) {
        for (key, lifecycle) in &self.lifecycle {
            // Revoke when an active entry stops being active or changes in any
            // way (an in-place contract upgrade must cancel guards minted
            // under the previous contract).
            if self
                .entries
                .get(key)
                .is_some_and(|entry| matches!(entry.state, ExtensionState::Active))
                && candidate.entries.get(key) != self.entries.get(key)
            {
                lifecycle.revoke();
            }
        }
        candidate.lifecycle = candidate
            .entries
            .iter()
            .filter(|(_, entry)| matches!(entry.state, ExtensionState::Active))
            .map(|(key, _)| {
                (
                    key.clone(),
                    self.lifecycle
                        .get(key)
                        .filter(|lifecycle| lifecycle.active.load(Ordering::Acquire))
                        .cloned()
                        .unwrap_or_else(ExtensionLifecycle::active),
                )
            })
            .collect();
        *self = candidate;
    }

    fn scope_bytes(&self, scope: ExtensionScope) -> Result<Vec<u8>, RegistryError> {
        serde_json::to_vec(&RegistrySnapshot {
            format_version: REGISTRY_FORMAT_VERSION,
            revision: self.revisions.get(&scope).copied().unwrap_or(0),
            principal_id: scope.principal_id,
            project_id: scope.project_id,
            entries: self
                .entries
                .values()
                .filter(|entry| entry.scope == scope)
                .cloned()
                .collect(),
        })
        .map_err(|_| RegistryError::InvalidSnapshot)
    }

    fn validate_all(&self) -> Result<(), RegistryError> {
        if self.entries.len() > MAX_EXTENSION_ENTRIES {
            return Err(RegistryError::LimitExceeded);
        }
        let scopes = self
            .entries
            .values()
            .map(|entry| entry.scope)
            .collect::<BTreeSet<_>>();
        let mut global_bytes = 0_usize;
        for scope in scopes {
            let count = self
                .entries
                .values()
                .filter(|entry| entry.scope == scope)
                .count();
            let project_bytes = self.scope_bytes(scope)?.len();
            global_bytes = global_bytes
                .checked_add(project_bytes)
                .ok_or(RegistryError::LimitExceeded)?;
            if count > MAX_EXTENSION_ENTRIES_PER_PROJECT
                || project_bytes > MAX_EXTENSION_PROJECT_SNAPSHOT_BYTES
            {
                return Err(RegistryError::ProjectLimitExceeded(scope.project_id));
            }
        }
        if global_bytes > MAX_EXTENSION_SNAPSHOT_BYTES {
            return Err(RegistryError::LimitExceeded);
        }
        for (key, entry) in &self.entries {
            entry.validate()?;
            if let ExtensionState::Superseded { by } = &entry.state {
                let target = self
                    .entries
                    .get(&(entry.scope, by.clone()))
                    .ok_or(RegistryError::InvalidLifecycleGraph)?;
                if target.contract.kind != entry.contract.kind
                    || !matches!(target.state, ExtensionState::Active)
                    || key.1 == *by
                {
                    return Err(RegistryError::InvalidLifecycleGraph);
                }
            }
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RegistrySnapshot {
    format_version: u16,
    revision: u64,
    principal_id: PrincipalId,
    project_id: ProjectId,
    entries: Vec<RegistryEntry>,
}

pub fn built_in_contracts() -> [ExtensionContract; 2] {
    let token = TrustedExtensionToken::daemon_bootstrap();
    [
        ExtensionContract::trusted(
            &token,
            ExtensionKind::NativeProvider,
            ExtensionIdentity::parse("kit.native-provider").expect("static identity"),
            ExtensionVersion::parse("1.0.0").expect("static version"),
            canonical_schema_digest(include_bytes!(
                "../../../docs/compatibility/schemas/native-provider-v1.json"
            )),
            implementation_merkle(&[
                (
                    "src/capabilities/native/catalog.rs",
                    include_bytes!("../native/catalog.rs"),
                ),
                (
                    "src/capabilities/native/dispatch.rs",
                    include_bytes!("../native/dispatch.rs"),
                ),
                (
                    "src/capabilities/native/orchestrate.rs",
                    include_bytes!("../native/orchestrate.rs"),
                ),
                (
                    "src/capabilities/registration/mod.rs",
                    include_bytes!("../registration/mod.rs"),
                ),
            ]),
            CompatibilityRange::new(
                CAPABILITY_EXTENSION_HOST_VERSION,
                ContractVersion::new(2, 0, 0),
            ),
            ExtensionMetadata {
                display_name: Some("Kit native capability provider".to_owned()),
                description: None,
                vendor: Some("Kit".to_owned()),
            },
        )
        .expect("static native provider contract"),
        ExtensionContract::trusted(
            &token,
            ExtensionKind::SchemaProjectionAdapter,
            ExtensionIdentity::parse("kit.schema-projection").expect("static identity"),
            ExtensionVersion::parse("1.0.0").expect("static version"),
            canonical_schema_digest(include_bytes!(
                "../../../docs/compatibility/schemas/schema-projection-v1.json"
            )),
            implementation_merkle(&[(
                "src/capabilities/schema/mod.rs",
                include_bytes!("../schema/mod.rs"),
            )]),
            CompatibilityRange::new(
                CAPABILITY_EXTENSION_HOST_VERSION,
                ContractVersion::new(2, 0, 0),
            ),
            ExtensionMetadata {
                display_name: Some("Kit schema projection adapter".to_owned()),
                description: None,
                vendor: Some("Kit".to_owned()),
            },
        )
        .expect("static schema projection contract"),
    ]
}

fn built_in_upgrade_allowed(existing: &RegistryEntry, incoming: &RegistryEntry) -> bool {
    matches!(existing.state, ExtensionState::Active)
        && existing.contract.kind == incoming.contract.kind
        && existing.contract.trust == incoming.contract.trust
        && incoming.contract.version >= existing.contract.version
}

fn trusted_build_allowlist(token: &TrustedExtensionToken) -> BTreeSet<ContentDigest> {
    let _ = token;
    built_in_contracts()
        .into_iter()
        .map(|contract| contract.canonical_identity())
        .collect()
}

pub fn implementation_merkle(files: &[(&str, &[u8])]) -> ContentDigest {
    let mut canonical = Vec::new();
    canonical.extend_from_slice(b"KIT-EXTENSION-IMPLEMENTATION-MERKLE\0");
    for (path, bytes) in files {
        put_bytes(&mut canonical, path.as_bytes());
        put_bytes(
            &mut canonical,
            ContentDigest::sha256(bytes).as_str().as_bytes(),
        );
    }
    ContentDigest::sha256(&canonical)
}

pub fn canonical_schema_digest(schema: &[u8]) -> ContentDigest {
    let value: serde_json::Value =
        serde_json::from_slice(schema).expect("built-in schema is valid JSON");
    ContentDigest::sha256(&serde_json::to_vec(&value).expect("JSON value is serializable"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryError {
    UnsupportedFormatVersion(u16),
    InvalidText,
    InvalidDigest,
    LimitExceeded,
    ProjectLimitExceeded(ProjectId),
    ProjectUnauthorized,
    IncompatibleContract(ExtensionReference),
    OutOfProcessRequired(ExtensionReference),
    TrustedAttestationRequired,
    TrustRouteMismatch,
    SandboxRequired,
    SandboxAuthorityExceeded,
    RouteMismatch,
    RouteNotAuthorized,
    ContractConflict(ExtensionReference),
    UnknownExtension(ExtensionReference),
    Revoked(ExtensionReference),
    Superseded {
        extension: ExtensionReference,
        by: ExtensionReference,
    },
    SelfSupersede,
    InactiveReplacement(ExtensionReference),
    IncompatibleReplacement,
    InvalidLifecycleGraph,
    InvalidSnapshot,
    NonCanonicalSnapshot,
    Persistence(String),
    Unavailable,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormatVersion(version) => {
                write!(formatter, "unsupported extension registry format {version}")
            }
            Self::InvalidText => formatter.write_str("extension text is invalid or too large"),
            Self::InvalidDigest => formatter.write_str("extension digest is invalid"),
            Self::LimitExceeded => formatter.write_str("global extension registry limit exceeded"),
            Self::ProjectLimitExceeded(project) => write!(
                formatter,
                "extension registry limit exceeded for project {project}"
            ),
            Self::ProjectUnauthorized => {
                formatter.write_str("extension project access is not authorized")
            }
            Self::IncompatibleContract(extension) => write!(
                formatter,
                "extension {extension} is incompatible with this host"
            ),
            Self::OutOfProcessRequired(extension) => write!(
                formatter,
                "untrusted extension {extension} must run out of process"
            ),
            Self::TrustedAttestationRequired => {
                formatter.write_str("trusted extension requires current build attestation")
            }
            Self::TrustRouteMismatch => {
                formatter.write_str("extension trust and route do not match")
            }
            Self::SandboxRequired => {
                formatter.write_str("untrusted extension route requires a sandbox profile")
            }
            Self::SandboxAuthorityExceeded => {
                formatter.write_str("sandbox profile exceeds broker-authorized authority")
            }
            Self::RouteMismatch => formatter.write_str("extension route authorization mismatch"),
            Self::RouteNotAuthorized => {
                formatter.write_str("extension route was not authorized by the capability broker")
            }
            Self::ContractConflict(extension) => {
                write!(formatter, "extension contract conflicts with {extension}")
            }
            Self::UnknownExtension(extension) => write!(formatter, "unknown extension {extension}"),
            Self::Revoked(extension) => write!(formatter, "extension {extension} is revoked"),
            Self::Superseded { extension, by } => {
                write!(formatter, "extension {extension} is superseded by {by}")
            }
            Self::SelfSupersede => formatter.write_str("extension cannot supersede itself"),
            Self::InactiveReplacement(extension) => {
                write!(formatter, "replacement extension {extension} is not active")
            }
            Self::IncompatibleReplacement => {
                formatter.write_str("replacement extension is incompatible")
            }
            Self::InvalidLifecycleGraph => {
                formatter.write_str("extension lifecycle graph is invalid")
            }
            Self::InvalidSnapshot => formatter.write_str("extension registry snapshot is invalid"),
            Self::NonCanonicalSnapshot => {
                formatter.write_str("extension registry snapshot is not canonical")
            }
            Self::Persistence(error) => {
                write!(formatter, "extension registry persistence: {error}")
            }
            Self::Unavailable => formatter.write_str("extension registry is unavailable"),
        }
    }
}

impl std::error::Error for RegistryError {}

fn authorize_project(
    authenticated: &AuthenticatedPrincipal,
    project_id: ProjectId,
) -> Result<(), RegistryError> {
    ScopedAuthorizer
        .authorize(
            authenticated,
            ResourceScope::new(authenticated.principal_id(), project_id),
            Grant::WorkspaceRead,
        )
        .map(|_| ())
        .map_err(|_| RegistryError::ProjectUnauthorized)
}

fn authorize_project_mutation(
    authenticated: &AuthenticatedPrincipal,
    project_id: ProjectId,
) -> Result<(), RegistryError> {
    ScopedAuthorizer
        .authorize(
            authenticated,
            ResourceScope::new(authenticated.principal_id(), project_id),
            Grant::WorkspaceWrite,
        )
        .map(|_| ())
        .map_err(|_| RegistryError::ProjectUnauthorized)
}

fn validate_sandbox(
    profile: &ExecutorProfile,
    authorization: &TransportAuthorization,
) -> Result<(), RegistryError> {
    if !matches!(
        profile.label(),
        ExecutionLabel::Restricted | ExecutionLabel::Hostile
    ) || profile.mounts().iter().any(|mount| {
        matches!(mount.role, MountRole::Root | MountRole::Source)
            && mount.access != MountAccess::ReadOnly
    }) || !authorization.allows_profile(profile)
    {
        return Err(RegistryError::SandboxAuthorityExceeded);
    }
    Ok(())
}

fn validate_text(value: &str) -> Result<(), RegistryError> {
    if value.is_empty()
        || value.len() > MAX_EXTENSION_TEXT_BYTES
        || value.chars().any(char::is_control)
    {
        Err(RegistryError::InvalidText)
    } else {
        Ok(())
    }
}

fn validate_digest(value: &str) -> Result<(), RegistryError> {
    if value.parse::<Digest>().is_ok()
        || value
            .strip_prefix("blake3:")
            .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        Ok(())
    } else {
        Err(RegistryError::InvalidDigest)
    }
}

const fn kind_tag(kind: ExtensionKind) -> u8 {
    match kind {
        ExtensionKind::NativeProvider => 0,
        ExtensionKind::McpServer => 1,
        ExtensionKind::SchemaProjectionAdapter => 2,
    }
}

const fn protocol_tag(protocol: ExtensionProtocol) -> u8 {
    match protocol {
        ExtensionProtocol::Mcp => 0,
        ExtensionProtocol::Acp => 1,
        ExtensionProtocol::A2a => 2,
        ExtensionProtocol::KitPluginV1 => 3,
    }
}

fn put_version(output: &mut Vec<u8>, version: ContractVersion) {
    output.extend_from_slice(&version.major().to_be_bytes());
    output.extend_from_slice(&version.minor().to_be_bytes());
    output.extend_from_slice(&version.patch().to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::auth::{
            contract::{Authenticator, GrantSnapshot},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        api::service::WorkerStore,
        capabilities::{
            kernel::identity::DigestAlgorithm,
            schema::{
                JSON_SCHEMA_2020_12, NormalizedSchema, ProjectionProfile, ProjectionTarget,
                SchemaProjectionAdapter, SchemaProjectionSet,
            },
        },
    };

    fn authenticated(principal: PrincipalId, project: ProjectId) -> AuthenticatedPrincipal {
        LocalPeerAuthenticator::new(BTreeMap::from([(
            1,
            GrantSnapshot::new(
                principal,
                project,
                [Grant::WorkspaceRead, Grant::WorkspaceWrite],
            ),
        )]))
        .authenticate(&LocalPeerObservation::from_transport(1, 1, 1))
        .unwrap()
    }

    fn genuine_native_contract() -> ExtensionContract {
        built_in_contracts()
            .into_iter()
            .find(|contract| contract.kind() == ExtensionKind::NativeProvider)
            .expect("native provider contract exists")
    }

    /// Simulates the contract a differently built kit binary would have
    /// registered: same reference, different digests (and optionally a
    /// different version).
    fn rebuilt_native_contract(
        version: Option<&str>,
        schema_hex: &str,
        implementation_hex: &str,
    ) -> ExtensionContract {
        let mut value = serde_json::to_value(genuine_native_contract()).unwrap();
        if let Some(version) = version {
            value["version"] = serde_json::Value::String(version.to_owned());
        }
        value["schema_digest"] =
            serde_json::Value::String(format!("sha256:{}", schema_hex.repeat(32)));
        value["implementation_digest"] =
            serde_json::Value::String(format!("sha256:{}", implementation_hex.repeat(32)));
        serde_json::from_value(value).unwrap()
    }

    fn active_entry(scope: ExtensionScope, contract: ExtensionContract) -> RegistryEntry {
        RegistryEntry {
            scope,
            contract,
            state: ExtensionState::Active,
        }
    }

    #[test]
    fn trusted_bootstrap_reregistration_with_changed_digests_supersedes_durably() {
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let scope = ExtensionScope::new(principal, project);
        let database = std::env::temp_dir().join(format!(
            "kit-extension-supersede-{}-{project}",
            std::process::id()
        ));
        let service = crate::test_support::open_service_store(&database).unwrap();
        let mut store = service.worker_append_store().unwrap();
        let stale = rebuilt_native_contract(None, "11", "22");
        let mut seeded = CapabilityExtensionRegistry::default();
        seeded.entries.insert(
            (scope, stale.reference()),
            active_entry(scope, stale.clone()),
        );
        seeded.revision = 1;
        seeded.revisions.insert(scope, 1);
        let bytes = seeded.scope_bytes(scope).unwrap();
        assert_eq!(
            store
                .persist_extension_registry_snapshot(
                    principal,
                    project,
                    0,
                    1,
                    &bytes,
                    1,
                    MAX_EXTENSION_ENTRIES,
                    MAX_EXTENSION_SNAPSHOT_BYTES,
                )
                .unwrap(),
            crate::store::sqlite::append::ExtensionRegistryCommit::Committed
        );

        let shared = Arc::new(RwLock::new(CapabilityExtensionRegistry::default()));
        let (guard, upgrades) =
            attest_native_extension_durable(&shared, scope, &mut store).unwrap();
        guard.ensure_current().unwrap();
        let genuine = genuine_native_contract();
        assert_eq!(
            upgrades,
            vec![ExtensionUpgradeAudit {
                reference: genuine.reference(),
                old_schema_digest: stale.schema_digest().clone(),
                new_schema_digest: genuine.schema_digest().clone(),
                old_implementation_digest: stale.implementation_digest().clone(),
                new_implementation_digest: genuine.implementation_digest().clone(),
            }]
        );
        {
            let registry = shared.read().unwrap();
            let entry = registry
                .entries
                .get(&(scope, genuine.reference()))
                .expect("superseding entry is stored");
            assert_eq!(entry.contract, genuine);
            assert!(matches!(entry.state, ExtensionState::Active));
        }
        let (revision, snapshots) = store.extension_registry_state().unwrap();
        assert_eq!(revision, 2);
        let payload = String::from_utf8(snapshots[0].1.clone()).unwrap();
        assert!(payload.contains(genuine.implementation_digest().as_str()));
        assert!(!payload.contains(stale.implementation_digest().as_str()));
        assert!(!payload.contains(stale.schema_digest().as_str()));

        // Re-attestation after the upgrade is a pure no-op.
        let (_, upgrades) = attest_native_extension_durable(&shared, scope, &mut store).unwrap();
        assert!(upgrades.is_empty());
        assert_eq!(store.extension_registry_state().unwrap().0, 2);

        drop(store);
        drop(service);
        let _ = std::fs::remove_file(database);
    }

    #[tokio::test]
    async fn trusted_supersede_revokes_the_old_contract_lifecycle() {
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let scope = ExtensionScope::new(principal, project);
        let stale = rebuilt_native_contract(None, "33", "44");
        let reference = stale.reference();
        let shared = Arc::new(RwLock::new(CapabilityExtensionRegistry::default()));
        {
            let mut registry = shared.write().unwrap();
            registry
                .entries
                .insert((scope, reference.clone()), active_entry(scope, stale));
            registry.trusted_active.insert((scope, reference.clone()));
        }
        let guard = CapabilityExtensionRegistry::lifecycle_guard(
            &shared,
            scope,
            &reference,
            ExtensionKind::NativeProvider,
            TrustClassification::Trusted,
        )
        .unwrap();
        let mut cancelled = guard.cancellation();
        let token = TrustedExtensionToken::daemon_bootstrap();
        assert_eq!(
            shared
                .write()
                .unwrap()
                .register_trusted(&token, scope, genuine_native_contract())
                .unwrap(),
            RegistrationOutcome::Superseded
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), cancelled.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(*cancelled.borrow());
        assert!(matches!(
            guard.ensure_current(),
            Err(RegistryError::Revoked(_))
        ));
    }

    #[test]
    fn trusted_downgrade_below_stored_version_keeps_contract_conflict() {
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let scope = ExtensionScope::new(principal, project);
        let newer = rebuilt_native_contract(Some("9.9.9"), "55", "66");
        let mut registry = CapabilityExtensionRegistry::default();
        registry
            .entries
            .insert((scope, newer.reference()), active_entry(scope, newer));
        let token = TrustedExtensionToken::daemon_bootstrap();
        assert!(matches!(
            registry.register_trusted(&token, scope, genuine_native_contract()),
            Err(RegistryError::ContractConflict(_))
        ));
    }

    #[test]
    fn trusted_supersede_requires_unchanged_kind_trust_and_active_state() {
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let scope = ExtensionScope::new(principal, project);
        let token = TrustedExtensionToken::daemon_bootstrap();
        let genuine = genuine_native_contract();

        // Kind change under the same reference keeps the conflict.
        let mut kind_changed = serde_json::to_value(&genuine).unwrap();
        kind_changed["kind"] = serde_json::Value::String("schema_projection_adapter".to_owned());
        kind_changed["implementation_digest"] =
            serde_json::Value::String(format!("sha256:{}", "77".repeat(32)));
        let kind_changed: ExtensionContract = serde_json::from_value(kind_changed).unwrap();
        let mut registry = CapabilityExtensionRegistry::default();
        registry.entries.insert(
            (scope, kind_changed.reference()),
            active_entry(scope, kind_changed),
        );
        assert!(matches!(
            registry.register_trusted(&token, scope, genuine.clone()),
            Err(RegistryError::ContractConflict(_))
        ));

        // Trust classification change under the same reference keeps the
        // conflict.
        let untrusted_same_reference = ExtensionContract::untrusted(
            ExtensionKind::NativeProvider,
            genuine.identity().clone(),
            genuine.version().clone(),
            ContentDigest::sha256(b"schema"),
            ContentDigest::sha256(b"implementation"),
            genuine.compatibility(),
            ExtensionProtocol::Mcp,
            "route-native",
            ContentDigest::sha256(b"profile").to_string(),
            ExtensionMetadata::default(),
        )
        .unwrap();
        let mut registry = CapabilityExtensionRegistry::default();
        registry.entries.insert(
            (scope, untrusted_same_reference.reference()),
            active_entry(scope, untrusted_same_reference),
        );
        assert!(matches!(
            registry.register_trusted(&token, scope, genuine.clone()),
            Err(RegistryError::ContractConflict(_))
        ));

        // A revoked entry is never resurrected by re-registration.
        let mut registry = CapabilityExtensionRegistry::default();
        registry.entries.insert(
            (scope, genuine.reference()),
            RegistryEntry {
                scope,
                contract: rebuilt_native_contract(None, "88", "99"),
                state: ExtensionState::Revoked,
            },
        );
        assert!(matches!(
            registry.register_trusted(&token, scope, genuine),
            Err(RegistryError::ContractConflict(_))
        ));
    }

    #[test]
    fn untrusted_reregistration_with_changed_contract_keeps_contract_conflict() {
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let authenticated = authenticated(principal, project);
        let make = |implementation: &[u8]| {
            ExtensionContract::untrusted(
                ExtensionKind::McpServer,
                ExtensionIdentity::parse("third-party.rebuilt").unwrap(),
                ExtensionVersion::parse("1.0.0").unwrap(),
                ContentDigest::sha256(b"schema"),
                ContentDigest::sha256(implementation),
                CompatibilityRange::new(
                    CAPABILITY_EXTENSION_HOST_VERSION,
                    ContractVersion::new(2, 0, 0),
                ),
                ExtensionProtocol::Mcp,
                "route",
                ContentDigest::sha256(b"profile").to_string(),
                ExtensionMetadata::default(),
            )
            .unwrap()
        };
        let mut registry = CapabilityExtensionRegistry::default();
        registry
            .register_untrusted(&authenticated, project, make(b"implementation-a"))
            .unwrap();
        assert!(matches!(
            registry.register_untrusted(&authenticated, project, make(b"implementation-b")),
            Err(RegistryError::ContractConflict(_))
        ));
    }

    #[test]
    fn repeated_tool_adapter_attestation_does_not_rewrite_the_registry() {
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let scope = ExtensionScope::new(principal, project);
        let database = std::env::temp_dir().join(format!(
            "kit-extension-attestation-noop-{}-{project}",
            std::process::id()
        ));
        let service = crate::test_support::open_service_store(&database).unwrap();
        let mut store = service.worker_append_store().unwrap();
        let shared = Arc::new(RwLock::new(CapabilityExtensionRegistry::default()));

        attest_native_extension_durable(&shared, scope, &mut store).unwrap();
        let before = store.extension_registry_state().unwrap();
        attest_native_extension_durable(&shared, scope, &mut store).unwrap();
        let after = store.extension_registry_state().unwrap();

        assert_eq!(after, before);
        drop(store);
        drop(service);
        let _ = std::fs::remove_file(database);
    }

    #[tokio::test]
    async fn revoke_signals_live_extension_and_disables_schema_projection() {
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let scope = ExtensionScope::new(principal, project);
        let authenticated = authenticated(principal, project);
        let trusted = TrustedExtensionToken::daemon_bootstrap();
        let shared = Arc::new(RwLock::new(CapabilityExtensionRegistry::default()));
        {
            let mut registry = shared.write().unwrap();
            for contract in built_in_contracts() {
                registry
                    .register_trusted(&trusted, scope, contract)
                    .unwrap();
            }
        }
        let adapter = SchemaProjectionAdapter::new(&shared, scope).unwrap();
        let guard =
            CapabilityExtensionRegistry::schema_projection_lifecycle_guard(&shared, scope).unwrap();
        let mut cancelled = guard.cancellation();
        let reference = built_in_contracts()
            .into_iter()
            .find(|contract| contract.kind() == ExtensionKind::SchemaProjectionAdapter)
            .unwrap()
            .reference();
        shared
            .write()
            .unwrap()
            .revoke(&authenticated, project, &reference)
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), cancelled.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(*cancelled.borrow());
        let normalized = NormalizedSchema::ingest(
            br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}"#,
            JSON_SCHEMA_2020_12,
            "test",
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        let profile = ProjectionProfile::new(
            ProjectionTarget::new("provider", "model", "runtime", 1).unwrap(),
            JSON_SCHEMA_2020_12,
            BTreeSet::from(["$schema".to_owned(), "type".to_owned()]),
            serde_json::Value::Bool(true),
            1024,
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        assert!(matches!(
            adapter.project(&mut SchemaProjectionSet::new(normalized), &profile),
            Err(crate::capabilities::schema::ProjectionError::AdapterUnavailable)
        ));
    }
}
