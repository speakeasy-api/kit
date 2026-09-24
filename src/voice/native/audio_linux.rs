//! Linux audio uses bundled libcubeb's dynamically loaded PulseAudio/ALSA backends.
//! Separate streams keep recording stopped until the user enables it.
use super::*;
use cubeb::{Context, DeviceState, DeviceType, MonoFrame, State, Stream, StreamBuilder};
use std::sync::Mutex;

type Pcm = MonoFrame<f32>;

pub(super) struct Audio {
    // Streams must be destroyed before their context (field declaration order).
    input: Option<Stream<Pcm>>,
    _output: Stream<Pcm>,
    capture_tx: SyncSender<Captured>,
    error_tx: SyncSender<String>,
    pub(super) captured: Receiver<Captured>,
    pub(super) playback: SyncSender<Playback>,
    pub(super) errors: Receiver<String>,
    context: Context,
}

fn params() -> cubeb::StreamParams {
    cubeb::StreamParamsBuilder::new()
        .format(cubeb::SampleFormat::Float32NE)
        .rate(RATE)
        .channels(1)
        .layout(cubeb::ChannelLayout::MONO)
        .take()
}

fn context() -> Result<Context> {
    let mut failures = Vec::new();
    // Prefer PulseAudio, then ALSA without requiring an audio server. cubeb's
    // backend argument is a preference, not a restriction: validate the result.
    for backend in [c"pulse", c"alsa"] {
        let result = Context::init(Some(c"Kit voice"), Some(backend))
            .map_err(message)
            .and_then(|context| {
                if !matches!(context.backend_id(), "pulse" | "alsa") {
                    return Err(format!(
                        "Unsupported audio backend: {}",
                        context.backend_id()
                    ));
                }
                for (kind, name) in [
                    (DeviceType::INPUT, "microphone"),
                    (DeviceType::OUTPUT, "audio output"),
                ] {
                    let devices = context.enumerate_devices(kind).map_err(message)?;
                    if !devices.iter().any(|device| {
                        device.state() == DeviceState::Enabled
                            && device.max_channels() > 0
                            && device.device_id() != Some("null")
                    }) {
                        return Err(format!("No available {name}"));
                    }
                }
                Ok(context)
            });
        match result {
            Ok(context) => return Ok(context),
            Err(error) => failures.push(format!("{}: {error}", backend.to_string_lossy())),
        }
    }
    Err(format!("Linux audio unavailable ({})", failures.join("; ")))
}

impl Audio {
    pub(super) fn new() -> Result<Self> {
        // Check initial device availability before the caller can signal an offer.
        // Devices can still disappear later, so capture startup remains fallible.
        let context = context()?;
        let params = params();
        let latency = context
            .min_latency(&params)
            .map_err(message)?
            .max(FRAME as u32);
        if context.backend_id() == "alsa" {
            // cubeb-sys 0.38 ALSA enumeration probes playback even for INPUT.
            // Init instead opens/configures SND_PCM_STREAM_CAPTURE. It leaves
            // the stream INACTIVE: rebuild/alsa_run exclude it from polling and
            // callbacks; only alsa_stream_start starts the prepared capture PCM.
            // Drop the probe without starting it, leaving the microphone closed.
            let mut probe = StreamBuilder::<Pcm>::new();
            probe
                .name("Kit voice microphone preflight")
                .default_input(&params)
                .latency(latency)
                .data_callback(|input, _| input.len() as isize)
                .state_callback(|_| {});
            drop(
                probe
                    .init(&context)
                    .map_err(|error| format!("Microphone preflight failed: {error}"))?,
            );
        }
        let (capture_tx, captured) = mpsc::sync_channel(8);
        let (playback, playback_rx) = mpsc::sync_channel(4);
        let (error_tx, errors) = mpsc::sync_channel(2);
        let output_errors = error_tx.clone();
        let mut playback_buffer = PlaybackBuffer::new(playback_rx);
        let mut builder = StreamBuilder::<Pcm>::new();
        builder
            .name("Kit voice playback")
            .default_output(&params)
            .latency(latency)
            .data_callback(move |_, output| playback_buffer.fill(output))
            .state_callback(move |state| {
                if state == State::Error {
                    let _ = output_errors.try_send("Audio playback failed".into());
                }
            });
        let output = builder.init(&context).map_err(message)?;
        output.start().map_err(message)?;
        Ok(Self {
            input: None,
            _output: output,
            capture_tx,
            error_tx,
            captured,
            playback,
            errors,
            context,
        })
    }

