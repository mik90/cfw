//! Live execution logging through the graph's pub/sub channels.
//!
//! Each recorded callback owns a publisher. `LogSession` consumes the matching
//! captures; there is no shared recorder queue or cross-thread drain.
use crate::BuiltGraph;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use task::{
    Callback, Context, LoanError, Publisher,
    execution_log_schema::{
        CallbackDescriptor, Direction, EndpointDescriptor, ExecutionDescriptor, ExecutionRecord,
        InternTables, Iox2EventRecord, LoggedIox2Event, LoggedMessage, Outcome, RecordingMode,
        RecordingOptions, Transport,
    },
    message::MessageHeader,
    time::FrameworkTime,
};

/// Publishers for every callbacks that has recording enabled.
pub struct ExecutionLogPublishers<'storage> {
    pub records: BTreeMap<String, Publisher<'storage, ExecutionRecord>>,
    pub iox2_events: BTreeMap<String, Publisher<'storage, Iox2EventRecord>>,
}

/// Persisted execution-log metadata. Contains no live records.
#[derive(Clone)]
pub struct ExecutionLogMetadata {
    pub descriptor: ExecutionDescriptor,
    pub intern_tables: InternTables,
    dropped_records: Arc<AtomicUsize>,
    dropped_iox2_event_records: Arc<AtomicUsize>,
}
impl ExecutionLogMetadata {
    pub fn dropped(&self) -> usize {
        self.dropped_records.load(Ordering::Relaxed)
            + self.dropped_iox2_event_records.load(Ordering::Relaxed)
    }
    pub fn dropped_records(&self) -> usize {
        self.dropped_records.load(Ordering::Relaxed)
    }
    pub fn dropped_iox2_event_records(&self) -> usize {
        self.dropped_iox2_event_records.load(Ordering::Relaxed)
    }
}

impl<'storage> BuiltGraph<'storage> {
    /// Wrap callbacks so their execution records and observed iox2 events are
    /// published instead of collected in a shared queue.
    pub fn with_execution_log(
        self,
        options: RecordingOptions,
        mut publishers: ExecutionLogPublishers<'storage>,
    ) -> Result<(Self, ExecutionLogMetadata), String> {
        for name in options.callbacks.keys() {
            if self
                .metadata()
                .callback_names
                .lookup_by_value(name)
                .is_none()
            {
                return Err(format!("unknown execution log callback '{name}'"));
            }
        }
        let intern_tables = self.metadata().intern_tables();
        let dropped_records = Arc::new(AtomicUsize::new(0));
        let dropped_iox2_event_records = Arc::new(AtomicUsize::new(0));
        let mut descriptors = Vec::new();
        let graph = self.try_map_callbacks(|_index, name, callback| {
            let mode = options
                .callbacks
                .get(name)
                .copied()
                .unwrap_or(options.default_mode);
            let endpoints = match (mode, callback.recording_endpoints()) {
                (RecordingMode::Full, None) => {
                    return Err(format!(
                        "callback '{name}' does not implement execution log metadata"
                    ));
                }
                (_, endpoints) => endpoints.unwrap_or_default(),
            };
            descriptors.push(CallbackDescriptor {
                name: name.into(),
                recording_mode: mode,
                endpoints: endpoints.clone(),
            });
            if mode == RecordingMode::Off {
                return Ok(callback);
            }
            let record_publisher = publishers
                .records
                .remove(name)
                .ok_or_else(|| format!("missing execution log publisher for '{name}'"))?;
            let event_publisher = publishers
                .iox2_events
                .remove(name)
                .ok_or_else(|| format!("missing iox2 event publisher for '{name}'"))?;
            let callback_id = intern_tables
                .callbacks
                .lookup_by_value(name)
                .expect("callback must be interned");
            let event_endpoints = endpoints
                .iter()
                .filter(|endpoint| {
                    endpoint.direction == Direction::Received
                        && endpoint.transport == Transport::Iox2Event
                })
                .cloned()
                .collect();
            Ok::<_, String>(Box::new(ExecutionLogRecorded {
                callback,
                record_publisher,
                event_publisher: Arc::new(Mutex::new(event_publisher)),
                dropped_records: Arc::clone(&dropped_records),
                dropped_iox2_event_records: Arc::clone(&dropped_iox2_event_records),
                event_endpoints,
                callback_id,
                mode,
                active: None,
            }) as Box<dyn Callback + 'storage>)
        })?;
        if !publishers.records.is_empty() || !publishers.iox2_events.is_empty() {
            return Err("execution log publishers do not match graph callbacks".into());
        }
        let metadata = ExecutionLogMetadata {
            descriptor: ExecutionDescriptor {
                callbacks: descriptors,
                logged_channels: Vec::new(),
            },
            intern_tables,
            dropped_records,
            dropped_iox2_event_records,
        };
        Ok((graph, metadata))
    }
}

struct ExecutionLogRecorded<'storage> {
    callback: Box<dyn Callback + 'storage>,
    record_publisher: Publisher<'storage, ExecutionRecord>,
    event_publisher: Arc<Mutex<Publisher<'storage, Iox2EventRecord>>>,
    dropped_records: Arc<AtomicUsize>,
    dropped_iox2_event_records: Arc<AtomicUsize>,
    event_endpoints: Vec<EndpointDescriptor>,
    callback_id: task::string_interner::CallbackId,
    mode: RecordingMode,
    active: Option<ExecutionRecord>,
}

