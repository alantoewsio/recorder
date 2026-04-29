use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread::JoinHandle;

use crossbeam_channel::{unbounded, Receiver};
use recorder_core::{
    AudioAnalyzer, AudioBuffer, AudioTap, MediaEvent, RecordingError, Result, SampleFormat,
};

const PLUGIN_ID: &str = "nvidia.parakeet-unified-en-0.6b";
const MODEL_NAME: &str = "nvidia/parakeet-unified-en-0.6b";
const DEFAULT_SAMPLE_RATE_HZ: u32 = 16_000;

/// Stable id for the Parakeet local analyzer (UI persistence, properties window).
pub const PARAKEET_PLUGIN_ID: &str = PLUGIN_ID;

#[derive(Debug, Clone)]
pub struct LocalAnalyzerPluginDescriptor {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

pub fn descriptors() -> Vec<LocalAnalyzerPluginDescriptor> {
    vec![LocalAnalyzerPluginDescriptor {
        id: PLUGIN_ID,
        name: "NVIDIA Parakeet Unified EN 0.6B",
        description:
            "Streams enhanced audio chunks to a local NeMo sidecar and emits transcript events.",
    }]
}

pub fn create_analyzer(id: &str) -> std::result::Result<Box<dyn AudioAnalyzer + Send>, String> {
    create_analyzer_with_config(id, ParakeetConfig::default())
}

/// Construct the Parakeet analyzer with explicit configuration (e.g. from UI settings).
pub fn create_analyzer_with_config(
    id: &str,
    config: ParakeetConfig,
) -> std::result::Result<Box<dyn AudioAnalyzer + Send>, String> {
    if id == PLUGIN_ID {
        Ok(Box::new(ParakeetUnifiedAnalyzer::new(config)))
    } else {
        Err(format!("unknown local analyzer plugin: {id}"))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ParakeetConfig {
    pub model_name: String,
    pub python: String,
    pub worker_script: PathBuf,
    pub chunk_seconds: f32,
    pub pre_ready_buffer_seconds: f32,
    pub sample_rate_hz: u32,
    pub stream_id: Option<String>,
    /// RMS below this is treated as silence and skipped without invoking the model.
    pub silence_rms_threshold: f32,
}

impl Default for ParakeetConfig {
    fn default() -> Self {
        let python = std::env::var("RECORDER_PARAKEET_PYTHON")
            .unwrap_or_else(|_| detect_project_python().unwrap_or_else(|| "python".into()));
        let worker_script = std::env::var("RECORDER_PARAKEET_WORKER")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("python")
                    .join("worker.py")
            });
        let chunk_seconds = std::env::var("RECORDER_PARAKEET_CHUNK_SECONDS")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(1.5);
        let silence_rms_threshold = std::env::var("RECORDER_PARAKEET_SILENCE_RMS")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| *v >= 0.0)
            .unwrap_or(0.005);

        Self {
            model_name: MODEL_NAME.to_string(),
            python,
            worker_script,
            chunk_seconds,
            pre_ready_buffer_seconds: 10.0,
            sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
            stream_id: Some("default".to_string()),
            silence_rms_threshold,
        }
    }
}

fn detect_project_python() -> Option<String> {
    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    for root in roots {
        for ancestor in root.ancestors() {
            for candidate in [
                ancestor.join(".venv").join("Scripts").join("python.exe"),
                ancestor.join(".venv").join("bin").join("python"),
            ] {
                if candidate.is_file() {
                    return Some(candidate.display().to_string());
                }
            }
        }
    }

    None
}

pub struct ParakeetUnifiedAnalyzer {
    config: ParakeetConfig,
    worker: Option<ParakeetWorker>,
    worker_ready: bool,
    reported_waiting_for_model: bool,
    pending: Vec<MediaEvent>,
    resampled_buffer: Vec<f32>,
    segment_id: u64,
    buffered_start_frame: Option<u64>,
    /// Running sentence text accumulated across chunks; emitted as `TranscriptFinal` once a
    /// terminal punctuation mark or a silent gap appears.
    pending_sentence: String,
    pending_sentence_start_segment: Option<u64>,
    pending_sentence_start_frame: u64,
    pending_sentence_end_frame: u64,
}