    pub(super) fn set_capture(&mut self, enabled: bool) -> Result<()> {
        if !enabled {
            // Destruction stops and joins callbacks even if stop reports an error.
            // Take first so every failure leaves capture closed; no sideband I/O.
            if let Some(input) = self.input.take() {
                let stopped = input.stop().map_err(message);
                drop(input);
                stopped?;
            }
        } else if self.input.is_none() {
            let params = params();
            let latency = self
                .context
                .min_latency(&params)
                .map_err(message)?
                .max(FRAME as u32);
            let mut capture = CaptureBuffer::new(self.capture_tx.clone());
            let errors = self.error_tx.clone();
            let mut builder = StreamBuilder::<Pcm>::new();
            builder
                .name("Kit voice microphone")
                .default_input(&params)
                .latency(latency)
                .data_callback(move |input, _| {
                    capture.push(input);
                    input.len() as isize
                })
                .state_callback(move |state| {
                    if state == State::Error {
                        let _ = errors.try_send("Microphone failed".into());
                    }
                });
            let input = builder.init(&self.context).map_err(message)?;
            input.start().map_err(message)?;
            self.input = Some(input);
        }
        Ok(())
    }
}

// libcubeb performs device-rate resampling and channel conversion. The callbacks
// only packetize fixed-size mono PCM at the existing Opus clock.
struct CaptureBuffer {
    tx: SyncSender<Captured>,
    samples: [f32; FRAME],
    used: usize,
    sequence: u64,
    began: Instant,
}

impl CaptureBuffer {
    fn new(tx: SyncSender<Captured>) -> Self {
        Self {
            tx,
            samples: [0.0; FRAME],
            used: 0,
            sequence: 0,
            began: Instant::now(),
        }
    }

    fn push(&mut self, input: &[Pcm]) {
        for frame in input {
            if self.used == 0 {
                self.began = Instant::now();
            }
            self.samples[self.used] = finite_pcm(frame.m);
            self.used += 1;
            if self.used == FRAME {
                let _ = self.tx.try_send(Captured {
                    began: self.began,
                    sequence: self.sequence,
                    samples: self.samples,
                });
                self.used = 0;
                self.sequence += 1;
            }
        }
    }
}

