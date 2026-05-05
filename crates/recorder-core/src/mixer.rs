//! N-input bus mixer: align multiple capture legs to one sample rate, mix, optional post-mix
//! processors, then a single [`AudioSink`] (often a [`crate::composite::CompositeSink`]).
//!
//! The legacy two-source [`StreamMixer`] / [`MixerConfig`] API is preserved as a thin
//! wrapper over [`BusMixer`].
//!
//! ```text
//!   StreamPipeline ─► MixerInputSink ─► (channel) ─┐
//!   ...              ...                           ├─► BusMixer thread ─► AudioSink
//!   StreamPipeline ─► MixerInputSink ─► (channel) ─┘
//! ```

use std::collections::VecDeque;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TryRecvError};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

use crate::buffer::AudioBuffer;
use crate::channel::{ChannelProcessor, ChannelProcessorConfig};
use crate::error::{RecordingError, Result};
use crate::format::{AudioFormat, SampleFormat};
use crate::metrics::PipelineMetrics;
use crate::processing::run_processor_chain;
use crate::traits::{AudioProcessor, AudioSink};

/// Output shape of the mix (also used as the bus mix law for the two-source preset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixMode {
    /// Sum all legs into mono (each leg downmixed); soft limiter on the sum.
    SumMono,
    /// Sum all legs to mono then duplicate to stereo.
    SumStereo,
    /// Exactly two legs: leg 0 → left, leg 1 → right (per-frame mono per leg).
    SplitStereo,
}

impl MixMode {
    pub fn output_channels(self) -> u16 {
        match self {
            MixMode::SumMono => 1,
            MixMode::SumStereo | MixMode::SplitStereo => 2,
        }
    }
}

/// One bus input: native capture format and linear gain applied after downmixing the leg
/// to mono (for sum modes) or before routing (split).
#[derive(Debug, Clone, Copy)]
pub struct BusLegConfig {
    pub source_format: AudioFormat,
    pub gain: f32,
}

impl BusLegConfig {
    pub fn new(source_format: AudioFormat, gain: f32) -> Self {
        Self {
            source_format,
            gain,
        }
    }
}

/// Full bus configuration for [`BusMixer::spawn`].
pub struct BusMixerConfig {
    /// Sample rate of the mixed output (all legs are converted here).
    pub bus_sample_rate_hz: u32,
    pub mix_mode: MixMode,
    pub legs: Vec<BusLegConfig>,
    pub jitter_window: Duration,
    /// Plugins run on the bus worker after sum / split / limit.
    pub post_mix_processors: Vec<Box<dyn AudioProcessor + Send>>,
    pub plugin_budget_per_plugin: Option<Duration>,
    pub metrics: Option<Arc<PipelineMetrics>>,
}

impl BusMixerConfig {
    pub fn output_format(&self) -> AudioFormat {
        AudioFormat::new(
            self.bus_sample_rate_hz,
            self.mix_mode.output_channels(),
            SampleFormat::F32,
        )
    }

    fn validate(&self) -> Result<()> {
        if self.legs.is_empty() {
            return Err(RecordingError::Config(
                "BusMixerConfig requires at least one leg".into(),
            ));
        }
        if self.mix_mode == MixMode::SplitStereo && self.legs.len() != 2 {
            return Err(RecordingError::Config(
                "SplitStereo bus requires exactly two legs".into(),
            ));
        }
        Ok(())
    }
}

/// Tiny `AudioSink` adapter: forwards each buffer it receives to a channel. Used to feed
/// [`BusMixer`] / [`StreamMixer`] from inside an existing `RecordingSession` writer thread.
#[derive(Clone)]
pub struct MixerInputSink {
    tx: Sender<AudioBuffer>,
}

impl MixerInputSink {
    pub fn new(tx: Sender<AudioBuffer>) -> Self {
        Self { tx }
    }
}

impl AudioSink for MixerInputSink {
    fn write_pcm_f32(&mut self, buffer: &AudioBuffer) -> Result<()> {
        let _ = self.tx.try_send(buffer.clone());
        Ok(())
    }
}

/// Configuration for [`StreamMixer`] (mic + speaker preset).
#[derive(Debug, Clone, Copy)]
pub struct MixerConfig {
    pub mode: MixMode,
    pub mic_format: AudioFormat,
    pub speaker_format: AudioFormat,
    pub jitter_window: Duration,
}

impl MixerConfig {
    pub fn output_format(&self) -> AudioFormat {
        AudioFormat::new(
            self.mic_format.sample_rate_hz,
            self.mode.output_channels(),
            SampleFormat::F32,
        )
    }
}

impl From<MixerConfig> for BusMixerConfig {
    fn from(cfg: MixerConfig) -> Self {
        Self {
            bus_sample_rate_hz: cfg.mic_format.sample_rate_hz,
            mix_mode: cfg.mode,
            legs: vec![
                BusLegConfig::new(cfg.mic_format, 1.0),
                BusLegConfig::new(cfg.speaker_format, 1.0),
            ],
            jitter_window: cfg.jitter_window,
            post_mix_processors: Vec::new(),
            plugin_budget_per_plugin: None,
            metrics: None,
        }
    }
}

/// N-input bus mixer thread handle.
pub struct BusMixer {
    join: Option<JoinHandle<()>>,
}

impl BusMixer {
    pub fn spawn(
        config: BusMixerConfig,
        receivers: Vec<Receiver<AudioBuffer>>,
        out_sink: Box<dyn AudioSink>,
    ) -> Result<Self> {
        config.validate()?;
        if receivers.len() != config.legs.len() {
            return Err(RecordingError::Config(format!(
                "BusMixer: {} receivers but {} leg configs",
                receivers.len(),
                config.legs.len()
            )));
        }
        let join = std::thread::Builder::new()
            .name("recorder-bus-mixer".into())
            .spawn(move || {
                if let Err(e) = run_bus_thread(config, receivers, out_sink) {
                    tracing::error!("bus mixer thread error: {e}");
                }
            })
            .map_err(|e| RecordingError::Config(format!("bus mixer thread spawn: {e}")))?;
        Ok(Self { join: Some(join) })
    }

