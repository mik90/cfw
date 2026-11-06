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
    Callback,
    execution_log_schema::{
        Direction, EndpointDescriptor, ExecutionRecord, LoggedMessage, Transport,
    },
};
use task_graph::{BuiltGraph, GraphMetadata};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergencePolicy {
    Strict,
    /// Continue after comparison failures or unavailable input payloads. Callback
    /// panics and capture failures remain terminal because state was mutated.
    BestEffort,
}
pub struct ExactReplayConfig {
    pub divergence_policy: DivergencePolicy,
    /// Shared entry cap for structured gap/mismatch/error details (zero disables
    /// details). Each reason is capped at report::MAX_DETAIL_BYTES.
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
pub enum ExactReplayStepError {
    InvalidLog(String),
    Setup(String),
    Divergence(String),
    Gap {
        channel: task::ChannelName,
        reason: String,
    },
    Callback {
        callback: task::CallbackName,
        reason: String,
    },
    Poisoned,
}
impl std::fmt::Display for ExactReplayStepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exact replay: {self:?}")
    }
}
impl std::error::Error for ExactReplayStepError {}
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
    pub callback: task::CallbackName,
    pub time: task::time::FrameworkTime,
    pub executed: bool,
}

struct PublisherIndices {
    current: u32,
    recorded: u32,
}