impl ParakeetUnifiedAnalyzer {
    pub fn new(config: ParakeetConfig) -> Self {
        Self {
            config,
            worker: None,
            worker_ready: false,
            reported_waiting_for_model: false,
            pending: Vec::new(),
            resampled_buffer: Vec::new(),
            segment_id: 0,
            buffered_start_frame: None,
            pending_sentence: String::new(),
            pending_sentence_start_segment: None,
            pending_sentence_start_frame: 0,
            pending_sentence_end_frame: 0,
        }
    }

    fn ensure_worker(&mut self) -> Result<()> {
        if self.worker.is_some() {
            return Ok(());
        }

        match ParakeetWorker::start(&self.config) {
            Ok(worker) => {
                self.pending.push(MediaEvent::AttributeDetected {
                    tap: AudioTap::Processed,
                    start_frame: 0,
                    end_frame: 0,
                    key: "parakeet.status".to_string(),
                    value: format!(
                        "worker started; loading {} with {}",
                        self.config.model_name, self.config.python
                    ),
                    confidence: None,
                });
                self.worker = Some(worker);
                Ok(())
            }
            Err(e) => {
                self.pending.push(MediaEvent::AttributeDetected {
                    tap: AudioTap::Processed,
                    start_frame: 0,
                    end_frame: 0,
                    key: "parakeet.error".to_string(),
                    value: e.clone(),
                    confidence: None,
                });
                Err(RecordingError::Plugin(e))
            }
        }
    }

    fn emit_ready_chunks(&mut self, input_rate_hz: u32) -> Result<()> {
        let chunk_samples =
            (self.config.sample_rate_hz as f32 * self.config.chunk_seconds).round() as usize;
        if chunk_samples == 0 {
            return Ok(());
        }

        if !self.worker_ready {
            let max_samples = (self.config.sample_rate_hz as f32
                * self.config.pre_ready_buffer_seconds)
                .round() as usize;
            if max_samples > 0 && self.resampled_buffer.len() > max_samples {
                let remove = self.resampled_buffer.len() - max_samples;
                self.resampled_buffer.drain(0..remove);
            }
            if !self.reported_waiting_for_model {
                self.pending.push(MediaEvent::AttributeDetected {
                    tap: AudioTap::Processed,
                    start_frame: 0,
                    end_frame: 0,
                    key: "parakeet.status".to_string(),
                    value: format!(
                        "waiting for model ready; buffering up to {:.1}s of recent audio",
                        self.config.pre_ready_buffer_seconds
                    ),
                    confidence: None,
                });
                self.reported_waiting_for_model = true;
            }
            return Ok(());
        }

        while self.resampled_buffer.len() >= chunk_samples {
            let samples = self
                .resampled_buffer
                .drain(0..chunk_samples)
                .collect::<Vec<_>>();
            let start_frame = self.buffered_start_frame.unwrap_or(0);
            let input_frames = ((samples.len() as u64) * input_rate_hz as u64
                / self.config.sample_rate_hz as u64)
                .max(1);
            let end_frame = start_frame.saturating_add(input_frames);
            let rms = segment_rms(&samples);

            if rms < self.config.silence_rms_threshold {
                self.pending.push(MediaEvent::AttributeDetected {
                    tap: AudioTap::Processed,
                    start_frame,
                    end_frame,
                    key: "parakeet.segment".to_string(),
                    value: format!(
                        "skipped silent segment {} ({:.1}s, rms {:.4} < {:.4})",
                        self.segment_id,
                        self.config.chunk_seconds,
                        rms,
                        self.config.silence_rms_threshold
                    ),
                    confidence: None,
                });
                self.flush_pending_sentence(end_frame);
            } else {
                let wav_path =
                    write_segment_wav(self.segment_id, self.config.sample_rate_hz, &samples)
                        .map_err(|e| {
                            RecordingError::Plugin(format!("parakeet segment wav: {e}"))
                        })?;
                if let Some(worker) = self.worker.as_mut() {
                    worker.send_segment(self.segment_id, start_frame, end_frame, rms, &wav_path)?;
                }
                self.pending.push(MediaEvent::AttributeDetected {
                    tap: AudioTap::Processed,
                    start_frame,
                    end_frame,
                    key: "parakeet.segment".to_string(),
                    value: format!(
                        "queued segment {} ({:.1}s, rms {:.4})",
                        self.segment_id, self.config.chunk_seconds, rms
                    ),
                    confidence: None,
                });
            }

            self.segment_id += 1;
            self.buffered_start_frame = Some(end_frame);
        }

        Ok(())
    }

