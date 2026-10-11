//! Kit classifies model failures at its own adapter, session and turn
//! boundaries. The generic loop reports none of them, and cancellation was
//! never a model error: it is observed through Kit's cancellation path.
use super::{AhpHost, lifecycle::ModelFailure};
use agentkit_core::TurnCancellation;
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, RetryObserver, SessionConfig,
    TurnRequest,
};
use async_trait::async_trait;
use std::sync::Arc;

/// What a failure is, classified without inventing provider facts.
struct Classification {
    class: &'static str,
    code: Option<String>,
    status: Option<u16>,
}

fn plain(class: &'static str) -> Option<Classification> {
    Some(Classification {
        class,
        code: None,
        status: None,
    })
}

/// Decides whether a failure returned through a model boundary is actually a
/// model failure. A tool error or a mutator/hook error can travel out of
/// `begin_turn` or `next_event`, and neither is a model failure; cancellation
/// is observed through Kit's own cancellation path. Reporting any of them here
/// would make `model.error` a catch-all rather than a model fact.
fn classify(error: &LoopError) -> Option<Classification> {
    match error {
        LoopError::Cancelled | LoopError::Tool(_) | LoopError::Mutator(_) => None,
        LoopError::ProviderFailure(failure) => Some(Classification {
            class: "provider",
            // Sanitized upstream classification, exactly as the retry layer
            // recorded it. No headers and no response body are carried, and an
            // unclassified upstream code is reported as absent rather than as
            // the literal placeholder.
            code: serde_json::to_value(failure.upstream.code)
                .ok()
                .and_then(|code| code.as_str().map(str::to_owned))
                .filter(|code| code != "unknown"),
            status: failure.upstream.http_status,
        }),
        LoopError::Provider(_) => plain("provider"),
        LoopError::InvalidState(_) => plain("adapter"),
        LoopError::Unsupported(_) => plain("unsupported"),
    }
}

async fn report(host: Option<&Arc<AhpHost>>, session: &str, failure: ModelFailure<'_>) {
    let Some(host) = host else { return };
    // A failed notification never replaces or masks the real failure.
    if let Err(error) = host.model_error(session, failure).await {
        tracing::warn!(%error, "AHP model error notification failed");
    }
}

async fn classified(host: Option<&Arc<AhpHost>>, session: &str, error: LoopError) -> LoopError {
    if let Some(classification) = classify(&error) {
        let message = error.to_string();
        report(
            host,
            session,
            ModelFailure {
                class: classification.class,
                message: &message,
                code: classification.code,
                status: classification.status,
            },
        )
        .await;
    }
    error
}

/// Wraps a Kit model adapter so inference failures reach the native hooks.
/// Retry policy, provider identity and model identity stay with the adapter.
pub struct HookedAdapter<A> {
    inner: A,
    host: Option<Arc<AhpHost>>,
}

impl<A> HookedAdapter<A> {
    pub fn new(inner: A, host: Option<Arc<AhpHost>>) -> Self {
        Self { inner, host }
    }
}

#[async_trait]
impl<A: ModelAdapter> ModelAdapter for HookedAdapter<A> {
    type Session = HookedSession<A::Session>;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        // Starting a session is the adapter's own boundary, not an inference
        // attempt: a failure here is a session failure, not a model error.
        let session = config.session_id.to_string();
        let inner = self.inner.start_session(config).await?;
        Ok(HookedSession {
            inner,
            host: self.host.clone(),
            session,
        })
    }

    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}

/// Session half of [`HookedAdapter`].
pub struct HookedSession<S> {
    inner: S,
    host: Option<Arc<AhpHost>>,
    session: String,
}

#[async_trait]
impl<S: ModelSession> ModelSession for HookedSession<S> {
    type Turn = HookedTurn<S::Turn>;

    fn set_retry_observer(&mut self, observer: Option<Arc<dyn RetryObserver>>) {
        self.inner.set_retry_observer(observer);
    }

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        match self.inner.begin_turn(request, cancellation).await {
            Ok(inner) => Ok(HookedTurn {
                inner,
                host: self.host.clone(),
                session: self.session.clone(),
                finished: false,
            }),
            Err(error) => Err(classified(self.host.as_ref(), &self.session, error).await),
        }
    }

    fn model_name(&self) -> Option<&str> {
        self.inner.model_name()
    }

    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}

/// Turn half of [`HookedAdapter`].
pub struct HookedTurn<T> {
    inner: T,
    host: Option<Arc<AhpHost>>,
    session: String,
    finished: bool,
}

#[async_trait]
impl<T: ModelTurn> ModelTurn for HookedTurn<T> {
    fn on_cancelled(&mut self) {
        self.inner.on_cancelled();
    }

    async fn next_event(
        &mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        match self.inner.next_event(cancellation).await {
            Ok(Some(event)) => {
                if matches!(event, ModelTurnEvent::Finished(_)) {
                    self.finished = true;
                }
                Ok(Some(event))
            }
            Ok(None) => {
                // The loop turns an unfinished stream into a provider error of
                // its own. No provider reported it, so report the stream fact.
                if !self.finished {
                    self.finished = true;
                    report(
                        self.host.as_ref(),
                        &self.session,
                        ModelFailure {
                            class: "stream",
                            message: "model turn ended without a Finished event",
                            code: None,
                            status: None,
                        },
                    )
                    .await;
                }
                Ok(None)
            }
            Err(error) => Err(classified(self.host.as_ref(), &self.session, error).await),
        }
    }
}
