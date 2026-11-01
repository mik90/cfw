//! Persisted execution-log schema shared by recording and replay.
use crate::{message::MessageHeader, time::FrameworkTime};
use std::collections::BTreeMap;
pub const EXECUTION_LOG_CHANNEL: &str = "execution_log";
/// Iox2 events observed at the readiness/injection boundary. Kept separate from
/// [`ExecutionRecord`] because an observation may not produce an execution.
pub const IOX2_EVENT_RECORD_CHANNEL: &str = "iox2_event_records";
pub const EXECUTION_LOG_DESCRIPTOR_ARTIFACT: &str = "execution_log_descriptor";
pub const INTERN_TABLES_ARTIFACT: &str = "intern_tables";

/// Persisted ID space shared by execution records and logging diagnostics.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct InternTables {
    pub channels: crate::string_interner::ChannelNameInterner,
    pub callbacks: crate::string_interner::CallbackNameInterner,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RecordingMode {
    /// Execute normally without recording invocations or iox2 event records.
    Off,
    /// Record callback identity, execution time, body duration and outcome only.
    DurationOnly,
    /// Also record input/output identities and inline iox2 events for exact replay.
    #[default]
    Full,
}
#[derive(Debug, Clone, Default)]
pub struct RecordingOptions {
    pub default_mode: RecordingMode,
    pub callbacks: BTreeMap<String, RecordingMode>,
}
impl RecordingOptions {
    pub fn new(default_mode: RecordingMode) -> Self {
        Self {
            default_mode,
            callbacks: BTreeMap::new(),
        }
    }
    pub fn with_callback(mut self, name: impl Into<String>, mode: RecordingMode) -> Self {
        self.callbacks.insert(name.into(), mode);
        self
    }
    pub fn mode_for(&self, name: &str) -> RecordingMode {
        self.callbacks
            .get(name)
            .copied()
            .unwrap_or(self.default_mode)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Direction {
    Received,
    Published,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Transport {
    Native,
    Iox2Data,
    Iox2Event,
}
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EndpointDescriptor {
    pub ordinal: usize,
    pub channel: String,
    pub direction: Direction,
    pub transport: Transport,
    pub payload_type: String,
    /// Channel-local publisher storage index; absent on input/event endpoints.
    pub publisher_index: Option<u32>,
}
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CallbackDescriptor {
    pub name: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub recording_mode: RecordingMode,
    pub endpoints: Vec<EndpointDescriptor>,
}
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ExecutionDescriptor {
    pub callbacks: Vec<CallbackDescriptor>,
    pub logged_channels: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LoggedMessage {
    pub ordinal: usize,
    pub header: MessageHeader,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LoggedIox2Event {
    pub ordinal: usize,
    pub event_id: usize,
    pub count: u64,
}
/// An iox2 event seen at the readiness/injection boundary. This is not the event
/// snapshot consumed by an invocation; those are recorded inline in
/// [`ExecutionRecord::iox2_events`].
///
/// The observation is independent of when required inputs or pool capacity permit
/// that callback to execute.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Iox2EventRecord {
    pub callback_id: crate::string_interner::CallbackId,
    pub observed_at: FrameworkTime,
    #[cfg_attr(feature = "serde", serde(alias = "event"))]
    pub iox2_event: LoggedIox2Event,
}
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Outcome {
    Committed,
    BodyError(String),
    BodyPanicked,
    Cancelled,
    /// Some publications may already have escaped. Outputs list attempts; they
    /// are not a complete publication ledger. Payload captures retain the prefix.
    CommitPanicked,
}
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ExecutionRecord {
    pub callback_id: crate::string_interner::CallbackId,
    pub execution_time: FrameworkTime,
    /// Measured body runtime, not simulator modeled occupancy.
    pub body_duration_ns: u64,
    pub inputs: Vec<LoggedMessage>,
    /// Iox2 events consumed by this invocation. Recorded inline because events
    /// carry no payload and are too small to justify an identity lookup.
    #[cfg_attr(feature = "serde", serde(alias = "events"))]
    pub iox2_events: Vec<LoggedIox2Event>,
    /// Iox2 events attempted by this invocation, with the same inline rationale.
    #[cfg_attr(feature = "serde", serde(alias = "output_events"))]
    pub output_iox2_events: Vec<LoggedIox2Event>,
    /// Committed outputs for Committed, attempts for CommitPanicked, otherwise empty.
    pub outputs: Vec<LoggedMessage>,
    pub outcome: Outcome,
}
