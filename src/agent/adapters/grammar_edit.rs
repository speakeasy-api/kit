use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{fmt, future::Future, pin::Pin};

use agentkit_core::{FinishReason, Item, ItemKind, MetadataMap, Part, TurnCancellation};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult,
    SessionConfig, StructuredOutputCapability, StructuredOutputEvidence, StructuredOutputRequest,
    TurnRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    domain::config::{
        GRAMMAR_EDIT_EXPERIMENT_ID, GrammarEditExperiment, RunConfigSnapshot,
        UnsupportedGrammarEditPolicy,
    },
    workspace::edit::{
        EditTrace, EditTraceId,
        ir::{EditIr, EditLimits, RevisionToken},
        normalize::{
            ModelEditFormat, NormalizationContext, NormalizeError, normalize_with_trace,
            structured_edit_schema,
        },
    },
};

pub const EDIT_OUTPUT_SCHEMA_ID: &str = "kit.edit-ir-input";
pub const EDIT_OUTPUT_SCHEMA_VERSION: u16 = 1;
pub const GRAMMAR_EDIT_INTENT_METADATA: &str = "kit.grammar_edit.intent";
pub const GRAMMAR_EDIT_OUTCOME_METADATA: &str = "kit.grammar_edit.outcome";
const GRAMMAR_EDIT_CONTRACT_METADATA: &str = "kit.grammar_edit.contract";

#[derive(Clone)]
pub struct GrammarEditContext {
    workspace: crate::workspace::revision::ManagedWorkspace,
    normalization: NormalizationContext,
    workspace_digest: String,
}

impl GrammarEditContext {
    pub fn open(
        root: impl AsRef<std::path::Path>,
        limits: EditLimits,
    ) -> Result<Self, GrammarEditError> {
        let root = std::fs::canonicalize(root)
            .map_err(|error| GrammarEditError::Workspace(error.to_string()))?;
        let workspace = crate::workspace::revision::ManagedWorkspace::open(&root)
            .map_err(|error| GrammarEditError::Workspace(error.to_string()))?;
        let revision = workspace
            .current_revision()
            .map_err(|error| GrammarEditError::Workspace(error.to_string()))?;
        let expected_revision = RevisionToken::parse(revision.id().to_string())
            .map_err(|error| GrammarEditError::Workspace(error.to_string()))?;
        Ok(Self {
            workspace,
            normalization: NormalizationContext::new(expected_revision, limits),
            workspace_digest: revision.digest().to_string(),
        })
    }

    pub(crate) fn from_workspace(
        workspace: crate::workspace::revision::ManagedWorkspace,
        limits: EditLimits,
    ) -> Result<Self, GrammarEditError> {
        let revision = workspace
            .current_revision()
            .map_err(|error| GrammarEditError::Workspace(error.to_string()))?;
        let expected_revision = RevisionToken::parse(revision.id().to_string())
            .map_err(|error| GrammarEditError::Workspace(error.to_string()))?;
        Ok(Self {
            workspace,
            normalization: NormalizationContext::new(expected_revision, limits),
            workspace_digest: revision.digest().to_string(),
        })
    }

    pub fn expected_revision(&self) -> &RevisionToken {
        self.normalization.expected_revision()
    }

    pub fn workspace_digest(&self) -> &str {
        &self.workspace_digest
    }