    fn drain_worker_messages(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        let messages: Vec<WorkerMessage> = worker.rx.try_iter().collect();

        for msg in messages {
            match msg {
                WorkerMessage::Status(status) => self.pending.push(MediaEvent::AttributeDetected {
                    tap: AudioTap::Processed,
                    start_frame: 0,
                    end_frame: 0,
                    key: "parakeet.status".to_string(),
                    value: status,
                    confidence: None,
                }),
                WorkerMessage::Ready => {
                    self.worker_ready = true;
                    self.pending.push(MediaEvent::AttributeDetected {
                        tap: AudioTap::Processed,
                        start_frame: 0,
                        end_frame: 0,
                        key: "parakeet.status".to_string(),
                        value: "model ready".to_string(),
                        confidence: None,
                    });
                }
                WorkerMessage::Final {
                    segment_id,
                    start_frame,
                    end_frame,
                    text,
                } => {
                    self.handle_chunk_text(segment_id, start_frame, end_frame, text);
                }
                WorkerMessage::Error(error) => self.pending.push(MediaEvent::AttributeDetected {
                    tap: AudioTap::Processed,
                    start_frame: 0,
                    end_frame: 0,
                    key: "parakeet.error".to_string(),
                    value: error,
                    confidence: None,
                }),
                WorkerMessage::Stderr(line) => {
                    if !is_noisy_stderr_line(&line) {
                        self.pending.push(MediaEvent::AttributeDetected {
                            tap: AudioTap::Processed,
                            start_frame: 0,
                            end_frame: 0,
                            key: "parakeet.stderr".to_string(),
                            value: line,
                            confidence: None,
                        });
                    }
                }
            }
        }
    }

