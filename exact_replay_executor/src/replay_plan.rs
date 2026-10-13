//! Exact per-port bindings planned before allocating a named graph.
//!
//! ```no_run
//! use exact_replay_executor::{ExactReplayPlan, ExactReplayExecutor, ReplayLog};
//! use task_graph::{GraphPlan, TaskRegistration};
//! fn replay(tasks: Vec<TaskRegistration>, log: ReplayLog) -> Result<(), Box<dyn std::error::Error>> {
//!     let mut plan = GraphPlan::default();
//!     for task in tasks { task.register(&mut plan)?; }
//!     let replay = ExactReplayPlan::declare(&mut plan, &log)?;
//!     let storage = plan.allocate()?;
//!     let bindings = storage.bind()?;
//!     let replay = replay.bind(&bindings)?;
//!     let graph = storage.build_with(&bindings)?;
//!     let report = ExactReplayExecutor::new(graph, log, replay)?.run()?;
//!     assert!(report.is_exact());
//!     Ok(())
//! }
//! ```
use crate::{ReplayBindings, ReplayError, ReplayLog, SourceCache, SourceCachePlan};
use std::collections::{BTreeMap, BTreeSet};
use task::recording::{Direction, Transport};
use task_graph::{GraphBindings, GraphPlan, capture::OutputBinding, replay::SourceBinding};

enum PortBinding {
    Input(SourceBinding),
    Output(OutputBinding),
}

/// A caller-supplied port binding, omitted from inferred wiring.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExplicitReplayPort {
    callback: String,
    output: bool,
    ordinal: usize,
}
impl ExplicitReplayPort {
    pub fn input(callback: impl Into<String>, ordinal: usize) -> Self {
        Self {
            callback: callback.into(),
            output: false,
            ordinal,
        }
    }
    pub fn output(callback: impl Into<String>, ordinal: usize) -> Self {
        Self {
            callback: callback.into(),
            output: true,
            ordinal,
        }
    }
}

