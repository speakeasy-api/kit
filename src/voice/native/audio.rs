//! Device PCM conversion with fixed history and no callback allocations.
use super::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, SupportedStreamConfig,
    SupportedStreamConfigRange,
};

pub(super) struct Audio {
    input: Option<cpal::Stream>,
    input_device: cpal::Device,
    capture_tx: SyncSender<Captured>,
    error_tx: SyncSender<String>,
    _output: cpal::Stream,
    pub(super) captured: Receiver<Captured>,
    pub(super) playback: SyncSender<Playback>,
    pub(super) errors: Receiver<String>,
}
fn audio_config(device: &cpal::Device, input: bool) -> Result<cpal::SupportedStreamConfig> {
    // CoreAudio reports the current stream format here. Prefer it over the
    // Opus rate to avoid an unnecessary hardware sample-rate transition.
    let default = if input {
        device.default_input_config()
    } else {
        device.default_output_config()
    };
    if let Ok(config) = select(default.ok(), &[], input) {
        return Ok(config);
    }
    let configs: Vec<_> = if input {
        device.supported_input_configs().map_err(message)?.collect()
    } else {
        device
            .supported_output_configs()
            .map_err(message)?
            .collect()
    };
    select(None, &configs, input)
}
impl Audio {
    pub(super) fn set_capture(&mut self, enabled: bool) -> Result<()> {
        if !enabled {
            // CPAL stream destruction stops the device callback.
            self.input = None;
        } else if self.input.is_none() {
            let input = capture(
                &self.input_device,
                audio_config(&self.input_device, true)?,
                self.capture_tx.clone(),
                self.error_tx.clone(),
            )?;
            input.play().map_err(message)?;
            self.input = Some(input);
        }
        Ok(())
    }
    pub(super) fn new() -> Result<Self> {
        let host = cpal::default_host();
        let input_device = host.default_input_device().ok_or("No default microphone")?;
        let output_device = host
            .default_output_device()
            .ok_or("No default audio output")?;
        // Validate availability without opening a recording stream.
        audio_config(&input_device, true)?;
        let (capture_tx, captured) = mpsc::sync_channel(8);
        let (playback, playback_rx) = mpsc::sync_channel::<Playback>(4);
        let (error_tx, errors) = mpsc::sync_channel(2);
        // Muting drops capture instead of requiring hardware pause support.
        let output_config = audio_config(&output_device, false)?;
        let output = self::playback(&output_device, output_config, playback_rx, error_tx.clone())?;
        output.play().map_err(message)?;
        Ok(Self {
            input: None,
            input_device,
            capture_tx,
            error_tx,
            _output: output,
            captured,
            playback,
            errors,
        })
    }
}

// Keep the native configuration intact; callbacks resample device PCM.
fn usable(format: SampleFormat, channels: u16, min: u32, max: u32) -> bool {
    matches!(
        format,
        SampleFormat::I8
            | SampleFormat::I16
            | SampleFormat::I24
            | SampleFormat::I32
            | SampleFormat::I64
            | SampleFormat::U8
            | SampleFormat::U16
            | SampleFormat::U24
            | SampleFormat::U32
            | SampleFormat::U64
            | SampleFormat::F32
            | SampleFormat::F64
    ) && (1..=32).contains(&channels)
        && min <= 192_000
        && max >= 8_000
}