    fn require_current_revision(&self) -> Result<(), GrammarEditError> {
        let current = self
            .workspace
            .current_revision()
            .map_err(|error| GrammarEditError::Workspace(error.to_string()))?;
        if current.id().to_string() != self.expected_revision().as_str()
            || current.digest().as_str() != self.workspace_digest
        {
            return Err(GrammarEditError::RevisionChanged);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GrammarEditLimits {
    pub max_schema_bytes: usize,
    pub max_output_bytes: usize,
    pub edit: EditLimits,
}

impl Default for GrammarEditLimits {
    fn default() -> Self {
        Self {
            max_schema_bytes: 64 * 1024,
            max_output_bytes: 64 * 1024 * 1024,
            edit: EditLimits::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditOutputMode {
    Ordinary,
    Constrained,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrammarEditIntentEvidence {
    pub experiment_identity: String,
    pub experiment_digest: String,
    pub selected_mode: EditOutputMode,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub capability_version: Option<String>,
    pub schema_digest: Option<String>,
    pub schema_name: Option<String>,
    pub schema_version: Option<u16>,
    pub schema_strict: Option<bool>,
    pub request_session_id: String,
    pub request_turn_id: String,
    pub expected_revision: String,
    pub workspace_digest: String,
    pub fallback_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrammarEditOutcomeEvidence {
    pub intent: GrammarEditIntentEvidence,
    pub honored: bool,
    pub structured: bool,
    pub result: String,
}

pub(crate) fn valid_outcome_evidence(evidence: &GrammarEditOutcomeEvidence) -> bool {
    let digest = evidence
        .intent
        .experiment_digest
        .strip_prefix("sha256:")
        .is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
    if evidence.intent.experiment_identity != GRAMMAR_EDIT_EXPERIMENT_ID
        || !digest
        || evidence
            .intent
            .provider
            .as_ref()
            .is_none_or(String::is_empty)
        || evidence.intent.model.as_ref().is_none_or(String::is_empty)
        || evidence.result != "accepted"
    {
        return false;
    }
    (match evidence.intent.selected_mode {
        EditOutputMode::Constrained => {
            evidence.honored
                && evidence.structured
                && evidence.intent.fallback_reason.is_none()
                && evidence
                    .intent
                    .capability_version
                    .as_ref()
                    .is_some_and(|version| !version.is_empty())
                && evidence
                    .intent
                    .schema_digest
                    .as_ref()
                    .is_some_and(|digest| digest.starts_with("sha256:") && digest.len() == 71)
                && evidence.intent.schema_name.as_deref() == Some(EDIT_OUTPUT_SCHEMA_ID)
                && evidence.intent.schema_version == Some(EDIT_OUTPUT_SCHEMA_VERSION)
                && evidence.intent.schema_strict == Some(true)
        }
        EditOutputMode::Ordinary => {
            !evidence.honored
                && !evidence.structured
                && evidence.intent.schema_digest.is_none()
                && evidence.intent.schema_name.is_none()
                && evidence.intent.schema_version.is_none()
                && evidence.intent.schema_strict.is_none()
                && evidence
                    .intent
                    .fallback_reason
                    .as_deref()
                    .is_none_or(|reason| reason == "unsupported_provider_model")
        }
    }) && RevisionToken::parse(&evidence.intent.expected_revision).is_ok()
        && evidence.intent.workspace_digest.starts_with("blake3:")
        && !evidence.intent.request_session_id.is_empty()
        && !evidence.intent.request_turn_id.is_empty()
}

#[derive(Clone, Debug)]
pub struct AcceptedEditOutput {
    mode: EditOutputMode,
    bytes: Vec<u8>,
    evidence: GrammarEditOutcomeEvidence,
}

impl AcceptedEditOutput {
    pub fn evidence(&self) -> &GrammarEditOutcomeEvidence {
        &self.evidence
    }
}

pub struct GrammarEditModelAdapter<M> {
    inner: M,
    config: RunConfigSnapshot,
    limits: GrammarEditLimits,
    context: Option<GrammarEditContext>,
}

impl<M> GrammarEditModelAdapter<M> {
    pub fn new(
        inner: M,
        config: RunConfigSnapshot,
        limits: GrammarEditLimits,
        context: Option<GrammarEditContext>,
    ) -> Self {
        Self {
            inner,
            config,
            limits,
            context,
        }
    }
}

impl<M> ModelAdapter for GrammarEditModelAdapter<M>
where
    M: ModelAdapter,
{
    type Session = GrammarEditSession<M::Session>;

    fn start_session<'life0, 'async_trait>(
        &'life0 self,
        config: SessionConfig,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Session, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let provider = self.inner.provider_name().map(str::to_owned);
            let inner = self.inner.start_session(config).await?;
            let model = inner.model_name().map(str::to_owned);
            Ok(GrammarEditSession {
                inner,
                experiment: self.config.effective().grammar_edit,
                experiment_digest: self.config.grammar_edit_experiment_digest(),
                provider,
                model,
                limits: self.limits,
                context: self.context.clone(),
            })
        })
    }

    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}

pub struct GrammarEditSession<S> {
    inner: S,
    experiment: GrammarEditExperiment,
    experiment_digest: String,
    provider: Option<String>,
    model: Option<String>,
    limits: GrammarEditLimits,
    context: Option<GrammarEditContext>,
}

impl<S> GrammarEditSession<S>
where
    S: ModelSession,
{
    fn project(
        &self,
        request: &mut TurnRequest,
    ) -> Result<Option<GrammarEditIntentEvidence>, LoopError> {
        let Some(context) = &self.context else {
            return Ok(None);
        };
        validate_limits(self.limits).map_err(grammar_loop_error)?;
        let capability = self.inner.structured_output_capability();
        let schema = edit_ir_input_schema(self.limits, context.expected_revision())
            .map_err(grammar_loop_error)?;
        let mut evidence = GrammarEditIntentEvidence {
            experiment_identity: GRAMMAR_EDIT_EXPERIMENT_ID.to_owned(),
            experiment_digest: self.experiment_digest.clone(),
            selected_mode: EditOutputMode::Ordinary,
            provider: self.provider.clone(),
            model: self.model.clone(),
            capability_version: capability.map(|value| value.version().to_owned()),
            schema_digest: None,
            schema_name: None,
            schema_version: None,
            schema_strict: None,
            request_session_id: request.session_id.to_string(),
            request_turn_id: request.turn_id.to_string(),
            expected_revision: context.expected_revision().to_string(),
            workspace_digest: context.workspace_digest().to_owned(),
            fallback_reason: None,
        };
        request.structured_output = None;
        if self.experiment.enabled {
            let schema_bytes = serde_json::to_vec(&schema)
                .map_err(|_| grammar_loop_error(GrammarEditError::InvalidSchema))?;
            let compatible = capability.is_some_and(|capability| {
                capability.strict()
                    && schema_bytes.len() <= capability.max_schema_bytes()
                    && schema_bytes.len() <= self.limits.max_schema_bytes
            });
            if compatible {
                let structured = StructuredOutputRequest::new(
                    EDIT_OUTPUT_SCHEMA_ID,
                    EDIT_OUTPUT_SCHEMA_VERSION,
                    true,
                    schema.clone(),
                )
                .and_then(|request| request.with_max_output_bytes(self.limits.max_output_bytes))
                .map_err(|error| LoopError::Provider(error.to_string()))?;
                evidence.selected_mode = EditOutputMode::Constrained;
                evidence.schema_digest = Some(structured.schema_digest().to_owned());
                evidence.schema_name = Some(structured.name().to_owned());
                evidence.schema_version = Some(structured.version());
                evidence.schema_strict = Some(structured.strict());
                request.structured_output = Some(structured);
            } else if self.experiment.unsupported_provider
                == UnsupportedGrammarEditPolicy::OrdinaryOutput
            {
                evidence.fallback_reason = Some("unsupported_provider_model".to_owned());
            } else {
                return Err(grammar_loop_error(
                    GrammarEditError::UnsupportedProviderModel {
                        provider: self.provider.clone().unwrap_or_default(),
                        model: self.model.clone().unwrap_or_default(),
                    },
                ));
            }
        }
        request.transcript.retain(|item| {
            item.metadata
                .get(GRAMMAR_EDIT_CONTRACT_METADATA)
                .and_then(Value::as_bool)
                != Some(true)
        });
        let mut metadata = MetadataMap::new();
        metadata.insert(GRAMMAR_EDIT_CONTRACT_METADATA.to_owned(), Value::Bool(true));
        request.transcript.insert(
            0,
            Item::text(
                ItemKind::Developer,
                ordinary_edit_instruction(&schema, context.expected_revision())
                    .map_err(grammar_loop_error)?,
            )
            .with_metadata(metadata),
        );
        request.metadata.insert(
            GRAMMAR_EDIT_INTENT_METADATA.to_owned(),
            serde_json::to_value(&evidence)
                .map_err(|error| LoopError::Provider(error.to_string()))?,
        );
        Ok(Some(evidence))
    }
}

impl<S> ModelSession for GrammarEditSession<S>
where
    S: ModelSession,
{
    type Turn = GrammarEditTurn<S::Turn>;

    fn begin_turn<'life0, 'async_trait>(
        &'life0 mut self,
        mut request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let evidence = self.project(&mut request)?;
            let turn = self.inner.begin_turn(request, cancellation).await?;
            Ok(GrammarEditTurn {
                inner: turn,
                evidence,
                limits: self.limits,
                context: self.context.clone(),
            })
        })
    }

    fn prepare_turn(&mut self, request: &mut TurnRequest) -> Result<(), LoopError> {
        self.inner.prepare_turn(request)?;
        self.project(request).map(|_| ())
    }

    fn model_name(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn structured_output_capability(&self) -> Option<&StructuredOutputCapability> {
        self.inner.structured_output_capability()
    }
}

pub struct GrammarEditTurn<T> {
    inner: T,
    evidence: Option<GrammarEditIntentEvidence>,
    limits: GrammarEditLimits,
    context: Option<GrammarEditContext>,
}

impl<T> ModelTurn for GrammarEditTurn<T>
where
    T: ModelTurn,
{
    fn next_event<'life0, 'async_trait>(
        &'life0 mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<ModelTurnEvent>, LoopError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let event = self.inner.next_event(cancellation).await?;
            match event {
                Some(ModelTurnEvent::Finished(mut result)) => {
                    let (Some(context), Some(evidence)) = (&self.context, &self.evidence) else {
                        return Ok(Some(ModelTurnEvent::Finished(result)));
                    };
                    let outcome = match context
                        .require_current_revision()
                        .and_then(|()| classify_terminal(&result, evidence, self.limits, context))
                    {
                        Ok(output) => output.evidence,
                        Err(error) => GrammarEditOutcomeEvidence {
                            intent: evidence.clone(),
                            honored: false,
                            structured: result.output_items.iter().any(|item| {
                                item.parts
                                    .iter()
                                    .any(|part| matches!(part, Part::Structured(_)))
                            }),
                            result: error.code().to_owned(),
                        },
                    };
                    result.metadata.insert(
                        GRAMMAR_EDIT_OUTCOME_METADATA.to_owned(),
                        serde_json::to_value(&outcome)
                            .map_err(|error| LoopError::Provider(error.to_string()))?,
                    );
                    Ok(Some(ModelTurnEvent::Finished(result)))
                }
                event => Ok(event),
            }
        })
    }
}

fn classify_terminal(
    result: &ModelTurnResult,
    intent: &GrammarEditIntentEvidence,
    limits: GrammarEditLimits,
    context: &GrammarEditContext,
) -> Result<AcceptedEditOutput, GrammarEditError> {
    validate_limits(limits)?;
    if intent.expected_revision != context.expected_revision().as_str()
        || intent.workspace_digest != context.workspace_digest()
    {
        return Err(GrammarEditError::MalformedOutput);
    }
    match &result.finish_reason {
        FinishReason::Completed => {}
        FinishReason::Cancelled => return Err(GrammarEditError::Cancelled),
        FinishReason::Error => return Err(GrammarEditError::ProviderError),
        FinishReason::Other(_) => return Err(GrammarEditError::OtherFinish),
        FinishReason::Blocked => return Err(GrammarEditError::Refusal),
        FinishReason::MaxTokens => return Err(GrammarEditError::PartialStream),
        FinishReason::ToolCall => return Err(GrammarEditError::ToolMix),
    }
    let mut structured = Vec::new();
    let mut text = Vec::new();
    let mut tool_calls = 0usize;
    let mut unsupported_parts = 0usize;
    for part in result.output_items.iter().flat_map(|item| &item.parts) {
        match part {
            Part::Structured(value) => structured.push(value),
            Part::Text(value) => text.push(value.text.as_bytes()),
            Part::ToolCall(_) => tool_calls += 1,
            Part::Reasoning(_) => {}
            Part::Media(_) | Part::File(_) | Part::ToolResult(_) | Part::Custom(_) => {
                unsupported_parts += 1;
            }
        }
    }
    if tool_calls != 0 {
        return Err(GrammarEditError::ToolMix);
    }
    let (mode, bytes, honored, is_structured) = match intent.selected_mode {
        EditOutputMode::Constrained
            if structured.len() == 1 && text.is_empty() && unsupported_parts == 0 =>
        {
            let provider_evidence: StructuredOutputEvidence = result
                .metadata
                .get("agentkit.structured_output")
                .cloned()
                .ok_or(GrammarEditError::ConstraintNotHonored)
                .and_then(|value| {
                    serde_json::from_value(value)
                        .map_err(|_| GrammarEditError::ConstraintNotHonored)
                })?;
            let evidence_matches = provider_evidence.honored
                && provider_evidence.error.is_none()
                && Some(provider_evidence.name.as_str()) == intent.schema_name.as_deref()
                && Some(provider_evidence.version) == intent.schema_version
                && Some(provider_evidence.strict) == intent.schema_strict
                && Some(provider_evidence.schema_digest.as_str())
                    == intent.schema_digest.as_deref()
                && provider_evidence.session_id == intent.request_session_id
                && provider_evidence.turn_id == intent.request_turn_id;
            let schema = edit_ir_input_schema(limits, context.expected_revision())?;
            if !evidence_matches || structured[0].schema.as_ref() != Some(&schema) {
                return Err(GrammarEditError::ConstraintNotHonored);
            }
            (
                EditOutputMode::Constrained,
                serde_json::to_vec(&structured[0].value)
                    .map_err(|_| GrammarEditError::MalformedOutput)?,
                true,
                true,
            )
        }
        EditOutputMode::Constrained => return Err(GrammarEditError::ConstraintNotHonored),
        EditOutputMode::Ordinary
            if structured.is_empty() && text.len() == 1 && unsupported_parts == 0 =>
        {
            (EditOutputMode::Ordinary, text[0].to_vec(), false, false)
        }
        EditOutputMode::Ordinary
            if structured.len() == 1 && text.is_empty() && unsupported_parts == 0 =>
        {
            return Err(GrammarEditError::UnexpectedConstrainedOutput);
        }
        EditOutputMode::Ordinary => return Err(GrammarEditError::MalformedOutput),
    };
    if bytes.len() > limits.max_output_bytes {
        return Err(GrammarEditError::OutputLimit {
            actual: bytes.len(),
            limit: limits.max_output_bytes,
        });
    }
    crate::workspace::edit::normalize::normalize(
        ModelEditFormat::StructuredJson,
        &bytes,
        &context.normalization,
    )
    .map_err(GrammarEditError::SemanticOutput)?;
    Ok(AcceptedEditOutput {
        mode,
        bytes,
        evidence: GrammarEditOutcomeEvidence {
            intent: intent.clone(),
            honored,
            structured: is_structured,
            result: "accepted".to_owned(),
        },
    })
}

pub(crate) fn accepted_turn_result(
    result: &agentkit_loop::TurnResult,
    limits: GrammarEditLimits,
    context: &GrammarEditContext,
) -> Result<AcceptedEditOutput, GrammarEditError> {
    let evidence: GrammarEditOutcomeEvidence = result
        .metadata
        .get(GRAMMAR_EDIT_OUTCOME_METADATA)
        .cloned()
        .ok_or(GrammarEditError::MalformedOutput)
        .and_then(|value| {
            serde_json::from_value(value).map_err(|_| GrammarEditError::MalformedOutput)
        })?;
    if evidence.result != "accepted" {
        return Err(GrammarEditError::MalformedOutput);
    }
    let model = ModelTurnResult {
        finish_reason: result.finish_reason.clone(),
        output_items: result.items.clone(),
        usage: result.usage.clone(),
        metadata: result.metadata.clone(),
        model: evidence.intent.model.clone(),
        response_id: None,
    };
    context.require_current_revision()?;
    let accepted = classify_terminal(&model, &evidence.intent, limits, context)?;
    if accepted.evidence != evidence || !valid_outcome_evidence(&evidence) {
        return Err(GrammarEditError::MalformedOutput);
    }
    Ok(accepted)
}

pub(crate) fn normalize_accepted(
    output: &AcceptedEditOutput,
    ordinary_format: ModelEditFormat,
    context: &NormalizationContext,
    trace: &mut impl EditTrace,
) -> Result<EditIr, GrammarEditError> {
    let format = if output.mode == EditOutputMode::Constrained {
        ModelEditFormat::StructuredJson
    } else {
        ordinary_format
    };
    normalize_with_trace(format, &output.bytes, context, trace).map_err(Into::into)
}

pub(crate) struct EditOrchestrator;

impl EditOrchestrator {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute(
        output: &AcceptedEditOutput,
        ordinary_format: ModelEditFormat,
        context: &GrammarEditContext,
        authenticated: &crate::api::auth::contract::AuthenticatedPrincipal,
        grants: &crate::api::auth::contract::GrantSnapshot,
        config: &RunConfigSnapshot,
        artifacts: &crate::store::artifacts::ArtifactStore,
        syntax_executors: &mut [&mut crate::executor::syntax::SyntaxExecutor],
        trace: &mut impl EditTrace,
    ) -> Result<crate::workspace::edit::recovery::MaterializedEdit, EditOrchestrationError> {
        context.require_current_revision()?;
        let ir = normalize_accepted(output, ordinary_format, &context.normalization, trace)?;
        Self::execute_ir(
            ir,
            context,
            authenticated,
            grants,
            config,
            artifacts,
            None,
            syntax_executors,
            trace,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_native(
        input: &[u8],
        context: &GrammarEditContext,
        authenticated: &crate::api::auth::contract::AuthenticatedPrincipal,
        grants: &crate::api::auth::contract::GrantSnapshot,
        config: &RunConfigSnapshot,
        artifacts: &crate::store::artifacts::ArtifactStore,
        cancellation: &Arc<AtomicBool>,
        syntax_executors: &mut [&mut crate::executor::syntax::SyntaxExecutor],
        trace: &mut impl EditTrace,
    ) -> Result<crate::workspace::edit::recovery::MaterializedEdit, EditOrchestrationError> {
        let ir = normalize_with_trace(
            ModelEditFormat::StructuredJson,
            input,
            &context.normalization,
            trace,
        )
        .map_err(GrammarEditError::Normalize)?;
        Self::execute_native_ir(
            ir,
            context,
            authenticated,
            grants,
            config,
            artifacts,
            cancellation,
            syntax_executors,
            trace,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_native_ir(
        ir: EditIr,
        context: &GrammarEditContext,
        authenticated: &crate::api::auth::contract::AuthenticatedPrincipal,
        grants: &crate::api::auth::contract::GrantSnapshot,
        config: &RunConfigSnapshot,
        artifacts: &crate::store::artifacts::ArtifactStore,
        cancellation: &Arc<AtomicBool>,
        syntax_executors: &mut [&mut crate::executor::syntax::SyntaxExecutor],
        trace: &mut impl EditTrace,
    ) -> Result<crate::workspace::edit::recovery::MaterializedEdit, EditOrchestrationError> {
        ensure_not_cancelled(Some(cancellation))?;
        let authority =
            crate::workspace::edit::validate::AuthenticatedEditAuthority::from_authenticated(
                authenticated,
                grants,
                config.project_id(),
            )?;
        let plan = crate::workspace::edit::validate::validate_authorized_traced(
            &context.workspace,
            &ir,
            context.normalization.limits(),
            authority,
            trace,
        )?;
        ensure_not_cancelled(Some(cancellation))?;
        let syntax_requirements = plan
            .changed_files()
            .iter()
            .filter(|path| path.as_str().ends_with(".rs"))
            .map(|path| {
                crate::workspace::edit::format::SyntaxRequirement::new(
                    path.clone(),
                    "rust",
                    crate::workspace::edit::format::RUST_GRAMMAR_VERSION,
                    true,
                )
                .expect("static Rust syntax requirement is valid")
            })
            .collect::<Vec<_>>();
        let staged = crate::workspace::edit::stage::stage_traced(
            plan,
            crate::workspace::edit::stage::StageLimits {
                max_time: std::time::Duration::from_secs(120),
                ..crate::workspace::edit::stage::StageLimits::default()
            },
            &syntax_requirements,
            syntax_executors,
            trace,
        )?;
        ensure_not_cancelled(Some(cancellation))?;
        staged
            .materialize_traced(
                artifacts,
                crate::workspace::edit::recovery::MaterializeOptions::new(
                    crate::store::artifacts::ArtifactRetention::Forever,
                )
                .with_cancellation(Arc::clone(cancellation)),
                trace,
            )
            .map_err(Into::into)
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_ir(
        ir: EditIr,
        context: &GrammarEditContext,
        authenticated: &crate::api::auth::contract::AuthenticatedPrincipal,
        grants: &crate::api::auth::contract::GrantSnapshot,
        config: &RunConfigSnapshot,
        artifacts: &crate::store::artifacts::ArtifactStore,
        cancellation: Option<&AtomicBool>,
        syntax_executors: &mut [&mut crate::executor::syntax::SyntaxExecutor],
        trace: &mut impl EditTrace,
    ) -> Result<crate::workspace::edit::recovery::MaterializedEdit, EditOrchestrationError> {
        ensure_not_cancelled(cancellation)?;
        let authority =
            crate::workspace::edit::validate::AuthenticatedEditAuthority::from_authenticated(
                authenticated,
                grants,
                config.project_id(),
            )?;
        ensure_not_cancelled(cancellation)?;
        let plan = crate::workspace::edit::validate::validate_authorized_traced(
            &context.workspace,
            &ir,
            context.normalization.limits(),
            authority,
            trace,
        )?;
        ensure_not_cancelled(cancellation)?;
        let staged = crate::workspace::edit::stage::stage_traced(
            plan,
            crate::workspace::edit::stage::StageLimits::default(),
            &[],
            syntax_executors,
            trace,
        )?;
        ensure_not_cancelled(cancellation)?;
        staged
            .materialize_traced(
                artifacts,
                crate::workspace::edit::recovery::MaterializeOptions::new(
                    crate::store::artifacts::ArtifactRetention::Forever,
                ),
                trace,
            )
            .map_err(Into::into)
    }
}

fn ensure_not_cancelled(cancellation: Option<&AtomicBool>) -> Result<(), EditOrchestrationError> {
    if cancellation.is_some_and(|signal| signal.load(Ordering::Acquire)) {
        Err(EditOrchestrationError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum EditOrchestrationError {
    Grammar(GrammarEditError),
    Validation(crate::workspace::edit::validate::ValidationError),
    Stage(crate::workspace::edit::stage::StageError),
    Cancelled,
    Recovery(crate::workspace::edit::recovery::RecoveryError),
}

impl fmt::Display for EditOrchestrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Grammar(error) => error.fmt(formatter),
            Self::Validation(error) => error.fmt(formatter),
            Self::Stage(error) => error.fmt(formatter),
            Self::Cancelled => formatter.write_str("edit cancelled before publication"),
            Self::Recovery(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for EditOrchestrationError {}

impl From<GrammarEditError> for EditOrchestrationError {
    fn from(error: GrammarEditError) -> Self {
        Self::Grammar(error)
    }
}

impl From<crate::workspace::edit::validate::ValidationError> for EditOrchestrationError {
    fn from(error: crate::workspace::edit::validate::ValidationError) -> Self {
        Self::Validation(error)
    }
}

impl From<crate::workspace::edit::stage::StageError> for EditOrchestrationError {
    fn from(error: crate::workspace::edit::stage::StageError) -> Self {
        Self::Stage(error)
    }
}

impl From<crate::workspace::edit::recovery::RecoveryError> for EditOrchestrationError {
    fn from(error: crate::workspace::edit::recovery::RecoveryError) -> Self {
        Self::Recovery(error)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EditPathTrace {
    ids: Vec<&'static str>,
}

impl EditPathTrace {
    pub fn ids(&self) -> &[&'static str] {
        &self.ids
    }
}

impl EditTrace for EditPathTrace {
    fn emit(&mut self, id: EditTraceId) {
        self.ids.push(id.as_str());
    }
}

// The schema intentionally states portable shape constraints only. UTF-8 byte
// totals, path semantics, range consistency, and aggregate content limits are
// enforced by the same bounded semantic decoder used for ordinary output.
fn edit_ir_input_schema(
    limits: GrammarEditLimits,
    expected_revision: &RevisionToken,
) -> Result<Value, GrammarEditError> {
    validate_limits(limits)?;
    let schema = structured_edit_schema(EDIT_OUTPUT_SCHEMA_ID, expected_revision, limits.edit);
    let actual = serde_json::to_vec(&schema)
        .map_err(|_| GrammarEditError::InvalidSchema)?
        .len();
    if actual > limits.max_schema_bytes {
        Err(GrammarEditError::SchemaLimit {
            actual,
            limit: limits.max_schema_bytes,
        })
    } else {
        Ok(schema)
    }
}

fn ordinary_edit_instruction(
    schema: &Value,
    expected_revision: &RevisionToken,
) -> Result<String, GrammarEditError> {
    let schema = serde_json::to_string(schema).map_err(|_| GrammarEditError::InvalidSchema)?;
    Ok(format!(
        "Return exactly one JSON edit object and no prose, Markdown, or tool call. Its expected_revision must be {expected_revision}. The object must satisfy this JSON Schema, including operation and limit constraints: {schema}"
    ))
}

fn validate_limits(limits: GrammarEditLimits) -> Result<(), GrammarEditError> {
    if limits.max_schema_bytes == 0
        || limits.max_output_bytes == 0
        || limits.edit.max_input_bytes == 0
        || limits.edit.max_operations == 0
        || limits.edit.max_path_bytes == 0
        || limits.edit.max_content_bytes == 0
    {
        Err(GrammarEditError::InvalidLimits)
    } else {
        Ok(())
    }
}

fn grammar_loop_error(error: GrammarEditError) -> LoopError {
    LoopError::Provider(format!("grammar edit: {error}"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GrammarEditError {
    InvalidLimits,
    InvalidSchema,
    UnsupportedProviderModel { provider: String, model: String },
    SchemaLimit { actual: usize, limit: usize },
    OutputLimit { actual: usize, limit: usize },
    Workspace(String),
    RevisionChanged,
    Cancelled,
    ProviderError,
    OtherFinish,
    Refusal,
    PartialStream,
    ToolMix,
    ConstraintNotHonored,
    UnexpectedConstrainedOutput,
    MalformedOutput,
    SemanticOutput(NormalizeError),
    Normalize(NormalizeError),
}

impl fmt::Display for GrammarEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("invalid grammar edit limits"),
            Self::InvalidSchema => formatter.write_str("invalid grammar edit schema"),
            Self::UnsupportedProviderModel { provider, model } => write!(
                formatter,
                "provider/model {provider}/{model} lacks compatible constrained output"
            ),
            Self::SchemaLimit { actual, limit } => {
                write!(formatter, "edit schema bytes {actual} exceed limit {limit}")
            }
            Self::OutputLimit { actual, limit } => {
                write!(formatter, "edit output bytes {actual} exceed limit {limit}")
            }
            Self::Workspace(reason) => write!(formatter, "workspace context unavailable: {reason}"),
            Self::RevisionChanged => {
                formatter.write_str("workspace revision changed during model edit")
            }
            Self::Cancelled => formatter.write_str("provider cancelled edit output"),
            Self::ProviderError => formatter.write_str("provider failed edit output"),
            Self::OtherFinish => {
                formatter.write_str("provider returned an unsupported finish reason")
            }
            Self::Refusal => formatter.write_str("provider refused constrained edit output"),
            Self::PartialStream => formatter.write_str("provider edit output stream was partial"),
            Self::ToolMix => formatter.write_str("provider mixed edit output with tool calls"),
            Self::ConstraintNotHonored => {
                formatter.write_str("provider did not honor constrained output")
            }
            Self::UnexpectedConstrainedOutput => {
                formatter.write_str("ordinary request cannot claim constrained output")
            }
            Self::MalformedOutput => formatter.write_str("provider edit output was malformed"),
            Self::SemanticOutput(error) => {
                write!(formatter, "semantic edit output rejected: {error}")
            }
            Self::Normalize(error) => error.fmt(formatter),
        }
    }
}

impl GrammarEditError {
    const fn code(&self) -> &'static str {
        match self {
            Self::InvalidLimits => "invalid_limits",
            Self::InvalidSchema => "invalid_schema",
            Self::UnsupportedProviderModel { .. } => "unsupported_provider_model",
            Self::SchemaLimit { .. } => "schema_limit",
            Self::OutputLimit { .. } => "output_limit",
            Self::Workspace(_) => "workspace_error",
            Self::RevisionChanged => "revision_changed",
            Self::Cancelled => "cancelled",
            Self::ProviderError => "provider_error",
            Self::OtherFinish => "other_finish",
            Self::Refusal => "refusal",
            Self::PartialStream => "partial_stream",
            Self::ToolMix => "tool_mix",
            Self::ConstraintNotHonored => "constraint_not_honored",
            Self::UnexpectedConstrainedOutput => "unexpected_constrained_output",
            Self::MalformedOutput => "malformed_output",
            Self::SemanticOutput(_) => "semantic_output",
            Self::Normalize(_) => "normalize_error",
        }
    }
}

impl std::error::Error for GrammarEditError {}

impl From<NormalizeError> for GrammarEditError {
    fn from(error: NormalizeError) -> Self {
        Self::Normalize(error)
    }
}

#[cfg(all(test, debug_assertions, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::{collections::BTreeSet, fs};

    use serde_json::json;

    use super::*;
    use crate::{
        api::auth::contract::{AuthenticatedPrincipal, GrantSnapshot},
        domain::{
            config::{
                ConfigLayer, GRAMMAR_EDIT_EXPERIMENT_VERSION, Grant, LayerStack, RunConfigContext,
            },
            ids::{PrincipalId, ProjectId, RunId},
        },
        store::artifacts::ArtifactStore,
        workspace::{edit::ir::RevisionToken, revision::ManagedWorkspace},
    };

    fn snapshot(enabled: bool) -> RunConfigSnapshot {
        let principal_id = PrincipalId::generate().unwrap();
        let project_id = ProjectId::generate().unwrap();
        let grants = BTreeSet::from([Grant::ModelCall, Grant::WorkspaceWrite]);
        let mut run = ConfigLayer::empty();
        run.grammar_edit = Some(GrammarEditExperiment {
            version: GRAMMAR_EDIT_EXPERIMENT_VERSION,
            enabled,
            unsupported_provider: UnsupportedGrammarEditPolicy::Fail,
        });
        let mut layers = LayerStack::safe_defaults();
        layers.run = Some(run);
        layers
            .materialize(
                RunConfigContext {
                    principal_id,
                    project_id,
                    run_id: RunId::generate().unwrap(),
                },
                &grants,
            )
            .unwrap()
    }

    fn run(mode: EditOutputMode) -> (EditPathTrace, GrammarEditOutcomeEvidence) {
        let id = RunId::generate().unwrap();
        let root = std::env::temp_dir().join(format!("kit-grammar-orchestrator-{id}"));
        let artifact_root = std::env::temp_dir().join(format!("kit-grammar-artifacts-{id}"));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let workspace = ManagedWorkspace::open(&root).unwrap();
        let current = workspace.current_revision().unwrap();
        let revision = RevisionToken::parse(current.id().to_string()).unwrap();
        let context = GrammarEditContext {
            workspace: workspace.clone(),
            normalization: NormalizationContext::new(revision.clone(), EditLimits::default()),
            workspace_digest: current.digest().to_string(),
        };
        let value = json!({
            "version": 1,
            "expected_revision": revision.to_string(),
            "operations": [{
                "op": "add_file",
                "path": "new.txt",
                "content": {"encoding": "utf8", "newline": "lf", "text": "new", "final_newline": true},
                "executable": false
            }]
        });
        let config = snapshot(mode == EditOutputMode::Constrained);
        let intent = GrammarEditIntentEvidence {
            experiment_identity: GRAMMAR_EDIT_EXPERIMENT_ID.to_owned(),
            experiment_digest: config.grammar_edit_experiment_digest(),
            selected_mode: mode,
            provider: Some("fixture".to_owned()),
            model: Some("fixture-model".to_owned()),
            capability_version: (mode == EditOutputMode::Constrained)
                .then(|| "fixture.output-format.v1".to_owned()),
            schema_digest: (mode == EditOutputMode::Constrained)
                .then(|| format!("sha256:{}", "1".repeat(64))),
            schema_name: (mode == EditOutputMode::Constrained)
                .then(|| EDIT_OUTPUT_SCHEMA_ID.to_owned()),
            schema_version: (mode == EditOutputMode::Constrained)
                .then_some(EDIT_OUTPUT_SCHEMA_VERSION),
            schema_strict: (mode == EditOutputMode::Constrained).then_some(true),
            request_session_id: "session".to_owned(),
            request_turn_id: "turn".to_owned(),
            expected_revision: revision.to_string(),
            workspace_digest: current.digest().to_string(),
            fallback_reason: None,
        };
        let evidence = GrammarEditOutcomeEvidence {
            intent,
            honored: mode == EditOutputMode::Constrained,
            structured: mode == EditOutputMode::Constrained,
            result: "accepted".to_owned(),
        };
        let output = AcceptedEditOutput {
            mode,
            bytes: serde_json::to_vec(&value).unwrap(),
            evidence: evidence.clone(),
        };
        let grants = GrantSnapshot::new(
            config.principal_id(),
            config.project_id(),
            [Grant::ModelCall, Grant::WorkspaceWrite],
        );
        let authenticated = AuthenticatedPrincipal::from_grants(grants.clone());
        let artifacts = ArtifactStore::open(&artifact_root).unwrap();
        let mut trace = EditPathTrace::default();
        let materialized = EditOrchestrator::execute(
            &output,
            ModelEditFormat::StructuredJson,
            &context,
            &authenticated,
            &grants,
            &config,
            &artifacts,
            &mut [&mut crate::executor::syntax::SyntaxExecutor::debug(
                "text",
                crate::workspace::edit::format::NATIVE_TEXT_VERSION,
                crate::executor::syntax::DebugSyntaxAction::Pass(None),
            )],
            &mut trace,
        )
        .unwrap();
        assert!(!materialized.transaction_id().is_empty());
        drop(workspace);
        drop(artifacts);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(artifact_root).unwrap();
        (trace, evidence)
    }

    #[test]
    fn ordinary_and_constrained_use_the_same_complete_production_orchestrator() {
        let (ordinary, _) = run(EditOutputMode::Ordinary);
        let (constrained, _) = run(EditOutputMode::Constrained);
        assert_eq!(ordinary, constrained);
        assert_eq!(
            ordinary.ids(),
            [
                EditTraceId::Normalize.as_str(),
                EditTraceId::EditIrNew.as_str(),
                EditTraceId::Validate.as_str(),
                EditTraceId::Stage.as_str(),
                EditTraceId::Recovery.as_str(),
            ]
        );
    }

    #[test]
    fn generated_schema_and_decoder_are_differentially_consistent() {
        let id = RunId::generate().unwrap();
        let root = std::env::temp_dir().join(format!("kit-grammar-schema-{id}"));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let limits = GrammarEditLimits {
            edit: EditLimits {
                max_operations: 4,
                max_path_bytes: 32,
                max_content_bytes: 8,
                ..EditLimits::default()
            },
            ..GrammarEditLimits::default()
        };
        let context = GrammarEditContext::open(&root, limits.edit).unwrap();
        let schema = edit_ir_input_schema(limits, context.expected_revision()).unwrap();
        let validator = jsonschema::draft202012::options().build(&schema).unwrap();
        let digest = format!("blake3:{}", "a".repeat(64));
        let valid = [
            json!({"version":1,"expected_revision":context.expected_revision(),"operations":[]}),
            json!({"version":1,"expected_revision":context.expected_revision(),"operations":[
                {"op":"add_file","path":"é.txt","content":{"encoding":"utf8","newline":"lf","text":"é\nx","final_newline":false},"executable":false}
            ]}),
            json!({"version":1,"expected_revision":context.expected_revision(),"operations":[
                {"op":"delete_file","path":"gone.txt","base_digest":digest},
                {"op":"move_file","from":"a.txt","to":"b.txt","base_digest":digest},
                {"op":"replace_range","path":"edit.txt","base_digest":digest,"range":{"start":0,"end":0},"expected":{"encoding":"utf8","newline":"lf","text":"","final_newline":false},"replacement":{"encoding":"utf8","newline":"crlf","text":"x\ny","final_newline":true},"executable":"preserve"}
            ]}),
        ];
        for value in valid {
            assert!(validator.is_valid(&value));
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(
                crate::workspace::edit::normalize::normalize(
                    ModelEditFormat::StructuredJson,
                    &bytes,
                    &context.normalization,
                )
                .is_ok()
            );
        }

        let semantic_invalid = [
            json!({"version":1,"expected_revision":context.expected_revision(),"operations":[
                {"op":"add_file","path":"../x","content":{"encoding":"utf8","newline":"lf","text":"x","final_newline":false},"executable":false}
            ]}),
            json!({"version":1,"expected_revision":context.expected_revision(),"operations":[
                {"op":"replace_range","path":"x","base_digest":digest,"range":{"start":2,"end":1},"expected":{"encoding":"utf8","newline":"lf","text":"","final_newline":false},"replacement":{"encoding":"utf8","newline":"lf","text":"","final_newline":false},"executable":"preserve"}
            ]}),
            json!({"version":1,"expected_revision":context.expected_revision(),"operations":[
                {"op":"add_file","path":"bytes.txt","content":{"encoding":"utf8","newline":"lf","text":"ééééé","final_newline":false},"executable":false}
            ]}),
        ];
        let ordinary_intent = GrammarEditIntentEvidence {
            experiment_identity: GRAMMAR_EDIT_EXPERIMENT_ID.to_owned(),
            experiment_digest: snapshot(false).grammar_edit_experiment_digest(),
            selected_mode: EditOutputMode::Ordinary,
            provider: Some("fixture".to_owned()),
            model: Some("fixture-model".to_owned()),
            capability_version: None,
            schema_digest: None,
            schema_name: None,
            schema_version: None,
            schema_strict: None,
            request_session_id: "session".to_owned(),
            request_turn_id: "turn".to_owned(),
            expected_revision: context.expected_revision().to_string(),
            workspace_digest: context.workspace_digest().to_owned(),
            fallback_reason: None,
        };
        let semantic_result = ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![Item::text(
                ItemKind::Assistant,
                serde_json::to_string(&semantic_invalid[0]).unwrap(),
            )],
            usage: None,
            metadata: MetadataMap::new(),
            model: Some("fixture-model".to_owned()),
            response_id: None,
        };
        assert!(matches!(
            classify_terminal(&semantic_result, &ordinary_intent, limits, &context),
            Err(GrammarEditError::SemanticOutput(_))
        ));
        for value in semantic_invalid {
            assert!(validator.is_valid(&value));
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(
                crate::workspace::edit::normalize::normalize(
                    ModelEditFormat::StructuredJson,
                    &bytes,
                    &context.normalization,
                )
                .is_err()
            );
        }

        for value in [
            json!({"version":1,"expected_revision":context.expected_revision(),"operations":[]}),
            json!({"version":1,"expected_revision":context.expected_revision(),"operations":[
                {"op":"replace_range","path":"x","base_digest":digest,"range":{"start":-1,"end":0},"expected":{"encoding":"utf8","newline":"lf","text":"","final_newline":false},"replacement":{"encoding":"utf8","newline":"lf","text":"","final_newline":false},"executable":"preserve"}
            ]}),
            json!({"version":1,"expected_revision":format!("r:{}", "f".repeat(64)),"operations":[]}),
        ] {
            let decoded = crate::workspace::edit::normalize::normalize(
                ModelEditFormat::StructuredJson,
                &serde_json::to_vec(&value).unwrap(),
                &context.normalization,
            );
            if decoded.is_ok() {
                assert!(validator.is_valid(&value));
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
}
