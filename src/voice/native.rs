//! Native, actor-owned WebRTC transport. The caller exchanges `offer` for an SDP
//! answer, then calls `accept_answer`. No browser, subprocess, or signaling I/O.
//!
//! API verified against str0m 0.23.1 (aws-lc-rs), cubeb 0.38.0 on Linux,
//! cpal 0.18.2 elsewhere,
//! and opus-pure 0.2.1, all with default features disabled. TUI availability is
//! gated by the default-off experimental.voice startup setting.
//! Supply a concrete local interface address; no STUN/TURN discovery is performed.
//! The answer must contain reachable ICE candidates. Capture starts closed.
//! Device PCM is mixed to mono and resampled to/from the 48 kHz Opus clock.

#[cfg(not(target_os = "linux"))]
mod audio;
#[cfg(target_os = "linux")]
#[path = "native/audio_linux.rs"]
mod audio;

use audio::Audio;

use opus_pure::{Application, OpusDecoder as Decoder, OpusEncoder as Encoder, RateControl};
use std::fmt::Display;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::sync::{mpsc as async_mpsc, oneshot};

pub type Result<T> = std::result::Result<T, String>;
const RATE: u32 = 48_000;
const FRAME: usize = 960;
const MAX_PCM: usize = 5_760;
const MAX_TEXT: usize = 64 * 1024;
const MAX_SDP: usize = 256 * 1024;
const MAX_AGE: Duration = Duration::from_millis(200);
const TICK: Duration = Duration::from_millis(5);
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(30);
// Match libopus OPUS_AUTO for 48 kHz mono / 20 ms:
// 60 * sample_rate / frame_size + sample_rate * channels = 51 kbps.
// opus-pure defaults to 64 kbps, so do not silently adopt that default.
fn voice_encoder() -> opus_pure::Result<Encoder> {
    let mut encoder = Encoder::new(RATE as i32, 1, Application::Voip)?;
    encoder.bitrate_bps = 51_000;
    encoder.complexity = 9;
    encoder.rate_control = RateControl::ConstrainedVbr;
    Ok(encoder)
}

fn message(error: impl Display) -> String {
    error.to_string()
}

#[derive(Debug)]
pub enum TransportEvent {
    Ready,
}
enum Command {
    Answer(String, oneshot::Sender<Result<()>>),
    Talk(bool, oneshot::Sender<Result<()>>),
}

/// Drop closes the command channel and stops the actor. Queues are bounded;
/// protocol congestion fails instead of silently losing events. `finished`
/// carries terminal errors, including event overflow. Capture starts muted.
pub struct Transport {
    pub offer: String,
    pub events: async_mpsc::Receiver<TransportEvent>,
    pub finished: oneshot::Receiver<Result<()>>,
    commands: async_mpsc::Sender<Command>,
}
/// A weak control handle cannot keep capture alive after transport shutdown.
pub struct Microphone(async_mpsc::WeakSender<Command>);
impl Microphone {
    /// Queue directly to the audio actor, independently of sideband I/O.
    /// Device errors terminate the actor and arrive on Transport::finished.
    pub fn set(&self, talking: bool) -> Result<()> {
        let commands = self.0.upgrade().ok_or("voice is stopped")?;
        let (ack, _) = oneshot::channel();
        commands
            .try_send(Command::Talk(talking, ack))
            .map_err(message)
    }
}

impl Transport {
    pub async fn accept_answer(&self, sdp: String) -> Result<()> {
        if sdp.len() > MAX_SDP {
            return Err("SDP answer exceeds 256 KiB".into());
        }
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Answer(sdp, tx))
            .await
            .map_err(message)?;
        rx.await.map_err(message)?
    }
    pub fn microphone(&self) -> Microphone {
        Microphone(self.commands.downgrade())
    }
}

