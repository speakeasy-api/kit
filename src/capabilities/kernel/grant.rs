use std::{collections::BTreeSet, fmt, sync::Arc};

use crate::{
    api::auth::contract::AuthenticatedPrincipal,
    domain::{
        config::{Grant, RunConfigSnapshot},
        ids::{PrincipalId, ProjectId, WorkspaceId},
    },
};

use super::{
    grant_ext::{GrantExtension, RequestExtension},
    identity::{CapabilityIdentity, Digest, DigestAlgorithm, put_bytes, put_digest},
};

#[derive(
    Clone, Copy, Debug, serde::Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    ModelCall,
    WorkspaceRead,
    WorkspaceWrite,
    ProcessSpawn,
    NetworkEgress,
}

impl EffectClass {
    pub const fn required_grant(self) -> Grant {
        match self {
            Self::ModelCall => Grant::ModelCall,
            Self::WorkspaceRead => Grant::WorkspaceRead,
            Self::WorkspaceWrite => Grant::WorkspaceWrite,
            Self::ProcessSpawn => Grant::ProcessSpawn,
            Self::NetworkEgress => Grant::NetworkEgress,
        }
    }

    pub(crate) const fn tag(self) -> u8 {
        match self {
            Self::ModelCall => 0,
            Self::WorkspaceRead => 1,
            Self::WorkspaceWrite => 2,
            Self::ProcessSpawn => 3,
            Self::NetworkEgress => 4,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArgumentConstraint(Arc<[u8]>);

impl ArgumentConstraint {
    pub fn new(predicate: impl Into<Arc<[u8]>>) -> Self {
        Self(predicate.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArgumentConstraints(BTreeSet<ArgumentConstraint>);

impl ArgumentConstraints {
    pub fn new<I, B>(predicates: I) -> Self
    where
        I: IntoIterator<Item = B>,
        B: Into<Arc<[u8]>>,
    {
        Self(
            predicates
                .into_iter()
                .map(ArgumentConstraint::new)
                .collect(),
        )
    }

    pub fn predicates(&self) -> &BTreeSet<ArgumentConstraint> {
        &self.0
    }

    // Additional exact predicates narrow authority; predicates are never interpreted or dropped.
    pub fn allows(&self, requested: &Self) -> bool {
        self.0.is_subset(&requested.0)
    }

    fn write_canonical(&self, output: &mut Vec<u8>) {
        output.extend_from_slice(&(self.0.len() as u64).to_be_bytes());
        for predicate in &self.0 {
            put_bytes(output, predicate.as_bytes());
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CapabilityGrant {
    principal_id: PrincipalId,
    project_id: ProjectId,
    workspace_id: WorkspaceId,
    capability: CapabilityIdentity,
    schema_digest: Digest,
    effect: EffectClass,
    argument_constraints: ArgumentConstraints,
    extension: GrantExtension,
}

impl CapabilityGrant {
    pub fn new(
        principal_id: PrincipalId,
        project_id: ProjectId,
        workspace_id: WorkspaceId,
        capability: CapabilityIdentity,
        schema_digest: Digest,
        effect: EffectClass,
        argument_constraints: ArgumentConstraints,
    ) -> Self {
        Self {
            principal_id,
            project_id,
            workspace_id,
            capability,
            schema_digest,
            effect,
            argument_constraints,
            extension: GrantExtension::default(),
        }
    }

    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub const fn capability(&self) -> &CapabilityIdentity {
        &self.capability
    }

    pub const fn schema_digest(&self) -> Digest {
        self.schema_digest
    }

    pub const fn effect(&self) -> EffectClass {
        self.effect
    }

    pub const fn argument_constraints(&self) -> &ArgumentConstraints {
        &self.argument_constraints
    }

    pub fn with_extension(mut self, extension: GrantExtension) -> Self {
        self.extension = extension;
        self
    }

    pub const fn extension(&self) -> &GrantExtension {
        &self.extension
    }

    fn match_outcome(&self, input: &BindingInputs) -> GrantMatch {
        if !self.matches_except_depth(input) {
            GrantMatch::Denied
        } else if input.delegation_depth > self.extension.maximum_delegation_depth() {
            GrantMatch::DepthExceeded
        } else {
            GrantMatch::Allowed
        }
    }

    fn matches_except_depth(&self, input: &BindingInputs) -> bool {
        self.principal_id == input.principal_id
            && self.project_id == input.project_id
            && self.workspace_id == input.workspace_id
            && self.capability == input.capability
            && self.schema_digest == input.schema_digest
            && self.effect == input.effect
            && self
                .argument_constraints
                .allows(&input.argument_constraints)
            && self.extension.allows_except_depth(&input.extension)
    }

    fn write_canonical(&self, output: &mut Vec<u8>) {
        put_bytes(output, self.principal_id.to_string().as_bytes());
        put_bytes(output, self.project_id.to_string().as_bytes());
        put_bytes(output, self.workspace_id.to_string().as_bytes());
        self.capability.write_canonical(output);
        put_digest(output, self.schema_digest);
        output.push(self.effect.tag());
        self.argument_constraints.write_canonical(output);
        self.extension.write_canonical(output);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityGrantSnapshot {
    principal_id: PrincipalId,
    project_id: ProjectId,
    config_digest: [u8; 32],
    grants: BTreeSet<CapabilityGrant>,
    digest: Digest,
}

impl CapabilityGrantSnapshot {
    pub fn new(
        config: &RunConfigSnapshot,
        grants: impl IntoIterator<Item = CapabilityGrant>,
        algorithm: DigestAlgorithm,
    ) -> Self {
        let grants = grants.into_iter().collect::<BTreeSet<_>>();
        let mut canonical = Vec::new();
        canonical.extend_from_slice(b"KCAPGRANT\0");
        put_bytes(&mut canonical, config.principal_id().to_string().as_bytes());
        put_bytes(&mut canonical, config.project_id().to_string().as_bytes());
        canonical.extend_from_slice(&config.digest());
        canonical.extend_from_slice(&(grants.len() as u64).to_be_bytes());
        for grant in &grants {
            grant.write_canonical(&mut canonical);
        }
        Self {
            principal_id: config.principal_id(),
            project_id: config.project_id(),
            config_digest: config.digest(),
            grants,
            digest: Digest::of(algorithm, &canonical),
        }
    }

    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub const fn config_digest(&self) -> [u8; 32] {
        self.config_digest
    }

    pub fn grants(&self) -> &BTreeSet<CapabilityGrant> {
        &self.grants
    }

    pub const fn digest(&self) -> Digest {
        self.digest
    }

    fn match_outcome(&self, input: &BindingInputs) -> GrantMatch {
        let mut outcome = GrantMatch::Denied;
        for grant in &self.grants {
            match grant.match_outcome(input) {
                GrantMatch::Allowed => return GrantMatch::Allowed,
                GrantMatch::DepthExceeded => outcome = GrantMatch::DepthExceeded,
                GrantMatch::Denied => {}
            }
        }
        outcome
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GrantMatch {
    Allowed,
    DepthExceeded,
    Denied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationSnapshot {
    path: Arc<[PrincipalId]>,
    maximum_depth: u16,
    grants: CapabilityGrantSnapshot,
    digest: Digest,
}

impl DelegationSnapshot {
    pub fn new(
        path: impl Into<Arc<[PrincipalId]>>,
        maximum_depth: u16,
        grants: CapabilityGrantSnapshot,
    ) -> Result<Self, DelegationError> {
        let path = path.into();
        if path.is_empty() {
            return Err(DelegationError::EmptyPath);
        }
        if path.len().saturating_sub(1) > usize::from(maximum_depth) {
            return Err(DelegationError::DepthExceeded);
        }
        if path.iter().collect::<BTreeSet<_>>().len() != path.len() {
            return Err(DelegationError::Loop);
        }
        if path.last().copied() != Some(grants.principal_id()) {
            return Err(DelegationError::PrincipalMismatch);
        }
        let mut canonical = Vec::new();
        canonical.extend_from_slice(b"KCAPDELEGATE\0");
        canonical.extend_from_slice(&maximum_depth.to_be_bytes());
        canonical.extend_from_slice(&(path.len() as u64).to_be_bytes());
        for principal_id in path.iter() {
            put_bytes(&mut canonical, principal_id.to_string().as_bytes());
        }
        put_digest(&mut canonical, grants.digest());
        let digest = Digest::of(grants.digest().algorithm(), &canonical);
        Ok(Self {
            path,
            maximum_depth,
            grants,
            digest,
        })
    }

    pub fn path(&self) -> &[PrincipalId] {
        &self.path
    }

    pub const fn maximum_depth(&self) -> u16 {
        self.maximum_depth
    }

    pub const fn grants(&self) -> &CapabilityGrantSnapshot {
        &self.grants
    }

    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationError {
    EmptyPath,
    DepthExceeded,
    Loop,
    PrincipalMismatch,
}

impl fmt::Display for DelegationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyPath => "delegation path must not be empty",
            Self::DepthExceeded => "delegation path exceeds its maximum depth",
            Self::Loop => "delegation path contains a loop",
            Self::PrincipalMismatch => "delegation path does not end at the grant principal",
        })
    }
}

impl std::error::Error for DelegationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingInputs {
    principal_id: PrincipalId,
    capability: CapabilityIdentity,
    schema_digest: Digest,
    effect: EffectClass,
    argument_constraints: ArgumentConstraints,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    config_snapshot_digest: [u8; 32],
    grant_snapshot_digest: Digest,
    delegation_digest: Option<Digest>,
    extension: RequestExtension,
    delegation_depth: u16,
}

impl BindingInputs {
    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    pub const fn capability(&self) -> &CapabilityIdentity {
        &self.capability
    }

    pub const fn schema_digest(&self) -> Digest {
        self.schema_digest
    }

    pub const fn effect(&self) -> EffectClass {
        self.effect
    }

    pub const fn argument_constraints(&self) -> &ArgumentConstraints {
        &self.argument_constraints
    }

    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub const fn config_snapshot_digest(&self) -> [u8; 32] {
        self.config_snapshot_digest
    }

    pub const fn grant_snapshot_digest(&self) -> Digest {
        self.grant_snapshot_digest
    }

    pub const fn delegation_digest(&self) -> Option<Digest> {
        self.delegation_digest
    }

    pub const fn extension(&self) -> &RequestExtension {
        &self.extension
    }

    pub const fn delegation_depth(&self) -> u16 {
        self.delegation_depth
    }

    pub(super) fn into_extension(self) -> RequestExtension {
        self.extension
    }

    fn write_canonical(&self, output: &mut Vec<u8>) {
        put_bytes(output, self.principal_id.to_string().as_bytes());
        self.capability.write_canonical(output);
        put_digest(output, self.schema_digest);
        output.push(self.effect.tag());
        self.argument_constraints.write_canonical(output);
        put_bytes(output, self.workspace_id.to_string().as_bytes());
        put_bytes(output, self.project_id.to_string().as_bytes());
        output.extend_from_slice(&self.config_snapshot_digest);
        put_digest(output, self.grant_snapshot_digest);
        match self.delegation_digest {
            Some(digest) => {
                output.push(1);
                put_digest(output, digest);
            }
            None => output.push(0),
        }
        self.extension.write_canonical(output);
        output.extend_from_slice(&self.delegation_depth.to_be_bytes());
    }
}

pub struct GrantRequest<'a> {
    pub authenticated: &'a AuthenticatedPrincipal,
    pub capability: &'a CapabilityIdentity,
    pub schema_digest: Digest,
    pub effect: EffectClass,
    pub argument_constraints: &'a ArgumentConstraints,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub config: &'a RunConfigSnapshot,
    pub grants: &'a CapabilityGrantSnapshot,
    pub delegation: Option<&'a DelegationSnapshot>,
    pub extension: RequestExtension,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantReasonCode {
    Granted,
    AuthenticationScopeMismatch,
    ConfigurationScopeMismatch,
    ConfigurationSnapshotChanged,
    EffectNotAuthenticated,
    EffectNotConfigured,
    NoMatchingGrant,
    DelegationPrincipalMismatch,
    DelegationDenied,
    DelegationDepthExceeded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantDecision {
    allowed: bool,
    reason: GrantReasonCode,
    snapshot_digest: Digest,
    binding_inputs: BindingInputs,
}

impl GrantDecision {
    pub const fn is_allowed(&self) -> bool {
        self.allowed
    }

    pub const fn reason(&self) -> GrantReasonCode {
        self.reason
    }

    pub const fn snapshot_digest(&self) -> Digest {
        self.snapshot_digest
    }

    pub const fn binding_inputs(&self) -> &BindingInputs {
        &self.binding_inputs
    }

    pub fn into_authorized_inputs(self) -> Option<BindingInputs> {
        self.allowed.then_some(self.binding_inputs)
    }
}

pub fn decide(request: GrantRequest<'_>) -> GrantDecision {
    let authenticated = request.authenticated.grant_snapshot();
    let delegation_depth = request
        .delegation
        .map_or(0, |delegation| delegation.path().len().saturating_sub(1));
    let delegation_depth = u16::try_from(delegation_depth).unwrap_or(u16::MAX);
    let binding_inputs = BindingInputs {
        principal_id: request.authenticated.principal_id(),
        capability: request.capability.clone(),
        schema_digest: request.schema_digest,
        effect: request.effect,
        argument_constraints: request.argument_constraints.clone(),
        workspace_id: request.workspace_id,
        project_id: request.project_id,
        config_snapshot_digest: request.config.digest(),
        grant_snapshot_digest: request.grants.digest(),
        delegation_digest: request.delegation.map(DelegationSnapshot::digest),
        extension: request.extension,
        delegation_depth,
    };
    let mut canonical = Vec::new();
    canonical.extend_from_slice(b"KCAPDECISION\0");
    binding_inputs.write_canonical(&mut canonical);
    put_bytes(
        &mut canonical,
        authenticated.project_id().to_string().as_bytes(),
    );
    canonical.extend_from_slice(&(authenticated.grants().len() as u64).to_be_bytes());
    for grant in authenticated.grants() {
        canonical.push(grant.tag());
    }
    let snapshot_digest = Digest::of(request.grants.digest().algorithm(), &canonical);

    let reason = if authenticated.project_id() != request.project_id
        || request.authenticated.principal_id() != request.config.principal_id()
    {
        GrantReasonCode::AuthenticationScopeMismatch
    } else if request.config.project_id() != request.project_id
        || request.grants.principal_id() != request.authenticated.principal_id()
        || request.grants.project_id() != request.project_id
    {
        GrantReasonCode::ConfigurationScopeMismatch
    } else if request.grants.config_digest() != request.config.digest() {
        GrantReasonCode::ConfigurationSnapshotChanged
    } else if !authenticated
        .grants()
        .contains(&request.effect.required_grant())
    {
        GrantReasonCode::EffectNotAuthenticated
    } else if !request
        .config
        .effective_authority()
        .contains(&request.effect.required_grant())
    {
        GrantReasonCode::EffectNotConfigured
    } else if binding_inputs.extension.egress().is_some()
        && !authenticated.grants().contains(&Grant::NetworkEgress)
    {
        GrantReasonCode::EffectNotAuthenticated
    } else if binding_inputs.extension.egress().is_some()
        && !request
            .config
            .effective_authority()
            .contains(&Grant::NetworkEgress)
    {
        GrantReasonCode::EffectNotConfigured
    } else if request.delegation.is_some_and(|delegation| {
        delegation.path().last().copied() != Some(request.authenticated.principal_id())
    }) {
        GrantReasonCode::DelegationPrincipalMismatch
    } else {
        match request.grants.match_outcome(&binding_inputs) {
            GrantMatch::Denied => GrantReasonCode::NoMatchingGrant,
            GrantMatch::DepthExceeded => GrantReasonCode::DelegationDepthExceeded,
            GrantMatch::Allowed => {
                request
                    .delegation
                    .map_or(GrantReasonCode::Granted, |delegation| {
                        match delegation.grants().match_outcome(&binding_inputs) {
                            GrantMatch::Allowed => GrantReasonCode::Granted,
                            GrantMatch::DepthExceeded => GrantReasonCode::DelegationDepthExceeded,
                            GrantMatch::Denied => GrantReasonCode::DelegationDenied,
                        }
                    })
            }
        }
    };

    GrantDecision {
        allowed: reason == GrantReasonCode::Granted,
        reason,
        snapshot_digest,
        binding_inputs,
    }
}