    fn handle_chunk_text(
        &mut self,
        segment_id: u64,
        start_frame: u64,
        end_frame: u64,
        text: String,
    ) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            self.pending.push(MediaEvent::AttributeDetected {
                tap: AudioTap::Processed,
                start_frame,
                end_frame,
                key: "parakeet.status".to_string(),
                value: format!("segment {segment_id} produced empty transcript"),
                confidence: None,
            });
            return;
        }

        self.pending.push(MediaEvent::AttributeDetected {
            tap: AudioTap::Processed,
            start_frame,
            end_frame,
            key: "parakeet.transcript".to_string(),
            value: format!("segment {segment_id}: {trimmed}"),
            confidence: None,
        });

        if self.pending_sentence.is_empty() {
            self.pending_sentence_start_segment = Some(segment_id);
            self.pending_sentence_start_frame = start_frame;
        }
        self.pending_sentence_end_frame = end_frame;
        if !self.pending_sentence.is_empty() && needs_space_join(&self.pending_sentence, trimmed) {
            self.pending_sentence.push(' ');
        }
        self.pending_sentence.push_str(trimmed);

        let buffer = std::mem::take(&mut self.pending_sentence);
        let (completed, remaining) = split_completed_sentences(&buffer);
        self.pending_sentence = remaining;

        if !completed.is_empty() {
            let final_segment_id = self.pending_sentence_start_segment.unwrap_or(segment_id);
            let final_start = self.pending_sentence_start_frame;
            let final_end = if self.pending_sentence.is_empty() {
                self.pending_sentence_end_frame
            } else {
                end_frame
            };
            self.pending.push(MediaEvent::TranscriptFinal {
                tap: AudioTap::Processed,
                stream_id: self.config.stream_id.clone(),
                segment_id: final_segment_id,
                start_frame: final_start,
                end_frame: final_end,
                speaker_id: None,
                text: completed,
                confidence: None,
            });
            if self.pending_sentence.is_empty() {
                self.pending_sentence_start_segment = None;
            } else {
                self.pending_sentence_start_segment = Some(segment_id);
                self.pending_sentence_start_frame = end_frame;
            }
        }

        let partial_text = self.pending_sentence.clone();
        let partial_start = self.pending_sentence_start_frame;
        let partial_end = self.pending_sentence_end_frame.max(end_frame);
        let partial_segment = self.pending_sentence_start_segment.unwrap_or(segment_id);
        self.pending.push(MediaEvent::TranscriptPartial {
            tap: AudioTap::Processed,
            stream_id: self.config.stream_id.clone(),
            segment_id: partial_segment,
            start_frame: partial_start,
            end_frame: partial_end,
            speaker_id: None,
            text: partial_text,
            confidence: None,
        });
    }

    fn flush_pending_sentence(&mut self, end_frame: u64) {
        if self.pending_sentence.trim().is_empty() {
            self.pending_sentence.clear();
            self.pending_sentence_start_segment = None;
            return;
        }

        let final_segment = self
            .pending_sentence_start_segment
            .unwrap_or(self.segment_id);
        let final_start = self.pending_sentence_start_frame;
        let final_end = self.pending_sentence_end_frame.max(end_frame);
        let text = std::mem::take(&mut self.pending_sentence);
        self.pending_sentence_start_segment = None;

        self.pending.push(MediaEvent::TranscriptFinal {
            tap: AudioTap::Processed,
            stream_id: self.config.stream_id.clone(),
            segment_id: final_segment,
            start_frame: final_start,
            end_frame: final_end,
            speaker_id: None,
            text: text.trim().to_string(),
            confidence: None,
        });

        self.pending.push(MediaEvent::TranscriptPartial {
            tap: AudioTap::Processed,
            stream_id: self.config.stream_id.clone(),
            segment_id: final_segment,
            start_frame: final_end,
            end_frame: final_end,
            speaker_id: None,
            text: String::new(),
            confidence: None,
        });
    }
}

impl Default for ParakeetUnifiedAnalyzer {
    fn default() -> Self {
        Self::new(ParakeetConfig::default())
    }
}

impl AudioAnalyzer for ParakeetUnifiedAnalyzer {
    fn name(&self) -> &str {
        "parakeet-unified-en-0.6b"
    }

    fn accept_audio(&mut self, input: &AudioBuffer) -> Result<()> {
        self.ensure_worker()?;
        self.drain_worker_messages();

        if self.buffered_start_frame.is_none() {
            self.buffered_start_frame = Some(input.frame_index);
        }

        let samples = downmix_and_resample(input, self.config.sample_rate_hz);
        self.resampled_buffer.extend(samples);
        self.emit_ready_chunks(input.format.sample_rate_hz)?;
        Ok(())
    }

    fn drain_events(&mut self) -> Vec<MediaEvent> {
        self.drain_worker_messages();
        std::mem::take(&mut self.pending)
    }
}

fn downmix_and_resample(input: &AudioBuffer, output_rate_hz: u32) -> Vec<f32> {
    let channels = input.format.channels.max(1) as usize;
    let mut mono = Vec::with_capacity(input.frames);

    for frame in 0..input.frames {
        let base = frame * channels;
        let mut sum = 0.0f32;
        for ch in 0..channels {
            sum += input.data.get(base + ch).copied().unwrap_or_default();
        }
        mono.push(sum / channels as f32);
    }

    if input.format.sample_format != SampleFormat::F32
        || input.format.sample_rate_hz == output_rate_hz
    {
        return mono;
    }

    resample_linear(&mono, input.format.sample_rate_hz, output_rate_hz)
}

