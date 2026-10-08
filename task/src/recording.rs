//! Owned execution metadata; recording never retains endpoint/storage borrows.
use crate::{Callback, Context, LoanError, message::MessageHeader, time::FrameworkTime};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};
pub const EXECUTION_LOG_CHANNEL: &str = "execution_log";
pub const EXECUTION_EVENT_CHANNEL: &str = "execution_events";
pub const EXECUTION_LOG_DESCRIPTOR_ARTIFACT: &str = "execution_log_descriptor";

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
    Ipc,
    Event,
}
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EndpointDescriptor {
    pub ordinal: usize,
    pub channel: String,
    pub direction: Direction,
    pub transport: Transport,
    pub payload_type: String,
}
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CallbackDescriptor {
    pub name: String,
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
pub struct LoggedEvent {
    pub ordinal: usize,
    pub event_id: usize,
    pub count: u64,
}
/// Per-recipient activation at the readiness/injection boundary, independently
/// of when required inputs or pool capacity permit that callback to execute.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ObservedEvent {
    pub callback_index: usize,
    pub observed_at: FrameworkTime,
    pub event: LoggedEvent,
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
    pub callback_index: usize,
    pub execution_time: FrameworkTime,
    /// Measured body runtime, not simulator modeled occupancy.
    pub body_duration_ns: u64,
    pub inputs: Vec<LoggedMessage>,
    pub events: Vec<LoggedEvent>,
    pub output_events: Vec<LoggedEvent>,
    /// Committed outputs for Committed, attempts for CommitPanicked, otherwise empty.
    pub outputs: Vec<LoggedMessage>,
    pub outcome: Outcome,
}
struct State {
    attached: bool,
    descriptor: Option<ExecutionDescriptor>,
    records: VecDeque<ExecutionRecord>,
    events: VecDeque<ObservedEvent>,
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
            descriptor: None,
            records: VecDeque::with_capacity(capacity),
            events: VecDeque::new(),
            capacity,
            dropped: 0,
        })))
    }
    pub fn descriptor(&self) -> Option<ExecutionDescriptor> {
        self.0.lock().unwrap().descriptor.clone()
    }
    pub fn dropped(&self) -> usize {
        self.0.lock().unwrap().dropped
    }
    pub fn drain(&self) -> Vec<ExecutionRecord> {
        self.0.lock().unwrap().records.drain(..).collect()
    }
    pub fn drain_events(&self) -> Vec<ObservedEvent> {
        self.0.lock().unwrap().events.drain(..).collect()
    }
    #[cfg(feature = "iceoryx2")]
    fn observe_event(&self, event: ObservedEvent) {
        let mut state = self.0.lock().unwrap();
        if state.records.len() + state.events.len() == state.capacity {
            state.dropped = state.dropped.saturating_add(1);
        } else {
            state.events.push_back(event);
        }
    }
    fn record(&self, record: ExecutionRecord) {
        let mut state = self.0.lock().unwrap();
        if state.records.len() + state.events.len() == state.capacity {
            state.dropped = state.dropped.saturating_add(1);
        } else {
            state.records.push_back(record);
        }
    }
    pub fn attach<'a>(
        &self,
        graph: crate::BuiltGraph<'a>,
    ) -> Result<crate::BuiltGraph<'a>, String> {
        {
            let mut state = self.0.lock().unwrap();
            if state.attached {
                return Err("recorder already attached".into());
            }
            state.attached = true;
        }
        let mut descriptors = Vec::new();
        let graph = graph.try_map_callbacks(|index, name, callback| {
            let endpoints = callback.recording_endpoints().ok_or_else(|| {
                format!("callback '{name}' does not implement recording metadata")
            })?;
            descriptors.push(CallbackDescriptor {
                name: name.into(),
                endpoints,
            });
            Ok::<_, String>(Box::new(Recorded {
                callback,
                recorder: self.clone(),
                index,
                active: None,
            }) as Box<dyn Callback + 'a>)
        })?;
        self.0.lock().unwrap().descriptor = Some(ExecutionDescriptor {
            callbacks: descriptors,
            logged_channels: Vec::new(),
        });
        Ok(graph)
    }
}
struct Recorded<'a> {
    callback: Box<dyn Callback + 'a>,
    recorder: ExecutionRecorder,
    index: usize,
    active: Option<ExecutionRecord>,
}
impl Callback for Recorded<'_> {
    fn recording_endpoints(&self) -> Option<Vec<EndpointDescriptor>> {
        self.callback.recording_endpoints()
    }
    fn visit_prepared_messages(&self, visit: &mut dyn FnMut(LoggedMessage)) {
        self.callback.visit_prepared_messages(visit);
    }
    fn visit_pending_messages(&self, visit: &mut dyn FnMut(LoggedMessage)) {
        self.callback.visit_pending_messages(visit);
    }
    fn visit_prepared_events(&self, visit: &mut dyn FnMut(LoggedEvent)) {
        self.callback.visit_prepared_events(visit);
    }
    fn visit_pending_events(&self, visit: &mut dyn FnMut(LoggedEvent)) {
        self.callback.visit_pending_events(visit);
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
        let ports: Vec<_> = {
            let state = self.recorder.0.lock().unwrap();
            state
                .descriptor
                .as_ref()
                .expect("recorder descriptor missing")
                .callbacks[self.index]
                .endpoints
                .iter()
                .filter(|port| {
                    port.direction == Direction::Received && port.transport == Transport::Event
                })
                .cloned()
                .collect()
        };
        assert_eq!(
            ports.len(),
            registrations.len(),
            "event registrations must match recording metadata"
        );
        for (registration, port) in registrations.iter_mut().zip(ports) {
            assert_eq!(
                registration.channel, port.channel,
                "event registration order must match metadata"
            );
            let previous = registration.observer.take();
            let recorder = self.recorder.clone();
            let callback_index = self.index;
            let ordinal = port.ordinal;
            registration.observer = Some(Arc::new(move |observed_at, event| {
                if let Some(previous) = &previous {
                    previous(observed_at, event);
                }
                recorder.observe_event(ObservedEvent {
                    callback_index,
                    observed_at,
                    event: LoggedEvent {
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
            callback_index: self.index,
            execution_time: context.now(),
            body_duration_ns: 0,
            inputs: Vec::new(),
            events: Vec::new(),
            output_events: Vec::new(),
            outputs: Vec::new(),
            outcome: Outcome::Cancelled,
        };
        self.callback
            .visit_prepared_messages(&mut |message| record.inputs.push(message));
        self.callback
            .visit_prepared_events(&mut |event| record.events.push(event));
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
        if let Some(record) = &mut self.active {
            self.callback.visit_pending_messages(&mut |mut message| {
                message.header.published_at = time;
                record.outputs.push(message);
            });
            self.callback
                .visit_pending_events(&mut |event| record.output_events.push(event));
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
            record.output_events.clear();
            self.recorder.record(record);
        }
        self.callback.discard_outputs();
    }
}
