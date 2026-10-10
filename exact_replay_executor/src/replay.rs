use crate::replay_log::Identity;
use crate::{
    ReplayBindings, ReplayLog,
    report::{DEFAULT_MAX_MISMATCH_DETAILS, ReplayReport},
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use task::{
    BuiltGraph, Callback, Context, GraphMetadata, LoanError,
    recording::{
        Direction, EndpointDescriptor, ExecutionRecord, LoggedEvent, LoggedMessage, Transport,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergencePolicy {
    Strict,
    /// Continue after comparison failures or unavailable input payloads. Callback
    /// panics and capture failures remain terminal because state was mutated.
    BestEffort,
}
pub struct ExactReplayConfig {
    pub divergence_policy: DivergencePolicy,
    pub max_mismatch_details: usize,
}
impl Default for ExactReplayConfig {
    fn default() -> Self {
        Self {
            divergence_policy: DivergencePolicy::Strict,
            max_mismatch_details: DEFAULT_MAX_MISMATCH_DETAILS,
        }
    }
}
#[derive(Debug)]
pub enum ReplayError {
    InvalidLog(String),
    Setup(String),
    Divergence(String),
    Gap { channel: String, reason: String },
    Callback { callback: String, reason: String },
    Poisoned,
}
impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exact replay: {self:?}")
    }
}
impl std::error::Error for ReplayError {}
#[derive(Clone, Default)]
pub struct StopSignal(Arc<AtomicBool>);
impl StopSignal {
    pub fn request_stop(&self) {
        self.0.store(true, Ordering::Release);
    }
}
impl task::executor::ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        StopSignal::request_stop(self);
    }
}
#[derive(Debug)]
pub struct ReplayStep {
    pub callback: String,
    pub time: task::time::FrameworkTime,
    pub executed: bool,
}