/// Validates a recorded task layout and prepares isolated per-port replay wiring.
pub struct ExactReplayPlan {
    ports: Vec<(String, usize, PortBinding)>,
}
impl ExactReplayPlan {
    pub fn declare(plan: &mut GraphPlan, log: &ReplayLog) -> Result<Self, ReplayError> {
        Self::declare_with_explicit(plan, log, &BTreeSet::new())
    }
    /// Keep selected ports explicit for custom/contextual decoders. Add their
    /// bindings to the returned ReplayBindings before constructing the executor.
    pub fn declare_with_explicit(
        plan: &mut GraphPlan,
        log: &ReplayLog,
        ports: &std::collections::BTreeSet<ExplicitReplayPort>,
    ) -> Result<Self, ReplayError> {
        let descriptor = log.descriptor_ref();
        if plan.task_names().count() != descriptor.callbacks.len() {
            return Err(ReplayError::Setup(
                "replay task count differs from descriptor".into(),
            ));
        }
        let mut requests = Vec::new();
        let mut remaining = ports.clone();
        let mut names = BTreeSet::new();
        for callback in &descriptor.callbacks {
            if !names.insert(&callback.name) {
                return Err(ReplayError::Setup(
                    "duplicate recorded callback name".into(),
                ));
            }
            let actual = plan
                .task_ports(&callback.name)
                .map_err(|e| ReplayError::Setup(e.to_string()))?;
            if actual.len() != callback.endpoints.len() {
                return Err(ReplayError::Setup(format!(
                    "endpoint count differs for '{}'",
                    callback.name
                )));
            }
            let mut ordinals = [0, 0];
            let mut mapped = BTreeMap::new();
            for port in actual {
                let direction = usize::from(port.output);
                mapped.insert((port.output, ordinals[direction]), port);
                ordinals[direction] += 1;
            }
            for expected in &callback.endpoints {
                let output = expected.direction == Direction::Published;
                let port = mapped.remove(&(output, expected.ordinal)).ok_or_else(|| {
                    ReplayError::Setup(format!(
                        "unknown/duplicate port for '{}' ordinal {}",
                        callback.name, expected.ordinal
                    ))
                })?;
                if port.channel != expected.channel
                    || port.payload_type != expected.payload_type
                    || port.transport != expected.transport
                {
                    return Err(ReplayError::Setup(format!(
                        "endpoint layout differs for '{}' ordinal {}",
                        callback.name, expected.ordinal
                    )));
                }
                if port.transport == Transport::Event {
                    continue;
                }
                if remaining.remove(&ExplicitReplayPort {
                    callback: callback.name.clone(),
                    output,
                    ordinal: expected.ordinal,
                }) {
                    continue;
                }
                let capable = if output {
                    plan.can_observe_output(&callback.name, expected.ordinal)
                } else {
                    plan.can_inject_input(&callback.name, expected.ordinal)
                }
                .map_err(|e| ReplayError::Setup(e.to_string()))?;
                if !capable {
                    return Err(ReplayError::Setup(format!(
                        "channel '{}': missing {} capability; use an explicit port binding",
                        port.channel,
                        if output {
                            "serialization"
                        } else {
                            "context-free decoding"
                        }
                    )));
                }
                if plan
                    .port_is_configured(&callback.name, expected.ordinal, output)
                    .map_err(|e| ReplayError::Setup(e.to_string()))?
                {
                    return Err(ReplayError::Setup(format!(
                        "replay port '{}' ordinal {} already configured",
                        callback.name, expected.ordinal
                    )));
                }
                requests.push((callback.name.clone(), expected.ordinal, output));
            }
        }
        if !remaining.is_empty() {
            return Err(ReplayError::Setup(format!(
                "explicit replay ports do not identify data endpoints: {remaining:?}"
            )));
        }
        // Validate the complete descriptor before any endpoints are added or rewired.
        let mut ports = Vec::new();
        for (name, ordinal, output) in requests {
            let bind = if output {
                plan.observe_output(&name, ordinal).map(PortBinding::Output)
            } else {
                plan.inject_input(&name, ordinal, 1).map(PortBinding::Input)
            }
            .map_err(|e| ReplayError::Setup(e.to_string()))?;
            ports.push((name, ordinal, bind));
        }
        Ok(Self { ports })
    }
    /// Bind before task factories consume their endpoints. No publisher indices
    /// are changed; the executor translates them into recorded identity space.
    pub fn bind<'a>(self, bindings: &GraphBindings<'a>) -> Result<ReplayBindings<'a>, ReplayError> {
        let mut replay = ReplayBindings::new();
        for (name, ordinal, port) in self.ports {
            match port {
                PortBinding::Input(bind) => replay.add_input(
                    name,
                    ordinal,
                    bind(bindings).map_err(|e| ReplayError::Setup(e.to_string()))?,
                )?,
                PortBinding::Output(bind) => replay.add_output(
                    name,
                    ordinal,
                    bind(bindings).map_err(|e| ReplayError::Setup(e.to_string()))?,
                )?,
            }
        }
        Ok(replay)
    }
    /// Typed extension for contextual forwarding graphs: reserve source retention
    /// from distinct full-header identities in the log, including reproduced outputs.
    pub fn source_cache<T: Send + Sync + 'static>(
        plan: &mut GraphPlan,
        log: &ReplayLog,
        channel: &str,
    ) -> Result<GraphSourceCachePlan<T>, ReplayError> {
        let plan = plan
            .native::<T>(channel)
            .map_err(|error| ReplayError::Setup(error.to_string()))?;
        let cache = SourceCachePlan::declare(plan, log.source_capacity(channel))
            .map_err(|error| ReplayError::Setup(format!("source cache '{channel}': {error:?}")))?;
        Ok(GraphSourceCachePlan {
            channel: channel.into(),
            cache,
        })
    }
}

/// A typed source dependency with retention capacity derived from the replay log.
pub struct GraphSourceCachePlan<T> {
    channel: String,
    cache: SourceCachePlan<T>,
}
impl<T: Send + Sync + 'static> GraphSourceCachePlan<T>
where
    for<'ctx> T: task::loggable::Loggable<Context<'ctx> = ()>,
{
    /// Install the loader automatically; return the typed cache for contextual decoders.
    pub fn bind<'a>(
        self,
        bindings: &GraphBindings<'a>,
        replay: &mut ReplayBindings<'a>,
    ) -> Result<SourceCache<'a, T>, ReplayError> {
        let cache = self
            .cache
            .bind(
                bindings
                    .native::<T>(&self.channel)
                    .map_err(|error| ReplayError::Setup(error.to_string()))?,
            )
            .map_err(|error| ReplayError::Setup(error.to_string()))?;
        let loader = cache.clone();
        replay.add_cache(self.channel, move |header, bytes| {
            loader.load(header, bytes)
        })?;
        Ok(cache)
    }
}