pub struct ExactReplayExecutor<'storage> {
    detail_endpoint: Option<(Direction, usize)>,
    detail_header: Option<task::message::MessageHeader>,
    callbacks: Vec<Box<dyn Callback + 'storage>>,
    mapping: Vec<usize>,
    publishers: Vec<BTreeMap<usize, PublisherIndices>>,
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
    /// Construct replay over callbacks whose outputs are already bound locally.
    /// `ExactReplayPlan::bind` configures graph-planned IPC outputs. For explicit
    /// IPC bindings, use `Iox2ChannelStorage::bind_with_delivery(OutputDelivery::Local)`.
    pub fn new(
        graph: BuiltGraph<'storage>,
        log: ReplayLog,
        bindings: ReplayBindings<'storage>,
    ) -> Result<Self, ExactReplayStepError> {
        Self::with_config(graph, log, bindings, ExactReplayConfig::default())
    }
    pub fn with_config(
        graph: BuiltGraph<'storage>,
        log: ReplayLog,
        mut bindings: ReplayBindings<'storage>,
        config: ExactReplayConfig,
    ) -> Result<Self, ExactReplayStepError> {
        let (nodes, mut metadata) = graph.into_parts();
        let mut callback_names = task::string_interner::CallbackNameInterner::new();
        for callback in &log.descriptor.callbacks {
            callback_names.intern(&callback.name);
        }
        metadata.callback_names = Arc::new(callback_names);
        if nodes.len() != log.descriptor.callbacks.len() {
            return Err(ExactReplayStepError::Setup(
                "callback count differs from descriptor".into(),
            ));
        }
        let mut mapping = Vec::new();
        let mut publishers = Vec::new();
        let mut inputs = HashSet::new();
        let mut outputs = HashSet::new();
        for descriptor in &log.descriptor.callbacks {
            let index = nodes
                .iter()
                .position(|node| node.name == descriptor.name.as_str())
                .ok_or_else(|| {
                    ExactReplayStepError::Setup(format!("missing callback '{}'", descriptor.name))
                })?;
            let mut actual = nodes[index].callback.recording_endpoints().ok_or_else(|| {
                ExactReplayStepError::Setup(format!(
                    "callback '{}' has no endpoint metadata",
                    descriptor.name
                ))
            })?;
            let mut expected = descriptor.endpoints.clone();
            actual.sort_by_key(|p| (p.direction == Direction::Published, p.ordinal));
            expected.sort_by_key(|p| (p.direction == Direction::Published, p.ordinal));
            let mut publisher_indices = BTreeMap::new();
            // Only publisher storage indices may differ between endpoint layouts.
            for (endpoint, recorded) in actual.iter_mut().zip(&expected) {
                if let (Some(current), Some(index)) =
                    (endpoint.publisher_index, recorded.publisher_index)
                {
                    publisher_indices.insert(
                        recorded.ordinal,
                        PublisherIndices {
                            current,
                            recorded: index,
                        },
                    );
                    endpoint.publisher_index = Some(index);
                }
            }
            if actual != expected {
                return Err(ExactReplayStepError::Setup(format!(
                    "endpoint layout differs for '{}'",
                    descriptor.name
                )));
            }
            for endpoint in &descriptor.endpoints {
                let key = (descriptor.name.clone(), endpoint.ordinal);
                if endpoint.transport == Transport::Iox2Event {
                    continue;
                }
                match endpoint.direction {
                    Direction::Received => {
                        let source = bindings.inputs.get(&key).ok_or_else(|| {
                            ExactReplayStepError::Setup(format!(
                                "missing hydration binding {key:?}"
                            ))
                        })?;
                        if source.channel() != endpoint.channel.as_str()
                            || source.payload_type() != endpoint.payload_type
                        {
                            return Err(ExactReplayStepError::Setup(format!(
                                "hydration type/channel mismatch for {key:?}"
                            )));
                        }
                        inputs.insert(key);
                    }
                    Direction::Published => {
                        let capture = bindings.outputs.get(&key).ok_or_else(|| {
                            ExactReplayStepError::Setup(format!("missing output capture {key:?}"))
                        })?;
                        if capture.channel() != endpoint.channel.as_str()
                            || capture.payload_type() != endpoint.payload_type
                        {
                            return Err(ExactReplayStepError::Setup(format!(
                                "capture type/channel mismatch for {key:?}"
                            )));
                        }
                        outputs.insert(key);
                    }
                }
            }
            if !nodes[index].callback.outputs_are_local() {
                return Err(ExactReplayStepError::Setup(format!(
                    "callback '{}': bind outputs with local delivery before exact replay",
                    descriptor.name
                )));
            }
            mapping.push(index);
            publishers.push(publisher_indices);
        }
        if inputs.len() != bindings.inputs.len() || outputs.len() != bindings.outputs.len() {
            return Err(ExactReplayStepError::Setup(
                "extra or event-only data bindings".into(),
            ));
        }
        for ((channel, header), bytes) in &log.payloads {
            if let Some(load) = bindings.caches.get_mut(channel) {
                load(*header, bytes).map_err(|e| {
                    ExactReplayStepError::Setup(format!("source cache '{channel}': {e}"))
                })?;
            }
        }
        let mut report = ReplayReport::new(log.executions.len(), config.max_mismatch_details);
        report.set_intern_tables(log.intern_tables.clone());
        Ok(Self {
            detail_endpoint: None,
            detail_header: None,
            callbacks: nodes.into_iter().map(|n| n.callback).collect(),
            mapping,
            publishers,
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
    pub fn step(&mut self) -> Result<Option<ReplayStep>, ExactReplayStepError> {
        if self.failed {
            return Err(ExactReplayStepError::Poisoned);
        }
        if self.cursor == self.log.executions.len() || self.stop.0.load(Ordering::Acquire) {
            return Ok(None);
        }
        let record = self.log.executions[self.cursor].clone();
        self.detail_endpoint = None;
        self.detail_header = None;
        self.cursor += 1;
        self.report.mark_consumed();
        let callback = self.log.descriptor.callbacks[record.callback_id.index()]
            .name
            .clone();
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.execute(&record)))
                .unwrap_or_else(|_| {
                    Err(ExactReplayStepError::Callback {
                        callback,
                        reason: "execution or hydration panicked".into(),
                    })
                });
        if let Err(error) = &result {
            self.failed = true;
            if !matches!(
                error,
                ExactReplayStepError::Gap { .. } | ExactReplayStepError::Divergence(_)
            ) {
                self.report.record_error();
                self.detail(
                    crate::report::ReplayDetailKind::Error,
                    None,
                    error.to_string(),
                );
            }
        }
        result.map(Some)
    }
    pub fn run(&mut self) -> Result<ReplayReport, ExactReplayStepError> {
        while self.step()?.is_some() {}
        Ok(self.replay_report())
    }
    fn gap(&mut self, channel: &str, reason: String) -> Result<(), ExactReplayStepError> {
        self.detail(
            crate::report::ReplayDetailKind::Gap,
            Some(channel),
            reason.clone(),
        );
        self.report.record_gap(channel);
        self.report.record_error();
        if self.config.divergence_policy == DivergencePolicy::Strict {
            Err(ExactReplayStepError::Gap {
                channel: channel.into(),
                reason,
            })
        } else {
            Ok(())
        }
    }
    fn mismatch(&mut self, channel: &str, detail: String) -> Result<(), ExactReplayStepError> {
        self.detail(
            crate::report::ReplayDetailKind::Mismatch,
            Some(channel),
            detail.clone(),
        );
        self.report.record_mismatch(channel, detail.clone());
        self.report.record_error();
        if self.config.divergence_policy == DivergencePolicy::Strict {
            Err(ExactReplayStepError::Divergence(detail))
        } else {
            Ok(())
        }
    }
    fn execute(&mut self, record: &ExecutionRecord) -> Result<ReplayStep, ExactReplayStepError> {
        let descriptor = self.log.descriptor.callbacks[record.callback_id.index()].clone();
        let name = descriptor.name;
        let index = self.mapping[record.callback_id.index()];
        let skipped = || ReplayStep {
            callback: name.clone(),
            time: record.execution_time,
            executed: false,
        };
        if record.truncated != 0 {
            self.gap(
                "execution_log",
                format!(
                    "'{name}' record dropped {} entries at capacity and is not exactly replayable",
                    record.truncated
                ),
            )?;
            return Ok(skipped());
        }
        self.callbacks[index].clear_replay_inputs().map_err(|e| {
            ExactReplayStepError::Callback {
                callback: name.clone(),
                reason: format!("input reset: {e}"),
            }
        })?;
        for capture in self.bindings.outputs.values_mut() {
            capture.clear();
        }
        for message in record.inputs.iter() {
            self.detail_endpoint = Some((Direction::Received, message.ordinal));
            self.detail_header = Some(message.header);
            let endpoint = endpoint(&descriptor.endpoints, Direction::Received, message.ordinal);
            let identity = (endpoint.channel.clone(), message.header);
            let logged = self.log.logged.contains(&endpoint.channel);
            let bytes = if logged {
                self.log.payloads.get(&identity).cloned()
            } else {
                self.reproduced.get(&identity).cloned()
            };
            let Some(bytes) = bytes else {
                self.gap(
                    &endpoint.channel,
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
                    &endpoint.channel,
                    format!("input '{name}' decoding/publication: {error}"),
                )?;
                return Ok(skipped());
            }
            if logged {
                self.report.record_logged(&endpoint.channel);
            } else {
                self.report.record_reproduced(&endpoint.channel);
            }
        }
        for event in record.iox2_events.iter() {
            self.detail_endpoint = Some((Direction::Received, event.ordinal));
            self.detail_header = None;
            self.callbacks[index]
                .stage_replay_iox2_event(event.ordinal, event.event_id, event.count)
                .map_err(|e| ExactReplayStepError::Callback {
                    callback: name.clone(),
                    reason: format!("event hydration: {e}"),
                })?;
        }
        self.detail_endpoint = None;
        self.detail_header = None;
        let Some(prepared) = task::callback::prepare_callback(self.callbacks[index].as_mut())
        else {
            return Err(ExactReplayStepError::Callback {
                callback: name.clone(),
                reason: "recorded snapshot does not satisfy required inputs".into(),
            });
        };
        let mut actual = Vec::new();
        prepared.visit_prepared_messages(&mut |message| actual.push(message));
        let actual_headers = headers(&actual);
        let expected_headers = headers(&record.inputs);
        if let Some(ordinal) = actual_headers
            .keys()
            .chain(expected_headers.keys())
            .find(|ordinal| actual_headers.get(ordinal) != expected_headers.get(ordinal))
        {
            let ordinal = *ordinal;
            drop(prepared);
            self.detail_endpoint = Some((Direction::Received, ordinal));
            let endpoint = endpoint(&descriptor.endpoints, Direction::Received, ordinal);
            self.gap(
                &endpoint.channel,
                format!("prepared snapshot differs for '{name}' input {ordinal}"),
            )?;
            return Ok(skipped());
        }
        let mut prepared_events = Vec::new();
        prepared.visit_prepared_iox2_events(&mut |event| prepared_events.push(event));
        if prepared_events.as_slice() != &record.iox2_events[..] {
            return Err(ExactReplayStepError::Callback {
                callback: name.clone(),
                reason: "hydrated event snapshot differs".into(),
            });
        }
        let completed = prepared
            .run(&self.metadata.context(record.execution_time))
            .map_err(|e| ExactReplayStepError::Callback {
                callback: name.clone(),
                reason: format!("{e:?}"),
            })?;
        let mut output_events = Vec::new();
        completed.visit_pending_iox2_events(&mut |event| output_events.push(event));
        completed.commit();
        if output_events.as_slice() != &record.output_iox2_events[..] {
            self.mismatch(
                "output_iox2_events",
                format!("'{name}' output event counts/IDs differ"),
            )?;
        }
        for endpoint in descriptor
            .endpoints
            .iter()
            .filter(|p| p.direction == Direction::Published && p.transport != Transport::Iox2Event)
        {
            self.detail_endpoint = Some((Direction::Published, endpoint.ordinal));
            self.detail_header = None;
            let mut actual = self
                .bindings
                .outputs
                .get_mut(&(name.clone(), endpoint.ordinal))
                .unwrap()
                .take()
                .map_err(|e| ExactReplayStepError::Callback {
                    callback: name.clone(),
                    reason: format!("capture: {e}"),
                })?;
            let indices = &self.publishers[record.callback_id.index()][&endpoint.ordinal];
            if let Some((header, _)) = actual
                .iter()
                .find(|(header, _)| header.publisher_index != indices.current)
            {
                self.detail_header = Some(*header);
                self.mismatch(
                    &endpoint.channel,
                    format!(
                        "'{name}' publisher {} captured index {}, expected current index {}",
                        endpoint.ordinal, header.publisher_index, indices.current
                    ),
                )?;
                // A misattributed endpoint cannot safely populate reproduced/source caches.
                continue;
            }
            for (header, _) in &mut actual {
                header.publisher_index = indices.recorded;
            }
            let expected: Vec<_> = record
                .outputs
                .iter()
                .filter(|m| m.ordinal == endpoint.ordinal)
                .collect();
            for (position, expected) in expected.iter().enumerate() {
                self.detail_header = Some(expected.header);
                let identity = (endpoint.channel.clone(), expected.header);
                let logged = self.log.logged.contains(&endpoint.channel);
                let expected_body = if logged {
                    match self.log.payloads.get(&identity).cloned() {
                        Some(bytes) => {
                            self.report.record_logged(&endpoint.channel);
                            Some(bytes)
                        }
                        None => {
                            self.gap(
                                &endpoint.channel,
                                format!("missing expected output for '{name}'"),
                            )?;
                            None
                        }
                    }
                } else {
                    if actual.get(position).is_some() {
                        self.report.record_reproduced(&endpoint.channel);
                    } else {
                        self.report.record_gap(&endpoint.channel);
                        self.detail(
                            crate::report::ReplayDetailKind::Gap,
                            Some(&endpoint.channel),
                            "output was not reproduced".into(),
                        );
                    }
                    None
                };
                match actual.get(position) {
                    None => self.mismatch(
                        &endpoint.channel,
                        format!(
                            "'{name}' publisher {} missing output {position}",
                            endpoint.ordinal
                        ),
                    )?,
                    Some((header, body))
                        if *header != expected.header
                            || expected_body
                                .as_ref()
                                .is_some_and(|expected| expected != body) =>
                    {
                        self.mismatch(
                            &endpoint.channel,
                            format!(
                                "'{name}' publisher {} output {position} differs",
                                endpoint.ordinal
                            ),
                        )?
                    }
                    _ => {}
                }
            }
            for (position, (header, _)) in actual.iter().enumerate().skip(expected.len()) {
                self.detail_header = Some(*header);
                self.mismatch(
                    &endpoint.channel,
                    format!(
                        "'{name}' publisher {} unexpected output {position}",
                        endpoint.ordinal
                    ),
                )?;
            }
            for (header, body) in actual {
                self.detail_header = Some(header);
                if !self.log.logged.contains(&endpoint.channel) {
                    let identity = (endpoint.channel.clone(), header);
                    if self.reproduced.insert(identity, body.clone()).is_some() {
                        return Err(ExactReplayStepError::Callback {
                            callback: name.clone(),
                            reason: "ambiguous reproduced publication".into(),
                        });
                    }
                    if let Some(load) = self.bindings.caches.get_mut(&endpoint.channel) {
                        load(header, &body).map_err(|e| ExactReplayStepError::Callback {
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
    fn detail(
        &mut self,
        kind: crate::report::ReplayDetailKind,
        channel: Option<&str>,
        reason: String,
    ) {
        let record = &self.log.executions[self.cursor - 1];
        let channel = channel.or_else(|| {
            self.detail_endpoint.and_then(|(direction, ordinal)| {
                self.log.descriptor.callbacks[record.callback_id.index()]
                    .endpoints
                    .iter()
                    .find(|endpoint| endpoint.direction == direction && endpoint.ordinal == ordinal)
                    .map(|endpoint| endpoint.channel.as_str())
            })
        });
        self.report.detail(crate::report::ReplayDetail {
            kind,
            callback: record.callback_id,
            channel: channel.and_then(|name| self.log.intern_tables.channels.lookup_by_value(name)),
            time: record.execution_time,
            endpoint: self.detail_endpoint,
            publication: self.detail_header,
            reason,
            reason_truncated: false,
        });
    }
}
fn endpoint(
    endpoints: &[EndpointDescriptor],
    direction: Direction,
    ordinal: usize,
) -> &EndpointDescriptor {
    endpoints
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
