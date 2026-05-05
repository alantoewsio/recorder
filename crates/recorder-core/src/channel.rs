use crate::buffer::AudioBuffer;
use crate::error::{RecordingError, Result};
use crate::traits::AudioProcessor;

const MIN_DB: f32 = -120.0;
pub const PARAMETRIC_EQ_BAND_COUNT: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelProcessorConfig {
    pub input_gain_db: f32,
    pub output_gain_db: f32,
    pub gate: NoiseGateConfig,
    pub compressor: CompressorConfig,
    pub parametric_eq: ParametricEqConfig,
}

impl Default for ChannelProcessorConfig {
    fn default() -> Self {
        Self {
            input_gain_db: 0.0,
            output_gain_db: 0.0,
            gate: NoiseGateConfig::default(),
            compressor: CompressorConfig::default(),
            parametric_eq: ParametricEqConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParametricEqConfig {
    pub enabled: bool,
    pub bands: [ParametricEqBandConfig; PARAMETRIC_EQ_BAND_COUNT],
}

impl Default for ParametricEqConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bands: [
                ParametricEqBandConfig::new(ParametricEqFilterType::Disabled, 80.0, 0.0, 0.9),
                ParametricEqBandConfig::new(ParametricEqFilterType::Disabled, 250.0, 0.0, 1.0),
                ParametricEqBandConfig::new(ParametricEqFilterType::Disabled, 1_000.0, 0.0, 1.0),
                ParametricEqBandConfig::new(ParametricEqFilterType::Disabled, 4_000.0, 0.0, 1.0),
                ParametricEqBandConfig::new(ParametricEqFilterType::Disabled, 10_000.0, 0.0, 1.0),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParametricEqFilterType {
    Disabled,
    Bell,
    LowPass,
    HighPass,
    LowShelf,
    HighShelf,
    Notch,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParametricEqBandConfig {
    pub filter_type: ParametricEqFilterType,
    pub frequency_hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

impl ParametricEqBandConfig {
    pub const fn new(
        filter_type: ParametricEqFilterType,
        frequency_hz: f32,
        gain_db: f32,
        q: f32,
    ) -> Self {
        Self {
            filter_type,
            frequency_hz,
            gain_db,
            q,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoiseGateConfig {
    pub enabled: bool,
    pub open_threshold_db: f32,
    pub close_threshold_db: f32,
}

impl Default for NoiseGateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            open_threshold_db: -45.0,
            close_threshold_db: -55.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressorConfig {
    pub enabled: bool,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_gain_db: f32,
}

impl Default for CompressorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_db: -18.0,
            ratio: 3.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            makeup_gain_db: 0.0,
        }
    }
}

pub struct ChannelProcessor {
    config: ChannelProcessorConfig,
    gate_open: bool,
    compressor_reduction_db: f32,
    eq_sample_rate_hz: u32,
    eq_channels: usize,
    eq_coefficients: [BiquadCoefficients; PARAMETRIC_EQ_BAND_COUNT],
    eq_states: Vec<BiquadState>,
    name: String,
}

impl ChannelProcessor {
    pub fn new(config: ChannelProcessorConfig) -> Self {
        Self {
            config,
            gate_open: true,
            compressor_reduction_db: 0.0,
            eq_sample_rate_hz: 0,
            eq_channels: 0,
            eq_coefficients: [BiquadCoefficients::identity(); PARAMETRIC_EQ_BAND_COUNT],
            eq_states: Vec::new(),
            name: "channel".to_string(),
        }
    }

    pub fn config(&self) -> ChannelProcessorConfig {
        self.config
    }

    pub fn set_config(&mut self, config: ChannelProcessorConfig) {
        if self.config.parametric_eq != config.parametric_eq {
            self.eq_sample_rate_hz = 0;
        }
        self.config = config;
    }

    pub fn gate_open(&self) -> bool {
        self.gate_open
    }

    pub fn compressor_reduction_db(&self) -> f32 {
        self.compressor_reduction_db
    }

    fn normalized_gate(&self) -> NoiseGateConfig {
        let mut gate = self.config.gate;
        if gate.close_threshold_db > gate.open_threshold_db {
            gate.close_threshold_db = gate.open_threshold_db;
        }
        gate
    }

    fn compressor_coefficients(&self, sample_rate_hz: u32) -> (f32, f32) {
        let sample_rate = sample_rate_hz.max(1) as f32;
        let attack_seconds = (self.config.compressor.attack_ms.max(0.01)) / 1000.0;
        let release_seconds = (self.config.compressor.release_ms.max(0.01)) / 1000.0;
        (
            (-1.0 / (attack_seconds * sample_rate)).exp(),
            (-1.0 / (release_seconds * sample_rate)).exp(),
        )
    }

    fn prepare_eq(&mut self, sample_rate_hz: u32, channels: usize) {
        if self.eq_sample_rate_hz == sample_rate_hz && self.eq_channels == channels {
            return;
        }
        self.eq_sample_rate_hz = sample_rate_hz;
        self.eq_channels = channels;
        for (coefficients, band) in self
            .eq_coefficients
            .iter_mut()
            .zip(self.config.parametric_eq.bands.iter())
        {
            *coefficients = if self.config.parametric_eq.enabled
                && band.filter_type != ParametricEqFilterType::Disabled
            {
                BiquadCoefficients::from_eq_band(sample_rate_hz, *band)
            } else {
                BiquadCoefficients::identity()
            };
        }
        self.eq_states
            .resize(PARAMETRIC_EQ_BAND_COUNT * channels, BiquadState::default());
    }

    fn process_eq_sample(&mut self, channel: usize, mut sample: f32) -> f32 {
        if !self.config.parametric_eq.enabled {
            return sample;
        }
        for band_index in 0..PARAMETRIC_EQ_BAND_COUNT {
            let state_index = band_index * self.eq_channels + channel;
            sample =
                self.eq_coefficients[band_index].process(sample, &mut self.eq_states[state_index]);
        }
        sample
    }
}

impl AudioProcessor for ChannelProcessor {
    fn name(&self) -> &str {
        &self.name
    }

    fn reset(&mut self) {
        self.gate_open = true;
        self.compressor_reduction_db = 0.0;
        self.eq_states.fill(BiquadState::default());
    }

    fn process(&mut self, input: &AudioBuffer, output: &mut AudioBuffer) -> Result<()> {
        if input.frames != output.frames || input.format != output.format {
            return Err(RecordingError::FormatMismatch {
                expected: input.format,
                got: output.format,
            });
        }

        let channels = input.format.channels as usize;
        if channels == 0 {
            return Err(RecordingError::Config(
                "ChannelProcessor requires at least one channel".into(),
            ));
        }

        output.captured_at = input.captured_at;
        output.frame_index = input.frame_index;

        let input_gain = db_to_gain(self.config.input_gain_db);
        let output_gain = db_to_gain(
            self.config.output_gain_db
                + if self.config.compressor.enabled {
                    self.config.compressor.makeup_gain_db
                } else {
                    0.0
                },
        );
        let gate = self.normalized_gate();
        let compressor = self.config.compressor;
        let ratio = compressor.ratio.max(1.0);
        let (attack_coeff, release_coeff) =
            self.compressor_coefficients(input.format.sample_rate_hz);
        self.prepare_eq(input.format.sample_rate_hz, channels);

        let mut data = Vec::with_capacity(input.data.len());
        for frame in input.data.chunks(channels).take(input.frames) {
            let mut processed_frame = Vec::with_capacity(channels);
            let mut input_peak = 0.0f32;
            for (channel, sample) in frame.iter().enumerate() {
                let eq_sample = self.process_eq_sample(channel, sample * input_gain);
                input_peak = input_peak.max(eq_sample.abs());
                processed_frame.push(eq_sample);
            }
            let level_db = amplitude_to_db(input_peak);

            if gate.enabled {
                if self.gate_open {
                    if level_db <= gate.close_threshold_db {
                        self.gate_open = false;
                    }
                } else if level_db >= gate.open_threshold_db {
                    self.gate_open = true;
                }
            } else {
                self.gate_open = true;
            }

            let desired_reduction_db = if compressor.enabled && level_db > compressor.threshold_db {
                let over_db = level_db - compressor.threshold_db;
                let compressed_over_db = over_db / ratio;
                compressed_over_db - over_db
            } else {
                0.0
            };
            let coeff = if desired_reduction_db < self.compressor_reduction_db {
                attack_coeff
            } else {
                release_coeff
            };
            self.compressor_reduction_db = desired_reduction_db
                + coeff * (self.compressor_reduction_db - desired_reduction_db);

            let dynamic_gain = if self.gate_open {
                db_to_gain(self.compressor_reduction_db)
            } else {
                0.0
            };
            let total_gain = dynamic_gain * output_gain;
            for sample in processed_frame {
                data.push((sample * total_gain).clamp(-1.0, 1.0));
            }
        }
        output.data = data.into();
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct BiquadState {
    z1: f32,
    z2: f32,
}

#[derive(Debug, Clone, Copy)]
struct BiquadCoefficients {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl BiquadCoefficients {
    const fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    fn from_eq_band(sample_rate_hz: u32, band: ParametricEqBandConfig) -> Self {
        let sample_rate = sample_rate_hz.max(1) as f32;
        let nyquist = sample_rate * 0.5;
        let frequency = band.frequency_hz.clamp(10.0, nyquist * 0.95);
        let q = band.q.clamp(0.1, 18.0);
        let gain_db = band.gain_db.clamp(-24.0, 24.0);
        if band.filter_type == ParametricEqFilterType::Disabled {
            return Self::identity();
        }
        let omega = std::f32::consts::TAU * frequency / sample_rate;
        let sin = omega.sin();
        let cos = omega.cos();
        let alpha = sin / (2.0 * q);
        let a = 10.0f32.powf(gain_db / 40.0);

        let (b0, b1, b2, a0, a1, a2) = match band.filter_type {
            ParametricEqFilterType::Bell => {
                if gain_db.abs() < 0.001 {
                    return Self::identity();
                }
                (
                    1.0 + alpha * a,
                    -2.0 * cos,
                    1.0 - alpha * a,
                    1.0 + alpha / a,
                    -2.0 * cos,
                    1.0 - alpha / a,
                )
            }
            ParametricEqFilterType::LowPass => {
                let b0 = (1.0 - cos) * 0.5;
                (b0, 1.0 - cos, b0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            ParametricEqFilterType::HighPass => {
                let b0 = (1.0 + cos) * 0.5;
                (b0, -(1.0 + cos), b0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            ParametricEqFilterType::Notch => {
                (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            ParametricEqFilterType::LowShelf | ParametricEqFilterType::HighShelf => {
                let sqrt_a = a.sqrt();
                let shelf_alpha = sin / 2.0 * ((a + 1.0 / a) * (1.0 / q - 1.0) + 2.0).sqrt();
                let two_sqrt_a_alpha = 2.0 * sqrt_a * shelf_alpha;
                if band.filter_type == ParametricEqFilterType::LowShelf {
                    (
                        a * ((a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha),
                        2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                        a * ((a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha),
                        (a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha,
                        -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                        (a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha,
                    )
                } else {
                    (
                        a * ((a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha),
                        -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                        a * ((a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha),
                        (a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha,
                        2.0 * ((a - 1.0) - (a + 1.0) * cos),
                        (a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha,
                    )
                }
            }
            ParametricEqFilterType::Disabled => return Self::identity(),
        };

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    fn process(self, input: f32, state: &mut BiquadState) -> f32 {
        let output = self.b0 * input + state.z1;
        state.z1 = self.b1 * input - self.a1 * output + state.z2;
        state.z2 = self.b2 * input - self.a2 * output;
        output
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

pub fn amplitude_to_db(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        MIN_DB
    } else {
        20.0 * amplitude.max(1.0e-6).log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    use crate::format::{AudioFormat, SampleFormat};

    fn buffer(samples: &[f32]) -> AudioBuffer {
        AudioBuffer::new(
            AudioFormat::new(48_000, 1, SampleFormat::F32),
            Arc::from(samples),
            samples.len(),
            Instant::now(),
            0,
        )
    }

    fn silent_like(input: &AudioBuffer) -> AudioBuffer {
        AudioBuffer::silent(
            input.format,
            input.frames,
            input.captured_at,
            input.frame_index,
        )
    }

    fn sine_buffer(frequency_hz: f32, frames: usize) -> AudioBuffer {
        let sample_rate = 48_000.0;
        let samples = (0..frames)
            .map(|frame| {
                (std::f32::consts::TAU * frequency_hz * frame as f32 / sample_rate).sin() * 0.1
            })
            .collect::<Vec<_>>();
        AudioBuffer::new(
            AudioFormat::new(sample_rate as u32, 1, SampleFormat::F32),
            Arc::from(samples),
            frames,
            Instant::now(),
            0,
        )
    }

    fn rms(buffer: &AudioBuffer) -> f32 {
        let mean_square = buffer
            .data
            .iter()
            .map(|sample| sample * sample)
            .sum::<f32>()
            / buffer.data.len() as f32;
        mean_square.sqrt()
    }

    #[test]
    fn input_and_output_gain_are_applied() {
        let input = buffer(&[0.1, -0.1, 0.2]);
        let mut output = silent_like(&input);
        let mut processor = ChannelProcessor::new(ChannelProcessorConfig {
            input_gain_db: 6.0,
            output_gain_db: -6.0,
            ..ChannelProcessorConfig::default()
        });

        processor.process(&input, &mut output).unwrap();

        for (got, expected) in output.data.iter().zip(input.data.iter()) {
            assert!(
                (got - expected).abs() < 0.001,
                "input/output gain should cancel: got {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn gate_uses_open_and_close_thresholds() {
        let input = buffer(&[0.001, 0.2, 0.08, 0.001]);
        let mut output = silent_like(&input);
        let mut processor = ChannelProcessor::new(ChannelProcessorConfig {
            gate: NoiseGateConfig {
                enabled: true,
                open_threshold_db: -20.0,
                close_threshold_db: -30.0,
            },
            ..ChannelProcessorConfig::default()
        });

        processor.process(&input, &mut output).unwrap();

        assert_eq!(
            output.data[0], 0.0,
            "gate should start closed below open threshold"
        );
        assert!(
            output.data[1] > 0.19,
            "gate should open above open threshold"
        );
        assert!(
            output.data[2] > 0.07,
            "gate should stay open until close threshold is crossed"
        );
        assert_eq!(
            output.data[3], 0.0,
            "gate should close below close threshold"
        );
    }

    #[test]
    fn compressor_reduces_level_above_threshold() {
        let input = buffer(&vec![0.8; 2_000]);
        let mut output = silent_like(&input);
        let mut processor = ChannelProcessor::new(ChannelProcessorConfig {
            compressor: CompressorConfig {
                enabled: true,
                threshold_db: -12.0,
                ratio: 4.0,
                attack_ms: 1.0,
                release_ms: 50.0,
                makeup_gain_db: 0.0,
            },
            ..ChannelProcessorConfig::default()
        });

        processor.process(&input, &mut output).unwrap();

        let tail = output.data[1_900];
        assert!(
            tail < 0.55,
            "compressor should reduce sustained loud audio: {tail}"
        );
        assert!(processor.compressor_reduction_db() < -3.0);
    }

    #[test]
    fn compressor_release_recovers_after_signal_drops() {
        let mut samples = vec![0.8; 1_000];
        samples.extend(vec![0.05; 8_000]);
        let input = buffer(&samples);
        let mut output = silent_like(&input);
        let mut processor = ChannelProcessor::new(ChannelProcessorConfig {
            compressor: CompressorConfig {
                enabled: true,
                threshold_db: -18.0,
                ratio: 6.0,
                attack_ms: 1.0,
                release_ms: 20.0,
                makeup_gain_db: 0.0,
            },
            ..ChannelProcessorConfig::default()
        });

        processor.process(&input, &mut output).unwrap();

        assert!(
            output.data[1_100] < 0.05,
            "release should still be recovering immediately after loud audio"
        );
        assert!(
            output.data[8_900] > 0.045,
            "release should recover after sustained quiet audio"
        );
    }

    #[test]
    fn makeup_gain_is_applied_after_compression() {
        let input = buffer(&vec![0.5; 2_000]);
        let mut dry = silent_like(&input);
        let mut makeup = silent_like(&input);
        let compressor = CompressorConfig {
            enabled: true,
            threshold_db: -18.0,
            ratio: 3.0,
            attack_ms: 1.0,
            release_ms: 50.0,
            makeup_gain_db: 0.0,
        };
        let mut dry_processor = ChannelProcessor::new(ChannelProcessorConfig {
            compressor,
            ..ChannelProcessorConfig::default()
        });
        let mut makeup_processor = ChannelProcessor::new(ChannelProcessorConfig {
            compressor: CompressorConfig {
                makeup_gain_db: 6.0,
                ..compressor
            },
            ..ChannelProcessorConfig::default()
        });

        dry_processor.process(&input, &mut dry).unwrap();
        makeup_processor.process(&input, &mut makeup).unwrap();

        assert!(makeup.data[1_900] > dry.data[1_900] * 1.9);
    }

    #[test]
    fn parametric_eq_boosts_matching_frequency() {
        let input = sine_buffer(1_000.0, 4_800);
        let mut output = silent_like(&input);
        let mut processor = ChannelProcessor::new(ChannelProcessorConfig {
            parametric_eq: ParametricEqConfig {
                enabled: true,
                bands: [
                    ParametricEqBandConfig::new(ParametricEqFilterType::Bell, 1_000.0, 12.0, 2.0),
                    ParametricEqBandConfig::new(ParametricEqFilterType::Disabled, 160.0, 0.0, 1.0),
                    ParametricEqBandConfig::new(ParametricEqFilterType::Disabled, 400.0, 0.0, 1.0),
                    ParametricEqBandConfig::new(
                        ParametricEqFilterType::Disabled,
                        2_000.0,
                        0.0,
                        1.0,
                    ),
                    ParametricEqBandConfig::new(
                        ParametricEqFilterType::Disabled,
                        5_000.0,
                        0.0,
                        1.0,
                    ),
                ],
            },
            ..ChannelProcessorConfig::default()
        });

        processor.process(&input, &mut output).unwrap();

        assert!(rms(&output) > rms(&input) * 2.0);
    }
}
