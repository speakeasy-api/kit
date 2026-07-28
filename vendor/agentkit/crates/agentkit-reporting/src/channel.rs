//! Channel-based reporter adapter.
//!
//! [`ChannelReporter`] forwards events to another thread or task via a
//! [`std::sync::mpsc::Sender`]. This keeps the observer contract synchronous
//! while allowing expensive event processing to happen off the driver's hot
//! path.

use std::sync::mpsc::{self, Receiver, Sender};

use agentkit_loop::{LoopObserver, ObservedEvent};

use crate::ReportError;
use crate::policy::FallibleObserver;

/// Reporter adapter that forwards events over a channel.
///
/// Use this when you need expensive or async event processing without
/// blocking the agent loop. The receiving end can live on a dedicated
/// thread or async task.
///
/// # Example
///
/// ```rust
/// use agentkit_reporting::ChannelReporter;
///
/// let (reporter, rx) = ChannelReporter::pair();
///
/// // Spawn a consumer on another thread.
/// std::thread::spawn(move || {
///     while let Ok(event) = rx.recv() {
///         println!("{event:?}");
///     }
/// });
///
/// // `reporter` implements `LoopObserver` — hand it to the agent loop.
/// ```
pub struct ChannelReporter {
    sender: std::sync::Mutex<Sender<ObservedEvent>>,
}

impl ChannelReporter {
    /// Creates a `ChannelReporter` from an existing sender.
    pub fn new(sender: Sender<ObservedEvent>) -> Self {
        Self {
            sender: std::sync::Mutex::new(sender),
        }
    }

    /// Creates a `ChannelReporter` together with the receiving end of the
    /// channel.
    pub fn pair() -> (Self, Receiver<ObservedEvent>) {
        let (sender, receiver) = mpsc::channel();
        (Self::new(sender), receiver)
    }
}

impl LoopObserver for ChannelReporter {
    fn handle_event(&self, event: ObservedEvent) {
        let _ = self
            .sender
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(event);
    }
}

impl FallibleObserver for ChannelReporter {
    fn try_handle_event(&self, event: &ObservedEvent) -> Result<(), ReportError> {
        self.sender
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(event.clone())
            .map_err(|_| ReportError::ChannelSend)
    }
}
