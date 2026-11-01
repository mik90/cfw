//! Owned execution metadata; recording never retains endpoint/storage borrows.
use crate::{Callback, Context, LoanError, message::MessageHeader, time::FrameworkTime};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Instant,
};
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
struct State {
    intern_tables: Option<InternTables>,
    attached: bool,
    descriptor: Option<ExecutionDescriptor>,
    records: VecDeque<ExecutionRecord>,
    iox2_event_records: VecDeque<Iox2EventRecord>,
    capacity: usize,
    dropped: usize,
}
#[derive(Clone)]
pub struct ExecutionRecorder(Arc<Mutex<State>>);
impl ExecutionRecorder {
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "execution recording capacity must be positive"
        );
        Self(Arc::new(Mutex::new(State {
            attached: false,
            intern_tables: None,
            descriptor: None,
            records: VecDeque::with_capacity(capacity),
            iox2_event_records: VecDeque::new(),
            capacity,
            dropped: 0,
        })))
    }
    pub fn descriptor(&self) -> Option<ExecutionDescriptor> {
        self.0.lock().unwrap().descriptor.clone()
    }
    pub fn intern_tables(&self) -> Option<InternTables> {
        self.0.lock().unwrap().intern_tables.clone()
    }
    pub fn dropped(&self) -> usize {
        self.0.lock().unwrap().dropped
    }
    pub fn drain(&self) -> Vec<ExecutionRecord> {
        self.0.lock().unwrap().records.drain(..).collect()
    }
    pub fn drain_iox2_event_records(&self) -> Vec<Iox2EventRecord> {
        self.0
            .lock()
            .unwrap()
            .iox2_event_records
            .drain(..)
            .collect()
    }
    #[cfg(feature = "iceoryx2")]
    fn observe_iox2_event(&self, event: Iox2EventRecord) {
        let mut state = self.0.lock().unwrap();
        if state.records.len() + state.iox2_event_records.len() == state.capacity {
            state.dropped = state.dropped.saturating_add(1);
        } else {
            state.iox2_event_records.push_back(event);
        }
    }
    fn record(&self, record: ExecutionRecord) {
        let mut state = self.0.lock().unwrap();
        if state.records.len() + state.iox2_event_records.len() == state.capacity {
            state.dropped = state.dropped.saturating_add(1);
        } else {
            state.records.push_back(record);
        }
    }
    /// Begin the recorder's single attachment using the execution's name tables.
    /// Finish the attachment before executing any instrumented callbacks.
    pub fn begin_attachment(&self, tables: InternTables) -> Result<RecordingAttachment, String> {
        let mut state = self.0.lock().unwrap();
        if state.attached {
            return Err("recorder already attached".into());
        }
        state.attached = true;
        state.intern_tables = Some(tables);
        Ok(RecordingAttachment {
            recorder: self.clone(),
            callbacks: Vec::new(),
        })
    }
}

/// Collects callback instrumentation and publishes its completed descriptor.
pub struct RecordingAttachment {
    recorder: ExecutionRecorder,
    callbacks: Vec<CallbackDescriptor>,
}