fn resample_linear(input: &[f32], input_rate_hz: u32, output_rate_hz: u32) -> Vec<f32> {
    if input.is_empty() || input_rate_hz == output_rate_hz {
        return input.to_vec();
    }

    let ratio = output_rate_hz as f64 / input_rate_hz as f64;
    let out_len = (input.len() as f64 * ratio).round().max(1.0) as usize;
    let mut out = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let src = i as f64 / ratio;
        let lo = src.floor() as usize;
        let hi = (lo + 1).min(input.len() - 1);
        let frac = (src - lo as f64) as f32;
        out.push(input[lo] * (1.0 - frac) + input[hi] * frac);
    }

    out
}

fn split_completed_sentences(text: &str) -> (String, String) {
    let bytes = text.as_bytes();
    let mut last_end: Option<usize> = None;
    for (i, &byte) in bytes.iter().enumerate() {
        if matches!(byte, b'.' | b'?' | b'!') {
            let next = bytes.get(i + 1).copied();
            let is_terminator =
                next.is_none_or(|c| matches!(c, b' ' | b'\n' | b'\t' | b'"' | b')'));
            if is_terminator {
                last_end = Some(i + 1);
            }
        }
    }
    match last_end {
        Some(end) => (
            text[..end].trim().to_string(),
            text[end..].trim_start().to_string(),
        ),
        None => (String::new(), text.to_string()),
    }
}

fn needs_space_join(existing: &str, addition: &str) -> bool {
    let last = existing.chars().last();
    let first = addition.chars().next();
    match (last, first) {
        (Some(last), Some(first)) => !last.is_whitespace() && !first.is_whitespace(),
        _ => false,
    }
}

fn is_noisy_stderr_line(line: &str) -> bool {
    const NOISE_FRAGMENTS: &[&str] = &[
        "dataloader:826",
        "dataloader:523",
        "Transcribing: 0it",
        "Transcribing: 1it",
    ];
    NOISE_FRAGMENTS
        .iter()
        .any(|fragment| line.contains(fragment))
}

fn segment_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum = samples.iter().map(|sample| sample * sample).sum::<f32>();
    (sum / samples.len() as f32).sqrt()
}

fn write_segment_wav(
    segment_id: u64,
    sample_rate_hz: u32,
    samples: &[f32],
) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join("recorder-parakeet");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("segment-{segment_id}.wav"));
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: sample_rate_hz,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    for sample in samples {
        let s = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        writer
            .write_sample(s)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    }
    writer
        .finalize()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(path)
}

struct ParakeetWorker {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<WorkerMessage>,
    _stdout_thread: JoinHandle<()>,
    _stderr_thread: JoinHandle<()>,
}

impl ParakeetWorker {
    fn start(config: &ParakeetConfig) -> std::result::Result<Self, String> {
        let mut child = Command::new(&config.python)
            .arg(&config.worker_script)
            .arg("--model")
            .arg(&config.model_name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                format!(
                    "failed to start Parakeet worker with {} {}: {e}",
                    config.python,
                    config.worker_script.display()
                )
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "parakeet worker stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "parakeet worker stdout unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "parakeet worker stderr unavailable".to_string())?;
        let (tx, rx) = unbounded();
        let stderr_tx = tx.clone();
        let stdout_thread = std::thread::spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if let Some(msg) = WorkerMessage::parse(&line) {
                    let _ = tx.send(msg);
                }
            }
        });
        let stderr_thread = std::thread::spawn(move || {
            for line in BufReader::new(stderr)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if !line.trim().is_empty() {
                    let _ = stderr_tx.send(WorkerMessage::Stderr(line));
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            rx,
            _stdout_thread: stdout_thread,
            _stderr_thread: stderr_thread,
        })
    }

    fn send_segment(
        &mut self,
        segment_id: u64,
        start_frame: u64,
        end_frame: u64,
        rms: f32,
        path: &Path,
    ) -> Result<()> {
        if let Ok(Some(status)) = self.child.try_wait() {
            return Err(RecordingError::Plugin(format!(
                "parakeet worker exited before segment {segment_id}: {status}"
            )));
        }

        writeln!(
            self.stdin,
            "TRANSCRIBE\t{segment_id}\t{start_frame}\t{end_frame}\t{rms:.6}\t{}",
            path.display()
        )
        .and_then(|_| self.stdin.flush())
        .map_err(|e| RecordingError::Plugin(format!("parakeet worker stdin: {e}")))
    }
}