fn finite_pcm(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

struct PlaybackBuffer {
    // cubeb requires a Sync callback, but Receiver is only Send. The callback
    // exclusively owns this wrapper and uses &mut access, never locks it. No
    // concurrent writers, guards, poison recovery, or callback-time blocking.
    rx: Mutex<Receiver<Playback>>,
    current: Option<Playback>,
    offset: usize,
}

impl PlaybackBuffer {
    fn new(rx: Receiver<Playback>) -> Self {
        Self {
            rx: Mutex::new(rx),
            current: None,
            offset: 0,
        }
    }

    fn fill(&mut self, output: &mut [Pcm]) -> isize {
        output.fill(Pcm { m: 0.0 });
        let Ok(rx) = self.rx.get_mut() else {
            return -1;
        };
        for frame in output.iter_mut() {
            if self
                .current
                .as_ref()
                .is_some_and(|p| self.offset == p.len || p.received.elapsed() > MAX_AGE)
            {
                self.current = None;
            }
            if self.current.is_none() {
                self.current = rx.try_recv().ok();
                self.offset = 0;
            }
            if let Some(p) = &self.current
                && self.offset < p.len
                && p.received.elapsed() <= MAX_AGE
            {
                frame.m = finite_pcm(p.samples[self.offset]);
                self.offset += 1;
            }
        }
        output.len() as isize
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

    #[test]
    fn capture_packetizes_across_callbacks_and_drops_overflow() {
        let (tx, rx) = mpsc::sync_channel(1);
        let mut capture = CaptureBuffer::new(tx);
        capture.push(&[Pcm { m: 0.25 }; FRAME - 1]);
        assert!(rx.try_recv().is_err());
        capture.push(&[Pcm { m: 0.25 }; FRAME + 1]);
        let first = rx.try_recv().unwrap();
        assert_eq!(first.sequence, 0);
        assert_eq!(first.samples, [0.25; FRAME]);
        capture.push(&[Pcm { m: f32::NAN }; FRAME]);
        let next = rx.try_recv().unwrap();
        assert_eq!(next.sequence, 2);
        assert_eq!(next.samples, [0.0; FRAME]);
    }

    #[test]
    fn capture_restart_discards_partial_frames() {
        let (tx, rx) = mpsc::sync_channel(2);
        let mut capture = CaptureBuffer::new(tx.clone());
        capture.push(&[Pcm { m: 0.75 }; FRAME - 1]);
        drop(capture);
        let mut restarted = CaptureBuffer::new(tx);
        restarted.push(&[Pcm { m: -0.25 }; FRAME]);
        let frame = rx.try_recv().unwrap();
        assert_eq!(frame.sequence, 0);
        assert_eq!(frame.samples, [-0.25; FRAME]);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    #[ignore = "Run only in a sandbox without audio libraries, devices, or server sockets"]
    fn no_audio_environment_fails_before_transport_startup() {
        assert!(Audio::new().is_err());
    }

    #[test]
    #[ignore = "Run with ALSA_CONFIG_PATH pointing to fixtures/alsa-playback-only.conf, libasound available, and no audio devices/server sockets"]
    fn alsa_playback_only_default_fails_before_transport_startup() {
        // A real ALSA asym PCM accepts playback but has no capture slave.
        // Enumeration alone incorrectly advertises an enabled input device.
        let context = context().expect("fixture must pass device enumeration");
        assert_eq!(context.backend_id(), "alsa");
        let params = params();
        let mut builder = StreamBuilder::<Pcm>::new();
        builder
            .default_output(&params)
            .latency(context.min_latency(&params).unwrap().max(FRAME as u32))
            .data_callback(|_, output| output.len() as isize)
            .state_callback(|_| {});
        drop(
            builder
                .init(&context)
                .expect("fixture must support playback"),
        );
        drop(context);
        let error = match Audio::new() {
            Ok(_) => panic!("playback-only default must fail microphone preflight"),
            Err(error) => error,
        };
        assert!(error.contains("Microphone preflight failed"), "{error}");
    }

    #[test]
    fn playback_silences_stale_missing_and_disconnected_audio() {
        let (tx, rx) = mpsc::sync_channel(4);
        let mut playback = PlaybackBuffer::new(rx);
        tx.try_send(Playback {
            received: Instant::now() - MAX_AGE - Duration::from_secs(1),
            samples: [0.9; MAX_PCM],
            len: 2,
        })
        .ok()
        .unwrap();
        tx.try_send(Playback {
            received: Instant::now(),
            samples: [2.0; MAX_PCM],
            len: 2,
        })
        .ok()
        .unwrap();
        let mut output = [Pcm { m: 0.9 }; 4];
        assert_eq!(playback.fill(&mut output), 4);
        assert_eq!(output.map(|f| f.m), [0.0, 1.0, 1.0, 0.0]);
        drop(tx);
        playback.fill(&mut output);
        assert_eq!(output.map(|f| f.m), [0.0; 4]);
    }
}