    pub fn stop(mut self) {
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for BusMixer {
    fn drop(&mut self) {
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Legacy two-input mixer handle.
pub struct StreamMixer {
    inner: BusMixer,
}

impl StreamMixer {
    pub fn spawn(
        config: MixerConfig,
        mic_rx: Receiver<AudioBuffer>,
        spk_rx: Receiver<AudioBuffer>,
        out_sink: Box<dyn AudioSink>,
    ) -> Result<Self> {
        let bus = BusMixerConfig::from(config);
        let inner = BusMixer::spawn(bus, vec![mic_rx, spk_rx], out_sink)?;
        Ok(Self { inner })
    }

    pub fn stop(self) {
        self.inner.stop();
    }
}

/// Pair of [`MixerInputSink`]s for the mic + speaker preset.
pub fn mixer_channels(
    capacity: usize,
) -> (
    (MixerInputSink, Receiver<AudioBuffer>),
    (MixerInputSink, Receiver<AudioBuffer>),
) {
    let (mic_tx, mic_rx) = bounded(capacity);
    let (spk_tx, spk_rx) = bounded(capacity);
    (
        (MixerInputSink::new(mic_tx), mic_rx),
        (MixerInputSink::new(spk_tx), spk_rx),
    )
}

/// `n` `(MixerInputSink, Receiver)` pairs with matching queue capacity.
pub fn bus_mixer_legs(capacity: usize, n: usize) -> Vec<(MixerInputSink, Receiver<AudioBuffer>)> {
    (0..n)
        .map(|_| {
            let (tx, rx) = bounded(capacity);
            (MixerInputSink::new(tx), rx)
        })
        .collect()
}

/// Runtime source id for the low-latency routing mixer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoutingMixerSource {
    Input(usize),
    Bus(usize),
    MainOutput,
}

/// Runtime target id for the low-latency routing mixer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoutingMixerTarget {
    Bus(usize),
    MainOutput,
    Output,
}

/// One routing edge in a live mixer graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingMixerRoute {
    pub source: RoutingMixerSource,
    pub target: RoutingMixerTarget,
}

/// Observes mixed/forwarded audio at a runtime node.
pub type RoutingMixerObserver = Arc<dyn Fn(&AudioBuffer) + Send + Sync>;

/// Mutable routing state for [`RoutingMixer`].
#[derive(Default, Clone)]
pub struct RoutingMixerState {
    pub routes: Vec<RoutingMixerRoute>,
    pub bus_observers: Vec<RoutingMixerObserver>,
    pub main_observer: Option<RoutingMixerObserver>,
    pub channel_processors: Vec<(RoutingMixerSource, ChannelProcessorConfig)>,
    pub disabled_sources: Vec<RoutingMixerSource>,
}

/// Low-latency route traversal and final-output mixer.
///
/// This is intended for DAW-style live monitoring where capture callbacks push buffers
/// into a routing graph, and an output-device callback pulls interleaved f32 samples.
/// It keeps an independent bounded queue per original source so multiple routed channels
/// are mixed concurrently instead of serialized into the output stream.
pub struct RoutingMixer {
    state: RwLock<Arc<RoutingMixerState>>,
    channel_processors: Mutex<BTreeMap<RoutingMixerSource, ChannelProcessor>>,
    output: Mutex<RoutingOutputState>,
}

impl RoutingMixer {
    pub fn new(output_format: AudioFormat, max_pending_frames: usize) -> Self {
        Self {
            state: RwLock::new(Arc::new(RoutingMixerState::default())),
            channel_processors: Mutex::new(BTreeMap::new()),
            output: Mutex::new(RoutingOutputState::new(output_format, max_pending_frames)),
        }
    }

    pub fn output_format(&self) -> AudioFormat {
        self.output
            .lock()
            .map(|output| output.format())
            .unwrap_or_else(|_| AudioFormat::new(48_000, 2, SampleFormat::F32))
    }

    pub fn update_state(&self, state: RoutingMixerState) {
        if let Ok(mut processors) = self.channel_processors.lock() {
            let configured = state
                .channel_processors
                .iter()
                .map(|(source, _)| source.clone())
                .collect::<BTreeSet<_>>();
            processors.retain(|source, _| configured.contains(source));
            for (source, config) in &state.channel_processors {
                processors
                    .entry(source.clone())
                    .and_modify(|processor| processor.set_config(*config))
                    .or_insert_with(|| ChannelProcessor::new(*config));
            }
        }
        if let Ok(mut guard) = self.state.write() {
            *guard = Arc::new(state);
        }
    }

    pub fn dispatch(&self, source: RoutingMixerSource, buffer: AudioBuffer) {
        if let Ok(state) = self.state.read().map(|guard| guard.clone()) {
            if routing_source_disabled(&state, &source) {
                return;
            }
            let buffer = self.process_channel(source.clone(), buffer);
            self.dispatch_inner(&state, source.clone(), source, buffer, &mut BTreeSet::new());
        }
    }

    pub fn fill_output(&self, out: &mut [f32]) {
        if let Ok(mut output) = self.output.try_lock() {
            output.fill(out);
        } else {
            out.fill(0.0);
        }
    }

    fn dispatch_inner(
        &self,
        state: &RoutingMixerState,
        source: RoutingMixerSource,
        origin: RoutingMixerSource,
        buffer: AudioBuffer,
        visited_busses: &mut BTreeSet<usize>,
    ) {
        let targets = state
            .routes
            .iter()
            .filter(|route| route.source == source)
            .map(|route| route.target.clone())
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return;
        }

        for target in targets {
            self.dispatch_target(
                state,
                source.clone(),
                origin.clone(),
                target,
                buffer.clone(),
                visited_busses,
            );
        }
    }

    fn dispatch_target(
        &self,
        state: &RoutingMixerState,
        source: RoutingMixerSource,
        origin: RoutingMixerSource,
        target: RoutingMixerTarget,
        buffer: AudioBuffer,
        visited_busses: &mut BTreeSet<usize>,
    ) {
        match target {
            RoutingMixerTarget::Bus(index) => {
                if !visited_busses.insert(index) {
                    return;
                }
                let bus_source = RoutingMixerSource::Bus(index);
                if routing_source_disabled(state, &bus_source) {
                    visited_busses.remove(&index);
                    return;
                }
                let buffer = self.process_channel(bus_source.clone(), buffer);
                if let Some(observer) = state.bus_observers.get(index) {
                    observer(&buffer);
                }
                self.dispatch_inner(state, bus_source, origin, buffer, visited_busses);
                visited_busses.remove(&index);
            }
            RoutingMixerTarget::MainOutput => {
                if routing_source_disabled(state, &RoutingMixerSource::MainOutput) {
                    return;
                }
                let buffer = self.process_channel(RoutingMixerSource::MainOutput, buffer);
                if let Some(observer) = &state.main_observer {
                    observer(&buffer);
                }
                if let Ok(mut output) = self.output.try_lock() {
                    output.push_buffer(&origin, &buffer);
                }
                self.dispatch_inner(
                    state,
                    RoutingMixerSource::MainOutput,
                    origin,
                    buffer,
                    visited_busses,
                );
            }
            RoutingMixerTarget::Output => {
                if !matches!(source, RoutingMixerSource::MainOutput) {
                    if routing_source_disabled(state, &RoutingMixerSource::MainOutput) {
                        return;
                    }
                    if let Ok(mut output) = self.output.try_lock() {
                        output.push_buffer(&origin, &buffer);
                    }
                }
            }
        }
    }