impl Drop for ParakeetWorker {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "STOP");
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

enum WorkerMessage {
    Status(String),
    Ready,
    Final {
        segment_id: u64,
        start_frame: u64,
        end_frame: u64,
        text: String,
    },
    Error(String),
    Stderr(String),
}

impl WorkerMessage {
    fn parse(line: &str) -> Option<Self> {
        let mut parts = line.splitn(5, '\t');
        match parts.next()? {
            "READY" => Some(Self::Ready),
            "STATUS" => Some(Self::Status(parts.collect::<Vec<_>>().join("\t"))),
            "ERROR" => Some(Self::Error(parts.collect::<Vec<_>>().join("\t"))),
            "FINAL" => Some(Self::Final {
                segment_id: parts.next()?.parse().ok()?,
                start_frame: parts.next()?.parse().ok()?,
                end_frame: parts.next()?.parse().ok()?,
                text: parts.next().unwrap_or_default().to_string(),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use recorder_core::{AudioFormat, SampleFormat};
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn resamples_and_downmixes_to_mono() {
        let input = AudioBuffer::new(
            AudioFormat::new(48_000, 2, SampleFormat::F32),
            Arc::<[f32]>::from(vec![0.5, -0.5, 1.0, 0.0]),
            2,
            Instant::now(),
            0,
        );

        let samples = downmix_and_resample(&input, 16_000);
        assert!(!samples.is_empty());
        assert!(samples.iter().all(|s| (-1.0..=1.0).contains(s)));
    }

    #[test]
    fn parses_worker_final_message() {
        let msg = WorkerMessage::parse("FINAL\t7\t10\t20\thello world");
        assert!(matches!(
            msg,
            Some(WorkerMessage::Final {
                segment_id: 7,
                start_frame: 10,
                end_frame: 20,
                ..
            })
        ));
    }

    #[test]
    fn split_sentences_returns_completed_and_partial() {
        let (completed, partial) = split_completed_sentences("Hello there. How are");
        assert_eq!(completed, "Hello there.");
        assert_eq!(partial, "How are");
    }

    #[test]
    fn split_sentences_keeps_partial_when_no_terminator() {
        let (completed, partial) = split_completed_sentences("the quick brown fox");
        assert!(completed.is_empty());
        assert_eq!(partial, "the quick brown fox");
    }

    #[test]
    fn split_sentences_finalizes_full_sentence() {
        let (completed, partial) = split_completed_sentences("Hi there! What's up? I am fine.");
        assert_eq!(completed, "Hi there! What's up? I am fine.");
        assert!(partial.is_empty());
    }

    #[test]
    fn aggregator_emits_partial_then_final_across_chunks() {
        let mut analyzer = ParakeetUnifiedAnalyzer::default();
        analyzer.handle_chunk_text(0, 0, 24_000, "Hi there".to_string());
        analyzer.handle_chunk_text(1, 24_000, 48_000, "how are you".to_string());
        analyzer.handle_chunk_text(2, 48_000, 72_000, "doing today?".to_string());

        let events = std::mem::take(&mut analyzer.pending);
        let final_text = events.iter().find_map(|event| match event {
            MediaEvent::TranscriptFinal { text, .. } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(
            final_text.as_deref(),
            Some("Hi there how are you doing today?")
        );
    }

    #[test]
    fn aggregator_flushes_pending_on_silence() {
        let mut analyzer = ParakeetUnifiedAnalyzer::default();
        analyzer.handle_chunk_text(0, 0, 24_000, "An incomplete thought".to_string());
        analyzer.flush_pending_sentence(48_000);

        let events = std::mem::take(&mut analyzer.pending);
        let final_text = events.iter().find_map(|event| match event {
            MediaEvent::TranscriptFinal { text, .. } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(final_text.as_deref(), Some("An incomplete thought"));
    }
}
