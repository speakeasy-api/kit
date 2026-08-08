use std::{
    collections::BTreeSet,
    fmt,
    sync::atomic::{AtomicU8, Ordering},
};

use crate::domain::{
    config::Grant,
    ids::{PrincipalId, ProjectId},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantSnapshot {
    principal_id: PrincipalId,
    project_id: ProjectId,
    grants: BTreeSet<Grant>,
    principal_grants: BTreeSet<PrincipalGrant>,
}

impl GrantSnapshot {
    pub fn new(
        principal_id: PrincipalId,
        project_id: ProjectId,
        grants: impl IntoIterator<Item = Grant>,
    ) -> Self {
        Self {
            principal_id,
            project_id,
            grants: grants.into_iter().collect(),
            principal_grants: BTreeSet::new(),
        }
    }

    pub fn with_principal_grant(mut self, grant: PrincipalGrant) -> Self {
        self.principal_grants.insert(grant);
        self
    }

    pub fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub fn grants(&self) -> &BTreeSet<Grant> {
        &self.grants
    }

    pub fn principal_grants(&self) -> &BTreeSet<PrincipalGrant> {
        &self.principal_grants
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PrincipalGrant {
    CreateProject,
    AccessOwnedProjects,
    ResolveApproval,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedPrincipal {
    grants: GrantSnapshot,
}

impl AuthenticatedPrincipal {
    pub(crate) fn from_grants(grants: GrantSnapshot) -> Self {
        Self { grants }
    }

    pub fn principal_id(&self) -> PrincipalId {
        self.grants.principal_id()
    }

    pub fn grant_snapshot(&self) -> &GrantSnapshot {
        &self.grants
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthDenial {
    Unauthenticated,
    Unauthorized,
}

impl fmt::Display for AuthDenial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthenticated => f.write_str("authentication denied"),
            Self::Unauthorized => f.write_str("authorization denied"),
        }
    }
}

impl std::error::Error for AuthDenial {}

pub type AuthDecision = Result<AuthenticatedPrincipal, AuthDenial>;

pub trait Authenticator<Observation> {
    fn authenticate(&self, observation: &Observation) -> AuthDecision;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceScope {
    Project {
        principal_id: PrincipalId,
        project_id: ProjectId,
    },
    ProjectCreation {
        principal_id: PrincipalId,
        project_id: ProjectId,
    },
}

impl ResourceScope {
    pub fn new(principal_id: PrincipalId, project_id: ProjectId) -> Self {
        Self::Project {
            principal_id,
            project_id,
        }
    }

    pub fn project_creation(principal_id: PrincipalId, project_id: ProjectId) -> Self {
        Self::ProjectCreation {
            principal_id,
            project_id,
        }
    }

    pub fn principal_id(self) -> PrincipalId {
        match self {
            Self::Project { principal_id, .. } | Self::ProjectCreation { principal_id, .. } => {
                principal_id
            }
        }
    }

    pub fn project_id(self) -> ProjectId {
        match self {
            Self::Project { project_id, .. } | Self::ProjectCreation { project_id, .. } => {
                project_id
            }
        }
    }
}

pub trait Authorizer {
    fn authorize(
        &self,
        authenticated: &AuthenticatedPrincipal,
        resource: ResourceScope,
        required: Grant,
    ) -> AuthDecision;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScopedAuthorizer;

impl Authorizer for ScopedAuthorizer {
    fn authorize(
        &self,
        authenticated: &AuthenticatedPrincipal,
        resource: ResourceScope,
        required: Grant,
    ) -> AuthDecision {
        let snapshot = authenticated.grant_snapshot();
        let authorized = match resource {
            ResourceScope::Project {
                principal_id,
                project_id,
            } => {
                snapshot.principal_id() == principal_id
                    && (snapshot.project_id() == project_id
                        || snapshot
                            .principal_grants()
                            .contains(&PrincipalGrant::AccessOwnedProjects))
                    && snapshot.grants().contains(&required)
            }
            ResourceScope::ProjectCreation {
                principal_id,
                project_id,
            } => {
                snapshot.principal_id() == principal_id
                    && required == Grant::WorkspaceWrite
                    && ((snapshot.project_id() == project_id
                        && snapshot.grants().contains(&required))
                        || snapshot
                            .principal_grants()
                            .contains(&PrincipalGrant::CreateProject))
            }
        };
        if !authorized {
            return Err(AuthDenial::Unauthorized);
        }
        Ok(authenticated.clone())
    }
}

#[derive(Debug, Default)]
pub struct AuthReadiness {
    installed: AtomicU8,
}

impl AuthReadiness {
    const AUTHENTICATOR: u8 = 1;
    const AUTHORIZER: u8 = 2;
    const READY: u8 = Self::AUTHENTICATOR | Self::AUTHORIZER;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn install_authenticator<O, A>(&self, _authenticator: &A)
    where
        A: Authenticator<O>,
    {
        self.installed
            .fetch_or(Self::AUTHENTICATOR, Ordering::Release);
    }

    pub fn install_authorizer<A>(&self, _authorizer: &A)
    where
        A: Authorizer,
    {
        self.installed.fetch_or(Self::AUTHORIZER, Ordering::Release);
    }

    pub fn is_ready(&self) -> bool {
        self.installed.load(Ordering::Acquire) == Self::READY
    }
}
