//! Log-driven input injection over the ordinary discrete-event simulator.
use crate::{SimulationConfig, SimulationState, StepError, StepResult};
use logging::{BoxedLogError, OwnedLogEntry, ReplaySource, SortedLogStreamReader};
use std::{
    collections::{BTreeMap, HashSet},
    num::Saturating,
    sync::{Arc, Mutex},
};
use task::{
    BuiltGraph,
    recording::{
        Direction, EXECUTION_EVENT_CHANNEL, EXECUTION_LOG_CHANNEL,
        EXECUTION_LOG_DESCRIPTOR_ARTIFACT, ExecutionDescriptor, ExecutionRecord, ObservedEvent,
        Transport,
    },
    time::FrameworkTime,
};

#[derive(Default)]
pub struct LogSimulationOptions {
    /// None starts at the first log timestamp (or zero for an empty log), with
    /// kernel polling disabled: recorded activations are authoritative. Some
    /// uses the supplied configuration verbatim, including its start time.
    pub simulation: Option<SimulationConfig>,
    /// Exclude computed/internal channels from input replay. Event records are
    /// filtered by their descriptor's channel; denying execution_events skips all.
    pub denylist: HashSet<String>,
}
enum Pending {
    Data(OwnedLogEntry),
    Event {
        callback: String,
        channel: String,
        event: ObservedEvent,
    },
}
impl Pending {
    fn time(&self) -> FrameworkTime {
        match self {
            Self::Data(entry) => entry.header.published_at,
            Self::Event { event, .. } => event.observed_at,
        }
    }
}
struct Feed<'a> {
    reader: SortedLogStreamReader,
    sources: BTreeMap<String, ReplaySource<'a>>,
    descriptor: Option<ExecutionDescriptor>,
    denied: HashSet<String>,
    pending: Option<Pending>,
    exhausted: bool,
    #[cfg(feature = "iceoryx2")]
    targets: BTreeMap<(String, usize), crate::state::EventTarget>,
}
impl Feed<'_> {
    fn next(&mut self) -> Result<(), BoxedLogError> {
        self.pending = None;
        while let Some(entry) = self.reader.next_entry()? {
            if self.denied.contains(&entry.channel_name) {
                continue;
            }
            match entry.channel_name.as_str() {
                EXECUTION_LOG_CHANNEL => {
                    let record: ExecutionRecord = serde_json::from_slice(&entry.serialized_body)?;
                    let descriptor = self
                        .descriptor
                        .as_ref()
                        .ok_or("execution record has no descriptor")?;
                    if record.callback_index >= descriptor.callbacks.len()
                        || record.execution_time != entry.header.published_at
                    {
                        return Err("invalid execution record index or timestamp".into());
                    }
                }
                EXECUTION_EVENT_CHANNEL => {
                    let event: ObservedEvent = serde_json::from_slice(&entry.serialized_body)?;
                    if event.observed_at != entry.header.published_at {
                        return Err("event timestamp differs from log header".into());
                    }
                    let callback = self
                        .descriptor
                        .as_ref()
                        .and_then(|d| d.callbacks.get(event.callback_index))
                        .ok_or("event record has no matching callback descriptor")?;
                    let mut matches = callback.endpoints.iter().filter(|p| {
                        p.direction == Direction::Received && p.ordinal == event.event.ordinal
                    });
                    let port = matches
                        .next()
                        .ok_or("event record has unknown subscriber ordinal")?;
                    if matches.next().is_some() || port.transport != Transport::Event {
                        return Err(
                            "event record has ambiguous or non-event subscriber ordinal".into()
                        );
                    }
                    if self.denied.contains(&port.channel) {
                        continue;
                    }
                    self.pending = Some(Pending::Event {
                        callback: callback.name.clone(),
                        channel: port.channel.clone(),
                        event,
                    });
                    return Ok(());
                }
                _ => {
                    if !self.sources.contains_key(&entry.channel_name) {
                        return Err(format!(
                            "no replay source for channel '{}'",
                            entry.channel_name
                        )
                        .into());
                    }
                    self.pending = Some(Pending::Data(entry));
                    return Ok(());
                }
            }
        }
        self.exhausted = true;
        Ok(())
    }
    fn inject_due(&mut self, time: FrameworkTime) -> Result<Option<FrameworkTime>, BoxedLogError> {
        while self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.time() <= time)
        {
            match self.pending.take().unwrap() {
                Pending::Data(entry) => self
                    .sources
                    .get_mut(&entry.channel_name)
                    .unwrap()
                    .inject(entry.header, &entry.serialized_body)
                    .map_err(|e| {
                        format!(
                            "replay channel '{}' at {}: {e}",
                            entry.channel_name, entry.header.published_at
                        )
                    })?,
                Pending::Event {
                    callback,
                    channel,
                    event,
                } => {
                    #[cfg(feature = "iceoryx2")]
                    {
                        let target = self
                            .targets
                            .get(&(callback.clone(), event.event.ordinal))
                            .ok_or_else(|| {
                                format!(
                                    "unknown event recipient '{callback}' subscriber {}",
                                    event.event.ordinal
                                )
                            })?;
                        if target.channel != channel {
                            return Err(format!("event recipient '{callback}' channel mismatch: recorded '{channel}', bound '{}'", target.channel).into());
                        }
                        target.inject(
                            event.observed_at,
                            iceoryx2::prelude::EventId::new(event.event.event_id),
                            event.event.count,
                        );
                    }
                    #[cfg(not(feature = "iceoryx2"))]
                    {
                        let _ = (callback, channel, event);
                        return Err("replaying event records requires iceoryx2".into());
                    }
                }
            }
            self.next()?;
        }
        Ok(self.pending.as_ref().map(Pending::time))
    }
}

