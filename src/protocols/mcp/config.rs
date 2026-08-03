use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use crate::{
    api::{auth::contract::AuthenticatedPrincipal, service::AttemptDriverClaim},
    capabilities::{
        catalog::{Availability, CapabilityKind},
        kernel::{grant::EffectClass, identity::Digest, invoke::RetrySafety},
    },
    domain::{
        config::Grant,
        config::RunConfigSnapshot,
        ids::{PrincipalId, ProjectId, WorkspaceId},
        lifecycle::AttemptOwnership,
        secret::SecretHandle,
    },
    protocols::mcp::features::ConfiguredServerIdentity,
    runtime::scheduler::reserve::BudgetLedger,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub id: String,
    pub transport: McpTransportConfig,
    pub owner: McpOwnerConfig,
    pub source: String,
    pub trust_domain: String,
    pub namespace: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_handle: Option<SecretHandle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_scope: Option<McpCredentialScopeConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<McpEgressConfig>,
    pub descriptors: Vec<McpDescriptorPolicyConfig>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTransportConfig {
    Http {
        endpoint: String,
    },
    Stdio {
        owned_process_profile: String,
        argv: Vec<String>,
        profile: Box<crate::executor::profile::ProfileSpec>,
        profile_digest: String,
        #[serde(default)]
        environment: BTreeMap<String, McpStdioEnvironmentConfig>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpStdioEnvironmentConfig {
    pub handle: SecretHandle,
    pub credential_scope: McpCredentialScopeConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpOwnerConfig {
    pub principal_id: PrincipalId,
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpCredentialScopeConfig {
    Project,
    Workspace { workspace_id: WorkspaceId },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpEgressConfig {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpDescriptorPolicyConfig {
    pub kind: CapabilityKind,
    pub remote: String,
    pub descriptor_digest: Digest,
    pub effect: EffectClass,
    pub retry_safety: RetrySafety,
    pub required_grants: BTreeSet<Grant>,
    #[serde(default)]
    pub auth_scopes: BTreeSet<String>,
    pub availability: Availability,
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<(), String> {
        ConfiguredServerIdentity::new(&self.id)
            .map_err(|error| format!("MCP server id: {error}"))?;
        for (name, value) in [
            ("source", self.source.as_str()),
            ("trust_domain", self.trust_domain.as_str()),
            ("namespace", self.namespace.as_str()),
            ("version", self.version.as_str()),
        ] {
            if value.is_empty()
                || value.len() > 256
                || value.bytes().any(|byte| !byte.is_ascii_graphic())
            {
                return Err(format!(
                    "MCP {name} must contain 1 to 256 visible ASCII bytes"
                ));
            }
        }
        if let Some(handle) = &self.credential_handle {
            let identifier = handle.identifier();
            let supported = identifier.strip_prefix("env:").is_some_and(|value| {
                !value.is_empty()
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            });
            if !supported {
                return Err("MCP credential handle uses an unsupported opaque scheme".to_owned());
            }
            if self.credential_scope.is_none() {
                return Err("MCP credential scope must be explicit".to_owned());
            }
        } else if self.credential_scope.is_some() {
            return Err("MCP credential scope requires a credential handle".to_owned());
        }
        if let Some(McpCredentialScopeConfig::Workspace { workspace_id }) = self.credential_scope
            && self
                .owner
                .workspace_id
                .is_some_and(|owner| owner != workspace_id)
        {
            return Err("MCP credential workspace scope differs from owner constraint".to_owned());
        }
        match (&self.transport, &self.egress) {
            (McpTransportConfig::Http { endpoint }, Some(egress)) => {
                if self.credential_handle.is_none() {
                    return Err("MCP HTTP transport requires a credential handle".to_owned());
                }
                let url = url::Url::parse(endpoint)
                    .map_err(|_| "MCP HTTP endpoint is invalid".to_owned())?;
                if url.scheme() != "https" {
                    return Err("MCP credential-bearing HTTP endpoints must use HTTPS".to_owned());
                }
                let port = url.port_or_known_default().ok_or_else(|| {
                    "MCP HTTP endpoint has no supported effective port".to_owned()
                })?;
                if url.scheme() != egress.scheme
                    || url.host_str() != Some(egress.host.as_str())
                    || port != egress.port
                {
                    return Err("MCP HTTP endpoint and egress grant differ".to_owned());
                }
            }
            (McpTransportConfig::Http { .. }, None) => {
                return Err("MCP HTTP transport requires an exact egress grant".to_owned());
            }
            (
                McpTransportConfig::Stdio {
                    owned_process_profile,
                    argv,
                    profile,
                    profile_digest,
                    environment,
                },
                None,
            ) if !owned_process_profile.is_empty()
                && owned_process_profile.len() <= 256
                && owned_process_profile
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic())
                && valid_stdio_argv(argv)
                && environment.len() <= 64
                && environment.iter().all(|(variable, credential)| {
                    !variable.is_empty()
                        && variable.len() <= 256
                        && variable
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                        && credential
                            .handle
                            .identifier()
                            .strip_prefix("env:")
                            .is_some_and(|name| {
                                !name.is_empty()
                                    && name
                                        .bytes()
                                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                            })
                        && match &credential.credential_scope {
                            McpCredentialScopeConfig::Project => true,
                            McpCredentialScopeConfig::Workspace { workspace_id } => self
                                .owner
                                .workspace_id
                                .is_none_or(|owner| owner == *workspace_id),
                        }
                })
                && crate::executor::profile::ExecutorProfile::new(profile.as_ref().clone())
                    .is_ok_and(|value| value.digest().to_string() == *profile_digest) => {}
            (McpTransportConfig::Stdio { .. }, None) => {
                return Err("MCP stdio owned-process profile is invalid".to_owned());
            }
            (McpTransportConfig::Stdio { .. }, Some(_)) => {
                return Err("MCP stdio transport cannot carry an egress grant".to_owned());
            }
        }
        if self.descriptors.is_empty() || self.descriptors.len() > 4096 {
            return Err("MCP server requires 1 to 4096 descriptor policies".to_owned());
        }
        let mut descriptors = BTreeSet::new();
        for descriptor in &self.descriptors {
            if descriptor.remote.is_empty()
                || descriptor.remote.len() > 16 * 1024
                || descriptor.remote.chars().any(char::is_control)
                || !descriptor
                    .required_grants
                    .contains(&descriptor.effect.required_grant())
                || descriptor.auth_scopes.len() > 64
                || descriptor.auth_scopes.iter().any(|scope| {
                    scope.is_empty()
                        || scope.len() > 512
                        || scope.bytes().any(|byte| !byte.is_ascii_graphic())
                })
            {
                return Err("MCP descriptor policy is invalid".to_owned());
            }
            if !descriptors.insert((
                descriptor.kind,
                descriptor.remote.as_str(),
                descriptor.descriptor_digest,
            )) {
                return Err("duplicate MCP descriptor policy".to_owned());
            }
        }
        Ok(())
    }
}

fn valid_stdio_argv(argv: &[String]) -> bool {
    !argv.is_empty()
        && argv.len() <= 64
        && std::path::Path::new(&argv[0]).is_absolute()
        && argv
            .iter()
            .try_fold(0_usize, |total, argument| {
                if argument.is_empty() || argument.len() > 4096 || argument.contains('\0') {
                    None
                } else {
                    total.checked_add(argument.len())
                }
            })
            .is_some_and(|total| total <= 64 * 1024)
}

impl McpOwnerConfig {
    fn authorizes(
        &self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        workspace_id: WorkspaceId,
    ) -> bool {
        self.principal_id == principal_id
            && self.project_id == project_id
            && self
                .workspace_id
                .is_none_or(|configured| configured == workspace_id)
    }
}

pub struct McpBootstrapContext<'a> {
    pub authenticated: &'a AuthenticatedPrincipal,
    pub config: &'a RunConfigSnapshot,
    pub workspace_id: WorkspaceId,
    pub workspace_revision: &'a str,
    pub attempt: AttemptOwnership,
    pub claim: AttemptDriverClaim,
    pub current_fence: Arc<std::sync::atomic::AtomicU64>,
    pub cancellation: Arc<std::sync::atomic::AtomicBool>,
    pub budget: Arc<BudgetLedger>,
    pub occurred_at: &'a crate::domain::events::UtcDateTime,
    pub resolved_auth:
        &'a BTreeMap<String, crate::agent::driver::restart::ResolvedMcpBootstrapAuth>,
    pub stdio_profiles: Option<&'a dyn crate::protocols::mcp::transport::OwnedStdioProfileProvider>,
    pub stdio_secrets: &'a [Arc<crate::domain::secret::SecretLease>],
}

pub enum McpBootstrapOutcome {
    Ready(Arc<crate::protocols::mcp::transport::McpCapabilityRuntime>),
    AuthRequired(Box<crate::capabilities::broker::AuthChallenge>),
}

#[derive(Debug)]
pub enum McpBootstrapError {
    Invalid(String),
    StdioServiceUnavailable {
        profile: String,
    },
    Cleanup {
        primary: String,
        cleanup: Vec<String>,
    },
}

impl std::fmt::Display for McpBootstrapError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(error) => formatter.write_str(error),
            Self::StdioServiceUnavailable { profile } => {
                write!(
                    formatter,
                    "MCP owned-process service is unavailable for profile {profile:?}"
                )
            }
            Self::Cleanup { primary, cleanup } => {
                formatter.write_str(primary)?;
                for error in cleanup {
                    write!(formatter, "; cleanup: {error}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for McpBootstrapError {}

impl From<String> for McpBootstrapError {
    fn from(error: String) -> Self {
        Self::Invalid(error)
    }
}

enum BootstrapStepError {
    Error(McpBootstrapError),
    AuthRequired(Box<crate::capabilities::broker::AuthChallenge>),
}

impl From<String> for BootstrapStepError {
    fn from(error: String) -> Self {
        Self::Error(error.into())
    }
}

impl From<McpBootstrapError> for BootstrapStepError {
    fn from(error: McpBootstrapError) -> Self {
        Self::Error(error)
    }
}

async fn cleanup_bootstrap_connections(
    connections: &mut Vec<Arc<crate::protocols::mcp::transport::ReadyConnection>>,
    store: &mut crate::store::sqlite::append::SqliteStore,
) -> Vec<String> {
    let mut errors = Vec::new();
    while let Some(connection) = connections.pop() {
        if let Err(error) = connection.close_owned(store).await {
            errors.push(error.to_string());
        }
    }
    errors
}

fn aggregate_bootstrap_error(
    error: impl Into<McpBootstrapError>,
    cleanup: Vec<String>,
) -> McpBootstrapError {
    let error = error.into();
    if cleanup.is_empty() {
        error
    } else {
        McpBootstrapError::Cleanup {
            primary: error.to_string(),
            cleanup,
        }
    }
}

async fn await_bootstrap<T, E>(
    cancellation: &std::sync::atomic::AtomicBool,
    future: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, BootstrapStepError>
where
    E: std::fmt::Display,
{
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            result = &mut future => {
                return result
                    .map_err(|error| BootstrapStepError::Error(error.to_string().into()));
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                if cancellation.load(std::sync::atomic::Ordering::Acquire) {
                    cancellation.store(true, std::sync::atomic::Ordering::Release);
                    return match future.await {
                        Ok(_) => Err(BootstrapStepError::Error(
                            "MCP bootstrap cancelled".to_owned().into(),
                        )),
                        Err(error) => Err(BootstrapStepError::Error(
                            format!("MCP bootstrap cancelled: {error}").into(),
                        )),
                    };
                }
            }
        }
    }
}

pub(crate) async fn bootstrap(
    servers: &[McpServerConfig],
    context: &McpBootstrapContext<'_>,
    store: &mut crate::store::sqlite::append::SqliteStore,
) -> Result<McpBootstrapOutcome, McpBootstrapError> {
    use crate::{
        capabilities::{
            catalog::{CatalogSnapshot, CatalogSource, SourceKind, TrustDomain},
            kernel::{
                grant_ext::{EgressConstraint, GrantExtension, RequestExtension},
                identity::{
                    CapabilityNamespace, CapabilitySource, CapabilityVersion, DigestAlgorithm,
                },
            },
        },
        domain::egress::{CredentialHandle, DestinationGrant, EgressPolicy},
        protocols::mcp::{
            features::{McpCatalog, McpCatalogConfig, McpCatalogPolicy, McpCatalogPolicyKey},
            transport::{
                McpRuntimeServer, StreamableHttpOutcome, TransportLimits, connect_configured_stdio,
                connect_configured_streamable_http, resolve_configured_streamable_http_auth,
                resume_configured_streamable_http,
            },
        },
    };

    let mut runtime_servers = Vec::with_capacity(servers.len());
    let mut opened = Vec::with_capacity(servers.len());
    for configured in servers {
        let server = async {
            configured.validate()?;
            if !configured.owner.authorizes(
                context.authenticated.principal_id(),
                context.config.project_id(),
                context.workspace_id,
            ) {
                return Err(format!(
                    "MCP server {:?} owner does not authorize this run",
                    configured.id
                )
                .into());
            }
            if let Some(McpCredentialScopeConfig::Workspace { workspace_id }) =
                configured.credential_scope
                && workspace_id != context.workspace_id
            {
                return Err(format!(
                    "MCP server {:?} credential does not authorize this workspace",
                    configured.id
                )
                .into());
            }
            let (extension, grant_extension, connection) = match &configured.transport {
                McpTransportConfig::Http { endpoint } => {
                    let credential = configured.credential_handle.clone().ok_or_else(|| {
                        format!("MCP server {:?} has no credential handle", configured.id)
                    })?;
                    let egress = configured.egress.as_ref().ok_or_else(|| {
                        format!("MCP server {:?} has no egress grant", configured.id)
                    })?;
                    let egress_constraint = EgressConstraint::new(
                        &egress.scheme,
                        &egress.host,
                        egress.port,
                        credential.clone(),
                    )
                    .map_err(|_| format!("MCP server {:?} has invalid egress", configured.id))?;
                    let extension = RequestExtension::new(
                        Some(egress_constraint.clone()),
                        Some(credential.clone()),
                    )
                    .with_workspace_revision(context.workspace_revision);
                    let lifecycle =
                        crate::capabilities::broker::OwnedBrokerInvocation::run_lifecycle(
                            &configured.id,
                            context.authenticated,
                            context.config,
                            context.workspace_id,
                            extension.clone(),
                            context.attempt,
                            context.claim,
                            Arc::clone(&context.current_fence),
                            Arc::clone(&context.cancellation),
                            context.occurred_at.clone(),
                        )
                        .map_err(|error| {
                            format!(
                                "MCP server {:?} lifecycle authority: {error}",
                                configured.id
                            )
                        })?;
                    let handle = CredentialHandle::new(credential.identifier().to_owned())
                        .map_err(|error| error.to_string())?;
                    let grant = DestinationGrant::new(
                        &egress.scheme,
                        &egress.host,
                        egress.port,
                        handle.clone(),
                    )
                    .map_err(|error| error.to_string())?;
                    let policy = EgressPolicy::new([grant]);
                    let request = lifecycle.invocation();
                    let connection = if let Some(resolved) =
                        context.resolved_auth.get(&configured.id)
                    {
                        resolve_configured_streamable_http_auth(
                            &configured.id,
                            endpoint,
                            &request,
                            context.authenticated,
                            if resolved.granted {
                                crate::capabilities::broker::AuthResolution::Granted
                            } else {
                                crate::capabilities::broker::AuthResolution::Denied
                            },
                            resolved,
                            store,
                        )
                        .map_err(|error| {
                            format!("MCP server {:?} auth resume: {error}", configured.id)
                        })?;
                        if !resolved.granted {
                            return Err(format!(
                                "MCP server {:?} authorization was denied",
                                configured.id
                            )
                            .into());
                        }
                        Arc::new(
                            await_bootstrap(
                                context.cancellation.as_ref(),
                                resume_configured_streamable_http(
                                    &configured.id,
                                    endpoint,
                                    &request,
                                    &policy,
                                    Arc::new(
                                        crate::protocols::mcp::transport::EnvironmentHttpCredentialBroker,
                                    ),
                                    store,
                                    TransportLimits::default(),
                                ),
                            )
                            .await
                            .map_err(|error| match error {
                                BootstrapStepError::Error(error) => BootstrapStepError::Error(
                                    format!("MCP server {:?} auth replay: {error}", configured.id)
                                        .into(),
                                ),
                                error => error,
                            })?,
                        )
                    } else {
                        match await_bootstrap(
                            context.cancellation.as_ref(),
                            connect_configured_streamable_http(
                                &configured.id,
                                endpoint,
                                &request,
                                &policy,
                                Arc::new(
                                    crate::protocols::mcp::transport::EnvironmentHttpCredentialBroker,
                                ),
                                store,
                                TransportLimits::default(),
                            ),
                        )
                        .await?
                        {
                            StreamableHttpOutcome::Ready(connection) => Arc::new(*connection),
                            StreamableHttpOutcome::AuthRequired(challenge) => {
                                return Err(BootstrapStepError::AuthRequired(Box::new(
                                    challenge.challenge,
                                )));
                            }
                        }
                    };
                    opened.push(Arc::clone(&connection));
                    let grant_extension = GrantExtension::new([egress_constraint], [credential], 0)
                        .map_err(|_| "MCP grant extension is invalid".to_owned())?;
                    (extension, grant_extension, connection)
                }
                McpTransportConfig::Stdio {
                    owned_process_profile,
                    environment,
                    ..
                } => {
                    let profiles = context.stdio_profiles.ok_or_else(|| {
                        McpBootstrapError::StdioServiceUnavailable {
                            profile: owned_process_profile.clone(),
                        }
                    })?;
                    let scanner_leases = context
                        .stdio_secrets
                        .iter()
                        .map(|secret| {
                            crate::domain::secret::SecretLease::new(secret.expose().to_vec())
                        })
                        .collect::<Vec<_>>();
                    for credential in environment.values() {
                        if let McpCredentialScopeConfig::Workspace { workspace_id } =
                            &credential.credential_scope
                            && *workspace_id != context.workspace_id
                        {
                            return Err(format!(
                                "MCP server {:?} stdio credential does not authorize this workspace",
                                configured.id
                            )
                            .into());
                        }
                    }
                    let credential = configured.credential_handle.clone();
                    let credentials = credential
                        .iter()
                        .cloned()
                        .chain(environment.values().map(|value| value.handle.clone()))
                        .collect::<BTreeSet<_>>();
                    let extension = RequestExtension::new(None, credential.clone())
                        .with_credentials(credentials.iter().cloned())
                        .map_err(|_| "MCP stdio request extension is invalid".to_owned())?
                        .with_workspace_revision(context.workspace_revision);
                    let lifecycle =
                        crate::capabilities::broker::OwnedBrokerInvocation::run_lifecycle(
                            &configured.id,
                            context.authenticated,
                            context.config,
                            context.workspace_id,
                            extension.clone(),
                            context.attempt,
                            context.claim,
                            Arc::clone(&context.current_fence),
                            Arc::clone(&context.cancellation),
                            context.occurred_at.clone(),
                        )
                        .map_err(|error| {
                            format!(
                                "MCP server {:?} lifecycle authority: {error}",
                                configured.id
                            )
                        })?;
                    let request = lifecycle.invocation();
                    let connection = Arc::new(
                        await_bootstrap(
                            context.cancellation.as_ref(),
                            connect_configured_stdio(
                                &configured.id,
                                owned_process_profile,
                                &request,
                                || {
                                    let mut launcher = profiles.prepare(
                                        owned_process_profile,
                                        context.attempt,
                                        &credentials,
                                    )?;
                                    launcher.add_scanner(
                                        crate::telemetry::redact::CaptureRedactor::new(
                                            &scanner_leases,
                                        )
                                        .scanner(),
                                    );
                                    Ok(launcher)
                                },
                                store,
                                TransportLimits::default(),
                            ),
                        )
                        .await?,
                    );
                    opened.push(Arc::clone(&connection));
                    let grant_extension = GrantExtension::new([], credentials, 0)
                        .map_err(|_| "MCP stdio grant extension is invalid".to_owned())?;
                    (extension, grant_extension, connection)
                }
            };
            let discovered = await_bootstrap(
                context.cancellation.as_ref(),
                connection.discover_features_owned(store),
            )
            .await
            .map_err(|error| match error {
                BootstrapStepError::Error(error) => BootstrapStepError::Error(
                    format!("MCP server {:?} discovery: {error}", configured.id).into(),
                ),
                error => error,
            })?;
            let mut policies = BTreeMap::new();
            for configured_policy in &configured.descriptors {
                let normalized = discovered
                    .tools()
                    .iter()
                    .filter(|descriptor| {
                        configured_policy.kind == CapabilityKind::Tool
                            && descriptor.name() == configured_policy.remote
                    })
                    .find_map(|descriptor| descriptor.normalize().ok())
                    .or_else(|| {
                        discovered
                            .resources()
                            .iter()
                            .filter(|descriptor| {
                                configured_policy.kind == CapabilityKind::Resource
                                    && descriptor.uri() == configured_policy.remote
                            })
                            .find_map(|descriptor| descriptor.normalize().ok())
                    })
                    .or_else(|| {
                        discovered
                            .resource_templates()
                            .iter()
                            .filter(|descriptor| {
                                configured_policy.kind == CapabilityKind::ResourceTemplate
                                    && descriptor.uri_template() == configured_policy.remote
                            })
                            .find_map(|descriptor| descriptor.normalize().ok())
                    })
                    .or_else(|| {
                        discovered
                            .prompts()
                            .iter()
                            .filter(|descriptor| {
                                configured_policy.kind == CapabilityKind::Prompt
                                    && descriptor.name() == configured_policy.remote
                            })
                            .find_map(|descriptor| descriptor.normalize().ok())
                    })
                    .ok_or_else(|| {
                        format!(
                            "MCP server {:?} did not discover configured descriptor {:?}",
                            configured.id, configured_policy.remote
                        )
                    })?;
                if normalized.descriptor_digest() != configured_policy.descriptor_digest {
                    return Err(format!(
                        "MCP server {:?} descriptor {:?} digest changed",
                        configured.id, configured_policy.remote
                    )
                    .into());
                }
                let mut policy = McpCatalogPolicy::new(
                    configured_policy.effect,
                    configured_policy.retry_safety,
                    configured_policy.required_grants.iter().copied(),
                    configured_policy.auth_scopes.iter().cloned(),
                    configured_policy.availability,
                );
                if let Some(credential) = &configured.credential_handle {
                    policy = policy.with_credential(credential.clone());
                }
                policies.insert(
                    McpCatalogPolicyKey::new(
                        normalized.identity().clone(),
                        normalized.kind(),
                        normalized.descriptor_digest(),
                    ),
                    policy,
                );
            }
            let identity =
                ConfiguredServerIdentity::new(&configured.id).map_err(|error| error.to_string())?;
            let catalog = McpCatalogConfig::new(
                identity,
                CatalogSource::new(
                    SourceKind::Mcp,
                    CapabilitySource::new(configured.source.clone())
                        .map_err(|error| error.to_string())?,
                    TrustDomain::new(&configured.trust_domain)
                        .map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?,
                CapabilityNamespace::new(configured.namespace.clone())
                    .map_err(|error| error.to_string())?,
                CapabilityVersion::new(configured.version.clone())
                    .map_err(|error| error.to_string())?,
                policies,
            )
            .map_err(|error| error.to_string())?;
            Ok(McpRuntimeServer::new(catalog, discovered, connection)
                .map_err(|error| error.to_string())?
                .with_authority(grant_extension, extension))
        };
        match server.await {
            Ok(server) => runtime_servers.push(server),
            Err(BootstrapStepError::Error(error)) => {
                let cleanup = cleanup_bootstrap_connections(&mut opened, store).await;
                return Err(aggregate_bootstrap_error(error, cleanup));
            }
            Err(BootstrapStepError::AuthRequired(challenge)) => {
                let cleanup = cleanup_bootstrap_connections(&mut opened, store).await;
                if cleanup.is_empty() {
                    return Ok(McpBootstrapOutcome::AuthRequired(challenge));
                }
                return Err(aggregate_bootstrap_error(
                    "MCP bootstrap authorization required but connection cleanup failed".to_owned(),
                    cleanup,
                ));
            }
        }
    }
    let snapshot =
        CatalogSnapshot::new([], DigestAlgorithm::Sha256).map_err(|error| error.to_string())?;
    let runtime =
        match crate::protocols::mcp::transport::McpCapabilityRuntime::from_configured_servers(
            McpCatalog::new(snapshot),
            runtime_servers,
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                let cleanup = cleanup_bootstrap_connections(&mut opened, store).await;
                return Err(aggregate_bootstrap_error(error.to_string(), cleanup));
            }
        };
    opened.clear();
    Ok(McpBootstrapOutcome::Ready(Arc::new(runtime)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    async fn cancellation_drains_phase() {
        let cancellation = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(AtomicUsize::new(1));
        let session = Arc::new(AtomicUsize::new(1));
        let process = Arc::new(AtomicUsize::new(1));
        let reservation = Arc::new(AtomicUsize::new(1));
        let signal = Arc::clone(&cancellation);
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            signal.store(true, Ordering::Release);
        });
        let result = await_bootstrap(cancellation.as_ref(), async {
            while !cancellation.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            gate.store(0, Ordering::Release);
            session.store(0, Ordering::Release);
            process.store(0, Ordering::Release);
            reservation.store(0, Ordering::Release);
            Err::<(), _>("cancelled")
        })
        .await;
        assert!(result.is_err());
        assert_eq!(gate.load(Ordering::Acquire), 0);
        assert_eq!(session.load(Ordering::Acquire), 0);
        assert_eq!(process.load(Ordering::Acquire), 0);
        assert_eq!(reservation.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn cancel_during_http_initialize_drains_bootstrap() {
        cancellation_drains_phase().await;
    }

    #[tokio::test]
    async fn cancel_during_discovery_page_drains_bootstrap() {
        cancellation_drains_phase().await;
    }

    #[tokio::test]
    async fn cancel_during_stdio_initialize_drains_bootstrap() {
        cancellation_drains_phase().await;
    }
}