pub(super) fn select(
    default: Option<SupportedStreamConfig>,
    configs: &[SupportedStreamConfigRange],
    input: bool,
) -> Result<SupportedStreamConfig> {
    if let Some(default) = default.filter(|c| {
        usable(
            c.sample_format(),
            c.channels(),
            c.sample_rate(),
            c.sample_rate(),
        )
    }) {
        return Ok(default);
    }
    configs
        .iter()
        .filter(|c| {
            usable(
                c.sample_format(),
                c.channels(),
                c.min_sample_rate(),
                c.max_sample_rate(),
            )
        })
        .map(|c| {
            c.with_sample_rate(RATE.clamp(
                c.min_sample_rate().max(8_000),
                c.max_sample_rate().min(192_000),
            ))
        })
        .min_by_key(|c| {
            (
                c.sample_rate().abs_diff(RATE),
                c.channels(),
                c.sample_format() != SampleFormat::F32,
            )
        })
        .ok_or_else(|| {
            let advertised = configs
                .iter()
                .take(8)
                .map(|c| {
                    format!(
                        "{} ch {:?} {}–{} Hz",
                        c.channels(),
                        c.sample_format(),
                        c.min_sample_rate(),
                        c.max_sample_rate()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Unsupported {} device: need integer or floating-point PCM, \
                 1–32 channels, 8000–192000 Hz; advertised configurations: {}",
                if input { "input" } else { "output" },
                if advertised.is_empty() {
                    "none"
                } else {
                    &advertised
                },
            )
        })
}

const TAPS: usize = 128;
const PHASES: usize = 1024;
/// Causal windowed-sinc low-pass resampler with 64 input samples of delay.
/// Integer rate accounting avoids drift. The fixed polyphase table is built
/// before stream creation; callbacks retain only a fixed history and phase.
struct Resampler {
    input: u32,
    output: u32,
    phase: u64,
    primed: bool,
    history: [f32; TAPS],
    head: usize,
    kernels: Vec<[f32; TAPS]>,
}
impl Resampler {
    fn new(input: u32, output: u32) -> Self {
        let cutoff = (output as f64 / input as f64).min(1.0) * 0.94;
        let kernels = if input == output {
            Vec::new()
        } else {
            (0..PHASES)
                .map(|phase| {
                    let mut kernel = [0.0; TAPS];
                    for (k, weight) in kernel.iter_mut().enumerate() {
                        let x = k as f64 + phase as f64 / PHASES as f64 - 64.0;
                        let z = std::f64::consts::PI * x * cutoff;
                        let sinc = if z.abs() < 1e-10 { 1.0 } else { z.sin() / z };
                        let window = 0.5 + 0.5 * (std::f64::consts::PI * x / 64.0).cos();
                        *weight = (cutoff * sinc * window) as f32;
                    }
                    let sum: f32 = kernel.iter().sum();
                    for weight in &mut kernel {
                        *weight /= sum;
                    }
                    kernel
                })
                .collect()
        };
        Self {
            input,
            output,
            phase: 0,
            primed: false,
            history: [0.0; TAPS],
            head: 0,
            kernels,
        }
    }
    fn next(&mut self, mut source: impl FnMut() -> Option<f32>) -> Option<f32> {
        if self.input == self.output {
            return source();
        }
        while !self.primed || self.phase >= u64::from(self.output) {
            let sample = source()?;
            self.head = (self.head + TAPS - 1) % TAPS;
            self.history[self.head] = sample;
            if self.primed {
                self.phase -= u64::from(self.output);
            }
            self.primed = true;
        }
        let kernel = &self.kernels[(self.phase * PHASES as u64 / u64::from(self.output)) as usize];
        let sample = kernel
            .iter()
            .enumerate()
            .map(|(k, w)| w * self.history[(self.head + k) % TAPS])
            .sum();
        self.phase += u64::from(self.input);
        Some(sample)
    }
}
fn mono<T: Sample>(frame: &[T]) -> f32
where
    f32: FromSample<T>,
{
    frame.iter().map(|v| f32::from_sample(*v)).sum::<f32>() / frame.len() as f32
}
fn pcm<T: Sample + FromSample<f32>>(sample: f32) -> T {
    T::from_sample(if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    })
}
fn input_stream<T: SizedSample>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: SyncSender<Captured>,
    errors: SyncSender<String>,
) -> Result<cpal::Stream>
where
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let mut resampler = Resampler::new(config.sample_rate, RATE);
    let mut samples = [0.0; FRAME];
    let mut used = 0;
    let mut sequence = 0;
    let mut began = Instant::now();
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let mut frames = data.chunks_exact(channels);
                while let Some(sample) = resampler.next(|| frames.next().map(mono)) {
                    if used == 0 {
                        began = Instant::now();
                    }
                    samples[used] = sample;
                    used += 1;
                    if used == FRAME {
                        let _ = tx.try_send(Captured {
                            began,
                            sequence,
                            samples,
                        });
                        used = 0;
                        sequence += 1;
                    }
                }
            },
            move |e| {
                let _ = errors.try_send(format!("Microphone: {e}"));
            },
            None,
        )
        .map_err(message)
}
fn output_stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    rx: Receiver<Playback>,
    errors: SyncSender<String>,
) -> Result<cpal::Stream> {
    let channels = usize::from(config.channels);
    let mut resampler = Resampler::new(RATE, config.sample_rate);
    let mut current: Option<Playback> = None;
    let mut offset = 0;
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                data.fill(pcm(0.0));
                for frame in data.chunks_exact_mut(channels) {
                    let sample = resampler
                        .next(|| {
                            if current
                                .as_ref()
                                .is_some_and(|p| offset == p.len || p.received.elapsed() > MAX_AGE)
                            {
                                current = None;
                            }
                            if current.is_none() {
                                current = rx.try_recv().ok();
                                offset = 0;
                            }
                            Some(
                                if let Some(p) = &current
                                    && offset < p.len
                                    && p.received.elapsed() <= MAX_AGE
                                {
                                    let sample = p.samples[offset];
                                    offset += 1;
                                    sample
                                } else {
                                    0.0
                                },
                            )
                        })
                        .unwrap_or(0.0);
                    frame.fill(pcm(sample));
                }
            },
            move |e| {
                let _ = errors.try_send(format!("Playback: {e}"));
            },
            None,
        )
        .map_err(message)
}
macro_rules! dispatch {
    ($format:expr, $function:ident, $($arg:expr),*) => {
        match $format {
            SampleFormat::I8 => $function::<i8>($($arg),*),
            SampleFormat::I16 => $function::<i16>($($arg),*),
            SampleFormat::I24 => $function::<cpal::I24>($($arg),*),
            SampleFormat::I32 => $function::<i32>($($arg),*),
            SampleFormat::I64 => $function::<i64>($($arg),*),
            SampleFormat::U8 => $function::<u8>($($arg),*),
            SampleFormat::U16 => $function::<u16>($($arg),*),
            SampleFormat::U24 => $function::<cpal::U24>($($arg),*),
            SampleFormat::U32 => $function::<u32>($($arg),*),
            SampleFormat::U64 => $function::<u64>($($arg),*),
            SampleFormat::F32 => $function::<f32>($($arg),*),
            SampleFormat::F64 => $function::<f64>($($arg),*),
            _ => Err("Unsupported PCM sample format".into()),
        }
    };
}
pub(super) fn capture(
    device: &cpal::Device,
    config: SupportedStreamConfig,
    tx: SyncSender<Captured>,
    errors: SyncSender<String>,
) -> Result<cpal::Stream> {
    dispatch!(
        config.sample_format(),
        input_stream,
        device,
        config.config(),
        tx,
        errors
    )
}
pub(super) fn playback(
    device: &cpal::Device,
    config: SupportedStreamConfig,
    rx: Receiver<Playback>,
    errors: SyncSender<String>,
) -> Result<cpal::Stream> {
    dispatch!(
        config.sample_format(),
        output_stream,
        device,
        config.config(),
        rx,
        errors
    )
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

    fn config(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            min,
            max,
            cpal::SupportedBufferSize::Unknown,
            format,
        )
    }

    #[test]
    fn selects_native_pcm_and_rate_without_relabeling() {
        for input in [true, false] {
            let selected =
                select(None, &[config(2, 44_100, 44_100, SampleFormat::I16)], input).unwrap();
            assert_eq!(selected.sample_rate(), 44_100);
            assert_eq!(selected.sample_format(), SampleFormat::I16);
            assert_eq!(selected.channels(), 2);
            let selected = select(
                None,
                &[
                    config(2, 44_100, 44_100, SampleFormat::F32),
                    config(6, 48_000, 96_000, SampleFormat::I32),
                ],
                input,
            )
            .unwrap();
            assert_eq!(selected.sample_rate(), RATE);
            assert_eq!(selected.channels(), 6);
        }
        assert_eq!(
            select(None, &[config(1, 8_000, 192_000, SampleFormat::F32)], true)
                .unwrap()
                .sample_rate(),
            RATE
        );
    }

    #[test]
    fn prefers_current_default_over_supported_opus_rate() {
        for input in [true, false] {
            let default = config(2, 44_100, 44_100, SampleFormat::I16).with_sample_rate(44_100);
            let supported = [config(1, 48_000, 48_000, SampleFormat::F32)];
            for configs in [&supported[..], &[][..]] {
                let selected = select(Some(default), configs, input).unwrap();
                assert_eq!(selected.sample_rate(), 44_100);
                assert_eq!(selected.channels(), 2);
                assert_eq!(selected.sample_format(), SampleFormat::I16);
            }
        }
    }

    #[test]
    fn unavailable_or_unusable_defaults_use_supported_ranges() {
        for input in [true, false] {
            for default in [
                None,
                Some(config(0, 44_100, 44_100, SampleFormat::F32).with_sample_rate(44_100)),
                Some(config(2, 4_000, 4_000, SampleFormat::I16).with_sample_rate(4_000)),
                Some(config(2, 384_000, 384_000, SampleFormat::F32).with_sample_rate(384_000)),
                Some(config(2, 48_000, 48_000, SampleFormat::DsdU8).with_sample_rate(48_000)),
            ] {
                let selected = select(
                    default,
                    &[config(2, 48_000, 48_000, SampleFormat::F32)],
                    input,
                )
                .unwrap();
                assert_eq!(selected.sample_rate(), 48_000);
                assert_eq!(selected.channels(), 2);
                assert_eq!(selected.sample_format(), SampleFormat::F32);
            }
        }
    }

    #[test]
    fn unsupported_diagnostics_include_direction_and_advertised_config() {
        for (input, direction) in [(true, "input"), (false, "output")] {
            let error =
                select(None, &[config(0, 4_000, 4_000, SampleFormat::I16)], input).unwrap_err();
            assert!(error.contains(direction));
            assert!(error.contains("0 ch I16 4000–4000 Hz"));
            assert!(error.contains("8000–192000 Hz"));
            assert!(
                select(None, &[], input)
                    .unwrap_err()
                    .contains("advertised configurations")
            );
        }
    }

    #[test]
    fn pcm_formats_normalize_mix_and_saturate() {
        macro_rules! check {
            ($($ty:ty),*) => {$(
                assert_eq!(mono(&[pcm::<$ty>(0.0)]), 0.0);
                assert_eq!(mono(&[pcm::<$ty>(-1.0)]), -1.0);
                assert!(mono(&[pcm::<$ty>(1.0)]) > 0.99);
                assert_eq!(pcm::<$ty>(2.0), pcm::<$ty>(1.0));
                assert_eq!(pcm::<$ty>(-2.0), pcm::<$ty>(-1.0));
                assert_eq!(pcm::<$ty>(f32::NAN), pcm::<$ty>(0.0));
            )*};
        }
        check!(
            i8,
            i16,
            cpal::I24,
            i32,
            i64,
            u8,
            u16,
            cpal::U24,
            u32,
            u64,
            f32,
            f64
        );
        assert_eq!(pcm::<u16>(0.0), 32768);
        assert_eq!(mono(&[16384_i16, -16384]), 0.0);
        assert_eq!(mono(&[0.5_f32, 0.25, -0.25, 0.5]), 0.25);
    }

    fn convert(data: &[f32], input: u32, output: u32, chunk: usize) -> Vec<f32> {
        let mut resampler = Resampler::new(input, output);
        let mut result = Vec::new();
        for chunk in data.chunks(chunk) {
            let mut iter = chunk.iter().copied();
            while let Some(sample) = resampler.next(|| iter.next()) {
                result.push(sample);
            }
        }
        result
    }

    #[test]
    fn resampling_is_continuous_across_callbacks_and_has_no_rate_drift() {
        for (input, output) in [
            (44_100, RATE),
            (RATE, 44_100),
            (96_000, RATE),
            (RATE, 16_000),
            (8_000, RATE),
            (192_000, RATE),
        ] {
            let data: Vec<_> = (0..input).map(|n| (n as f32 * 0.01).sin()).collect();
            let whole = convert(&data, input, output, data.len());
            assert_eq!(whole.len(), output as usize);
            assert_eq!(whole, convert(&data, input, output, 137));
        }
        let data = [0.0, 0.25, -0.5, 1.0];
        assert_eq!(convert(&data, RATE, RATE, 1), data);
    }

    #[test]
    fn resampling_preserves_dc_and_voice_tone() {
        for (input, output) in [(44_100, RATE), (RATE, 44_100), (RATE, 8_000)] {
            let dc = convert(&vec![0.5; input as usize / 10], input, output, 17);
            assert!(dc[256..].iter().all(|v| (v - 0.5).abs() < 1e-5));
            let data: Vec<_> = (0..input / 10)
                .map(|n| (std::f32::consts::TAU * 1000.0 * n as f32 / input as f32).sin())
                .collect();
            let converted = convert(&data, input, output, 31);
            let error = converted
                .iter()
                .enumerate()
                .skip(256)
                .map(|(n, value)| {
                    let expected = (std::f32::consts::TAU
                        * 1000.0
                        * (n as f32 / output as f32 - 64.0 / input as f32))
                        .sin();
                    (value - expected).powi(2)
                })
                .sum::<f32>()
                / (converted.len() - 256) as f32;
            assert!(error.sqrt() < 0.005, "{input}->{output}: {error}");
        }
    }

    #[test]
    fn downsampling_rejects_out_of_band_aliases() {
        for (input, output, frequency) in [
            (96_000, RATE, 30_000.0),
            (RATE, 16_000, 12_000.0),
            (192_000, RATE, 60_000.0),
        ] {
            let data: Vec<_> = (0..input / 10)
                .map(|n| (std::f32::consts::TAU * frequency * n as f32 / input as f32).sin())
                .collect();
            let converted = convert(&data, input, output, 43);
            let rms = (converted[256..].iter().map(|v| v * v).sum::<f32>()
                / (converted.len() - 256) as f32)
                .sqrt();
            assert!(rms < 0.01, "{input}->{output}: {rms}");
        }
    }
}
