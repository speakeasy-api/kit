//! Configurable failure policies for reporters.
//!
//! The [`FailurePolicy`] enum controls what happens when a reporter encounters
//! an error. Wrap any [`FallibleObserver`] in a [`PolicyReporter`] to get a
//! [`LoopObserver`] that applies the chosen policy automatically.

use crate::ReportError;
use agentkit_loop::{LoopObserver, ObservedEvent};

/// Policy that determines how reporter errors are handled.
///
/// Reporter failures are non-fatal by default — a broken log writer shouldn't
/// crash the agent. Hosts can configure stricter behaviour by choosing a
/// different policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FailurePolicy {
    /// Silently discard errors.
    #[default]
    Ignore,
    /// Log errors to stderr via `eprintln!`.
    Log,
    /// Collect errors for later inspection via
    /// [`PolicyReporter::take_errors`].
    Accumulate,
    /// Panic on the first error.
    FailFast,
}

/// A reporter whose event handling can fail.
///
/// Implement this trait for reporters that perform I/O or other fallible
/// operations. Wrap the implementation in [`PolicyReporter`] to obtain a
/// [`LoopObserver`] with configurable error handling.
pub trait FallibleObserver: Send + Sync {
    /// Process an event, returning an error if something goes wrong.
    /// Implementations store mutable state behind interior mutability so the
    /// wrapper can be shared as `Arc<dyn LoopObserver>`.
    fn try_handle_event(&self, event: &ObservedEvent) -> Result<(), ReportError>;
}

/// Adapter that wraps a [`FallibleObserver`] and applies a [`FailurePolicy`].
///
/// This turns any fallible reporter into a [`LoopObserver`] suitable for
/// passing to the agent loop.
///
/// # Example
///
/// ```rust
/// use agentkit_reporting::{ChannelReporter, FailurePolicy, PolicyReporter};
///
/// let (reporter, rx) = ChannelReporter::pair();
/// let reporter = PolicyReporter::new(reporter, FailurePolicy::Log);
/// // `reporter` now implements `LoopObserver` and logs send failures to stderr.
/// ```
pub struct PolicyReporter<T> {
    inner: T,
    policy: FailurePolicy,
    errors: std::sync::Mutex<Vec<ReportError>>,
}

impl<T: FallibleObserver> PolicyReporter<T> {
    /// Creates a new `PolicyReporter` wrapping the given observer with the
    /// specified failure policy.
    pub fn new(inner: T, policy: FailurePolicy) -> Self {
        Self {
            inner,
            policy,
            errors: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Returns a reference to the inner observer.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Returns the configured failure policy.
    pub fn policy(&self) -> FailurePolicy {
        self.policy
    }

    /// Drains and returns all accumulated errors.
    ///
    /// Only meaningful when the policy is [`FailurePolicy::Accumulate`].
    pub fn take_errors(&self) -> Vec<ReportError> {
        std::mem::take(&mut *self.errors.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl<T: FallibleObserver> LoopObserver for PolicyReporter<T> {
    fn handle_event(&self, event: ObservedEvent) {
        if let Err(e) = self.inner.try_handle_event(&event) {
            match self.policy {
                FailurePolicy::Ignore => {}
                FailurePolicy::Log => {
                    eprintln!("reporter error: {e}");
                }
                FailurePolicy::Accumulate => {
                    self.errors
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(e);
                }
                FailurePolicy::FailFast => {
                    panic!("reporter error: {e}");
                }
            }
        }
    }
}
