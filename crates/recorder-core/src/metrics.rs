use std::sync::atomic::AtomicU64;

/// Counters for overload and plugin timing issues (processed path only for timeouts).
#[derive(Default)]
pub struct PipelineMetrics {
    pub raw_frames_dropped: AtomicU64,
    pub processed_frames_dropped: AtomicU64,
    pub analyzer_frames_dropped: AtomicU64,
    pub plugin_timeouts: AtomicU64,
}
