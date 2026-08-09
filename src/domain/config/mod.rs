use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use super::crypto::sha256;
use super::ids::{PrincipalId, ProjectId, RunId};

pub const CONFIG_SCHEMA_VERSION: u32 = 2;
pub const GRAMMAR_EDIT_EXPERIMENT_ID: &str = "m004-w09.grammar-edit";
pub const GRAMMAR_EDIT_EXPERIMENT_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerKind {
    BuiltIn,
    User,
    Project,
    Run,
    Experiment,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigField {
    MaxTokens,
    MaxCostMicrousd,
    MaxTurns,
    MaxConcurrentRuns,
    MaxConcurrentTools,
    EventRetentionDays,
    ArtifactRetentionDays,
    Provider,
    Executor,
    GrammarEditExperiment,
    Grants,
}

impl ConfigField {
    pub const ALL: [Self; 11] = [
        Self::MaxTokens,
        Self::MaxCostMicrousd,
        Self::MaxTurns,
        Self::MaxConcurrentRuns,
        Self::MaxConcurrentTools,
        Self::EventRetentionDays,
        Self::ArtifactRetentionDays,
        Self::Provider,
        Self::Executor,
        Self::GrammarEditExperiment,
        Self::Grants,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::MaxTokens => "budgets.max_tokens",
            Self::MaxCostMicrousd => "budgets.max_cost_microusd",
            Self::MaxTurns => "budgets.max_turns",
            Self::MaxConcurrentRuns => "concurrency.max_runs",
            Self::MaxConcurrentTools => "concurrency.max_tools",
            Self::EventRetentionDays => "retention.event_days",
            Self::ArtifactRetentionDays => "retention.artifact_days",
            Self::Provider => "provider",
            Self::Executor => "executor",
            Self::GrammarEditExperiment => "experiments.grammar_edit",
            Self::Grants => "grants",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Anthropic,
    OpenAi,
    OpenRouter,
    Ollama,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Executor {
    Local,
    RestrictedContainer,
    IsolatedVm,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    ModelCall,
    WorkspaceRead,
    WorkspaceWrite,
    ProcessSpawn,
    NetworkEgress,
    VerificationTargeted,
    VerificationFull,
    HostProcessCompatibility,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedGrammarEditPolicy {
    #[default]
    Fail,
    OrdinaryOutput,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrammarEditExperiment {
    pub version: u16,
    pub enabled: bool,
    pub unsupported_provider: UnsupportedGrammarEditPolicy,
}

impl GrammarEditExperiment {
    pub const fn disabled() -> Self {
        Self {
            version: GRAMMAR_EDIT_EXPERIMENT_VERSION,
            enabled: false,
            unsupported_provider: UnsupportedGrammarEditPolicy::Fail,
        }
    }

    pub fn digest(&self) -> String {
        let bytes =
            serde_json::to_vec(self).expect("experiment config serialization is infallible");
        format!("sha256:{}", hex(&sha256(&bytes)))
    }
}

impl Default for GrammarEditExperiment {
    fn default() -> Self {
        Self::disabled()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetLayer {
    pub max_tokens: Option<u64>,
    pub max_cost_microusd: Option<u64>,
    pub max_turns: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConcurrencyLayer {
    pub max_runs: Option<u32>,
    pub max_tools: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionLayer {
    pub event_days: Option<u32>,
    pub artifact_days: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigLayer {
    pub schema_version: u32,
    #[serde(default)]
    pub budgets: BudgetLayer,
    #[serde(default)]
    pub concurrency: ConcurrencyLayer,
    #[serde(default)]
    pub retention: RetentionLayer,
    #[serde(default)]
    pub provider: Option<Provider>,
    #[serde(default)]
    pub executor: Option<Executor>,
    #[serde(default)]
    pub grammar_edit: Option<GrammarEditExperiment>,
    #[serde(default)]
    pub grants: Option<BTreeSet<Grant>>,
}

impl ConfigLayer {
    pub fn empty() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            budgets: BudgetLayer::default(),
            concurrency: ConcurrencyLayer::default(),
            retention: RetentionLayer::default(),
            provider: None,
            executor: None,
            grammar_edit: None,
            grants: None,
        }
    }

    pub fn safe_defaults() -> Self {
        Self::safe_defaults_for(Provider::OpenAi)
    }

    pub fn safe_defaults_for(provider: Provider) -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            budgets: BudgetLayer {
                // Cumulative across the run, and every turn re-counts the
                // full transcript it sends: a real coding-agent run over a
                // large repository spends 100k+ tokens per turn across tens
                // of turns. 1M proved too small the first time kit ran a
                // real task against its own repository.
                max_tokens: Some(20_000_000),
                max_cost_microusd: Some(100_000_000),
                max_turns: Some(1_000),
            },
            concurrency: ConcurrencyLayer {
                max_runs: Some(8),
                max_tools: Some(64),
            },
            retention: RetentionLayer {
                event_days: Some(30),
                artifact_days: Some(30),
            },
            provider: Some(provider),
            executor: Some(Executor::RestrictedContainer),
            grammar_edit: Some(GrammarEditExperiment::disabled()),
            grants: Some(
                [
                    Grant::ModelCall,
                    Grant::WorkspaceRead,
                    Grant::WorkspaceWrite,
                    Grant::ProcessSpawn,
                    Grant::NetworkEgress,
                    Grant::VerificationTargeted,
                ]
                .into_iter()
                .collect(),
            ),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LayerStack {
    pub built_in: ConfigLayer,
    #[serde(default)]
    pub user: Option<ConfigLayer>,
    #[serde(default)]
    pub project: Option<ConfigLayer>,
    #[serde(default)]
    pub run: Option<ConfigLayer>,
    #[serde(default)]
    pub experiment: Option<ConfigLayer>,
}

impl LayerStack {
    pub fn safe_defaults() -> Self {
        Self::safe_defaults_for(Provider::OpenAi)
    }

    pub fn safe_defaults_for(provider: Provider) -> Self {
        Self {
            built_in: ConfigLayer::safe_defaults_for(provider),
            user: None,
            project: None,
            run: None,
            experiment: None,
        }
    }
}

pub trait RunConfigMaterializer {
    fn layers(&self, context: RunConfigContext) -> Result<LayerStack, ConfigError>;

    fn materialize(
        &self,
        context: RunConfigContext,
        authenticated_grants: &BTreeSet<Grant>,
        run: Option<ConfigLayer>,
        experiment: Option<ConfigLayer>,
    ) -> Result<RunConfigSnapshot, ConfigError> {
        let mut layers = self.layers(context)?;
        layers.run = run;
        layers.experiment = experiment;
        layers.materialize(context, authenticated_grants)
    }
}

#[derive(Clone, Debug)]
pub struct StaticRunConfigMaterializer {
    layers: LayerStack,
}

impl StaticRunConfigMaterializer {
    pub fn new(layers: LayerStack) -> Self {
        Self { layers }
    }

    pub fn for_provider(provider: Provider) -> Self {
        Self::new(LayerStack::safe_defaults_for(provider))
    }
}

impl Default for StaticRunConfigMaterializer {
    fn default() -> Self {
        Self::new(LayerStack::safe_defaults())
    }
}

impl RunConfigMaterializer for StaticRunConfigMaterializer {
    fn layers(&self, _context: RunConfigContext) -> Result<LayerStack, ConfigError> {
        Ok(self.layers.clone())
    }
}

impl RunConfigMaterializer for LayerStack {
    fn layers(&self, _context: RunConfigContext) -> Result<LayerStack, ConfigError> {
        Ok(self.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunConfigContext {
    pub principal_id: PrincipalId,
    pub project_id: ProjectId,
    pub run_id: RunId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveConfig {
    pub max_tokens: u64,
    pub max_cost_microusd: u64,
    pub max_turns: u32,
    pub max_concurrent_runs: u32,
    pub max_concurrent_tools: u32,
    pub event_retention_days: u32,
    pub artifact_retention_days: u32,
    pub provider: Provider,
    pub executor: Executor,
    pub grammar_edit: GrammarEditExperiment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    UnsupportedSchemaVersion { layer: LayerKind, found: u32 },
    MissingBuiltInField(ConfigField),
    InvalidRange { field: ConfigField, value: u64 },
    GrantExpansion { layer: LayerKind, grant: Grant },
    InvalidExperimentLayer(LayerKind),
    UnsupportedExperimentVersion(u16),
    GrammarEditReleaseDisabled,
    InvalidCanonicalSnapshot(&'static str),
    UnsupportedSnapshotVersion(u32),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion { layer, found } => {
                write!(f, "unsupported {layer:?} config schema version {found}")
            }
            Self::MissingBuiltInField(field) => {
                write!(f, "built-in config is missing {}", field.name())
            }
            Self::InvalidRange { field, value } => {
                write!(f, "{} has invalid value {value}", field.name())
            }
            Self::GrantExpansion { layer, grant } => {
                write!(f, "{layer:?} config attempts to expand grant {grant:?}")
            }
            Self::InvalidExperimentLayer(layer) => {
                write!(
                    f,
                    "grammar edit experiment cannot be set by {layer:?} config"
                )
            }
            Self::UnsupportedExperimentVersion(version) => {
                write!(f, "unsupported grammar edit experiment version {version}")
            }
            Self::GrammarEditReleaseDisabled => {
                f.write_str("grammar edit activation is disabled in release builds")
            }
            Self::InvalidCanonicalSnapshot(reason) => {
                write!(f, "invalid canonical config snapshot: {reason}")
            }
            Self::UnsupportedSnapshotVersion(version) => {
                write!(f, "unsupported config snapshot version {version}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunConfigSnapshot {
    version: u32,
    principal_id: PrincipalId,
    project_id: ProjectId,
    run_id: RunId,
    effective: EffectiveConfig,
    provenance: BTreeMap<ConfigField, LayerKind>,
    effective_authority: BTreeSet<Grant>,
    digest: [u8; 32],
}

pub type EffectiveConfigSnapshot = RunConfigSnapshot;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectiveConfigReference {
    pub digest: String,
    pub experiment_identity: String,
    pub experiment_digest: String,
    pub provenance: BTreeMap<ConfigField, LayerKind>,
}

impl RunConfigSnapshot {
    pub fn reference(&self) -> EffectiveConfigReference {
        EffectiveConfigReference {
            digest: format!("sha256:{}", self.digest_hex()),
            experiment_identity: GRAMMAR_EDIT_EXPERIMENT_ID.to_owned(),
            experiment_digest: self.grammar_edit_experiment_digest(),
            provenance: self.provenance.clone(),
        }
    }
    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn effective(&self) -> &EffectiveConfig {
        &self.effective
    }

    pub fn provenance(&self) -> &BTreeMap<ConfigField, LayerKind> {
        &self.provenance
    }

    pub fn effective_authority(&self) -> &BTreeSet<Grant> {
        &self.effective_authority
    }

    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub fn digest_hex(&self) -> String {
        hex(&self.digest)
    }

    pub fn grammar_edit_experiment_digest(&self) -> String {
        self.effective.grammar_edit.digest()
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"KCFG");
        put_u32(&mut bytes, self.version);
        put_string(&mut bytes, &self.principal_id.to_string());
        put_string(&mut bytes, &self.project_id.to_string());
        put_string(&mut bytes, &self.run_id.to_string());
        put_u64(&mut bytes, self.effective.max_tokens);
        put_u64(&mut bytes, self.effective.max_cost_microusd);
        put_u32(&mut bytes, self.effective.max_turns);
        put_u32(&mut bytes, self.effective.max_concurrent_runs);
        put_u32(&mut bytes, self.effective.max_concurrent_tools);
        put_u32(&mut bytes, self.effective.event_retention_days);
        put_u32(&mut bytes, self.effective.artifact_retention_days);
        bytes.push(self.effective.provider.tag());
        bytes.push(self.effective.executor.tag());
        put_u16(&mut bytes, self.effective.grammar_edit.version);
        bytes.push(u8::from(self.effective.grammar_edit.enabled));
        bytes.push(self.effective.grammar_edit.unsupported_provider.tag());
        put_u16(&mut bytes, self.effective_authority.len() as u16);
        bytes.extend(self.effective_authority.iter().map(|grant| grant.tag()));
        for field in ConfigField::ALL {
            bytes.push(self.provenance[&field].tag());
        }
        bytes
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, ConfigError> {
        let mut reader = Reader::new(bytes);
        if reader.take(4)? != b"KCFG" {
            return Err(ConfigError::InvalidCanonicalSnapshot("bad magic"));
        }
        let version = reader.u32()?;
        if version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSnapshotVersion(version));
        }
        let principal_id = reader
            .string()?
            .parse()
            .map_err(|_| ConfigError::InvalidCanonicalSnapshot("invalid principal id"))?;
        let project_id = reader
            .string()?
            .parse()
            .map_err(|_| ConfigError::InvalidCanonicalSnapshot("invalid project id"))?;
        let run_id = reader
            .string()?
            .parse()
            .map_err(|_| ConfigError::InvalidCanonicalSnapshot("invalid run id"))?;
        let effective = EffectiveConfig {
            max_tokens: reader.u64()?,
            max_cost_microusd: reader.u64()?,
            max_turns: reader.u32()?,
            max_concurrent_runs: reader.u32()?,
            max_concurrent_tools: reader.u32()?,
            event_retention_days: reader.u32()?,
            artifact_retention_days: reader.u32()?,
            provider: Provider::from_tag(reader.byte()?)?,
            executor: Executor::from_tag(reader.byte()?)?,
            grammar_edit: GrammarEditExperiment {
                version: reader.u16()?,
                enabled: match reader.byte()? {
                    0 => false,
                    1 => true,
                    _ => return Err(ConfigError::InvalidCanonicalSnapshot("invalid boolean")),
                },
                unsupported_provider: UnsupportedGrammarEditPolicy::from_tag(reader.byte()?)?,
            },
        };
        validate_effective(&effective)?;
        let grant_count = usize::from(reader.u16()?);
        let mut effective_authority = BTreeSet::new();
        for _ in 0..grant_count {
            if !effective_authority.insert(Grant::from_tag(reader.byte()?)?) {
                return Err(ConfigError::InvalidCanonicalSnapshot("duplicate grant"));
            }
        }
        let mut provenance = BTreeMap::new();
        for field in ConfigField::ALL {
            provenance.insert(field, LayerKind::from_tag(reader.byte()?)?);
        }
        if !reader.is_empty() {
            return Err(ConfigError::InvalidCanonicalSnapshot("trailing bytes"));
        }
        let mut snapshot = Self {
            version,
            principal_id,
            project_id,
            run_id,
            effective,
            provenance,
            effective_authority,
            digest: [0; 32],
        };
        snapshot.digest = sha256(&snapshot.canonical_bytes());
        Ok(snapshot)
    }
}

impl LayerStack {
    pub fn materialize(
        &self,
        context: RunConfigContext,
        authenticated_grants: &BTreeSet<Grant>,
    ) -> Result<RunConfigSnapshot, ConfigError> {
        validate_layer(&self.built_in, LayerKind::BuiltIn)?;
        let mut effective = EffectiveConfig {
            max_tokens: required(self.built_in.budgets.max_tokens, ConfigField::MaxTokens)?,
            max_cost_microusd: required(
                self.built_in.budgets.max_cost_microusd,
                ConfigField::MaxCostMicrousd,
            )?,
            max_turns: required(self.built_in.budgets.max_turns, ConfigField::MaxTurns)?,
            max_concurrent_runs: required(
                self.built_in.concurrency.max_runs,
                ConfigField::MaxConcurrentRuns,
            )?,
            max_concurrent_tools: required(
                self.built_in.concurrency.max_tools,
                ConfigField::MaxConcurrentTools,
            )?,
            event_retention_days: required(
                self.built_in.retention.event_days,
                ConfigField::EventRetentionDays,
            )?,
            artifact_retention_days: required(
                self.built_in.retention.artifact_days,
                ConfigField::ArtifactRetentionDays,
            )?,
            provider: required(self.built_in.provider, ConfigField::Provider)?,
            executor: required(self.built_in.executor, ConfigField::Executor)?,
            grammar_edit: required(
                self.built_in.grammar_edit,
                ConfigField::GrammarEditExperiment,
            )?,
        };
        validate_effective(&effective)?;
        let built_in_grants = self
            .built_in
            .grants
            .as_ref()
            .ok_or(ConfigError::MissingBuiltInField(ConfigField::Grants))?;
        let mut authority = authenticated_grants
            .intersection(built_in_grants)
            .copied()
            .collect::<BTreeSet<_>>();
        let mut provenance = ConfigField::ALL
            .into_iter()
            .map(|field| (field, LayerKind::BuiltIn))
            .collect::<BTreeMap<_, _>>();

        for (kind, layer) in [
            (LayerKind::User, self.user.as_ref()),
            (LayerKind::Project, self.project.as_ref()),
            (LayerKind::Run, self.run.as_ref()),
            (LayerKind::Experiment, self.experiment.as_ref()),
        ] {
            if let Some(layer) = layer {
                apply_layer(&mut effective, &mut authority, &mut provenance, layer, kind)?;
            }
        }

        let mut snapshot = RunConfigSnapshot {
            version: CONFIG_SCHEMA_VERSION,
            principal_id: context.principal_id,
            project_id: context.project_id,
            run_id: context.run_id,
            effective,
            provenance,
            effective_authority: authority,
            digest: [0; 32],
        };
        snapshot.digest = sha256(&snapshot.canonical_bytes());
        Ok(snapshot)
    }
}

fn required<T>(value: Option<T>, field: ConfigField) -> Result<T, ConfigError> {
    value.ok_or(ConfigError::MissingBuiltInField(field))
}

fn validate_layer(layer: &ConfigLayer, kind: LayerKind) -> Result<(), ConfigError> {
    if layer.schema_version != CONFIG_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedSchemaVersion {
            layer: kind,
            found: layer.schema_version,
        });
    }
    if layer.grammar_edit.is_some()
        && !matches!(
            kind,
            LayerKind::BuiltIn | LayerKind::Run | LayerKind::Experiment
        )
    {
        return Err(ConfigError::InvalidExperimentLayer(kind));
    }
    if let Some(experiment) = layer.grammar_edit {
        validate_experiment(experiment)?;
    }
    validate_optional_range(
        layer.budgets.max_tokens,
        ConfigField::MaxTokens,
        100_000_000,
    )?;
    validate_optional_range(
        layer.budgets.max_cost_microusd,
        ConfigField::MaxCostMicrousd,
        1_000_000_000_000,
    )?;
    validate_optional_range(layer.budgets.max_turns, ConfigField::MaxTurns, 1_000_000)?;
    validate_optional_range(
        layer.concurrency.max_runs,
        ConfigField::MaxConcurrentRuns,
        10_000,
    )?;
    validate_optional_range(
        layer.concurrency.max_tools,
        ConfigField::MaxConcurrentTools,
        10_000,
    )?;
    validate_optional_range(
        layer.retention.event_days,
        ConfigField::EventRetentionDays,
        36_500,
    )?;
    validate_optional_range(
        layer.retention.artifact_days,
        ConfigField::ArtifactRetentionDays,
        36_500,
    )?;
    Ok(())
}

fn validate_effective(config: &EffectiveConfig) -> Result<(), ConfigError> {
    validate_experiment(config.grammar_edit)?;
    validate_value(config.max_tokens, ConfigField::MaxTokens, 100_000_000)?;
    validate_value(
        config.max_cost_microusd,
        ConfigField::MaxCostMicrousd,
        1_000_000_000_000,
    )?;
    validate_value(config.max_turns, ConfigField::MaxTurns, 1_000_000)?;
    validate_value(
        config.max_concurrent_runs,
        ConfigField::MaxConcurrentRuns,
        10_000,
    )?;
    validate_value(
        config.max_concurrent_tools,
        ConfigField::MaxConcurrentTools,
        10_000,
    )?;
    validate_value(
        config.event_retention_days,
        ConfigField::EventRetentionDays,
        36_500,
    )?;
    validate_value(
        config.artifact_retention_days,
        ConfigField::ArtifactRetentionDays,
        36_500,
    )
}

fn validate_optional_range<T>(
    value: Option<T>,
    field: ConfigField,
    max: u64,
) -> Result<(), ConfigError>
where
    T: Into<u64>,
{
    value.map_or(Ok(()), |value| validate_value(value, field, max))
}

fn validate_value<T>(value: T, field: ConfigField, max: u64) -> Result<(), ConfigError>
where
    T: Into<u64>,
{
    let value = value.into();
    if value == 0 || value > max {
        Err(ConfigError::InvalidRange { field, value })
    } else {
        Ok(())
    }
}

fn apply_layer(
    effective: &mut EffectiveConfig,
    authority: &mut BTreeSet<Grant>,
    provenance: &mut BTreeMap<ConfigField, LayerKind>,
    layer: &ConfigLayer,
    kind: LayerKind,
) -> Result<(), ConfigError> {
    validate_layer(layer, kind)?;
    macro_rules! overlay {
        ($source:expr, $target:expr, $field:expr) => {
            if let Some(value) = $source {
                $target = value;
                provenance.insert($field, kind);
            }
        };
    }
    overlay!(
        layer.budgets.max_tokens,
        effective.max_tokens,
        ConfigField::MaxTokens
    );
    overlay!(
        layer.budgets.max_cost_microusd,
        effective.max_cost_microusd,
        ConfigField::MaxCostMicrousd
    );
    overlay!(
        layer.budgets.max_turns,
        effective.max_turns,
        ConfigField::MaxTurns
    );
    overlay!(
        layer.concurrency.max_runs,
        effective.max_concurrent_runs,
        ConfigField::MaxConcurrentRuns
    );
    overlay!(
        layer.concurrency.max_tools,
        effective.max_concurrent_tools,
        ConfigField::MaxConcurrentTools
    );
    overlay!(
        layer.retention.event_days,
        effective.event_retention_days,
        ConfigField::EventRetentionDays
    );
    overlay!(
        layer.retention.artifact_days,
        effective.artifact_retention_days,
        ConfigField::ArtifactRetentionDays
    );
    overlay!(layer.provider, effective.provider, ConfigField::Provider);
    overlay!(layer.executor, effective.executor, ConfigField::Executor);
    overlay!(
        layer.grammar_edit,
        effective.grammar_edit,
        ConfigField::GrammarEditExperiment
    );
    if let Some(requested) = &layer.grants {
        if let Some(grant) = requested.difference(authority).next() {
            return Err(ConfigError::GrantExpansion {
                layer: kind,
                grant: *grant,
            });
        }
        *authority = authority.intersection(requested).copied().collect();
        provenance.insert(ConfigField::Grants, kind);
    }
    Ok(())
}

impl LayerKind {
    const fn tag(self) -> u8 {
        match self {
            Self::BuiltIn => 0,
            Self::User => 1,
            Self::Project => 2,
            Self::Run => 3,
            Self::Experiment => 4,
        }
    }

    fn from_tag(tag: u8) -> Result<Self, ConfigError> {
        match tag {
            0 => Ok(Self::BuiltIn),
            1 => Ok(Self::User),
            2 => Ok(Self::Project),
            3 => Ok(Self::Run),
            4 => Ok(Self::Experiment),
            _ => Err(ConfigError::InvalidCanonicalSnapshot("invalid layer tag")),
        }
    }
}

macro_rules! tagged_enum {
    ($type:ty, $($tag:literal => $variant:path),+ $(,)?) => {
        impl $type {
            pub(crate) const fn tag(self) -> u8 {
                match self {
                    $($variant => $tag),+
                }
            }

            fn from_tag(tag: u8) -> Result<Self, ConfigError> {
                match tag {
                    $($tag => Ok($variant)),+,
                    _ => Err(ConfigError::InvalidCanonicalSnapshot("invalid enum tag")),
                }
            }
        }
    };
}

tagged_enum!(Provider, 0 => Provider::Anthropic, 1 => Provider::OpenAi, 2 => Provider::OpenRouter, 3 => Provider::Ollama);
tagged_enum!(Executor, 0 => Executor::Local, 1 => Executor::RestrictedContainer, 2 => Executor::IsolatedVm);
tagged_enum!(Grant, 0 => Grant::ModelCall, 1 => Grant::WorkspaceRead, 2 => Grant::WorkspaceWrite, 3 => Grant::ProcessSpawn, 4 => Grant::NetworkEgress, 5 => Grant::VerificationTargeted, 6 => Grant::VerificationFull, 7 => Grant::HostProcessCompatibility);
tagged_enum!(UnsupportedGrammarEditPolicy, 0 => UnsupportedGrammarEditPolicy::Fail, 1 => UnsupportedGrammarEditPolicy::OrdinaryOutput);

fn validate_experiment(experiment: GrammarEditExperiment) -> Result<(), ConfigError> {
    if experiment.version != GRAMMAR_EDIT_EXPERIMENT_VERSION {
        Err(ConfigError::UnsupportedExperimentVersion(
            experiment.version,
        ))
    } else if experiment.enabled && !cfg!(debug_assertions) {
        Err(ConfigError::GrammarEditReleaseDisabled)
    } else {
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn put_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_string(bytes: &mut Vec<u8>, value: &str) {
    put_u16(bytes, value.len() as u16);
    bytes.extend_from_slice(value.as_bytes());
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], ConfigError> {
        if self.remaining.len() < length {
            return Err(ConfigError::InvalidCanonicalSnapshot("truncated"));
        }
        let (value, remaining) = self.remaining.split_at(length);
        self.remaining = remaining;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, ConfigError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ConfigError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, ConfigError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, ConfigError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn string(&mut self) -> Result<&'a str, ConfigError> {
        let length = usize::from(self.u16()?);
        std::str::from_utf8(self.take(length)?)
            .map_err(|_| ConfigError::InvalidCanonicalSnapshot("invalid utf-8"))
    }

    fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }
}
