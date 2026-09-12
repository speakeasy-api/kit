//! Opt-in subscription voice. Audio and protocol state belong to this process;
//! only explicit delegations enter the existing TUI agent session.

mod native;
mod sideband;
mod signaling;

use crate::credentials::CredentialStorage;
use futures_util::future::{Either, select};
use std::{collections::HashSet, future::Future, net::SocketAddr, task::Poll, time::Duration};
use tokio::sync::{mpsc, oneshot};

const MAX_TEXT: usize = 16 * 1024;
const MAX_HANDOFFS: usize = 256;

pub(crate) enum VoiceEvent {
    Ready,
    Transcript { speaker: String, text: String },
    Delegation { id: String, text: String },
    Error(String),
    Stopped,
}

enum Control {
    Result { id: String, text: String },
}

/// Dropping the command sender requests shutdown, including session.close on
/// the authenticated sideband. No automatic reconnect or quota-consuming retry.
pub(crate) struct VoiceSession {
    pub(crate) events: mpsc::Receiver<VoiceEvent>,
    commands: Option<mpsc::Sender<Control>>,
    microphone: Option<native::Microphone>,
    cancel: Option<oneshot::Sender<()>>,
}

// Startup futures can be dropped by the deadline or UI cancellation. Transfer
// the connected sideband to a bounded cleanup task on every such exit.
struct StartupSideband(Option<sideband::Sideband>);
impl Drop for StartupSideband {
    fn drop(&mut self) {
        if let Some(mut sideband) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(2), sideband.close()).await;
            });
        }
    }
}

impl VoiceSession {
    pub(crate) async fn start(storage: CredentialStorage) -> Result<Self, String> {
        tokio::time::timeout(Duration::from_secs(90), Self::connect(storage))
            .await
            .map_err(|_| "voice startup timed out; reconnect manually".to_owned())?
    }

    async fn connect(storage: CredentialStorage) -> Result<Self, String> {
        let mut transport = native::start(local_address()?).await?;
        let answer = signaling::establish_with_storage(&transport.offer, &storage)
            .await
            .map_err(|error| error.to_string())?;
        let mut cleanup = StartupSideband(Some(sideband::connect(answer.sideband).await?));
        transport.accept_answer(answer.sdp).await?;
        match select(&mut transport.finished, Box::pin(transport.events.recv())).await {
            Either::Left((result, _)) => {
                return Err(result
                    .unwrap_or_else(|_| Err("voice transport stopped".into()))
                    .err()
                    .unwrap_or_else(|| "voice transport stopped".into()));
            }
            Either::Right((event, _)) => match event {
                Some(native::TransportEvent::Ready) => {}
                None => return Err("voice transport stopped before connection".into()),
            },
        }
        let mut sideband = cleanup.0.take().ok_or("voice sideband missing")?;
        let microphone = transport.microphone();
        let (commands, command_rx) = mpsc::channel(16);
        let (events_tx, events) = mpsc::channel(32);
        emit(&events_tx, VoiceEvent::Ready)?;
        let (cancel, cancelled) = oneshot::channel();
        tokio::spawn(async move {
            // select polls cancellation first and drops the losing future.
            let result = match select(
                cancelled,
                Box::pin(run(&mut transport, &mut sideband, command_rx, &events_tx)),
            )
            .await
            {
                Either::Left(_) => Ok(()),
                Either::Right((result, _)) => result,
            };
            // Close the audio actor first, even if the service stops responding.
            // Do not poll its terminal oneshot again after run consumed it.
            drop(transport);
            let _ = tokio::time::timeout(Duration::from_secs(2), sideband.close()).await;
            let event = match result {
                Ok(()) => VoiceEvent::Stopped,
                Err(error) => VoiceEvent::Error(error),
            };
            let _ = events_tx.try_send(event);
        });
        Ok(Self {
            events,
            commands: Some(commands),
            microphone: Some(microphone),
            cancel: Some(cancel),
        })
    }

    /// Queues a request; device errors are delivered through `events`.
    pub(crate) fn set_microphone(&self, enabled: bool) -> Result<(), String> {
        self.microphone
            .as_ref()
            .ok_or_else(|| "voice is stopped".to_owned())?
            .set(enabled)
    }

    pub(crate) fn send_result(&self, id: String, text: String) -> Result<(), String> {
        if id.len() > 1024 || text.len() > MAX_TEXT {
            return Err("voice result exceeds the size limit".into());
        }
        self.send(Control::Result { id, text })
    }

    fn send(&self, control: Control) -> Result<(), String> {
        self.commands
            .as_ref()
            .ok_or_else(|| "voice is stopped".to_owned())?
            .try_send(control)
            .map_err(|_| "voice command queue is closed or full".to_owned())
    }

    pub(crate) fn stop(&mut self) {
        self.microphone = None;
        self.cancel = None;
        self.commands = None;
    }
}

