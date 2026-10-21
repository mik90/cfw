//! Exact per-endpoint bindings planned before allocating graph storage.
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

enum EndpointBinding {
    Input(SourceBinding),
    Output(OutputBinding),
}

/// A caller-supplied endpoint binding, omitted from inferred wiring.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExplicitReplayEndpoint {
    callback: String,
    output: bool,
    ordinal: usize,
}
impl ExplicitReplayEndpoint {
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

/// Validates a recorded task layout and prepares isolated per-endpoint replay wiring.
pub struct ExactReplayPlan {
    endpoints: Vec<(String, usize, EndpointBinding)>,
    #[cfg(feature = "iceoryx2")]
    local_outputs: Vec<(String, usize, bool)>,
}
impl ExactReplayPlan {
    pub fn declare(plan: &mut GraphPlan, log: &ReplayLog) -> Result<Self, ReplayError> {
        Self::declare_with_explicit(plan, log, &BTreeSet::new())
    }
    /// Keep selected endpoints explicit for custom/contextual decoders. Add their
    /// bindings to the returned ReplayBindings before constructing the executor.
    pub fn declare_with_explicit(
        plan: &mut GraphPlan,
        log: &ReplayLog,
        endpoints: &std::collections::BTreeSet<ExplicitReplayEndpoint>,
    ) -> Result<Self, ReplayError> {
        let descriptor = log.descriptor_ref();
        if plan.task_names().count() != descriptor.callbacks.len() {
            return Err(ReplayError::Setup(
                "replay task count differs from descriptor".into(),
            ));
        }
        let mut requests = Vec::new();
        #[cfg(feature = "iceoryx2")]
        let mut local_outputs = Vec::new();
        let mut remaining = endpoints.clone();
        let mut names = BTreeSet::new();
        for callback in &descriptor.callbacks {
            if !names.insert(&callback.name) {
                return Err(ReplayError::Setup(
                    "duplicate recorded callback name".into(),
                ));
            }
            let actual = plan
                .task_endpoints(&callback.name)
                .map_err(|e| ReplayError::Setup(e.to_string()))?;
            if actual.len() != callback.endpoints.len() {
                return Err(ReplayError::Setup(format!(
                    "endpoint count differs for '{}'",
                    callback.name
                )));
            }
            let mut ordinals = [0, 0];
            let mut mapped = BTreeMap::new();
            for endpoint in actual {
                let direction = usize::from(endpoint.output);
                mapped.insert((endpoint.output, ordinals[direction]), endpoint);
                ordinals[direction] += 1;
            }
            for expected in &callback.endpoints {
                let output = expected.direction == Direction::Published;
                let endpoint = mapped.remove(&(output, expected.ordinal)).ok_or_else(|| {
                    ReplayError::Setup(format!(
                        "unknown/duplicate endpoint for '{}' ordinal {}",
                        callback.name, expected.ordinal
                    ))
                })?;
                if endpoint.channel != expected.channel
                    || endpoint.payload_type != expected.payload_type
                    || endpoint.transport != expected.transport
                {
                    return Err(ReplayError::Setup(format!(
                        "endpoint layout differs for '{}' ordinal {}",
                        callback.name, expected.ordinal
                    )));
                }
                #[cfg(feature = "iceoryx2")]
                if output && endpoint.transport != Transport::Native {
                    local_outputs.push((
                        endpoint.channel.clone(),
                        endpoint.index,
                        endpoint.transport == Transport::Event,
                    ));
                }
                if endpoint.transport == Transport::Event {
                    continue;
                }
                if remaining.remove(&ExplicitReplayEndpoint {
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
                        "channel '{}': missing {} capability; use an explicit endpoint binding",
                        endpoint.channel,
                        if output {
                            "serialization"
                        } else {
                            "context-free decoding"
                        }
                    )));
                }
                if plan
                    .endpoint_is_configured(&callback.name, expected.ordinal, output)
                    .map_err(|e| ReplayError::Setup(e.to_string()))?
                {
                    return Err(ReplayError::Setup(format!(
                        "replay endpoint '{}' ordinal {} already configured",
                        callback.name, expected.ordinal
                    )));
                }
                requests.push((callback.name.clone(), expected.ordinal, output));
            }
        }
        if !remaining.is_empty() {
            return Err(ReplayError::Setup(format!(
                "explicit replay endpoints do not identify data endpoints: {remaining:?}"
            )));
        }
        // Validate the complete descriptor before any endpoints are added or rewired.
        let mut endpoints = Vec::new();
        for (name, ordinal, output) in requests {
            let bind = if output {
                plan.observe_output(&name, ordinal)
                    .map(EndpointBinding::Output)
            } else {
                plan.inject_input(&name, ordinal, 1)
                    .map(EndpointBinding::Input)
            }
            .map_err(|e| ReplayError::Setup(e.to_string()))?;
            endpoints.push((name, ordinal, bind));
        }
        Ok(Self {
            endpoints,
            #[cfg(feature = "iceoryx2")]
            local_outputs,
        })
    }
    /// Bind before task factories consume their endpoints. No publisher indices
    /// are changed; the executor translates them into recorded identity space.
    /// IPC publishers and notifiers, including explicit outputs, use local delivery.
    pub fn bind<'a>(self, bindings: &GraphBindings<'a>) -> Result<ReplayBindings<'a>, ReplayError> {
        #[cfg(feature = "iceoryx2")]
        for (channel, index, notifier) in self.local_outputs {
            bindings
                .set_ipc_output_delivery(
                    &channel,
                    index,
                    notifier,
                    task::iox2::OutputDelivery::Local,
                )
                .map_err(|e| ReplayError::Setup(e.to_string()))?;
        }
        let mut replay = ReplayBindings::new();
        for (name, ordinal, endpoint) in self.endpoints {
            match endpoint {
                EndpointBinding::Input(bind) => replay.add_input(
                    name,
                    ordinal,
                    bind(bindings).map_err(|e| ReplayError::Setup(e.to_string()))?,
                )?,
                EndpointBinding::Output(bind) => replay.add_output(
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
