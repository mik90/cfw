//! Exact per-port bindings planned before allocating a named graph.
//!
//! ```no_run
//! use exact_replay_executor::{AutomaticExactReplayPlan, ExactReplayExecutor, ReplayLog};
//! use task::automatic::{NamedPlan, TaskRegistration};
//! fn replay(tasks: Vec<TaskRegistration>, log: ReplayLog) -> Result<(), Box<dyn std::error::Error>> {
//!     let mut plan = NamedPlan::default();
//!     let tasks = tasks.into_iter().map(|task| task.register(&mut plan))
//!         .collect::<Result<Vec<_>, _>>()?;
//!     let replay = AutomaticExactReplayPlan::declare(&mut plan, &log)?;
//!     let storage = plan.allocate()?;
//!     let bindings = storage.bind()?;
//!     let replay = replay.bind(&bindings)?;
//!     let mut graph = storage.graph_builder();
//!     for task in tasks { task.add_to_graph(&mut graph, &bindings); }
//!     let graph = graph.build().map_err(|error| format!("{error:?}"))?;
//!     let report = ExactReplayExecutor::new(graph, log, replay)?.run()?;
//!     assert!(report.is_exact());
//!     Ok(())
//! }
//! ```
use crate::{ReplayBindings, ReplayError, ReplayLog, SourceCache, SourceCachePlan};
pub use task::automatic::exact::ExplicitReplayPort;
use task::automatic::{
    NamedBindings, NamedPlan,
    exact::{ExactPort, NamedExactPlan},
};

pub struct AutomaticExactReplayPlan(NamedExactPlan);
impl AutomaticExactReplayPlan {
    pub fn declare(plan: &mut NamedPlan, log: &ReplayLog) -> Result<Self, ReplayError> {
        plan.declare_exact_replay(log.descriptor_ref())
            .map(Self)
            .map_err(|error| ReplayError::Setup(error.to_string()))
    }
    /// Keep selected ports explicit for custom/contextual decoders. Add their
    /// bindings to the returned ReplayBindings before constructing the executor.
    pub fn declare_with_explicit(
        plan: &mut NamedPlan,
        log: &ReplayLog,
        ports: &std::collections::BTreeSet<ExplicitReplayPort>,
    ) -> Result<Self, ReplayError> {
        plan.declare_exact_replay_with_explicit(log.descriptor_ref(), ports)
            .map(Self)
            .map_err(|error| ReplayError::Setup(error.to_string()))
    }
    /// Bind before task factories consume their endpoints. No publisher indices
    /// are changed; the executor translates them into recorded identity space.
    pub fn bind<'a>(self, bindings: &NamedBindings<'a>) -> Result<ReplayBindings<'a>, ReplayError> {
        let mut replay = ReplayBindings::new();
        for (name, ordinal, port) in self
            .0
            .bind(bindings)
            .map_err(|error| ReplayError::Setup(error.to_string()))?
        {
            match port {
                ExactPort::Input(source) => replay.add_input(
                    name,
                    ordinal,
                    logging::ReplaySource::from_serialized(source),
                )?,
                ExactPort::Output(capture) => replay.add_output(name, ordinal, capture)?,
            }
        }
        Ok(replay)
    }
    /// Typed extension for contextual forwarding graphs: reserve source retention
    /// from distinct full-header identities in the log, including reproduced outputs.
    pub fn source_cache<T: Send + Sync + 'static>(
        plan: &mut NamedPlan,
        log: &ReplayLog,
        channel: &str,
    ) -> Result<AutomaticSourceCachePlan<T>, ReplayError> {
        let plan = plan
            .native::<T>(channel)
            .map_err(|error| ReplayError::Setup(error.to_string()))?;
        let cache = SourceCachePlan::declare(plan, log.source_capacity(channel))
            .map_err(|error| ReplayError::Setup(format!("source cache '{channel}': {error:?}")))?;
        Ok(AutomaticSourceCachePlan {
            channel: channel.into(),
            cache,
        })
    }
}

/// A typed source dependency with retention capacity derived from the replay log.
pub struct AutomaticSourceCachePlan<T> {
    channel: String,
    cache: SourceCachePlan<T>,
}
impl<T: Send + Sync + 'static> AutomaticSourceCachePlan<T>
where
    for<'ctx> T: task::loggable::Loggable<Context<'ctx> = ()>,
{
    /// Install the loader automatically; return the typed cache for contextual decoders.
    pub fn bind<'a>(
        self,
        bindings: &NamedBindings<'a>,
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
