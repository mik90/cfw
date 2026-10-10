//! Known recording loss, not crash or arbitrary truncation detection.
use crate::BoxedLogError;

pub const RECORDING_INCOMPLETENESS_ARTIFACT: &str = "recording_incompleteness";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CaptureLoss {
    pub channel: String,
    pub writer_drops: u64,
    pub reader_drops: u64,
    pub receive_errors: u64,
}

/// Sticky snapshot written after a known failure. Repeated snapshots are allowed.
/// Counts are cumulative for this session's endpoints/recorder, not aggregated
/// across independent sessions sharing a writer. Any snapshot disqualifies replay.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RecordingIncompleteness {
    pub captures: Vec<CaptureLoss>,
    /// Aggregate loss from the recorder's shared execution/event queue budget.
    pub recorder_entries_dropped: usize,
    pub errors: Vec<String>,
}

/// Presence alone is disqualifying, including malformed marker contents.
pub fn reject_incomplete_recording(artifact: Option<&[u8]>) -> Result<(), BoxedLogError> {
    if artifact.is_some() {
        return Err("log recording is incomplete (recording_incompleteness artifact)".into());
    }
    Ok(())
}