impl RecordingAttachment {
    /// Register a callback's metadata and instrument it for the selected mode.
    pub fn wrap_callback<'a>(
        &mut self,
        descriptor: CallbackDescriptor,
        callback: Box<dyn Callback + 'a>,
    ) -> Result<Box<dyn Callback + 'a>, String> {
        let callback_id = self
            .recorder
            .0
            .lock()
            .unwrap()
            .intern_tables
            .as_ref()
            .expect("attachment has name tables")
            .callbacks
            .lookup_by_value(&descriptor.name)
            .ok_or_else(|| format!("unknown recording callback '{}'", descriptor.name))?;
        let index = self.callbacks.len();
        let mode = descriptor.recording_mode;
        self.callbacks.push(descriptor);
        if mode == RecordingMode::Off {
            return Ok(callback);
        }
        Ok(Box::new(Recorded {
            callback,
            recorder: self.recorder.clone(),
            index,
            callback_id,
            active: None,
            mode,
        }))
    }

    /// Publish metadata after every callback has been registered.
    pub fn finish(self) {
        self.recorder.0.lock().unwrap().descriptor = Some(ExecutionDescriptor {
            callbacks: self.callbacks,
            logged_channels: Vec::new(),
        });
    }
}
struct Recorded<'a> {
    callback_id: crate::string_interner::CallbackId,
    mode: RecordingMode,
    callback: Box<dyn Callback + 'a>,
    recorder: ExecutionRecorder,
    index: usize,
    active: Option<ExecutionRecord>,
}
impl Callback for Recorded<'_> {
    fn outputs_are_local(&self) -> bool {
        self.callback.outputs_are_local()
    }
    fn clear_replay_inputs(&mut self) -> Result<(), LoanError> {
        self.callback.clear_replay_inputs()
    }
    fn stage_replay_iox2_event(
        &mut self,
        ordinal: usize,
        event_id: usize,
        count: u64,
    ) -> Result<(), LoanError> {
        self.callback
            .stage_replay_iox2_event(ordinal, event_id, count)
    }
    fn recording_endpoints(&self) -> Option<Vec<EndpointDescriptor>> {
        self.callback.recording_endpoints()
    }
    fn has_iox2_endpoints(&self) -> bool {
        self.callback.has_iox2_endpoints()
    }
    fn has_triggering_inputs(&self) -> bool {
        self.callback.has_triggering_inputs()
    }
    fn visit_prepared_messages(&self, visit: &mut dyn FnMut(LoggedMessage)) {
        self.callback.visit_prepared_messages(visit);
    }
    fn visit_pending_messages(&self, visit: &mut dyn FnMut(LoggedMessage)) {
        self.callback.visit_pending_messages(visit);
    }
    fn visit_prepared_iox2_events(&self, visit: &mut dyn FnMut(LoggedIox2Event)) {
        self.callback.visit_prepared_iox2_events(visit);
    }
    fn visit_pending_iox2_events(&self, visit: &mut dyn FnMut(LoggedIox2Event)) {
        self.callback.visit_pending_iox2_events(visit);
    }
    fn set_waker(&mut self, wake: crate::wake::WakeHandle) {
        self.callback.set_waker(wake);
    }
    fn has_pending_inputs(&self) -> bool {
        self.callback.has_pending_inputs()
    }
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        self.callback.visit_channel_names(visit);
    }
    fn update_inputs(&mut self) {
        self.callback.update_inputs();
    }
    fn required_inputs_available(&self) -> bool {
        self.callback.required_inputs_available()
    }
    fn required_inputs_ready(&self) -> bool {
        self.callback.required_inputs_ready()
    }
    fn finish_inputs(&mut self) {
        self.callback.finish_inputs();
    }
    #[cfg(feature = "iceoryx2")]
    fn take_iox2_events(&mut self) -> Vec<crate::iox2::Iox2EventRegistration> {
        let mut registrations = self.callback.take_iox2_events();
        if self.mode != RecordingMode::Full {
            return registrations;
        }
        let endpoints: Vec<_> = {
            let state = self.recorder.0.lock().unwrap();
            state
                .descriptor
                .as_ref()
                .expect("recorder descriptor missing")
                .callbacks[self.index]
                .endpoints
                .iter()
                .filter(|endpoint| {
                    endpoint.direction == Direction::Received
                        && endpoint.transport == Transport::Iox2Event
                })
                .cloned()
                .collect()
        };
        assert_eq!(
            endpoints.len(),
            registrations.len(),
            "event registrations must match recording metadata"
        );
        for (registration, endpoint) in registrations.iter_mut().zip(endpoints) {
            assert_eq!(
                registration.channel, endpoint.channel,
                "event registration order must match metadata"
            );
            let previous = registration.observer.take();
            let recorder = self.recorder.clone();
            let callback_id = self.callback_id;
            let ordinal = endpoint.ordinal;
            registration.observer = Some(Arc::new(move |observed_at, event| {
                if let Some(previous) = &previous {
                    previous(observed_at, event);
                }
                recorder.observe_iox2_event(Iox2EventRecord {
                    callback_id,
                    observed_at,
                    iox2_event: LoggedIox2Event {
                        ordinal,
                        event_id: event.event_id.as_value(),
                        count: event.count,
                    },
                });
            }));
        }
        registrations
    }
    fn run(&mut self, context: &Context) -> Result<(), LoanError> {
        let mut record = ExecutionRecord {
            callback_id: self.callback_id,
            execution_time: context.now(),
            body_duration_ns: 0,
            inputs: Vec::new(),
            iox2_events: Vec::new(),
            output_iox2_events: Vec::new(),
            outputs: Vec::new(),
            outcome: Outcome::Cancelled,
        };
        if self.mode == RecordingMode::Full {
            self.callback
                .visit_prepared_messages(&mut |message| record.inputs.push(message));
            self.callback
                .visit_prepared_iox2_events(&mut |event| record.iox2_events.push(event));
        }
        let start = Instant::now();
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.callback.run(context)));
        record.body_duration_ns = start.elapsed().as_nanos().try_into().unwrap_or(u64::MAX);
        record.outcome = match &result {
            Ok(Ok(())) => Outcome::Cancelled,
            Ok(Err(error)) => Outcome::BodyError(format!("{error:?}")),
            Err(_) => Outcome::BodyPanicked,
        };
        self.active = Some(record);
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
    fn flush_outputs(&mut self, time: FrameworkTime) {
        if self.mode == RecordingMode::Full
            && let Some(record) = &mut self.active
        {
            self.callback.visit_pending_messages(&mut |mut message| {
                message.header.published_at = time;
                record.outputs.push(message);
            });
            self.callback
                .visit_pending_iox2_events(&mut |event| record.output_iox2_events.push(event));
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.callback.flush_outputs(time)
        }));
        if let Some(mut record) = self.active.take() {
            record.outcome = if result.is_ok() {
                Outcome::Committed
            } else {
                Outcome::CommitPanicked
            };
            self.recorder.record(record);
        }
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
    fn discard_outputs(&mut self) {
        if let Some(mut record) = self.active.take() {
            record.outputs.clear();
            record.output_iox2_events.clear();
            self.recorder.record(record);
        }
        self.callback.discard_outputs();
    }
}