    fn process_channel(&self, source: RoutingMixerSource, buffer: AudioBuffer) -> AudioBuffer {
        let Ok(mut processors) = self.channel_processors.try_lock() else {
            return buffer;
        };
        let Some(processor) = processors.get_mut(&source) else {
            return buffer;
        };
        let mut output = AudioBuffer::silent(
            buffer.format,
            buffer.frames,
            buffer.captured_at,
            buffer.frame_index,
        );
        match processor.process(&buffer, &mut output) {
            Ok(()) => output,
            Err(_) => buffer,
        }
    }
}

fn routing_source_disabled(state: &RoutingMixerState, source: &RoutingMixerSource) -> bool {
    state
        .disabled_sources
        .iter()
        .any(|disabled| disabled == source)
}

struct RoutingOutputState {
    sample_rate_hz: u32,
    channels: usize,
    max_pending_frames: usize,
    sources: BTreeMap<RoutingMixerSource, RoutingSourceQueue>,
}

impl RoutingOutputState {
    fn new(format: AudioFormat, max_pending_frames: usize) -> Self {
        Self {
            sample_rate_hz: format.sample_rate_hz,
            channels: format.channels as usize,
            max_pending_frames,
            sources: BTreeMap::new(),
        }
    }

    fn format(&self) -> AudioFormat {
        AudioFormat::new(
            self.sample_rate_hz,
            self.channels.min(u16::MAX as usize) as u16,
            SampleFormat::F32,
        )
    }

    fn fill(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        if self.channels == 0 {
            return;
        }
        let frames = out.len() / self.channels;
        let source_count = self.sources.len().max(1) as f32;
        let mix_gain = (1.0 / source_count.sqrt()).min(1.0);
        for queue in self.sources.values_mut() {
            for frame in 0..frames {
                for ch in 0..self.channels {
                    let index = frame * self.channels + ch;
                    out[index] += queue.pending.pop_front().unwrap_or(0.0) * mix_gain;
                }
            }
        }
        for sample in out {
            *sample = sample.clamp(-1.0, 1.0);
        }
        self.sources.retain(|_, queue| !queue.pending.is_empty());
    }

    fn push_buffer(&mut self, source: &RoutingMixerSource, buffer: &AudioBuffer) {
        let in_channels = buffer.format.channels as usize;
        if in_channels == 0 || self.channels == 0 {
            return;
        }
        let queue = self.sources.entry(source.clone()).or_default();
        if queue.source_rate_hz != Some(buffer.format.sample_rate_hz)
            || queue.source_channels != in_channels
        {
            queue.pending.clear();
            queue.source_rate_hz = Some(buffer.format.sample_rate_hz);
            queue.source_channels = in_channels;
            queue.resample_pos = 0.0;
        }
        if buffer.format.sample_rate_hz != self.sample_rate_hz {
            queue.push_resampled(buffer, self.sample_rate_hz, self.channels);
            self.trim_queue(source);
            return;
        }
        for frame in buffer.data.chunks(in_channels).take(buffer.frames) {
            for out_ch in 0..self.channels {
                let sample = if in_channels == 1 {
                    frame[0]
                } else if out_ch < in_channels {
                    frame[out_ch]
                } else {
                    frame[in_channels - 1]
                };
                queue.pending.push_back(sample);
            }
        }
        self.trim_queue(source);
    }

    fn trim_queue(&mut self, source: &RoutingMixerSource) {
        let max_samples = self.max_pending_frames * self.channels;
        if let Some(queue) = self.sources.get_mut(source) {
            while queue.pending.len() > max_samples {
                queue.pending.pop_front();
            }
        }
    }
}

#[derive(Default)]
struct RoutingSourceQueue {
    pending: VecDeque<f32>,
    source_rate_hz: Option<u32>,
    source_channels: usize,
    resample_pos: f64,
}

impl RoutingSourceQueue {
    fn push_resampled(
        &mut self,
        buffer: &AudioBuffer,
        output_rate_hz: u32,
        output_channels: usize,
    ) {
        let input_channels = buffer.format.channels as usize;
        if input_channels == 0 || output_channels == 0 || buffer.frames == 0 {
            return;
        }
        let step = buffer.format.sample_rate_hz as f64 / output_rate_hz as f64;
        while self.resample_pos < buffer.frames.saturating_sub(1) as f64 {
            let frame0 = self.resample_pos.floor() as usize;
            let frame1 = (frame0 + 1).min(buffer.frames - 1);
            let frac = (self.resample_pos - frame0 as f64) as f32;
            for out_ch in 0..output_channels {
                let ch = if input_channels == 1 {
                    0
                } else {
                    out_ch.min(input_channels - 1)
                };
                let s0 = buffer.data[frame0 * input_channels + ch];
                let s1 = buffer.data[frame1 * input_channels + ch];
                self.pending.push_back(s0 + (s1 - s0) * frac);
            }
            self.resample_pos += step;
        }
        self.resample_pos -= buffer.frames as f64;
        if self.resample_pos < 0.0 {
            self.resample_pos = 0.0;
        }
    }
}

const BLOCK_SIZE: usize = 480;
const TOPUP_TIMEOUT: Duration = Duration::from_millis(20);

/// Per-leg resampling and buffering at the bus sample rate.
struct MixLegState {
    channels: usize,
    resampler: Option<SincFixedIn<f32>>,
    pending_planar: Vec<VecDeque<f32>>,
    planar_in: Vec<Vec<f32>>,
    planar_out: Vec<Vec<f32>>,
    interleaved: VecDeque<f32>,
}

