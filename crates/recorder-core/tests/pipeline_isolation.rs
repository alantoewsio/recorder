use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use recorder_core::buffer::AudioBuffer;
use recorder_core::events::{media_event_queue, AudioTap, MediaEvent};
use recorder_core::format::{AudioFormat, SampleFormat};
use recorder_core::pipeline::{PipelineConfig, PipelineMetrics, SpinProcessor, StreamPipeline};
use recorder_core::ring::frame_queue;
use recorder_core::traits::{AudioAnalyzer, AudioSink};
use recorder_core::VoiceActivityAnalyzer;

struct CountSink(Arc<AtomicU64>);

impl AudioSink for CountSink {
    fn write_pcm_f32(
        &mut self,
        buffer: &recorder_core::AudioBuffer,
    ) -> std::result::Result<(), recorder_core::RecordingError> {
        self.0.fetch_add(buffer.frames as u64, Ordering::Relaxed);
        Ok(())
    }
}

fn voice_event(active: bool) -> MediaEvent {
    MediaEvent::VoiceActivity {
        tap: AudioTap::Processed,
        start_frame: 0,
        end_frame: 1,
        active,
        level: if active { 1.0 } else { 0.0 },
        confidence: 1.0,
    }
}

#[test]
fn raw_path_receives_all_frames_when_plugin_spins() {
    let fmt = AudioFormat::new(48_000, 1, SampleFormat::F32);
    let (raw_tx, raw_rx) = frame_queue(64);
    let (proc_tx, proc_rx) = frame_queue(64);
    let metrics = Arc::new(PipelineMetrics::default());

    let pipeline = StreamPipeline::new(
        PipelineConfig {
            format: fmt,
            raw_queue_capacity: 64,
            processed_queue_capacity: 64,
            analyzer_queue_capacity: 64,
            plugin_budget_per_plugin: Some(Duration::from_millis(1)),
        },
        Some(raw_tx),
        Some(proc_tx),
        Vec::new(),
        vec![Box::new(SpinProcessor {
            spin_for: Duration::from_millis(50),
        })],
        metrics.clone(),
    );

    const N: usize = 20;
    for i in 0..N {
        let buf = AudioBuffer::silent(fmt, 128, Instant::now(), i as u64);
        pipeline.ingest(buf);
    }

    let mut raw_frames = 0u64;
    while let Ok(b) = raw_rx.inner.try_recv() {
        raw_frames += b.frames as u64;
    }
    assert_eq!(
        raw_frames,
        (N * 128) as u64,
        "raw path must capture every frame independent of plugin timing"
    );

    let mut proc_frames = 0u64;
    while let Ok(b) = proc_rx.inner.try_recv() {
        proc_frames += b.frames as u64;
    }
    assert_eq!(
        proc_frames,
        (N * 128) as u64,
        "processed path should still deliver passthrough frames when plugin overruns budget"
    );

    assert!(
        metrics.plugin_timeouts.load(Ordering::Relaxed) > 0,
        "spinning plugin should have triggered at least one timeout counter"
    );
}

#[test]
fn media_event_queue_drops_when_full() {
    let (tx, rx) = media_event_queue(1);

    tx.try_send(voice_event(true)).expect("first event fits");
    assert!(tx.try_send(voice_event(false)).is_err());

    let events: Vec<_> = rx.try_iter().collect();
    assert_eq!(events, vec![voice_event(true)]);
}

#[test]
fn voice_activity_analyzer_emits_state_changes() {
    let fmt = AudioFormat::new(48_000, 1, SampleFormat::F32);
    let mut analyzer = VoiceActivityAnalyzer::new(0.1);

    analyzer
        .accept_audio(&AudioBuffer::silent(fmt, 8, Instant::now(), 0))
        .unwrap();
    let events = analyzer.drain_events();
    assert!(matches!(
        events.as_slice(),
        [MediaEvent::VoiceActivity { active: false, .. }]
    ));

    let loud = AudioBuffer::new(fmt, vec![0.5; 8].into(), 8, Instant::now(), 8);
    analyzer.accept_audio(&loud).unwrap();
    let events = analyzer.drain_events();
    assert!(matches!(
        events.as_slice(),
        [MediaEvent::VoiceActivity { active: true, .. }]
    ));
}

#[test]
fn analyzer_tap_receives_processed_audio_after_raw_clone() {
    let fmt = AudioFormat::new(48_000, 1, SampleFormat::F32);
    let (raw_tx, raw_rx) = frame_queue(64);
    let (analyzer_tx, analyzer_rx) = frame_queue(64);
    let metrics = Arc::new(PipelineMetrics::default());

    let pipeline = StreamPipeline::new(
        PipelineConfig {
            format: fmt,
            raw_queue_capacity: 64,
            processed_queue_capacity: 64,
            analyzer_queue_capacity: 64,
            plugin_budget_per_plugin: Some(Duration::from_millis(5)),
        },
        Some(raw_tx),
        None,
        vec![analyzer_tx],
        vec![Box::new(recorder_core::GainProcessor::new(2.0))],
        metrics,
    );

    pipeline.ingest(AudioBuffer::new(
        fmt,
        vec![0.25; 8].into(),
        8,
        Instant::now(),
        0,
    ));

    let raw = raw_rx.inner.try_recv().expect("raw frame");
    let analyzed = analyzer_rx.inner.try_recv().expect("analyzer frame");

    assert_eq!(raw.data[0], 0.25);
    assert_eq!(analyzed.data[0], 0.5);
}

#[test]
fn counting_sinks_via_session_threads() {
    let fmt = AudioFormat::new(48_000, 1, SampleFormat::F32);
    let (raw_tx, raw_rx) = frame_queue(64);
    let (proc_tx, proc_rx) = frame_queue(64);
    let metrics = Arc::new(PipelineMetrics::default());

    let pipeline = Arc::new(StreamPipeline::new(
        PipelineConfig {
            format: fmt,
            raw_queue_capacity: 64,
            processed_queue_capacity: 64,
            analyzer_queue_capacity: 64,
            plugin_budget_per_plugin: Some(Duration::from_millis(5)),
        },
        Some(raw_tx),
        Some(proc_tx),
        Vec::new(),
        vec![Box::new(recorder_core::GainProcessor::new(2.0))],
        metrics,
    ));

    let raw_count = Arc::new(AtomicU64::new(0));
    let proc_count = Arc::new(AtomicU64::new(0));
    let rc_raw = raw_count.clone();
    let j_raw = std::thread::spawn(move || {
        let mut sink = CountSink(rc_raw);
        while let Ok(b) = raw_rx.inner.recv() {
            let _ = sink.write_pcm_f32(&b);
        }
    });
    let rc_proc = proc_count.clone();
    let j_proc = std::thread::spawn(move || {
        let mut sink = CountSink(rc_proc);
        while let Ok(b) = proc_rx.inner.recv() {
            let _ = sink.write_pcm_f32(&b);
        }
    });

    for i in 0..10 {
        pipeline.ingest(AudioBuffer::silent(fmt, 64, Instant::now(), i));
    }
    pipeline.close();
    let _ = j_raw.join();
    let _ = j_proc.join();

    assert_eq!(raw_count.load(Ordering::Relaxed), 10 * 64);
    assert_eq!(proc_count.load(Ordering::Relaxed), 10 * 64);
}