impl ExecutionLogRecorded<'_> {
    fn publish_record(&mut self, record: ExecutionRecord) {
        let mut header = MessageHeader::new(record.execution_time);
        header.publisher_index = self.record_publisher.publisher_index();
        if self
            .record_publisher
            .publish_with_header(header, record)
            .is_err()
        {
            self.dropped_records.fetch_add(1, Ordering::Relaxed);
        }
    }
}
impl Callback for ExecutionLogRecorded<'_> {
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
    fn set_waker(&mut self, wake: task::wake::WakeHandle) {
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
    fn take_iox2_events<'a>(&mut self, visit: &mut dyn FnMut(task::iox2::Iox2EventRegistration<'a>))
    where
        Self: 'a,
    {
        let mut registrations = Vec::new();
        self.callback
            .take_iox2_events(&mut |registration| registrations.push(registration));
        if self.mode != RecordingMode::Full {
            for registration in registrations {
                visit(registration);
            }
            return;
        }
        assert_eq!(
            self.event_endpoints.len(),
            registrations.len(),
            "event registrations must match execution log metadata"
        );
        for (mut registration, endpoint) in registrations.into_iter().zip(&self.event_endpoints) {
            assert_eq!(
                registration.channel, endpoint.channel,
                "event registration order must match metadata"
            );
            let previous = registration.observer.take();
            let publisher = Arc::clone(&self.event_publisher);
            let dropped = Arc::clone(&self.dropped_iox2_event_records);
            let callback_id = self.callback_id;
            let ordinal = endpoint.ordinal;
            registration.observer = Some(Arc::new(move |observed_at, event| {
                if let Some(previous) = &previous {
                    previous(observed_at, event);
                }
                let mut publisher = publisher.lock().unwrap_or_else(|p| p.into_inner());
                let mut header = MessageHeader::new(observed_at);
                header.publisher_index = publisher.publisher_index();
                let record = Iox2EventRecord {
                    callback_id,
                    observed_at,
                    iox2_event: LoggedIox2Event {
                        ordinal,
                        event_id: event.event_id.as_value(),
                        count: event.count,
                    },
                };
                if publisher.publish_with_header(header, record).is_err() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }));
            visit(registration);
        }
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
            self.publish_record(record);
        }
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
    fn discard_outputs(&mut self) {
        if let Some(mut record) = self.active.take() {
            record.outputs.clear();
            record.output_iox2_events.clear();
            self.publish_record(record);
        }
        self.callback.discard_outputs();
    }
}
