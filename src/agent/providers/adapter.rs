use std::{future::Future, pin::Pin, sync::Arc};

use agentkit_core::TurnCancellation;
use agentkit_loop::{LoopError, ModelAdapter, ModelSession, SessionConfig, TurnRequest};

use crate::{
    agent::providers::streaming::{BoundedTurn, CanaryRedactor, StreamCommit, StreamLimits},
    domain::secret::SecretLease,
};

pub trait StreamCommitFactory: Send + Sync + 'static {
    fn for_request(&self, request: &TurnRequest) -> Result<Box<dyn StreamCommit>, LoopError>;
}

#[derive(Clone, Default)]
pub struct ModelStreamPolicy {
    pub stream: StreamLimits,
    pub canaries: Vec<String>,
    pub secrets: Vec<Arc<SecretLease>>,
    pub retain_reasoning_summaries: bool,
}

impl std::fmt::Debug for ModelStreamPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelStreamPolicy")
            .field("stream", &self.stream)
            .field("canary_count", &self.canaries.len())
            .field("secret_count", &self.secrets.len())
            .field(
                "retain_reasoning_summaries",
                &self.retain_reasoning_summaries,
            )
            .finish()
    }
}

pub struct StreamPolicyAdapter<M> {
    inner: M,
    policy: ModelStreamPolicy,
    commits: Arc<dyn StreamCommitFactory>,
}

impl<M> StreamPolicyAdapter<M> {
    pub fn new(inner: M, policy: ModelStreamPolicy, commits: Arc<dyn StreamCommitFactory>) -> Self {
        Self {
            inner,
            policy,
            commits,
        }
    }
}

impl<M> ModelAdapter for StreamPolicyAdapter<M>
where
    M: ModelAdapter,
{
    type Session = StreamPolicySession<M::Session>;

    fn start_session<'life0, 'async_trait>(
        &'life0 self,
        config: SessionConfig,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Session, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            Ok(StreamPolicySession {
                inner: self.inner.start_session(config).await?,
                policy: self.policy.clone(),
                commits: Arc::clone(&self.commits),
            })
        })
    }

    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}

pub struct StreamPolicySession<S> {
    inner: S,
    policy: ModelStreamPolicy,
    commits: Arc<dyn StreamCommitFactory>,
}

impl<S> ModelSession for StreamPolicySession<S>
where
    S: ModelSession,
{
    type Turn = BoundedTurn<S::Turn, Box<dyn StreamCommit>>;

    fn begin_turn<'life0, 'async_trait>(
        &'life0 mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
            {
                return Err(LoopError::Cancelled);
            }
            let commit = self.commits.for_request(&request)?;
            let redactor = CanaryRedactor::new(self.policy.canaries.clone())
                .with_secrets(&self.policy.secrets);
            let turn =
                self.inner.begin_turn(request, cancellation).await.map_err(
                    |error| match error {
                        LoopError::Cancelled => LoopError::Cancelled,
                        error => LoopError::Provider(redactor.redact_text(&error.to_string())),
                    },
                )?;
            Ok(BoundedTurn::new(turn, commit, self.policy.stream, redactor)
                .with_reasoning_summaries(self.policy.retain_reasoning_summaries))
        })
    }

    fn model_name(&self) -> Option<&str> {
        self.inner.model_name()
    }

    fn prepare_turn(&mut self, request: &mut TurnRequest) -> Result<(), LoopError> {
        self.inner.prepare_turn(request)
    }

    fn structured_output_capability(&self) -> Option<&agentkit_loop::StructuredOutputCapability> {
        self.inner.structured_output_capability()
    }
}