impl MixLegState {
    fn new(channels: u16, source_rate: u32, target_rate: u32) -> Result<Self> {
        let channels = channels as usize;
        let resampler = if source_rate == target_rate {
            None
        } else {
            let ratio = target_rate as f64 / source_rate as f64;
            let chunk_size = 1024usize;
            let params = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            };
            Some(
                SincFixedIn::<f32>::new(ratio, 2.0, params, chunk_size, channels)
                    .map_err(|e| RecordingError::Config(format!("rubato init: {e}")))?,
            )
        };
        Ok(Self {
            channels,
            resampler,
            pending_planar: vec![VecDeque::new(); channels],
            planar_in: vec![Vec::new(); channels],
            planar_out: vec![Vec::new(); channels],
            interleaved: VecDeque::new(),
        })
    }

    fn frames_ready(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.interleaved.len() / self.channels
        }
    }

    fn ingest(&mut self, buf: &AudioBuffer) {
        if let Some(rs) = self.resampler.as_mut() {
            for (i, sample) in buf.data.iter().copied().enumerate() {
                self.pending_planar[i % self.channels].push_back(sample);
            }
            let needed = rs.input_frames_next();
            while self.pending_planar.iter().all(|q| q.len() >= needed) {
                for ch in 0..self.channels {
                    self.planar_in[ch].clear();
                    for _ in 0..needed {
                        self.planar_in[ch].push(self.pending_planar[ch].pop_front().unwrap());
                    }
                    self.planar_out[ch].clear();
                    self.planar_out[ch].resize(rs.output_frames_max(), 0.0);
                }
                let mut planar_in_slices: Vec<&[f32]> =
                    self.planar_in.iter().map(|v| v.as_slice()).collect();
                let mut planar_out_slices: Vec<&mut [f32]> = self
                    .planar_out
                    .iter_mut()
                    .map(|v| v.as_mut_slice())
                    .collect();
                match rs.process_into_buffer(
                    planar_in_slices.as_mut_slice(),
                    planar_out_slices.as_mut_slice(),
                    None,
                ) {
                    Ok((_, written)) => {
                        for frame in 0..written {
                            for ch in 0..self.channels {
                                self.interleaved.push_back(
                                    self.planar_out[ch].get(frame).copied().unwrap_or(0.0),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("rubato process error: {e}");
                        break;
                    }
                }
            }
        } else {
            self.interleaved.extend(buf.data.iter().copied());
        }
    }

    fn pop_frames(&mut self, frames: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * self.channels);
        for _ in 0..(frames * self.channels) {
            out.push(self.interleaved.pop_front().unwrap_or(0.0));
        }
        out
    }
}

fn run_bus_thread(
    mut config: BusMixerConfig,
    receivers: Vec<Receiver<AudioBuffer>>,
    mut out_sink: Box<dyn AudioSink>,
) -> Result<()> {
    let n = receivers.len();
    let target_rate = config.bus_sample_rate_hz;
    let mut legs: Vec<MixLegState> = config
        .legs
        .iter()
        .map(|leg| {
            MixLegState::new(
                leg.source_format.channels,
                leg.source_format.sample_rate_hz,
                target_rate,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let mut done: Vec<bool> = vec![false; n];
    let frame_index_anchor = std::time::Instant::now();
    let mut output_frame_index: u64 = 0;

    let max_jitter_frames =
        ((config.jitter_window.as_nanos() * u128::from(target_rate)) / 1_000_000_000) as usize;

    let metrics_owned = config
        .metrics
        .clone()
        .unwrap_or_else(|| Arc::new(PipelineMetrics::default()));
    let budget = config.plugin_budget_per_plugin;
    let mut post_chain = std::mem::take(&mut config.post_mix_processors);

    loop {
        for i in 0..n {
            top_up_leg(&receivers[i], &mut legs[i], &mut done[i]);
        }

        for i in 0..n {
            if legs[i].frames_ready() > max_jitter_frames {
                let drop = legs[i].frames_ready() - max_jitter_frames;
                legs[i].pop_frames(drop);
            }
        }

        let have: Vec<usize> = (0..n).map(|i| legs[i].frames_ready()).collect();

        if done.iter().all(|&d| d) && have.iter().all(|&h| h == 0) {
            break;
        }

        let frames = compute_emit_frames(&have, &done, n);

        if frames == 0 {
            if !done.iter().all(|&d| d) {
                std::thread::yield_now();
            }
            continue;
        }

        let mut blocks: Vec<Vec<f32>> = Vec::with_capacity(n);
        for i in 0..n {
            blocks.push(legs[i].pop_frames(frames));
        }

        let mixed = mix_n_blocks(&blocks, &config.legs, config.mix_mode, frames);
        let out_format = config.output_format();
        let captured_at = frame_index_anchor + frame_duration(output_frame_index, target_rate);
        let mut buf = AudioBuffer::new(
            out_format,
            Arc::from(mixed.into_boxed_slice()),
            frames,
            captured_at,
            output_frame_index,
        );
        output_frame_index += frames as u64;

        if !post_chain.is_empty() {
            buf = run_processor_chain(
                buf,
                &mut post_chain,
                budget.as_ref().copied(),
                &metrics_owned,
            );
        }

        out_sink.write_pcm_f32(&buf)?;
    }

    let _ = out_sink.flush();
    Ok(())
}

fn compute_emit_frames(have: &[usize], done: &[bool], n: usize) -> usize {
    let alive: Vec<usize> = (0..n).filter(|&i| !done[i]).collect();
    if alive.is_empty() {
        return have.iter().copied().max().unwrap_or(0).min(BLOCK_SIZE);
    }
    let with_data: Vec<usize> = alive.iter().copied().filter(|&i| have[i] > 0).collect();
    if with_data.is_empty() {
        return 0;
    }
    if with_data.len() == alive.len() {
        let m = with_data.iter().map(|&i| have[i]).min().unwrap();
        return m.min(BLOCK_SIZE);
    }
    with_data
        .iter()
        .map(|&i| have[i].min(BLOCK_SIZE))
        .max()
        .unwrap_or(0)
}

fn frame_duration(frame_index: u64, sample_rate: u32) -> Duration {
    let ns = (u128::from(frame_index) * 1_000_000_000) / u128::from(sample_rate.max(1));
    Duration::from_nanos(ns.min(u64::MAX as u128) as u64)
}

fn top_up_leg(rx: &Receiver<AudioBuffer>, state: &mut MixLegState, done: &mut bool) {
    while let Ok(buf) = rx.try_recv() {
        state.ingest(&buf);
    }
    while !*done && state.frames_ready() < BLOCK_SIZE {
        match rx.recv_timeout(TOPUP_TIMEOUT) {
            Ok(buf) => state.ingest(&buf),
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => {
                *done = true;
                break;
            }
        }
    }
    if !*done && matches!(rx.try_recv(), Err(TryRecvError::Disconnected)) {
        *done = true;
    }
}

fn mix_n_blocks(
    blocks: &[Vec<f32>],
    legs: &[BusLegConfig],
    mix_mode: MixMode,
    frames: usize,
) -> Vec<f32> {
    let n = blocks.len();
    let leg_ch: Vec<usize> = legs
        .iter()
        .map(|l| l.source_format.channels.max(1) as usize)
        .collect();
    let out_ch = mix_mode.output_channels() as usize;
    let mut out = Vec::with_capacity(frames * out_ch);

    match mix_mode {
        MixMode::SplitStereo => {
            let mic_ch = leg_ch.get(0).copied().unwrap_or(1);
            let spk_ch = leg_ch.get(1).copied().unwrap_or(1);
            let b0 = &blocks[0];
            let b1 = &blocks[1];
            let g0 = legs[0].gain;
            let g1 = legs.get(1).map(|l| l.gain).unwrap_or(1.0);
            for f in 0..frames {
                let m0 = downmix_to_mono(b0, f, mic_ch) * g0;
                let m1 = downmix_to_mono(b1, f, spk_ch) * g1;
                out.push(soft_limit(m0));
                out.push(soft_limit(m1));
            }
        }
        MixMode::SumMono | MixMode::SumStereo => {
            for f in 0..frames {
                let mut acc = 0.0f32;
                for i in 0..n {
                    let ch = leg_ch.get(i).copied().unwrap_or(1);
                    let m = downmix_to_mono(&blocks[i], f, ch)
                        * legs.get(i).map(|l| l.gain).unwrap_or(1.0);
                    acc += m;
                }
                let s = soft_limit(acc);
                if mix_mode == MixMode::SumMono {
                    out.push(s);
                } else {
                    out.push(s);
                    out.push(s);
                }
            }
        }
    }

    out
}

fn downmix_to_mono(buf: &[f32], frame: usize, channels: usize) -> f32 {
    let base = frame * channels;
    if channels == 0 {
        return 0.0;
    }
    let mut sum = 0.0f32;
    let mut count = 0u32;
    for ch in 0..channels {
        if let Some(s) = buf.get(base + ch) {
            sum += *s;
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        sum / count as f32
    }
}

fn soft_limit(x: f32) -> f32 {
    const KNEE: f32 = 0.5;
    if x.abs() <= KNEE {
        x
    } else {
        let sign = x.signum();
        let over = x.abs() - KNEE;
        sign * (KNEE + (1.0 - KNEE) * (over / (1.0 - KNEE)).tanh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    #[derive(Default)]
    struct CaptureSink {
        inner: Arc<Mutex<Vec<AudioBuffer>>>,
    }

    impl AudioSink for CaptureSink {
        fn write_pcm_f32(&mut self, buffer: &AudioBuffer) -> Result<()> {
            self.inner.lock().unwrap().push(buffer.clone());
            Ok(())
        }
    }

    fn dc_buffer(format: AudioFormat, frames: usize, value: f32, frame_index: u64) -> AudioBuffer {
        let n = format.samples_per_frame(frames);
        let data: Arc<[f32]> = vec![value; n].into();
        AudioBuffer::new(format, data, frames, Instant::now(), frame_index)
    }

    fn tone_buffer(
        format: AudioFormat,
        freq_hz: f32,
        frames: usize,
        frame_index: u64,
    ) -> AudioBuffer {
        let channels = format.channels as usize;
        let mut data = Vec::with_capacity(frames * channels);
        for frame in 0..frames {
            let t = (frame_index as usize + frame) as f32 / format.sample_rate_hz as f32;
            let sample = (2.0 * std::f32::consts::PI * freq_hz * t).sin() * 0.25;
            for _ in 0..channels {
                data.push(sample);
            }
        }
        AudioBuffer::new(format, data.into(), frames, Instant::now(), frame_index)
    }

    fn mono_samples(buffers: &[AudioBuffer]) -> Vec<f32> {
        let mut out = Vec::new();
        for buffer in buffers {
            let channels = buffer.format.channels as usize;
            if channels == 0 {
                continue;
            }
            for frame in buffer.data.chunks(channels) {
                out.push(frame[0]);
            }
        }
        out
    }

    fn dominant_bin(samples: &[f32], sample_rate: f32, target_hz: f32) -> f32 {
        let n = samples.len().max(1) as f32;
        let mut re = 0.0;
        let mut im = 0.0;
        for (i, sample) in samples.iter().copied().enumerate() {
            let phase = 2.0 * std::f32::consts::PI * target_hz * i as f32 / sample_rate;
            re += sample * phase.cos();
            im -= sample * phase.sin();
        }
        (re * re + im * im).sqrt() / n
    }

    fn queue_tone(
        sink: &mut MixerInputSink,
        format: AudioFormat,
        freq_hz: f32,
        chunk_frames: usize,
        chunks: usize,
    ) {
        for i in 0..chunks {
            let frame_index = (i * chunk_frames) as u64;
            sink.write_pcm_f32(&tone_buffer(format, freq_hz, chunk_frames, frame_index))
                .unwrap();
        }
    }

    #[test]
    fn limiter_clamps_into_unit_range() {
        for raw in [-3.0f32, -1.5, -1.0, -0.6, 0.0, 0.6, 1.0, 1.5, 3.0] {
            let limited = soft_limit(raw);
            assert!(
                limited.abs() < 1.0 + 1e-6,
                "soft_limit({raw}) = {limited} escaped [-1, 1]"
            );
        }
        assert!((soft_limit(0.25) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn split_stereo_routes_mic_left_speaker_right() {
        let mic_format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let spk_format = AudioFormat::new(48_000, 2, SampleFormat::F32);
        let cfg = MixerConfig {
            mode: MixMode::SplitStereo,
            mic_format,
            speaker_format: spk_format,
            jitter_window: Duration::from_millis(200),
        };
        let captured = Arc::new(Mutex::new(Vec::<AudioBuffer>::new()));
        let sink = Box::new(CaptureSink {
            inner: captured.clone(),
        });
        let ((mut mic_sink, mic_rx), (mut spk_sink, spk_rx)) = mixer_channels(8);
        let mixer = StreamMixer::spawn(cfg, mic_rx, spk_rx, sink).expect("spawn");

        for i in 0..3 {
            mic_sink
                .write_pcm_f32(&dc_buffer(mic_format, 480, 0.4, (i * 480) as u64))
                .unwrap();
            spk_sink
                .write_pcm_f32(&dc_buffer(spk_format, 480, 0.2, (i * 480) as u64))
                .unwrap();
        }
        drop(mic_sink);
        drop(spk_sink);
        mixer.stop();

        let buffers = captured.lock().unwrap().clone();
        assert!(!buffers.is_empty(), "expected mixed output");
        let buf = &buffers[0];
        assert_eq!(buf.format.channels, 2);
        assert!((buf.data[0] - 0.4).abs() < 1e-3, "left = {}", buf.data[0]);
        assert!((buf.data[1] - 0.2).abs() < 1e-3, "right = {}", buf.data[1]);
    }

    #[test]
    fn dual_live_emits_mic_only_while_speaker_queue_empty() {
        let mic_format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let spk_format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let cfg = MixerConfig {
            mode: MixMode::SumMono,
            mic_format,
            speaker_format: spk_format,
            jitter_window: Duration::from_millis(200),
        };
        let captured = Arc::new(Mutex::new(Vec::<AudioBuffer>::new()));
        let sink = Box::new(CaptureSink {
            inner: captured.clone(),
        });
        let ((mut mic_sink, mic_rx), (spk_sink, spk_rx)) = mixer_channels(8);
        let mixer = StreamMixer::spawn(cfg, mic_rx, spk_rx, sink).expect("spawn");

        mic_sink
            .write_pcm_f32(&dc_buffer(mic_format, 480, 0.1, 0))
            .unwrap();
        std::thread::sleep(Duration::from_millis(40));

        drop(mic_sink);
        drop(spk_sink);
        mixer.stop();

        let buffers = captured.lock().unwrap().clone();
        assert!(
            !buffers.is_empty(),
            "expected mic-only mix while speaker has not produced yet"
        );
        assert!(
            (buffers[0].data[0] - 0.1).abs() < 1e-3,
            "got {}",
            buffers[0].data[0]
        );
    }

    #[test]
    fn sum_mono_combines_both_sources() {
        let mic_format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let spk_format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let cfg = MixerConfig {
            mode: MixMode::SumMono,
            mic_format,
            speaker_format: spk_format,
            jitter_window: Duration::from_millis(200),
        };
        let captured = Arc::new(Mutex::new(Vec::<AudioBuffer>::new()));
        let sink = Box::new(CaptureSink {
            inner: captured.clone(),
        });
        let ((mut mic_sink, mic_rx), (mut spk_sink, spk_rx)) = mixer_channels(8);
        let mixer = StreamMixer::spawn(cfg, mic_rx, spk_rx, sink).expect("spawn");

        for i in 0..3 {
            mic_sink
                .write_pcm_f32(&dc_buffer(mic_format, 480, 0.1, (i * 480) as u64))
                .unwrap();
            spk_sink
                .write_pcm_f32(&dc_buffer(spk_format, 480, 0.2, (i * 480) as u64))
                .unwrap();
        }
        drop(mic_sink);
        drop(spk_sink);
        mixer.stop();

        let buffers = captured.lock().unwrap().clone();
        assert!(!buffers.is_empty());
        let buf = &buffers[0];
        assert_eq!(buf.format.channels, 1);
        assert!((buf.data[0] - 0.3).abs() < 1e-3, "got {}", buf.data[0]);
    }

    #[test]
    fn jitter_window_caps_buffering_when_one_side_runs_long() {
        let mic_format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let spk_format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let cfg = MixerConfig {
            mode: MixMode::SumMono,
            mic_format,
            speaker_format: spk_format,
            jitter_window: Duration::from_millis(50),
        };
        let captured = Arc::new(Mutex::new(Vec::<AudioBuffer>::new()));
        let sink = Box::new(CaptureSink {
            inner: captured.clone(),
        });
        let ((mut mic_sink, mic_rx), (_spk_sink, spk_rx)) = mixer_channels(64);
        drop(_spk_sink);
        let mixer = StreamMixer::spawn(cfg, mic_rx, spk_rx, sink).expect("spawn");

        for i in 0..100 {
            mic_sink
                .write_pcm_f32(&dc_buffer(mic_format, 480, 0.05, (i * 480) as u64))
                .unwrap();
        }
        drop(mic_sink);
        mixer.stop();

        let buffers = captured.lock().unwrap().clone();
        let total_frames: usize = buffers.iter().map(|b| b.frames).sum();
        assert!(total_frames > 0, "mixer produced no frames");
        assert!(total_frames <= 48_000, "produced {total_frames} > 48000");
    }

    #[test]
    fn resampler_runs_for_44100_speaker_into_48000_mic() {
        let mic_format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let spk_format = AudioFormat::new(44_100, 2, SampleFormat::F32);
        let cfg = MixerConfig {
            mode: MixMode::SumStereo,
            mic_format,
            speaker_format: spk_format,
            jitter_window: Duration::from_millis(500),
        };
        let captured = Arc::new(Mutex::new(Vec::<AudioBuffer>::new()));
        let sink = Box::new(CaptureSink {
            inner: captured.clone(),
        });
        let ((mut mic_sink, mic_rx), (mut spk_sink, spk_rx)) = mixer_channels(32);
        let mixer = StreamMixer::spawn(cfg, mic_rx, spk_rx, sink).expect("spawn");

        for i in 0..16 {
            mic_sink
                .write_pcm_f32(&dc_buffer(mic_format, 480, 0.0, (i * 480) as u64))
                .unwrap();
            spk_sink
                .write_pcm_f32(&dc_buffer(spk_format, 1024, 0.0, (i * 1024) as u64))
                .unwrap();
        }
        drop(mic_sink);
        drop(spk_sink);
        mixer.stop();

        let buffers = captured.lock().unwrap().clone();
        assert!(!buffers.is_empty(), "mixer produced no output");
        assert_eq!(buffers[0].format.sample_rate_hz, 48_000);
        assert_eq!(buffers[0].format.channels, 2);
    }

    /// Three mono legs at 48 kHz summed to mono; proves N-input path and alignment.
    #[test]
    fn bus_mixer_three_inputs_sum_mono() {
        let f = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let bus_cfg = BusMixerConfig {
            bus_sample_rate_hz: 48_000,
            mix_mode: MixMode::SumMono,
            legs: vec![
                BusLegConfig::new(f, 1.0),
                BusLegConfig::new(f, 1.0),
                BusLegConfig::new(f, 1.0),
            ],
            jitter_window: Duration::from_millis(200),
            post_mix_processors: Vec::new(),
            plugin_budget_per_plugin: None,
            metrics: None,
        };
        let legs = bus_mixer_legs(16, 3);
        let mut sinks: Vec<MixerInputSink> = Vec::new();
        let mut rxs = Vec::new();
        for (s, r) in legs {
            sinks.push(s);
            rxs.push(r);
        }
        let captured = Arc::new(Mutex::new(Vec::<AudioBuffer>::new()));
        let sink = Box::new(CaptureSink {
            inner: captured.clone(),
        });
        let mixer = BusMixer::spawn(bus_cfg, rxs, sink).expect("spawn");

        for i in 0..2 {
            let base = (i * 480) as u64;
            sinks[0]
                .write_pcm_f32(&dc_buffer(f, 480, 0.1, base))
                .unwrap();
            sinks[1]
                .write_pcm_f32(&dc_buffer(f, 480, 0.2, base))
                .unwrap();
            sinks[2]
                .write_pcm_f32(&dc_buffer(f, 480, 0.3, base))
                .unwrap();
        }
        drop(sinks);
        mixer.stop();

        let buffers = captured.lock().unwrap().clone();
        assert!(!buffers.is_empty());
        assert!((buffers[0].data[0] - 0.6).abs() < 0.05, "sum 0.1+0.2+0.3");
    }

    #[test]
    fn bus_mixer_preserves_distinct_input_tones() {
        let f = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let bus_cfg = BusMixerConfig {
            bus_sample_rate_hz: 48_000,
            mix_mode: MixMode::SumMono,
            legs: vec![BusLegConfig::new(f, 1.0), BusLegConfig::new(f, 1.0)],
            jitter_window: Duration::from_millis(200),
            post_mix_processors: Vec::new(),
            plugin_budget_per_plugin: None,
            metrics: None,
        };
        let legs = bus_mixer_legs(64, 2);
        let mut sinks = Vec::new();
        let mut rxs = Vec::new();
        for (s, r) in legs {
            sinks.push(s);
            rxs.push(r);
        }
        queue_tone(&mut sinks[0], f, 440.0, 480, 32);
        queue_tone(&mut sinks[1], f, 880.0, 480, 32);
        drop(sinks);

        let captured = Arc::new(Mutex::new(Vec::<AudioBuffer>::new()));
        let sink = Box::new(CaptureSink {
            inner: captured.clone(),
        });
        let mixer = BusMixer::spawn(bus_cfg, rxs, sink).expect("spawn");
        mixer.stop();

        let samples = mono_samples(&captured.lock().unwrap());
        assert!(samples.len() >= 8_000, "not enough mixed samples");
        let bin_440 = dominant_bin(&samples, 48_000.0, 440.0);
        let bin_880 = dominant_bin(&samples, 48_000.0, 880.0);
        let bin_220 = dominant_bin(&samples, 48_000.0, 220.0);
        assert!(bin_440 > 0.03, "440 Hz tone missing: {bin_440}");
        assert!(bin_880 > 0.03, "880 Hz tone missing: {bin_880}");
        assert!(
            bin_220 < bin_440 * 0.5,
            "unexpected pitched-down artifact: 220={bin_220}, 440={bin_440}"
        );
    }

    #[test]
    fn bus_mixer_resamples_input_tone_without_pitch_shift() {
        let input_format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let bus_rate = 44_100;
        let bus_cfg = BusMixerConfig {
            bus_sample_rate_hz: bus_rate,
            mix_mode: MixMode::SumMono,
            legs: vec![BusLegConfig::new(input_format, 1.0)],
            jitter_window: Duration::from_millis(500),
            post_mix_processors: Vec::new(),
            plugin_budget_per_plugin: None,
            metrics: None,
        };
        let legs = bus_mixer_legs(64, 1);
        let (mut input, rx) = legs.into_iter().next().unwrap();
        queue_tone(&mut input, input_format, 440.0, 1024, 32);
        drop(input);

        let captured = Arc::new(Mutex::new(Vec::<AudioBuffer>::new()));
        let sink = Box::new(CaptureSink {
            inner: captured.clone(),
        });
        let mixer = BusMixer::spawn(bus_cfg, vec![rx], sink).expect("spawn");
        mixer.stop();

        let buffers = captured.lock().unwrap().clone();
        assert!(!buffers.is_empty(), "mixer produced no output");
        assert_eq!(buffers[0].format.sample_rate_hz, bus_rate);
        let samples = mono_samples(&buffers);
        assert!(samples.len() >= 8_000, "not enough resampled samples");
        let bin_440 = dominant_bin(&samples, bus_rate as f32, 440.0);
        let pitched_down = 440.0 * bus_rate as f32 / input_format.sample_rate_hz as f32;
        let bin_pitched_down = dominant_bin(&samples, bus_rate as f32, pitched_down);
        assert!(
            bin_440 > 0.03,
            "440 Hz tone missing after resample: {bin_440}"
        );
        assert!(
            bin_pitched_down < bin_440 * 0.5,
            "resampler shifted pitch: {pitched_down} Hz bin {bin_pitched_down}, 440 Hz bin {bin_440}"
        );
    }

    #[test]
    fn routing_mixer_preserves_original_sources_through_bus() {
        let format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let mixer = RoutingMixer::new(format, 4_800);
        mixer.update_state(RoutingMixerState {
            routes: vec![
                RoutingMixerRoute {
                    source: RoutingMixerSource::Input(0),
                    target: RoutingMixerTarget::Bus(0),
                },
                RoutingMixerRoute {
                    source: RoutingMixerSource::Input(1),
                    target: RoutingMixerTarget::Bus(0),
                },
                RoutingMixerRoute {
                    source: RoutingMixerSource::Bus(0),
                    target: RoutingMixerTarget::MainOutput,
                },
            ],
            ..RoutingMixerState::default()
        });

        mixer.dispatch(
            RoutingMixerSource::Input(0),
            tone_buffer(format, 440.0, 4_800, 0),
        );
        mixer.dispatch(
            RoutingMixerSource::Input(1),
            tone_buffer(format, 880.0, 4_800, 0),
        );

        let mut out = vec![0.0; 4_800];
        mixer.fill_output(&mut out);
        let bin_440 = dominant_bin(&out, 48_000.0, 440.0);
        let bin_880 = dominant_bin(&out, 48_000.0, 880.0);
        let bin_220 = dominant_bin(&out, 48_000.0, 220.0);
        assert!(bin_440 > 0.07, "440 Hz tone missing: {bin_440}");
        assert!(bin_880 > 0.07, "880 Hz tone missing: {bin_880}");
        assert!(
            bin_220 < bin_440 * 0.35,
            "unexpected pitched-down artifact: {bin_220}"
        );
    }

    #[test]
    fn routing_mixer_resamples_to_output_rate_without_pitch_shift() {
        let input_format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let output_format = AudioFormat::new(44_100, 1, SampleFormat::F32);
        let mixer = RoutingMixer::new(output_format, 9_600);
        mixer.update_state(RoutingMixerState {
            routes: vec![RoutingMixerRoute {
                source: RoutingMixerSource::Input(0),
                target: RoutingMixerTarget::MainOutput,
            }],
            ..RoutingMixerState::default()
        });

        mixer.dispatch(
            RoutingMixerSource::Input(0),
            tone_buffer(input_format, 440.0, 9_600, 0),
        );

        let frames_out = 8_812;
        let mut out = vec![0.0; frames_out];
        mixer.fill_output(&mut out);
        let bin_440 = dominant_bin(&out, 44_100.0, 440.0);
        let pitched_down = 440.0 * 44_100.0 / 48_000.0;
        let bin_pitched_down = dominant_bin(&out, 44_100.0, pitched_down);
        assert!(
            bin_440 > 0.10,
            "resampled 440 Hz tone was not preserved: {bin_440}"
        );
        assert!(
            bin_pitched_down < bin_440 * 0.6,
            "tone still appears shifted toward {pitched_down:.1} Hz: {bin_pitched_down}"
        );
    }

    #[test]
    fn routing_mixer_applies_per_channel_processors() {
        let format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let mixer = RoutingMixer::new(format, 512);
        mixer.update_state(RoutingMixerState {
            routes: vec![RoutingMixerRoute {
                source: RoutingMixerSource::Input(0),
                target: RoutingMixerTarget::MainOutput,
            }],
            channel_processors: vec![(
                RoutingMixerSource::Input(0),
                ChannelProcessorConfig {
                    input_gain_db: 6.0,
                    output_gain_db: -6.0,
                    gate: crate::channel::NoiseGateConfig {
                        enabled: true,
                        open_threshold_db: -20.0,
                        close_threshold_db: -30.0,
                    },
                    ..ChannelProcessorConfig::default()
                },
            )],
            ..RoutingMixerState::default()
        });

        mixer.dispatch(
            RoutingMixerSource::Input(0),
            AudioBuffer::new(
                format,
                vec![0.001, 0.2, 0.08, 0.001].into(),
                4,
                Instant::now(),
                0,
            ),
        );

        let mut out = vec![0.0; 4];
        mixer.fill_output(&mut out);
        assert_eq!(out[0], 0.0);
        assert!(out[1] > 0.19);
        assert!(out[2] > 0.07);
        assert_eq!(out[3], 0.0);
    }

    #[test]
    fn routing_mixer_disabled_bus_stops_forwarding_audio() {
        let format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let mixer = RoutingMixer::new(format, 512);
        mixer.update_state(RoutingMixerState {
            routes: vec![
                RoutingMixerRoute {
                    source: RoutingMixerSource::Input(0),
                    target: RoutingMixerTarget::Bus(0),
                },
                RoutingMixerRoute {
                    source: RoutingMixerSource::Bus(0),
                    target: RoutingMixerTarget::MainOutput,
                },
            ],
            disabled_sources: vec![RoutingMixerSource::Bus(0)],
            ..RoutingMixerState::default()
        });

        mixer.dispatch(
            RoutingMixerSource::Input(0),
            AudioBuffer::new(format, vec![0.5; 128].into(), 128, Instant::now(), 0),
        );

        let mut out = vec![1.0; 128];
        mixer.fill_output(&mut out);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn routing_mixer_disabled_main_stops_output_audio() {
        let format = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let mixer = RoutingMixer::new(format, 512);
        mixer.update_state(RoutingMixerState {
            routes: vec![RoutingMixerRoute {
                source: RoutingMixerSource::Input(0),
                target: RoutingMixerTarget::MainOutput,
            }],
            disabled_sources: vec![RoutingMixerSource::MainOutput],
            ..RoutingMixerState::default()
        });

        mixer.dispatch(
            RoutingMixerSource::Input(0),
            AudioBuffer::new(format, vec![0.5; 128].into(), 128, Instant::now(), 0),
        );

        let mut out = vec![1.0; 128];
        mixer.fill_output(&mut out);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn bus_post_mix_gain_applies() {
        use crate::pipeline::GainProcessor;

        let f = AudioFormat::new(48_000, 1, SampleFormat::F32);
        let bus_cfg = BusMixerConfig {
            bus_sample_rate_hz: 48_000,
            mix_mode: MixMode::SumMono,
            legs: vec![BusLegConfig::new(f, 1.0)],
            jitter_window: Duration::from_millis(200),
            post_mix_processors: vec![Box::new(GainProcessor::new(2.0))],
            plugin_budget_per_plugin: Some(Duration::from_millis(50)),
            metrics: None,
        };
        let legs = bus_mixer_legs(8, 1);
        let (mut s, rx) = legs.into_iter().next().unwrap();
        let captured = Arc::new(Mutex::new(Vec::<AudioBuffer>::new()));
        let sink = Box::new(CaptureSink {
            inner: captured.clone(),
        });
        let mixer = BusMixer::spawn(bus_cfg, vec![rx], sink).expect("spawn");
        s.write_pcm_f32(&dc_buffer(f, 480, 0.1, 0)).unwrap();
        drop(s);
        mixer.stop();

        let buffers = captured.lock().unwrap().clone();
        assert!(!buffers.is_empty());
        assert!(
            (buffers[0].data[0] - 0.2).abs() < 1e-2,
            "got {}",
            buffers[0].data[0]
        );
    }
}