/// Select the OS default IPv4 route's local address without sending a probe.
/// UDP `connect` only configures a socket; this helper performs no send, receive,
/// DNS lookup, or STUN request. The destination is the RFC 5737 documentation
/// address, not a paid service. The returned port is zero for ephemeral binding.
/// VPN/policy routing may require the caller to choose an explicit interface
/// instead. IPv6-only hosts must pass an explicit IPv6 address to `start`.
pub fn default_bind_address() -> Result<SocketAddr> {
    let socket = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).map_err(message)?;
    socket
        .connect((std::net::Ipv4Addr::new(192, 0, 2, 1), 9))
        .map_err(message)?;
    let mut local = socket.local_addr().map_err(message)?;
    if local.ip().is_unspecified() || local.ip().is_loopback() {
        return Err("No default IPv4 voice interface; select an explicit local address".into());
    }
    local.set_port(0);
    Ok(local)
}

/// Creates devices and an offer on a dedicated thread. No packets are sent
/// before an answer. Capture is opened only when the microphone is enabled.
pub async fn start(bind: SocketAddr) -> Result<Transport> {
    if bind.ip().is_unspecified() || bind.ip().is_multicast() {
        return Err("Voice requires a concrete unicast local interface address".into());
    }
    let (commands, rx) = async_mpsc::channel(32);
    let (events_tx, events) = async_mpsc::channel(32);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (done_tx, finished) = oneshot::channel();
    std::thread::Builder::new()
        .name("kit-native-voice".into())
        .spawn(move || {
            let result = match Actor::new(bind, events_tx) {
                Ok((actor, offer)) => {
                    if ready_tx.send(Ok(offer)).is_ok() {
                        actor.run(rx)
                    } else {
                        Ok(())
                    }
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.clone()));
                    Err(error)
                }
            };
            let _ = done_tx.send(result);
        })
        .map_err(message)?;
    let offer = ready_rx.await.map_err(message)??;
    Ok(Transport {
        offer,
        events,
        finished,
        commands,
    })
}

struct Captured {
    began: Instant,
    sequence: u64,
    samples: [f32; FRAME],
}
struct Playback {
    received: Instant,
    samples: [f32; MAX_PCM],
    len: usize,
}