/// Binds already-planned input sources to a streaming log. It replays inputs and
/// arrivals, not the recorded callback execution order. Callback schedules must
/// supply explicit modeled durations. EOF still allows downstream work to drain.
/// Infinite periodic schedules are bounded by run_until_idle's step limit.
pub struct LogSimulation<'storage> {
    simulation: SimulationState<'storage>,
    // Retain sources through downstream draining, even after the stream action
    // returns None and is removed from the scheduler.
    feed: Arc<Mutex<Feed<'storage>>>,
}
impl<'storage> LogSimulation<'storage> {
    pub fn new(
        graph: BuiltGraph<'storage>,
        reader: SortedLogStreamReader,
        sources: impl IntoIterator<Item = ReplaySource<'storage>>,
    ) -> Result<Self, BoxedLogError> {
        Self::with_options(graph, reader, sources, LogSimulationOptions::default())
    }
    pub fn with_options(
        graph: BuiltGraph<'storage>,
        reader: SortedLogStreamReader,
        sources: impl IntoIterator<Item = ReplaySource<'storage>>,
        options: LogSimulationOptions,
    ) -> Result<Self, BoxedLogError> {
        let config = options.simulation.unwrap_or_else(|| SimulationConfig {
            start_time: reader
                .first_log_time()
                .unwrap_or_else(|| FrameworkTime::from_nanoseconds(0)),
            poll_external_events: false,
            ..Default::default()
        });
        let mut sources_by_channel = BTreeMap::new();
        for source in sources {
            let channel = source.channel().to_owned();
            if channel == EXECUTION_LOG_CHANNEL || channel == EXECUTION_EVENT_CHANNEL {
                return Err(format!("reserved replay source channel '{channel}'").into());
            }
            if sources_by_channel.insert(channel.clone(), source).is_some() {
                return Err(format!("duplicate replay source for '{channel}'").into());
            }
        }
        for channel in reader.channel_names() {
            if channel != EXECUTION_LOG_CHANNEL
                && channel != EXECUTION_EVENT_CHANNEL
                && !options.denylist.contains(channel)
                && !sources_by_channel.contains_key(channel)
            {
                return Err(format!(
                    "no replay source for channel '{channel}' (add a source or explicitly deny it)"
                )
                .into());
            }
        }
        let descriptor = reader
            .artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT)
            .map(serde_json::from_slice::<ExecutionDescriptor>)
            .transpose()?;
        if let Some(descriptor) = &descriptor {
            let mut names = HashSet::new();
            for callback in &descriptor.callbacks {
                if !names.insert(&callback.name) {
                    return Err(
                        format!("duplicate recorded callback name '{}'", callback.name).into(),
                    );
                }
                let mut ports = HashSet::new();
                for port in &callback.endpoints {
                    let receiving = port.direction == Direction::Received;
                    if !ports.insert((receiving, port.ordinal)) {
                        return Err(
                            format!("duplicate endpoint ordinal on '{}'", callback.name).into()
                        );
                    }
                }
            }
        }
        let mut simulation = SimulationState::with_config(graph, config)?;
        let mut feed = Feed {
            reader,
            sources: sources_by_channel,
            descriptor,
            denied: options.denylist,
            pending: None,
            exhausted: false,
            #[cfg(feature = "iceoryx2")]
            targets: simulation.replay_event_targets(),
        };
        feed.next()?;
        let first = feed.pending.as_ref().map(Pending::time);
        let feed = Arc::new(Mutex::new(feed));
        if let Some(at) = first {
            let source = feed.clone();
            simulation.schedule_stream(at, move |time| {
                source
                    .lock()
                    .map_err(|e| e.to_string())?
                    .inject_due(time)
                    .map_err(|e| e.to_string())
            })?;
        }
        Ok(Self { simulation, feed })
    }
    pub fn step(&mut self) -> Result<StepResult, StepError> {
        self.simulation.step()
    }
    pub fn run_until_idle(&mut self, max_steps: usize) -> Result<Vec<StepResult>, StepError> {
        self.simulation.run_until_idle(max_steps)
    }
    pub fn current_time(&self) -> FrameworkTime {
        self.simulation.current_time()
    }
    pub fn step_count(&self) -> Saturating<usize> {
        self.simulation.step_count()
    }
    pub fn input_exhausted(&self) -> bool {
        self.feed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .exhausted
    }
}