pub struct ExactReplayExecutor<'storage> {
    callbacks: Vec<Box<dyn Callback + 'storage>>,
    mapping: Vec<usize>,
    metadata: GraphMetadata,
    log: ReplayLog,
    bindings: ReplayBindings<'storage>,
    reproduced: BTreeMap<Identity, Vec<u8>>,
    config: ExactReplayConfig,
    report: ReplayReport,
    cursor: usize,
    failed: bool,
    stop: StopSignal,
}
impl<'storage> ExactReplayExecutor<'storage> {
    pub fn new(
        graph: BuiltGraph<'storage>,
        log: ReplayLog,
        bindings: ReplayBindings<'storage>,
    ) -> Result<Self, ReplayError> {
        Self::with_config(graph, log, bindings, ExactReplayConfig::default())
    }
    pub fn with_config(
        graph: BuiltGraph<'storage>,
        log: ReplayLog,
        mut bindings: ReplayBindings<'storage>,
        config: ExactReplayConfig,
    ) -> Result<Self, ReplayError> {
        let (mut nodes, mut metadata) = graph.into_parts();
        let mut callback_names = task::string_interner::CallbackNameInterner::new();
        for callback in &log.descriptor.callbacks {
            callback_names.intern(&callback.name);
        }
        metadata.callback_names = Arc::new(callback_names);
        if nodes.len() != log.descriptor.callbacks.len() {
            return Err(ReplayError::Setup(
                "callback count differs from descriptor".into(),
            ));
        }
        let mut mapping = Vec::new();
        let mut inputs = HashSet::new();
        let mut outputs = HashSet::new();
        for descriptor in &log.descriptor.callbacks {
            let index = nodes
                .iter()
                .position(|node| node.name == descriptor.name)
                .ok_or_else(|| {
                    ReplayError::Setup(format!("missing callback '{}'", descriptor.name))
                })?;
            let mut actual = nodes[index].callback.recording_endpoints().ok_or_else(|| {
                ReplayError::Setup(format!(
                    "callback '{}' has no endpoint metadata",
                    descriptor.name
                ))
            })?;
            let mut expected = descriptor.endpoints.clone();
            actual.sort_by_key(|p| (p.direction == Direction::Published, p.ordinal));
            expected.sort_by_key(|p| (p.direction == Direction::Published, p.ordinal));
            // Publisher declaration order may differ in the replay plan.
            for port in &mut actual {
                if port.publisher_index.is_some() {
                    port.publisher_index = expected
                        .iter()
                        .find(|p| p.direction == port.direction && p.ordinal == port.ordinal)
                        .and_then(|p| p.publisher_index);
                }
            }
            if actual != expected {
                return Err(ReplayError::Setup(format!(
                    "endpoint layout differs for '{}'",
                    descriptor.name
                )));
            }
            for port in &descriptor.endpoints {
                let key = (descriptor.name.clone(), port.ordinal);
                if port.transport == Transport::Event {
                    continue;
                }
                match port.direction {
                    Direction::Received => {
                        let source = bindings.inputs.get(&key).ok_or_else(|| {
                            ReplayError::Setup(format!("missing hydration binding {key:?}"))
                        })?;
                        if source.channel() != port.channel
                            || source.payload_type() != port.payload_type
                        {
                            return Err(ReplayError::Setup(format!(
                                "hydration type/channel mismatch for {key:?}"
                            )));
                        }
                        inputs.insert(key);
                    }
                    Direction::Published => {
                        nodes[index]
                            .callback
                            .set_replay_publisher_index(
                                port.ordinal,
                                port.publisher_index.expect("validated data output index"),
                            )
                            .map_err(|e| {
                                ReplayError::Setup(format!("publisher mapping for {key:?}: {e:?}"))
                            })?;
                        let capture = bindings.outputs.get(&key).ok_or_else(|| {
                            ReplayError::Setup(format!("missing output capture {key:?}"))
                        })?;
                        if capture.channel() != port.channel
                            || capture.payload_type() != port.payload_type
                        {
                            return Err(ReplayError::Setup(format!(
                                "capture type/channel mismatch for {key:?}"
                            )));
                        }
                        outputs.insert(key);
                    }
                }
            }
            nodes[index].callback.enable_exact_replay().map_err(|e| {
                ReplayError::Setup(format!("callback '{}': {e:?}", descriptor.name))
            })?;
            mapping.push(index);
        }
        if inputs.len() != bindings.inputs.len() || outputs.len() != bindings.outputs.len() {
            return Err(ReplayError::Setup(
                "extra or event-only data bindings".into(),
            ));
        }
        for ((channel, header), bytes) in &log.payloads {
            if let Some(load) = bindings.caches.get_mut(channel) {
                load(*header, bytes)
                    .map_err(|e| ReplayError::Setup(format!("source cache '{channel}': {e}")))?;
            }
        }
        let report = ReplayReport::new(log.executions.len(), config.max_mismatch_details);
        Ok(Self {
            callbacks: nodes.into_iter().map(|n| n.callback).collect(),
            mapping,
            metadata,
            log,
            bindings,
            reproduced: BTreeMap::new(),
            config,
            report,
            cursor: 0,
            failed: false,
            stop: StopSignal::default(),
        })
    }
    pub fn stop_signal(&self) -> StopSignal {
        self.stop.clone()
    }
    pub fn replay_report(&self) -> ReplayReport {
        self.report.clone()
    }
    pub fn step(&mut self) -> Result<Option<ReplayStep>, ReplayError> {
        if self.failed {
            return Err(ReplayError::Poisoned);
        }
        if self.cursor == self.log.executions.len() || self.stop.0.load(Ordering::Acquire) {
            return Ok(None);
        }
        let record = self.log.executions[self.cursor].clone();
        self.cursor += 1;
        self.report.mark_consumed();
        let callback = self.log.descriptor.callbacks[record.callback_index]
            .name
            .clone();
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.execute(&record)))
                .unwrap_or_else(|_| {
                    Err(ReplayError::Callback {
                        callback,
                        reason: "execution or hydration panicked".into(),
                    })
                });
        if let Err(error) = &result {
            self.failed = true;
            if !matches!(error, ReplayError::Gap { .. } | ReplayError::Divergence(_)) {
                self.report.record_error();
            }
        }
        result.map(Some)
    }
    pub fn run(&mut self) -> Result<ReplayReport, ReplayError> {
        while self.step()?.is_some() {}
        Ok(self.replay_report())
    }
    fn gap(&mut self, channel: &str, reason: String) -> Result<(), ReplayError> {
        self.report.record_gap(channel);
        self.report.record_error();
        if self.config.divergence_policy == DivergencePolicy::Strict {
            Err(ReplayError::Gap {
                channel: channel.into(),
                reason,
            })
        } else {
            Ok(())
        }
    }
    fn mismatch(&mut self, channel: &str, detail: String) -> Result<(), ReplayError> {
        self.report.record_mismatch(channel, detail.clone());
        self.report.record_error();
        if self.config.divergence_policy == DivergencePolicy::Strict {
            Err(ReplayError::Divergence(detail))
        } else {
            Ok(())
        }
    }
    fn execute(&mut self, record: &ExecutionRecord) -> Result<ReplayStep, ReplayError> {
        let descriptor = self.log.descriptor.callbacks[record.callback_index].clone();
        let name = descriptor.name;
        let index = self.mapping[record.callback_index];
        let skipped = || ReplayStep {
            callback: name.clone(),
            time: record.execution_time,
            executed: false,
        };
        self.callbacks[index]
            .clear_replay_inputs()
            .map_err(|e| ReplayError::Callback {
                callback: name.clone(),
                reason: format!("input reset: {e:?}"),
            })?;
        for capture in self.bindings.outputs.values_mut() {
            capture.clear();
        }
        for message in &record.inputs {
            let port = endpoint(&descriptor.endpoints, Direction::Received, message.ordinal);
            let identity = (port.channel.clone(), message.header);
            let logged = self.log.logged.contains(&port.channel);
            let bytes = if logged {
                self.log.payloads.get(&identity).cloned()
            } else {
                self.reproduced.get(&identity).cloned()
            };
            let Some(bytes) = bytes else {
                self.gap(
                    &port.channel,
                    format!("missing input for '{name}': {:?}", message.header),
                )?;
                return Ok(skipped());
            };
            if let Err(error) = self
                .bindings
                .inputs
                .get_mut(&(name.clone(), message.ordinal))
                .unwrap()
                .inject(message.header, &bytes)
            {
                self.gap(
                    &port.channel,
                    format!("input '{name}' decoding/publication: {error}"),
                )?;
                return Ok(skipped());
            }
            if logged {
                self.report.record_logged(&port.channel);
            } else {
                self.report.record_reproduced(&port.channel);
            }
        }
        for event in &record.events {
            self.callbacks[index]
                .stage_replay_event(event.ordinal, event.event_id, event.count)
                .map_err(|e| ReplayError::Callback {
                    callback: name.clone(),
                    reason: format!("event hydration: {e:?}"),
                })?;
        }
        let mut hydration_error = None;
        let mut output_events = Vec::new();
        let mut checked = Checked {
            callback: self.callbacks[index].as_mut(),
            expected: record,
            hydration_error: &mut hydration_error,
            output_events: &mut output_events,
        };
        let executed =
            task::execute_callback(&mut checked, &self.metadata.context(record.execution_time));
        if let Some(ordinal) = hydration_error {
            let port = endpoint(&descriptor.endpoints, Direction::Received, ordinal);
            self.gap(
                &port.channel,
                format!("prepared snapshot differs for '{name}' input {ordinal}"),
            )?;
            return Ok(skipped());
        }
        executed.map_err(|e| ReplayError::Callback {
            callback: name.clone(),
            reason: format!("{e:?}"),
        })?;
        if output_events != record.output_events {
            self.mismatch(
                "execution_events",
                format!("'{name}' output event counts/IDs differ"),
            )?;
        }
        for port in descriptor
            .endpoints
            .iter()
            .filter(|p| p.direction == Direction::Published && p.transport != Transport::Event)
        {
            let actual = self
                .bindings
                .outputs
                .get_mut(&(name.clone(), port.ordinal))
                .unwrap()
                .take()
                .map_err(|e| ReplayError::Callback {
                    callback: name.clone(),
                    reason: format!("capture: {e}"),
                })?;
            let expected: Vec<_> = record
                .outputs
                .iter()
                .filter(|m| m.ordinal == port.ordinal)
                .collect();
            for (position, expected) in expected.iter().enumerate() {
                let identity = (port.channel.clone(), expected.header);
                let logged = self.log.logged.contains(&port.channel);
                let expected_body = if logged {
                    match self.log.payloads.get(&identity).cloned() {
                        Some(bytes) => {
                            self.report.record_logged(&port.channel);
                            Some(bytes)
                        }
                        None => {
                            self.gap(
                                &port.channel,
                                format!("missing expected output for '{name}'"),
                            )?;
                            None
                        }
                    }
                } else {
                    if actual.get(position).is_some() {
                        self.report.record_reproduced(&port.channel);
                    } else {
                        self.report.record_gap(&port.channel);
                    }
                    None
                };
                match actual.get(position) {
                    None => self.mismatch(
                        &port.channel,
                        format!(
                            "'{name}' publisher {} missing output {position}",
                            port.ordinal
                        ),
                    )?,
                    Some((header, body))
                        if *header != expected.header
                            || expected_body
                                .as_ref()
                                .is_some_and(|expected| expected != body) =>
                    {
                        self.mismatch(
                            &port.channel,
                            format!(
                                "'{name}' publisher {} output {position} differs",
                                port.ordinal
                            ),
                        )?
                    }
                    _ => {}
                }
            }
            for position in expected.len()..actual.len() {
                self.mismatch(
                    &port.channel,
                    format!(
                        "'{name}' publisher {} unexpected output {position}",
                        port.ordinal
                    ),
                )?;
            }
            for (header, body) in actual {
                if !self.log.logged.contains(&port.channel) {
                    let identity = (port.channel.clone(), header);
                    if self.reproduced.insert(identity, body.clone()).is_some() {
                        return Err(ReplayError::Callback {
                            callback: name.clone(),
                            reason: "ambiguous reproduced publication".into(),
                        });
                    }
                    if let Some(load) = self.bindings.caches.get_mut(&port.channel) {
                        load(header, &body).map_err(|e| ReplayError::Callback {
                            callback: name.clone(),
                            reason: format!("source cache: {e}"),
                        })?;
                    }
                }
            }
        }
        Ok(ReplayStep {
            callback: name,
            time: record.execution_time,
            executed: true,
        })
    }
}
fn endpoint(
    ports: &[EndpointDescriptor],
    direction: Direction,
    ordinal: usize,
) -> &EndpointDescriptor {
    ports
        .iter()
        .find(|p| p.direction == direction && p.ordinal == ordinal)
        .expect("validated endpoint")
}
fn headers(messages: &[LoggedMessage]) -> BTreeMap<usize, Vec<task::message::MessageHeader>> {
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for message in messages {
        groups
            .entry(message.ordinal)
            .or_default()
            .push(message.header);
    }
    groups
}
struct Checked<'a> {
    callback: &'a mut dyn Callback,
    expected: &'a ExecutionRecord,
    hydration_error: &'a mut Option<usize>,
    output_events: &'a mut Vec<LoggedEvent>,
}
impl Callback for Checked<'_> {
    fn update_inputs(&mut self) {
        self.callback.update_inputs();
    }
    fn run(&mut self, context: &Context) -> Result<(), LoanError> {
        let mut actual = Vec::new();
        self.callback
            .visit_prepared_messages(&mut |message| actual.push(message));
        let actual = headers(&actual);
        let expected = headers(&self.expected.inputs);
        if let Some(ordinal) = actual
            .keys()
            .chain(expected.keys())
            .find(|ordinal| actual.get(ordinal) != expected.get(ordinal))
        {
            *self.hydration_error = Some(*ordinal);
            return Err(LoanError::Transport("hydration snapshot mismatch".into()));
        }
        let mut events = Vec::new();
        self.callback
            .visit_prepared_events(&mut |event| events.push(event));
        if events != self.expected.events {
            return Err(LoanError::Transport(
                "hydrated event snapshot differs".into(),
            ));
        }
        if !self.callback.required_inputs_ready() {
            return Err(LoanError::Transport(
                "recorded snapshot does not satisfy required inputs".into(),
            ));
        }
        self.callback.run(context)
    }
    fn flush_outputs(&mut self, time: task::time::FrameworkTime) {
        self.callback
            .visit_pending_events(&mut |event| self.output_events.push(event));
        self.callback.flush_outputs(time);
    }
    fn discard_outputs(&mut self) {
        self.callback.discard_outputs();
    }
    fn finish_inputs(&mut self) {
        self.callback.finish_inputs();
    }
}