fn negotiation(bind: SocketAddr) -> Result<(Rtc, Mid, ChannelId, String, SdpPendingOffer)> {
    // Explicit provider avoids process-global installation/state.
    let mut rtc = Rtc::builder()
        .set_crypto_provider(str0m::crypto::from_feature_flags().into())
        .clear_codecs()
        .enable_opus(true)
        .set_reordering_size_audio(10)
        .set_send_buffer_audio(50)
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(bind, "udp").map_err(message)?);
    let mut sdp = rtc.sdp_api();
    let mid = sdp.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let channel = sdp.add_channel("oai-events".into());
    let (offer, pending) = sdp.apply().ok_or("No SDP changes")?;
    Ok((rtc, mid, channel, offer.to_sdp_string(), pending))
}
struct Actor {
    rtc: Rtc,
    mid: Mid,
    channel: ChannelId,
    pending: Option<SdpPendingOffer>,
    socket: UdpSocket,
    local: SocketAddr,
    audio: Audio,
    encoder: Encoder,
    decoder: Decoder,
    events: async_mpsc::Sender<TransportEvent>,
    talking_since: Option<Instant>,
    ready: bool,
    media_origin: Option<(u64, u64)>,
    started: Instant,
}
impl Actor {
    fn new(bind: SocketAddr, events: async_mpsc::Sender<TransportEvent>) -> Result<(Self, String)> {
        let audio = Audio::new()?;
        let encoder = voice_encoder().map_err(message)?;
        let decoder = Decoder::new(RATE as i32, 1).map_err(message)?;
        let socket = UdpSocket::bind(bind).map_err(message)?;
        socket.set_write_timeout(Some(TICK)).map_err(message)?;
        let local = socket.local_addr().map_err(message)?;
        let (rtc, mid, channel, offer, pending) = negotiation(local)?;
        Ok((
            Self {
                rtc,
                mid,
                channel,
                pending: Some(pending),
                socket,
                local,
                audio,
                encoder,
                decoder,
                events,
                talking_since: None,
                ready: false,
                media_origin: None,
                started: Instant::now(),
            },
            offer,
        ))
    }
    fn emit(&self, event: TransportEvent) -> Result<()> {
        self.events
            .try_send(event)
            .map_err(|_| "Voice event receiver closed or lagging".into())
    }
    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Answer(sdp, ack) => {
                let result = (|| {
                    let answer = SdpAnswer::from_sdp_string(&sdp).map_err(message)?;
                    let pending = self.pending.take().ok_or("SDP answer already accepted")?;
                    self.rtc
                        .sdp_api()
                        .accept_answer(pending, answer)
                        .map_err(message)?;
                    if self
                        .rtc
                        .media(self.mid)
                        .ok_or("Answer rejected audio")?
                        .direction()
                        != Direction::SendRecv
                    {
                        return Err("Answer did not negotiate duplex audio".into());
                    }
                    let writer = self.rtc.writer(self.mid).ok_or("Answer rejected audio")?;
                    if !writer
                        .payload_params()
                        .any(|p| p.spec().codec == Codec::Opus)
                    {
                        return Err("Answer did not negotiate Opus".into());
                    }
                    Ok(())
                })();
                let fatal = result.clone();
                let _ = ack.send(result);
                fatal?;
            }
            Command::Talk(talking, ack) => {
                let result = if talking && !self.ready {
                    Err("Voice data channel is not ready".into())
                } else if talking == self.talking_since.is_some() {
                    Ok(())
                } else {
                    self.media_origin = None;
                    self.encoder.reset_state().map_err(message)?;
                    for _ in 0..8 {
                        let _ = self.audio.captured.try_recv();
                    }
                    if talking {
                        self.talking_since = Some(Instant::now());
                        self.audio.set_capture(true)
                    } else {
                        self.talking_since = None;
                        self.audio.set_capture(false)
                    }
                };
                let fatal = result.clone();
                let _ = ack.send(result);
                if self.ready {
                    fatal?;
                }
            }
        }
        Ok(())
    }
    fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::ChannelOpen(id, label) if id == self.channel && label == "oai-events" => {
                self.ready = true;
                self.emit(TransportEvent::Ready)?;
            }
            Event::ChannelClose(id) if id == self.channel => {
                return Err("Voice data channel closed".into());
            }
            Event::ChannelData(data) if data.id == self.channel => {
                if data.binary || data.data.len() > MAX_TEXT {
                    return Err("Invalid or oversized voice protocol event".into());
                }
                // The authenticated sideband alone owns control/delegation.
                std::str::from_utf8(&data.data).map_err(message)?;
            }
            Event::MediaData(data) if data.mid == self.mid => {
                if data.params.spec().codec != Codec::Opus || data.data.len() > 8192 {
                    return Err("Invalid voice audio payload".into());
                }
                let mut frame = Playback {
                    received: Instant::now(),
                    samples: [0.0; MAX_PCM],
                    len: 0,
                };
                frame.len = self
                    .decoder
                    .decode(&data.data, MAX_PCM, &mut frame.samples)
                    .map_err(message)?;
                if frame.len > 0 {
                    let _ = self.audio.playback.try_send(frame);
                }
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                return Err("Voice ICE disconnected".into());
            }
            _ => {}
        }
        Ok(())
    }
    fn capture(&mut self) -> Result<()> {
        for _ in 0..8 {
            let Ok(frame) = self.audio.captured.try_recv() else {
                break;
            };
            let Some(since) = self.talking_since else {
                continue;
            };
            if frame.began < since || frame.began.elapsed() > MAX_AGE {
                continue;
            }
            let mut encoded = [0; 1275];
            let len = self
                .encoder
                .encode(&frame.samples, FRAME, &mut encoded)
                .map_err(message)?;
            let writer = self
                .rtc
                .writer(self.mid)
                .ok_or("Voice audio track unavailable")?;
            let pt = writer
                .payload_params()
                .find(|p| p.spec().codec == Codec::Opus)
                .ok_or("Opus payload unavailable")?
                .pt();
            // Callback delivery times are not an audio clock: one callback can
            // contain several frames. Preserve exactly 960 ticks between frames,
            // including gaps from queue drops; rebase only at a new talkspurt.
            let wall_ticks = (frame.began.duration_since(self.started).as_micros()
                * u128::from(RATE)
                / 1_000_000) as u64;
            let (sequence, origin) = *self
                .media_origin
                .get_or_insert((frame.sequence, wall_ticks));
            let ticks = origin + (frame.sequence - sequence) * FRAME as u64;
            writer
                .write(
                    pt,
                    Instant::now(),
                    MediaTime::new(ticks, Frequency::FORTY_EIGHT_KHZ),
                    encoded[..len].to_vec(),
                )
                .map_err(message)?;
        }
        Ok(())
    }
    fn run(mut self, mut commands: async_mpsc::Receiver<Command>) -> Result<()> {
        let mut packet = [0; 65_535];
        loop {
            for _ in 0..32 {
                match commands.try_recv() {
                    Ok(command) => self.command(command)?,
                    Err(async_mpsc::error::TryRecvError::Empty) => break,
                    Err(async_mpsc::error::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            if let Ok(error) = self.audio.errors.try_recv() {
                return Err(error);
            }
            if self.events.is_closed() {
                return Ok(());
            }
            if !self.ready && self.started.elapsed() > NEGOTIATION_TIMEOUT {
                return Err("Voice negotiation timed out".into());
            }
            if self.pending.is_some() {
                std::thread::sleep(TICK);
                continue;
            }
            self.capture()?;
            let mut deadline = Instant::now();
            // Bound work between command checks, including under a remote flood.
            for _ in 0..256 {
                match self.rtc.poll_output().map_err(message)? {
                    Output::Timeout(time) => {
                        deadline = time;
                        break;
                    }
                    Output::Transmit(tx) => {
                        self.socket
                            .send_to(&tx.contents, tx.destination)
                            .map_err(message)?;
                    }
                    Output::Event(event) => self.event(event)?,
                }
            }
            let wait = deadline.saturating_duration_since(Instant::now()).min(TICK);
            if wait.is_zero() {
                self.rtc
                    .handle_input(Input::Timeout(Instant::now()))
                    .map_err(message)?;
                continue;
            }
            self.socket.set_read_timeout(Some(wait)).map_err(message)?;
            match self.socket.recv_from(&mut packet) {
                Ok((len, source)) => {
                    if let Ok(contents) = packet[..len].try_into() {
                        let input = Input::Receive(
                            Instant::now(),
                            Receive {
                                proto: Protocol::Udp,
                                source,
                                destination: self.local,
                                contents,
                            },
                        );
                        if self.rtc.accepts(&input) {
                            self.rtc.handle_input(input).map_err(message)?;
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    self.rtc
                        .handle_input(Input::Timeout(Instant::now()))
                        .map_err(message)?;
                }
                Err(e) => return Err(message(e)),
            }
        }
    }
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
    #[tokio::test]
    async fn microphone_bypasses_full_delegation_queue_and_does_not_keep_actor_alive() {
        // The command receiver is the audio actor boundary: no devices/network.
        let (commands, mut actor) = async_mpsc::channel(4);
        let (_, events) = async_mpsc::channel(1);
        let (_, finished) = oneshot::channel();
        let transport = Transport {
            offer: String::new(),
            events,
            finished,
            commands,
        };
        let (results, _stalled_sideband) = async_mpsc::channel(1);
        let (_, events) = async_mpsc::channel(1);
        let mut session = super::super::VoiceSession {
            events,
            commands: Some(results),
            microphone: Some(transport.microphone()),
            cancel: None,
        };
        session.send_result("id".into(), "result".into()).unwrap();
        assert!(
            session
                .send_result("other".into(), "blocked".into())
                .is_err()
        );
        session.set_microphone(true).unwrap();
        session.set_microphone(false).unwrap();
        assert!(matches!(actor.recv().await, Some(Command::Talk(true, _))));
        assert!(matches!(actor.recv().await, Some(Command::Talk(false, _))));
        drop(transport);
        assert!(actor.recv().await.is_none());
        assert!(session.set_microphone(true).is_err());
        session.stop();
        assert!(session.set_microphone(true).is_err());
    }

    #[test]
    fn offer_and_answer_are_local_and_opus_only() {
        let (mut caller, _, _, offer, pending) =
            negotiation("127.0.0.1:19000".parse().unwrap()).unwrap();
        assert!(offer.contains("opus/48000/2")); // RFC 7587 mandates /2, even for mono.
        assert!(offer.contains("a=sendrecv"));
        assert!(offer.contains("m=application"));
        assert!(!offer.contains("VP8"));
        let (mut peer, _, _, _, _) = negotiation("127.0.0.1:19001".parse().unwrap()).unwrap();
        let offer = str0m::change::SdpOffer::from_sdp_string(&offer).unwrap();
        let answer = peer.sdp_api().accept_offer(offer).unwrap();
        caller.sdp_api().accept_answer(pending, answer).unwrap();
    }
    #[test]
    fn opus_mono_twenty_millisecond_roundtrip() {
        let mut encoder = voice_encoder().unwrap();
        let mut decoder = Decoder::new(RATE as i32, 1).unwrap();
        let mut encoded = [0; 1275];
        let size = encoder.encode(&[0.0; FRAME], FRAME, &mut encoded).unwrap();
        let mut decoded = [0.0; MAX_PCM];
        assert_eq!(
            decoder
                .decode(&encoded[..size], MAX_PCM, &mut decoded)
                .unwrap(),
            FRAME
        );
        assert!(decoded[..FRAME].iter().all(|v| v.is_finite()));
    }
    #[test]
    fn opus_receive_capacity_does_not_stretch_packets() {
        for samples in [120, 240, 480, FRAME, 1_920, 2_880, MAX_PCM] {
            let mut encoder = voice_encoder().unwrap();
            let mut decoder = Decoder::new(RATE as i32, 1).unwrap();
            let pcm: Vec<_> = (0..samples)
                .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.1)
                .collect();
            let mut packet = [0; opus_pure::MAX_PACKET_BYTES];
            let bytes = encoder.encode(&pcm, samples, &mut packet).unwrap();
            let mut output = [f32::NAN; MAX_PCM];
            let produced = decoder
                .decode(&packet[..bytes], MAX_PCM, &mut output)
                .unwrap();
            assert_eq!(produced, samples);
            assert_eq!(decoder.last_packet_duration(), samples);
            assert!(output[..produced].iter().all(|s| s.is_finite()));
            assert!(output[produced..].iter().all(|s| s.is_nan()));
        }
    }

    #[test]
    fn opus_reset_preserves_settings_and_starts_a_fresh_talkspurt() {
        let mut encoder = voice_encoder().unwrap();
        let pcm: Vec<_> = (0..FRAME)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.1)
            .collect();
        let mut packet = [0; 1275];
        for _ in 0..3 {
            encoder.encode(&pcm, FRAME, &mut packet).unwrap();
        }
        encoder.reset_state().unwrap();
        assert_eq!(encoder.bitrate_bps, 51_000);
        assert_eq!(encoder.complexity, 9);
        assert_eq!(encoder.rate_control, RateControl::ConstrainedVbr);
        assert_eq!(encoder.application(), Application::Voip);
        assert_eq!(encoder.sample_rate(), RATE as i32);
        assert_eq!(encoder.channels(), 1);
        let bytes = encoder.encode(&pcm, FRAME, &mut packet).unwrap();
        let mut fresh = voice_encoder().unwrap();
        let mut expected = [0; 1275];
        let expected_bytes = fresh.encode(&pcm, FRAME, &mut expected).unwrap();
        assert_eq!(&packet[..bytes], &expected[..expected_bytes]);
    }

    // All packets stay in memory: exercise real ICE, DTLS, SCTP, and SRTP APIs
    // without binding sockets, opening devices, or contacting a live service.
    fn transfer(from: &mut Rtc, to: &mut Rtc, now: Instant, events: &mut Vec<Event>) {
        loop {
            match from.poll_output().unwrap() {
                Output::Timeout(_) => break,
                Output::Event(event) => events.push(event),
                Output::Transmit(packet) => {
                    to.handle_input(Input::Receive(
                        now,
                        Receive {
                            proto: Protocol::Udp,
                            source: packet.source,
                            destination: packet.destination,
                            contents: (&packet.contents[..]).try_into().unwrap(),
                        },
                    ))
                    .unwrap();
                }
            }
        }
    }

    #[test]
    fn in_memory_webrtc_transports_text_and_opus_both_ways() {
        let (mut caller, mid, channel, offer, pending) =
            negotiation("127.0.0.1:19002".parse().unwrap()).unwrap();
        let mut peer = Rtc::builder()
            .set_crypto_provider(str0m::crypto::from_feature_flags().into())
            .clear_codecs()
            .enable_opus(true)
            .build(Instant::now());
        peer.add_local_candidate(
            Candidate::host("127.0.0.1:19003".parse().unwrap(), "udp").unwrap(),
        );
        let answer = peer
            .sdp_api()
            .accept_offer(str0m::change::SdpOffer::from_sdp_string(&offer).unwrap())
            .unwrap();
        caller.sdp_api().accept_answer(pending, answer).unwrap();
        let mut now = Instant::now();
        let mut caller_events = Vec::new();
        let mut peer_events = Vec::new();
        for _ in 0..300 {
            caller.handle_input(Input::Timeout(now)).unwrap();
            peer.handle_input(Input::Timeout(now)).unwrap();
            transfer(&mut caller, &mut peer, now, &mut caller_events);
            transfer(&mut peer, &mut caller, now, &mut peer_events);
            now += Duration::from_millis(10);
        }
        assert!(caller_events.iter().any(|e| matches!(e, Event::ChannelOpen(id, label) if *id == channel && label == "oai-events")));
        let remote_channel = peer_events
            .iter()
            .find_map(|e| match e {
                Event::ChannelOpen(id, label) if label == "oai-events" => Some(*id),
                _ => None,
            })
            .unwrap();
        assert!(
            caller
                .channel(channel)
                .unwrap()
                .write(false, b"caller-event")
                .unwrap()
        );
        assert!(
            peer.channel(remote_channel)
                .unwrap()
                .write(false, b"peer-event")
                .unwrap()
        );
        let mut encoder = voice_encoder().unwrap();
        let mut packet = [0; 1275];
        let len = encoder.encode(&[0.0; FRAME], FRAME, &mut packet).unwrap();
        for rtc in [&mut caller, &mut peer] {
            let writer = rtc.writer(mid).unwrap();
            let pt = writer
                .payload_params()
                .find(|p| p.spec().codec == Codec::Opus)
                .unwrap()
                .pt();
            writer
                .write(
                    pt,
                    now,
                    MediaTime::new(0, Frequency::FORTY_EIGHT_KHZ),
                    packet[..len].to_vec(),
                )
                .unwrap();
        }
        for _ in 0..100 {
            caller.handle_input(Input::Timeout(now)).unwrap();
            peer.handle_input(Input::Timeout(now)).unwrap();
            transfer(&mut caller, &mut peer, now, &mut caller_events);
            transfer(&mut peer, &mut caller, now, &mut peer_events);
            now += Duration::from_millis(10);
        }
        for (events, text) in [
            (&caller_events, b"peer-event".as_slice()),
            (&peer_events, b"caller-event".as_slice()),
        ] {
            assert!(events.iter().any(
                |e| matches!(e, Event::ChannelData(data) if !data.binary && data.data == text)
            ));
            let media = events
                .iter()
                .find_map(|e| match e {
                    Event::MediaData(data) => Some(data),
                    _ => None,
                })
                .unwrap();
            let mut decoder = Decoder::new(RATE as i32, 1).unwrap();
            assert_eq!(
                decoder
                    .decode(&media.data, MAX_PCM, &mut [0.0; MAX_PCM])
                    .unwrap(),
                FRAME
            );
        }
    }
}
