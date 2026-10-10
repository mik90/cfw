//! Log-driven input injection over the ordinary discrete-event simulator.
use crate::{SimulationConfig, SimulationState, StepError, StepResult};
use logging::{BoxedLogError, ReplayFeed, ReplaySource, SortedLogStreamReader};
use std::{
    collections::HashSet,
    num::Saturating,
    sync::{Arc, Mutex},
};
use task::{BuiltGraph, time::FrameworkTime};

#[derive(Default)]
pub struct LogSimulationOptions {
    /// None starts at the first log timestamp (zero for empty logs), with kernel
    /// polling disabled. An explicit configuration is used verbatim.
    pub simulation: Option<SimulationConfig>,
    /// Exclude computed channels; event filtering uses descriptor channel names.
    pub denylist: HashSet<String>,
}
pub struct LogSimulation<'storage> {
    simulation: SimulationState<'storage>,
    // Keep sources alive through downstream work after the stream reaches EOF.
    feed: Arc<Mutex<ReplayFeed<'storage>>>,
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
        let feed = ReplayFeed::new(reader, sources, options.denylist)?;
        let mut simulation = SimulationState::with_config(graph, config)?;
        #[cfg(feature = "iceoryx2")]
        let targets = simulation.replay_event_targets();
        let first = feed.next_time();
        let feed = Arc::new(Mutex::new(feed));
        if let Some(at) = first {
            let source = feed.clone();
            simulation.schedule_stream(at, move |time| source.lock().map_err(|e| e.to_string())?.inject_due(time, |callback, channel, event| {
                #[cfg(feature = "iceoryx2")]
                {
                    let target = targets.get(&(callback.into(), event.event.ordinal)).ok_or_else(|| format!("unknown event recipient '{callback}' subscriber {}", event.event.ordinal))?;
                    if target.channel != channel { return Err(format!("event recipient '{callback}' channel mismatch: recorded '{channel}', bound '{}'", target.channel).into()); }
                    target.inject(event.observed_at, iceoryx2::prelude::EventId::new(event.event.event_id), event.event.count);
                    Ok(())
                }
                #[cfg(not(feature = "iceoryx2"))]
                { let _ = (callback, channel, event); Err("replaying event records requires iceoryx2".into()) }
            }).map_err(|e| e.to_string()))?;
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
            .exhausted()
    }
}
