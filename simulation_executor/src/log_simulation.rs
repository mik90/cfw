//! Log-driven input injection over the ordinary discrete-event simulator.
use crate::{SimulationConfig, SimulationState, StepError, StepResult};
use logging::{BoxedLogError, ReplayFeed, ReplaySource, SortedLogStreamReader};
use std::{
    collections::HashSet,
    num::Saturating,
    sync::{Arc, Mutex},
    time::Duration,
};
use task::{BuiltGraph, time::FrameworkTime};

#[derive(Default)]
pub struct LogSimulationOptions {
    pub eof: EofPolicy,
    /// None starts at the first log timestamp (zero for empty logs), with kernel
    /// polling disabled. An explicit configuration is used verbatim.
    pub simulation: Option<SimulationConfig>,
    /// Exclude computed channels; event filtering uses descriptor channel names.
    pub denylist: HashSet<String>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EofPolicy {
    /// Stop timer-only activations after final injection, then drain causal work.
    #[default]
    Drain,
    /// Run timers until the tail boundary after the final selected injection
    /// (simulation start for empty input), then drain causal work. Timer-only
    /// activations at the boundary are suppressed; in-flight work may finish later.
    Tail(Duration),
    /// Continue ordinary scheduling until explicitly stopped by the caller.
    Continue,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionReason {
    Drained,
    TailDrained,
    StepBudgetExhausted,
    Cancelled,
}
#[derive(Debug)]
pub struct LogSimulationCompletion {
    pub reason: CompletionReason,
    pub steps: usize,
    pub at: FrameworkTime,
    pub input_exhausted: bool,
}
pub struct LogSimulation<'storage> {
    eof: EofPolicy,
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
        let timers = simulation.timer_control();
        let eof = options.eof;
        #[cfg(feature = "iceoryx2")]
        let targets = simulation.replay_event_targets();
        let first = feed.next_time();
        let feed = Arc::new(Mutex::new(feed));
        if let Some(at) = first {
            let source = feed.clone();
            let mut tail = false;
            simulation.schedule_stream(at, move |time| {
                if tail { timers.store(false, std::sync::atomic::Ordering::Release); return Ok(None); }
                let next = source.lock().map_err(|e| e.to_string())?.inject_due(time, |callback, channel, event| {
                #[cfg(feature = "iceoryx2")]
                {
                    let target = targets.get(&(callback.into(), event.event.ordinal)).ok_or_else(|| format!("unknown event recipient '{callback}' subscriber {}", event.event.ordinal))?;
                    if target.channel != channel { return Err(format!("event recipient '{callback}' channel mismatch: recorded '{channel}', bound '{}'", target.channel).into()); }
                    target.inject(event.observed_at, iceoryx2::prelude::EventId::new(event.event.event_id), event.event.count);
                    Ok(())
                }
                #[cfg(not(feature = "iceoryx2"))]
                { let _ = (callback, channel, event); Err("replaying event records requires iceoryx2".into()) }
            }).map_err(|e| e.to_string())?;
                if next.is_none() {
                    match eof {
                        EofPolicy::Drain => timers.store(false, std::sync::atomic::Ordering::Release),
                        EofPolicy::Tail(duration) if !duration.is_zero() => {
                            tail = true;
                            return Ok(Some(time.checked_add_duration(duration).ok_or("EOF tail time overflow")?));
                        }
                        EofPolicy::Tail(_) => timers.store(false, std::sync::atomic::Ordering::Release),
                        EofPolicy::Continue => {},
                    }
                }
                Ok(next)
            })?;
        } else {
            match eof {
                EofPolicy::Continue => {}
                EofPolicy::Tail(duration) if !duration.is_zero() => {
                    let end = simulation
                        .current_time()
                        .checked_add_duration(duration)
                        .ok_or("EOF tail time overflow")?;
                    simulation.schedule_at(end, move |_| {
                        timers.store(false, std::sync::atomic::Ordering::Release);
                        Ok(())
                    })?;
                }
                _ => timers.store(false, std::sync::atomic::Ordering::Release),
            }
        }
        Ok(Self {
            simulation,
            feed,
            eof,
        })
    }
    pub fn step(&mut self) -> Result<StepResult, StepError> {
        self.simulation.step()
    }
    pub fn run_until_idle(&mut self, max_steps: usize) -> Result<Vec<StepResult>, StepError> {
        self.simulation.run_until_idle(max_steps)
    }
    /// Bounded completion, including feedback cycles. Cancellation is checked at
    /// step boundaries; callback failures retain the ordinary poisoned-state rules.
    pub fn run_until(
        &mut self,
        max_steps: usize,
        mut running: impl FnMut() -> bool,
    ) -> Result<LogSimulationCompletion, StepError> {
        let mut reason = CompletionReason::StepBudgetExhausted;
        let mut steps = 0;
        while steps < max_steps {
            if !running() {
                reason = CompletionReason::Cancelled;
                break;
            }
            let step = self.step()?;
            steps += 1;
            if step.idle {
                reason = if matches!(self.eof, EofPolicy::Tail(_)) {
                    CompletionReason::TailDrained
                } else {
                    CompletionReason::Drained
                };
                break;
            }
        }
        Ok(LogSimulationCompletion {
            reason,
            steps,
            at: self.current_time(),
            input_exhausted: self.input_exhausted(),
        })
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