// A UDP connect selects an OS route but sends no datagram. An explicit local
// interface override is useful on VPNs/multihomed hosts; this is not persisted.
fn local_address() -> Result<SocketAddr, String> {
    if let Some(address) = std::env::var_os("KIT_VOICE_BIND") {
        return address
            .to_str()
            .ok_or_else(|| "KIT_VOICE_BIND must be UTF-8".to_owned())?
            .parse()
            .map_err(|_| "KIT_VOICE_BIND must be a local IP:port (port 0 is recommended)".into());
    }
    native::default_bind_address()
}

async fn run(
    transport: &mut native::Transport,
    sideband: &mut sideband::Sideband,
    mut commands: mpsc::Receiver<Control>,
    events: &mpsc::Sender<VoiceEvent>,
) -> Result<(), String> {
    let mut seen = HashSet::new();
    let mut pending = HashSet::new();
    enum Event {
        Command(Option<Control>),
        Finished(Result<Result<(), String>, oneshot::error::RecvError>),
        Transport(Option<native::TransportEvent>),
        Payload(Result<Option<serde_json::Value>, String>),
    }
    loop {
        let event = {
            let mut payload = std::pin::pin!(sideband.recv());
            std::future::poll_fn(|cx| {
                // A dropped UI must stop capture before processing further events.
                if let Poll::Ready(command) = commands.poll_recv(cx) {
                    return Poll::Ready(Event::Command(command));
                }
                if let Poll::Ready(result) = std::pin::Pin::new(&mut transport.finished).poll(cx) {
                    return Poll::Ready(Event::Finished(result));
                }
                if let Poll::Ready(event) = transport.events.poll_recv(cx) {
                    return Poll::Ready(Event::Transport(event));
                }
                payload.as_mut().poll(cx).map(Event::Payload)
            })
            .await
        };
        match event {
            Event::Command(command) => match command {
                None => return Ok(()),
                Some(Control::Result { id, text }) => {
                    if !pending.remove(&id) {
                        return Err("voice result has no matching delegation".into());
                    }
                    for frame in signaling::delegation_context_append(
                        &id,
                        &text,
                        Some(signaling::ContextAppendChannel::Speakable),
                    ) {
                        sideband.send(&frame.to_string()).await?;
                    }
                }
            },
            Event::Finished(result) => {
                return result.map_err(|_| "voice transport stopped unexpectedly".to_owned())?;
            }
            Event::Transport(event) => match event {
                // Protocol authority is the authenticated sideband, not SCTP.
                Some(native::TransportEvent::Ready) => {}
                None => return Err("voice transport event stream ended".into()),
            },
            Event::Payload(payload) => {
                let Some(value) = payload? else {
                    return Err("voice service disconnected; reconnect manually".into());
                };
                if let Some(handoff) = signaling::parse_handoff(&value.to_string())
                    .map_err(|_| "invalid voice delegation".to_owned())?
                {
                    let id = handoff.item_id;
                    if id.is_empty() || id.len() > 1024 || handoff.input_transcript.len() > MAX_TEXT
                    {
                        return Err("voice delegation exceeds the size limit".into());
                    }
                    if seen.contains(&id) {
                        continue;
                    }
                    if seen.len() == MAX_HANDOFFS {
                        return Err("voice delegation limit reached; reconnect manually".into());
                    }
                    seen.insert(id.clone());
                    pending.insert(id.clone());
                    emit(
                        events,
                        VoiceEvent::Delegation {
                            id,
                            text: handoff.input_transcript,
                        },
                    )?;
                } else {
                    match value["type"].as_str() {
                        Some("error") => {
                            return Err(
                                "voice service reported an error; reconnect manually".into()
                            );
                        }
                        Some("turn.done") => {
                            if let (Some(speaker), Some(text)) = (
                                value["turn"]["role"].as_str(),
                                value["turn"]["transcript"].as_str(),
                            ) && matches!(speaker, "user" | "assistant")
                                && text.len() <= MAX_TEXT
                            {
                                emit(
                                    events,
                                    VoiceEvent::Transcript {
                                        speaker: speaker.into(),
                                        text: text.into(),
                                    },
                                )?;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn emit(events: &mpsc::Sender<VoiceEvent>, event: VoiceEvent) -> Result<(), String> {
    events
        .try_send(event)
        .map_err(|_| "voice event queue is closed or full".into())
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    clippy::disallowed_macros,
    clippy::unwrap_used,
    clippy::expect_used
)]
mod tests {
    use super::*;
    #[test]
    fn stopping_closes_control_queue_without_network_or_devices() {
        let (tx, mut rx) = mpsc::channel(1);
        let (_, events) = mpsc::channel(1);
        let mut session = VoiceSession {
            events,
            commands: Some(tx),
            microphone: None,
            cancel: None,
        };
        session.stop();
        assert!(session.set_microphone(true).is_err());
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }
}
