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
    #[serde(default, skip_serializing_if = "McpResponderConfig::is_disabled")]
    pub responders: McpResponderConfig,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpResponderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<McpSamplingResponderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<McpFormElicitationResponderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_elicitation: Option<McpUrlElicitationResponderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<McpRootsResponderConfig>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpSamplingResponderConfig {
    pub model_id: String,
    #[serde(default)]
    pub approval: McpSamplingApprovalMode,
    pub timeout_millis: u64,
    pub max_cost_microusd: u64,
    pub max_tokens: u32,
    pub max_messages: usize,
    pub max_content_items: usize,
    pub max_content_bytes: usize,
    pub max_output_bytes: usize,
    pub max_output_content_items: usize,
    pub max_system_prompt_bytes: usize,
    pub max_stop_sequences: usize,
    pub max_stop_sequence_bytes: usize,
    pub max_temperature: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<McpSamplingPricingPolicy>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpSamplingPricingPolicy {
    pub version: String,
    pub provider: String,
    pub model: String,
    pub tokenizer_bytes_per_token: u32,
    pub input: crate::agent::accounting::CostRate,
    pub cache_read: crate::agent::accounting::CostRate,
    pub cache_write: crate::agent::accounting::CostRate,
    pub output: crate::agent::accounting::CostRate,
    pub reasoning: crate::agent::accounting::CostRate,
    #[serde(default)]
    pub local_free: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSamplingApprovalMode {
    #[default]
    None,
    RequestOnly,
    RequestAndResponse,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpFormElicitationResponderConfig {
    pub timeout_millis: u64,
    pub max_message_bytes: usize,
    pub max_schema_bytes: usize,
    pub max_properties: usize,
    pub max_property_name_bytes: usize,
    pub max_response_bytes: usize,
    pub public_data_only: bool,
    pub safe_fields: BTreeSet<String>,
    pub allowed_schema: crate::protocols::mcp::responders::FormElicitationSchema,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpUrlElicitationResponderConfig {
    pub timeout_millis: u64,
    pub max_message_bytes: usize,
    pub max_url_bytes: usize,
    pub max_elicitation_id_bytes: usize,
    pub max_response_bytes: usize,
    pub allowed_origins: Vec<McpUrlOriginConfig>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpUrlOriginConfig {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpRootsResponderConfig {
    pub timeout_millis: u64,
    pub max_roots: usize,
    pub max_uri_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_shared_filesystem: Option<McpSharedFilesystemConfig>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpSharedFilesystemConfig {
    pub local_source: std::path::PathBuf,
    pub server_source: std::path::PathBuf,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirect_grants: Vec<McpRedirectGrantConfig>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpRedirectGrantConfig {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub credential_handle: SecretHandle,
    pub credential_scope: McpCredentialScopeConfig,
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
                let url = crate::domain::egress::EgressPolicy::canonical_url(endpoint)
                    .map_err(|_| "MCP HTTP endpoint is invalid".to_owned())?;
                if url.scheme() != "https" {
                    return Err("MCP credential-bearing HTTP endpoints must use HTTPS".to_owned());
                }
                let port = url.port_or_known_default().ok_or_else(|| {
                    "MCP HTTP endpoint has no supported effective port".to_owned()
                })?;
                let initial = crate::domain::egress::DestinationGrant::new(
                    &egress.scheme,
                    &egress.host,
                    egress.port,
                    crate::domain::egress::CredentialHandle::new(
                        self.credential_handle
                            .as_ref()
                            .expect("checked above")
                            .identifier(),
                    )
                    .map_err(|_| "MCP HTTP egress grant is invalid".to_owned())?,
                )
                .map_err(|_| "MCP HTTP egress grant is invalid".to_owned())?;
                let initial_destination = (
                    initial.destination().scheme(),
                    initial.destination().host(),
                    initial.destination().port(),
                );
                let exact = crate::domain::egress::EgressPolicy::new([initial]);
                exact
                    .grant_for_url(url.as_str())
                    .map_err(|_| "MCP HTTP endpoint is not a strict exact URL".to_owned())?;
                if port != egress.port {
                    return Err("MCP HTTP endpoint and egress grant differ".to_owned());
                }
                if egress.redirect_grants.len() > crate::domain::egress::MAX_REDIRECTS {
                    return Err("MCP HTTP redirect grant limit exceeded".to_owned());
                }
                let mut destinations = BTreeSet::from([initial_destination]);
                for redirect in &egress.redirect_grants {
                    validate_credential_handle(&redirect.credential_handle)?;
                    if redirect.scheme != "https"
                        || !credential_scope_authorizes_owner(
                            &redirect.credential_scope,
                            &self.owner,
                        )
                    {
                        return Err("MCP HTTP redirect grant is invalid or duplicate".to_owned());
                    }
                    let redirect_grant = crate::domain::egress::DestinationGrant::new(
                        &redirect.scheme,
                        &redirect.host,
                        redirect.port,
                        crate::domain::egress::CredentialHandle::new(
                            redirect.credential_handle.identifier(),
                        )
                        .map_err(|_| "MCP redirect credential is invalid".to_owned())?,
                    )
                    .map_err(|_| "MCP HTTP redirect grant is invalid or duplicate".to_owned())?;
                    if !destinations.insert((
                        redirect_grant.destination().scheme(),
                        redirect_grant.destination().host(),
                        redirect_grant.destination().port(),
                    )) {
                        return Err("MCP HTTP redirect grant is invalid or duplicate".to_owned());
                    }
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
        self.responders.validate(&self.transport)?;
        Ok(())
    }
}

impl McpResponderConfig {
    fn is_disabled(&self) -> bool {
        self.sampling.is_none()
            && self.elicitation.is_none()
            && self.url_elicitation.is_none()
            && self.roots.is_none()
    }

    fn validate(&self, transport: &McpTransportConfig) -> Result<(), String> {
        if let Some(policy) = &self.sampling
            && (!(1..=300_000).contains(&policy.timeout_millis)
                || policy.model_id.is_empty()
                || policy.model_id.len() > 256
                || policy.model_id.chars().any(char::is_control)
                || policy.pricing.as_ref().is_none_or(|pricing| {
                    !pricing.valid() || (policy.max_cost_microusd == 0 && !pricing.local_free)
                })
                || policy.max_tokens == 0
                || policy.max_tokens > 1_000_000
                || !bounded(policy.max_messages, 1024)
                || !bounded(policy.max_content_items, 64)
                || !bounded(policy.max_content_bytes, 1024 * 1024)
                || !(1024..=1024 * 1024).contains(&policy.max_output_bytes)
                || !bounded(policy.max_output_content_items, 64)
                || !bounded(policy.max_system_prompt_bytes, 1024 * 1024)
                || policy.max_stop_sequences > 64
                || !bounded(policy.max_stop_sequence_bytes, 16 * 1024)
                || !policy.max_temperature.is_finite()
                || !(0.0..=2.0).contains(&policy.max_temperature))
        {
            return Err("MCP sampling responder limits are invalid".to_owned());
        }
        if let Some(policy) = &self.elicitation
            && (!(1..=300_000).contains(&policy.timeout_millis)
                || !bounded(policy.max_message_bytes, 64 * 1024)
                || !bounded(policy.max_schema_bytes, 1024 * 1024)
                || !bounded(policy.max_properties, 256)
                || !bounded(policy.max_property_name_bytes, 256)
                || !(1024..=1024 * 1024).contains(&policy.max_response_bytes)
                || !policy.public_data_only
                || policy.allowed_schema.properties.is_empty()
                || policy.allowed_schema.properties.len() > policy.max_properties
                || !serde_json::to_vec(&policy.allowed_schema)
                    .is_ok_and(|schema| schema.len() <= policy.max_schema_bytes)
                || policy.allowed_schema.title.as_deref().is_some_and(|value| {
                    !crate::protocols::mcp::responders::public_form_text(value)
                })
                || policy
                    .allowed_schema
                    .description
                    .as_deref()
                    .is_some_and(|value| {
                        !crate::protocols::mcp::responders::public_form_text(value)
                    })
                || policy.safe_fields.len() != policy.allowed_schema.properties.len()
                || policy.safe_fields.iter().any(|name| {
                    name.len() > policy.max_property_name_bytes
                        || !crate::protocols::mcp::responders::public_form_text(name)
                })
                || policy
                    .allowed_schema
                    .properties
                    .keys()
                    .any(|name| !policy.safe_fields.contains(name))
                || policy
                    .allowed_schema
                    .properties
                    .iter()
                    .any(|(name, schema)| {
                        name.is_empty()
                            || name.len() > policy.max_property_name_bytes
                            || name.chars().any(char::is_control)
                            || !crate::protocols::mcp::responders::public_form_property(
                                name, schema,
                            )
                            || !crate::protocols::mcp::responders::supported_form_property(schema)
                    })
                || policy
                    .allowed_schema
                    .required
                    .as_ref()
                    .is_some_and(|required| {
                        let unique = required.iter().collect::<BTreeSet<_>>();
                        unique.len() != required.len()
                            || required
                                .iter()
                                .any(|name| !policy.allowed_schema.properties.contains_key(name))
                    }))
        {
            return Err("MCP form elicitation responder limits are invalid".to_owned());
        }
        if let Some(policy) = &self.url_elicitation {
            if !(1..=300_000).contains(&policy.timeout_millis)
                || !bounded(policy.max_message_bytes, 64 * 1024)
                || !bounded(
                    policy.max_url_bytes,
                    crate::domain::egress::MAX_EGRESS_URL_BYTES,
                )
                || !bounded(policy.max_elicitation_id_bytes, 4096)
                || !(1024..=64 * 1024).contains(&policy.max_response_bytes)
                || policy.allowed_origins.is_empty()
                || policy.allowed_origins.len() > crate::domain::egress::MAX_REDIRECTS + 1
            {
                return Err("MCP URL elicitation responder limits are invalid".to_owned());
            }
            let mut origins = BTreeSet::new();
            for origin in &policy.allowed_origins {
                let credential = crate::domain::egress::CredentialHandle::new("url:inert")
                    .expect("static credential handle");
                let grant = crate::domain::egress::DestinationGrant::new(
                    &origin.scheme,
                    &origin.host,
                    origin.port,
                    credential,
                )
                .map_err(|_| "MCP URL elicitation origin is invalid".to_owned())?;
                if grant.destination().scheme() != crate::domain::egress::Scheme::Https
                    || !origins.insert((
                        grant.destination().scheme(),
                        grant.destination().host(),
                        grant.destination().port(),
                    ))
                {
                    return Err("MCP URL elicitation origin is invalid or duplicate".to_owned());
                }
            }
        }
        if let Some(policy) = &self.roots {
            if !(1..=300_000).contains(&policy.timeout_millis)
                || policy.max_roots != 1
                || !bounded(policy.max_uri_bytes, 16 * 1024)
            {
                return Err("MCP roots responder limits are invalid".to_owned());
            }
            match (transport, &policy.http_shared_filesystem) {
                (McpTransportConfig::Stdio { .. }, None) => {}
                (McpTransportConfig::Stdio { .. }, Some(_)) => {
                    return Err("MCP stdio roots cannot configure an HTTP mapping".to_owned());
                }
                (McpTransportConfig::Http { .. }, None) => {}
                (McpTransportConfig::Http { .. }, Some(mapping))
                    if mapping.local_source.is_absolute()
                        && mapping.server_source.is_absolute()
                        && safe_absolute_path(&mapping.local_source)
                        && safe_absolute_path(&mapping.server_source) => {}
                (McpTransportConfig::Http { .. }, Some(_)) => {
                    return Err("MCP HTTP shared-filesystem mapping is invalid".to_owned());
                }
            }
        }
        Ok(())
    }
}

impl McpSamplingPricingPolicy {
    pub fn valid_for(&self, provider: &str, model: &str) -> bool {
        self.valid()
            && self.provider == provider
            && self.model == model
            && (!self.local_free || provider == "ollama")
    }

    fn valid(&self) -> bool {
        let rates = [
            self.input,
            self.cache_read,
            self.cache_write,
            self.output,
            self.reasoning,
        ];
        let all_zero = rates.iter().all(|rate| rate.currency_micros == 0);
        !self.version.is_empty()
            && self.version.len() <= 256
            && self.version.bytes().all(|byte| byte.is_ascii_graphic())
            && !self.provider.is_empty()
            && self.provider.len() <= 256
            && !self.model.is_empty()
            && self.model.len() <= 256
            && self.tokenizer_bytes_per_token > 0
            && rates.iter().all(|rate| rate.per_units > 0)
            && self.local_free == all_zero
    }
}

fn bounded(value: usize, maximum: usize) -> bool {
    (1..=maximum).contains(&value)
}

fn validate_credential_handle(handle: &SecretHandle) -> Result<(), String> {
    let identifier = handle.identifier();
    if identifier.strip_prefix("env:").is_some_and(|value| {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    }) {
        Ok(())
    } else {
        Err("MCP credential handle uses an unsupported opaque scheme".to_owned())
    }
}

fn credential_scope_authorizes_owner(
    scope: &McpCredentialScopeConfig,
    owner: &McpOwnerConfig,
) -> bool {
    match scope {
        McpCredentialScopeConfig::Project => true,
        McpCredentialScopeConfig::Workspace { workspace_id } => owner
            .workspace_id
            .is_none_or(|owner| owner == *workspace_id),
    }
}

fn safe_absolute_path(path: &std::path::Path) -> bool {
    path.components().all(|component| {
        matches!(
            component,
            std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::Normal(_)
        )
    })
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
    pub project_root: &'a std::path::Path,
    pub attempt: AttemptOwnership,
    pub claim: AttemptDriverClaim,
    pub current_fence: Arc<std::sync::atomic::AtomicU64>,
    pub current_claim_generation: Arc<std::sync::atomic::AtomicU64>,
    pub revision_live: Arc<std::sync::atomic::AtomicBool>,
    pub cancellation: Arc<std::sync::atomic::AtomicBool>,
    pub budget: Arc<BudgetLedger>,
    pub scheduler: crate::runtime::scheduler::DurableScheduler,
    pub responder_outcomes: &'a crate::protocols::mcp::responders::ResponderOutcomes,
    pub callback_database: &'a std::path::Path,
    pub artifacts: Arc<crate::store::artifacts::ArtifactStore>,
    pub claim_verifier: crate::protocols::mcp::responders::ClaimVerifier,
    pub occurred_at: &'a crate::domain::events::UtcDateTime,
    pub resolved_auth:
        &'a BTreeMap<String, crate::agent::driver::restart::ResolvedMcpBootstrapAuth>,
    pub stdio_profiles: Option<&'a dyn crate::protocols::mcp::transport::OwnedStdioProfileProvider>,
    pub resolved_secrets: &'a Arc<BTreeMap<SecretHandle, Arc<crate::domain::secret::SecretLease>>>,
    pub callback_secrets: &'a BTreeMap<String, Vec<Arc<crate::domain::secret::SecretLease>>>,
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
            let responder_authority = crate::protocols::mcp::responders::ResponderAuthority::new(
                context.attempt,
                context.claim,
                Arc::clone(&context.current_fence),
                Arc::clone(&context.current_claim_generation),
                {
                    let root = context.project_root.to_owned();
                    let expected = context.workspace_revision.to_owned();
                    Arc::new(move || {
                        crate::workspace::revision::ManagedWorkspace::open(&root)
                            .and_then(|workspace| workspace.current_revision())
                            .is_ok_and(|revision| revision.id().to_string() == expected)
                    })
                },
                configured.id.clone(),
                Arc::clone(&context.budget),
                Arc::clone(&context.cancellation),
                Arc::clone(&context.claim_verifier),
            )
            .with_scheduler(context.scheduler.clone());
            let root_proof = crate::protocols::mcp::responders::SourceRootProof::issue(
                configured,
                context.project_root,
            )?;
            let responder_outcomes = context
                .responder_outcomes
                .clone()
                .with_default_elicitation(
                    configured,
                    context.callback_database,
                    Arc::clone(&context.artifacts),
                    context.project_root,
                    context.authenticated.principal_id(),
                    context.config.project_id(),
                    context.attempt,
                    context.claim,
                    context.workspace_id,
                    context.workspace_revision,
                    context.config.effective().artifact_retention_days,
                    Arc::clone(&context.cancellation),
                )?;
            let responders = crate::protocols::mcp::responders::install(
                configured,
                responder_authority,
                &responder_outcomes,
                root_proof,
                TransportLimits::default().channel_capacity(),
            )?;
            let handler = responders.handler_config();
            let initialize_arguments = handler.initialize_arguments();
            let (extension, grant_extension, connection) = match &configured.transport {
                McpTransportConfig::Http { endpoint } => {
                    let credential = configured.credential_handle.clone().ok_or_else(|| {
                        format!("MCP server {:?} has no credential handle", configured.id)
                    })?;
                    let egress = configured.egress.as_ref().ok_or_else(|| {
                        format!("MCP server {:?} has no egress grant", configured.id)
                    })?;
                    let endpoint = EgressPolicy::canonical_url(endpoint)
                        .map_err(|_| format!("MCP server {:?} endpoint is invalid", configured.id))?
                        .to_string();
                    let egress_constraint = EgressConstraint::new(
                        &egress.scheme,
                        &egress.host,
                        egress.port,
                        credential.clone(),
                    )
                    .map_err(|_| format!("MCP server {:?} has invalid egress", configured.id))?;
                    let mut egress_constraints = vec![egress_constraint.clone()];
                    let mut destination_grants =
                        Vec::with_capacity(egress.redirect_grants.len().saturating_add(1));
                    let handle = CredentialHandle::new(credential.identifier().to_owned())
                        .map_err(|error| error.to_string())?;
                    destination_grants.push(
                        DestinationGrant::new(&egress.scheme, &egress.host, egress.port, handle)
                            .map_err(|error| error.to_string())?,
                    );
                    let mut credentials = BTreeSet::from([credential.clone()]);
                    let mut credential_scopes = BTreeMap::from([(
                        credential.clone(),
                        configured
                            .credential_scope
                            .clone()
                            .ok_or_else(|| "MCP HTTP credential scope is missing".to_owned())?,
                    )]);
                    for redirect in &egress.redirect_grants {
                        if let McpCredentialScopeConfig::Workspace { workspace_id } =
                            redirect.credential_scope
                            && workspace_id != context.workspace_id
                        {
                            return Err(format!(
                                "MCP server {:?} redirect credential does not authorize this workspace",
                                configured.id
                            )
                            .into());
                        }
                        credentials.insert(redirect.credential_handle.clone());
                        if credential_scopes
                            .insert(
                                redirect.credential_handle.clone(),
                                redirect.credential_scope.clone(),
                            )
                            .is_some_and(|scope| scope != redirect.credential_scope)
                        {
                            return Err(format!(
                                "MCP server {:?} credential has ambiguous scopes",
                                configured.id
                            )
                            .into());
                        }
                        egress_constraints.push(
                            EgressConstraint::new(
                                &redirect.scheme,
                                &redirect.host,
                                redirect.port,
                                redirect.credential_handle.clone(),
                            )
                            .map_err(|_| {
                                format!(
                                    "MCP server {:?} has invalid redirect egress",
                                    configured.id
                                )
                            })?,
                        );
                        destination_grants.push(
                            DestinationGrant::new(
                                &redirect.scheme,
                                &redirect.host,
                                redirect.port,
                                CredentialHandle::new(
                                    redirect.credential_handle.identifier().to_owned(),
                                )
                                .map_err(|error| error.to_string())?,
                            )
                            .map_err(|error| error.to_string())?,
                        );
                    }
                    let extension = RequestExtension::new(
                        Some(egress_constraint.clone()),
                        Some(credential.clone()),
                    )
                    .with_egresses(egress_constraints.iter().skip(1).cloned())
                    .map_err(|_| "MCP redirect egress extension is invalid".to_owned())?
                    .with_credentials(
                        credentials
                            .iter()
                            .filter(|value| *value != &credential)
                            .cloned(),
                    )
                    .map_err(|_| "MCP redirect credential extension is invalid".to_owned())?
                    .with_workspace_revision(context.workspace_revision);
                    let lifecycle =
                        crate::capabilities::broker::OwnedBrokerInvocation::run_lifecycle(
                            &configured.id,
                            initialize_arguments.clone(),
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
                    let policy = EgressPolicy::new(destination_grants);
                    let request = lifecycle.invocation();
                    let connection = if let Some(resolved) =
                        context.resolved_auth.get(&configured.id)
                    {
                        resolve_configured_streamable_http_auth(
                            &configured.id,
                            &endpoint,
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
                                    &endpoint,
                                    &request,
                                    &policy,
                                    Arc::new(
                                        crate::protocols::mcp::transport::EnvironmentHttpCredentialBroker::new(
                                             context.authenticated.principal_id(),
                                             context.config.project_id(),
                                             context.workspace_id,
                                             credential_scopes.clone(),
                                        ).with_callback_scanner(responders.secret_scanner()),
                                    ),
                                    store,
                                    TransportLimits::default(),
                                    handler.clone(),
                                ),
                            )
                            .await
                            .map_err(|error| match error {
                                BootstrapStepError::Error(error) => BootstrapStepError::Error(
                                    format!("MCP server {:?} auth replay: {error}", configured.id)
                                        .into(),
                                ),
                                error => error,
                            })?
                            .with_responders(responders.clone()),
                        )
                    } else {
                        match await_bootstrap(
                            context.cancellation.as_ref(),
                            connect_configured_streamable_http(
                                &configured.id,
                                &endpoint,
                                &request,
                                &policy,
                                Arc::new(
                                    crate::protocols::mcp::transport::EnvironmentHttpCredentialBroker::new(
                                         context.authenticated.principal_id(),
                                         context.config.project_id(),
                                         context.workspace_id,
                                         credential_scopes,
                                    ).with_callback_scanner(responders.secret_scanner()),
                                ),
                                store,
                                TransportLimits::default(),
                                handler.clone(),
                            ),
                        )
                        .await?
                        {
                            StreamableHttpOutcome::Ready(connection) => Arc::new(
                                (*connection).with_responders(responders.clone()),
                            ),
                            StreamableHttpOutcome::AuthRequired(challenge) => {
                                return Err(BootstrapStepError::AuthRequired(Box::new(
                                    challenge.challenge,
                                )));
                            }
                        }
                    };
                    opened.push(Arc::clone(&connection));
                    let grant_extension =
                        GrantExtension::new(egress_constraints, credentials, 0)
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
                        .callback_secrets
                        .get(&configured.id)
                        .ok_or_else(|| {
                            format!(
                                "MCP server {:?} callback secret scope is unavailable",
                                configured.id
                            )
                        })?
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
                            initialize_arguments.clone(),
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
                                        context.resolved_secrets,
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
                                handler.clone(),
                            ),
                        )
                        .await?
                        .with_responders(responders.clone()),
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

    #[test]
    fn responder_config_is_strict_and_deny_by_default() {
        let empty: McpResponderConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, McpResponderConfig::default());
        assert!(serde_json::from_str::<McpResponderConfig>(r#"{"url_elicitation":{}}"#).is_err());
        assert!(
            serde_json::from_str::<McpSamplingResponderConfig>(
                r#"{"timeout_millis":1,"max_tokens":1,"max_messages":1,"max_content_items":1,"max_content_bytes":1,"max_system_prompt_bytes":1,"max_stop_sequences":0,"max_stop_sequence_bytes":1,"max_temperature":1.0,"unknown":true}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn responder_limits_fail_closed() {
        let responders = McpResponderConfig {
            sampling: Some(McpSamplingResponderConfig {
                model_id: "test-model".to_owned(),
                approval: McpSamplingApprovalMode::None,
                timeout_millis: 0,
                max_cost_microusd: 1,
                max_tokens: 1,
                max_messages: 1,
                max_content_items: 1,
                max_content_bytes: 1,
                max_output_bytes: 1,
                max_output_content_items: 1,
                max_system_prompt_bytes: 1,
                max_stop_sequences: 0,
                max_stop_sequence_bytes: 1,
                max_temperature: 1.0,
                pricing: None,
            }),
            elicitation: None,
            url_elicitation: None,
            roots: None,
        };
        assert!(
            responders
                .validate(&McpTransportConfig::Http {
                    endpoint: "https://example.invalid/mcp".to_owned(),
                })
                .is_err()
        );
    }

    #[test]
    fn sampling_requires_complete_pinned_pricing() {
        let pricing = McpSamplingPricingPolicy {
            version: "price-v1".to_owned(),
            provider: "openai".to_owned(),
            model: "model".to_owned(),
            tokenizer_bytes_per_token: 4,
            input: crate::agent::accounting::CostRate::new(1, 1),
            cache_read: crate::agent::accounting::CostRate::new(1, 1),
            cache_write: crate::agent::accounting::CostRate::new(1, 1),
            output: crate::agent::accounting::CostRate::new(1, 1),
            reasoning: crate::agent::accounting::CostRate::new(1, 1),
            local_free: false,
        };
        assert!(pricing.valid_for("openai", "model"));
        assert!(!pricing.valid_for("openrouter", "model"));
        let mut missing = pricing;
        missing.tokenizer_bytes_per_token = 0;
        assert!(!missing.valid());

        let without_pricing = McpResponderConfig {
            sampling: Some(McpSamplingResponderConfig {
                model_id: "model".to_owned(),
                approval: McpSamplingApprovalMode::None,
                timeout_millis: 1_000,
                max_cost_microusd: 100,
                max_tokens: 32,
                max_messages: 4,
                max_content_items: 4,
                max_content_bytes: 1_024,
                max_output_bytes: 1_024,
                max_output_content_items: 4,
                max_system_prompt_bytes: 1_024,
                max_stop_sequences: 4,
                max_stop_sequence_bytes: 64,
                max_temperature: 1.0,
                pricing: None,
            }),
            ..McpResponderConfig::default()
        };
        assert!(
            without_pricing
                .validate(&McpTransportConfig::Http {
                    endpoint: "https://example.invalid/mcp".to_owned(),
                })
                .is_err()
        );

        let zero = crate::agent::accounting::CostRate::new(0, 1);
        let free_local = McpSamplingPricingPolicy {
            version: "local-free-v1".to_owned(),
            provider: "ollama".to_owned(),
            model: "llama".to_owned(),
            tokenizer_bytes_per_token: 4,
            input: zero,
            cache_read: zero,
            cache_write: zero,
            output: zero,
            reasoning: zero,
            local_free: true,
        };
        assert!(free_local.valid_for("ollama", "llama"));
        assert!(!free_local.valid_for("openai", "llama"));
    }

    #[test]
    fn elicitation_allowlist_rejects_unicode_secret_fields() {
        let field = "ѕecret".to_owned();
        let responders = McpResponderConfig {
            sampling: None,
            elicitation: Some(McpFormElicitationResponderConfig {
                timeout_millis: 100,
                max_message_bytes: 128,
                max_schema_bytes: 1024,
                max_properties: 1,
                max_property_name_bytes: 64,
                max_response_bytes: 1024,
                public_data_only: true,
                safe_fields: BTreeSet::from([field.clone()]),
                allowed_schema: serde_json::from_value(serde_json::json!({
                    "type": "object",
                    "properties": {(field): {"type": "string"}}
                }))
                .unwrap(),
            }),
            url_elicitation: None,
            roots: None,
        };
        assert!(
            responders
                .validate(&McpTransportConfig::Http {
                    endpoint: "https://example.invalid/mcp".to_owned(),
                })
                .is_err()
        );
    }

    #[test]
    fn url_elicitation_origins_are_exact_https_and_bounded() {
        let valid = McpResponderConfig {
            url_elicitation: Some(McpUrlElicitationResponderConfig {
                timeout_millis: 1_000,
                max_message_bytes: 256,
                max_url_bytes: 1024,
                max_elicitation_id_bytes: 128,
                max_response_bytes: 1024,
                allowed_origins: vec![McpUrlOriginConfig {
                    scheme: "https".to_owned(),
                    host: "auth.example.com".to_owned(),
                    port: 443,
                }],
            }),
            ..Default::default()
        };
        assert!(
            valid
                .validate(&McpTransportConfig::Http {
                    endpoint: "https://example.invalid/mcp".to_owned(),
                })
                .is_ok()
        );
        for (scheme, host, port) in [
            ("http", "auth.example.com", 80),
            ("https", "localhost", 443),
            ("https", "127.0.0.1", 443),
            ("https", "auth.example.com", 22),
        ] {
            let mut invalid = valid.clone();
            let origin = &mut invalid.url_elicitation.as_mut().unwrap().allowed_origins[0];
            origin.scheme = scheme.to_owned();
            origin.host = host.to_owned();
            origin.port = port;
            assert!(
                invalid
                    .validate(&McpTransportConfig::Http {
                        endpoint: "https://example.invalid/mcp".to_owned(),
                    })
                    .is_err()
            );
        }
    }
}
