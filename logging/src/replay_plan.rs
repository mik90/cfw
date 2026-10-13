//! Channel-wide replay source planning.
use std::collections::BTreeSet;
use task_graph::{BuildError, GraphBindings, GraphPlan};

#[derive(Clone, Debug)]
pub struct ReplayOptions {
    pub capacity: usize,
    pub excluded_channels: BTreeSet<String>,
}
impl ReplayOptions {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            excluded_channels: BTreeSet::new(),
        }
    }
    pub fn exclude(mut self, channel: impl Into<String>) -> Self {
        self.excluded_channels.insert(channel.into());
        self
    }
}

/// Ordinary channel-wide replay. Register the intended workload first; this
/// neither disables producer callbacks nor replaces exact per-port hydration.
/// Contextual decoders (including borrowed forwarding) use explicit sources.
///
/// ```
/// use logging::{GraphReplayPlan, ReplayOptions};
/// use task::SubscriberPolicy;
/// use task_graph::GraphPlan;
/// let mut plan = GraphPlan::default();
/// // Task macros discover codecs automatically. Manual declarations opt in:
/// let input = plan.subscriber::<u64>("input", 4, SubscriberPolicy::default())?;
/// # #[cfg(feature = "serde")]
/// plan.register_replay_native::<u64>("input")?;
/// # #[cfg(feature = "serde")]
/// let replay = GraphReplayPlan::declare(&mut plan, ["input"], &ReplayOptions::new(1))?;
/// let storage = plan.allocate()?;
/// let bindings = storage.bind()?;
/// # #[cfg(feature = "serde")]
/// let sources = replay.bind(&bindings)?;
/// // Pass sources to the ordinary replay executor. For log-driven selection,
/// // use from_log before allocation; bind_feed also carries exclusions forward.
/// # Ok::<(), task_graph::BuildError>(())
/// ```
pub struct GraphReplayPlan {
    plan: task_graph::replay::ReplayPlan,
    excluded: BTreeSet<String>,
}
impl GraphReplayPlan {
    pub fn declare(
        plan: &mut GraphPlan,
        channels: impl IntoIterator<Item = impl AsRef<str>>,
        options: &ReplayOptions,
    ) -> Result<Self, BuildError> {
        let channels: BTreeSet<_> = channels
            .into_iter()
            .map(|c| c.as_ref().to_owned())
            .filter(|c| !options.excluded_channels.contains(c))
            .collect();
        for channel in &channels {
            if channel == task::recording::EXECUTION_LOG_CHANNEL
                || channel == task::recording::EXECUTION_EVENT_CHANNEL
            {
                return Err(BuildError(format!(
                    "reserved replay source channel '{channel}'"
                )));
            }
        }
        Ok(Self {
            plan: plan.declare_replay_sources(&channels, options.capacity)?,
            excluded: options.excluded_channels.clone(),
        })
    }
    /// Select data channels in this log; recorded execution/event channels are
    /// interpreted by ReplayFeed, not injected as ordinary payloads.
    #[cfg(feature = "serde")]
    pub fn from_log(
        plan: &mut GraphPlan,
        reader: &crate::SortedLogStreamReader,
        options: &ReplayOptions,
    ) -> Result<Self, crate::BoxedLogError> {
        crate::incompleteness::reject_incomplete_recording(
            reader.artifact(crate::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT),
        )?;
        Ok(Self::declare(
            plan,
            reader.channel_names().iter().filter(|channel| {
                channel.as_str() != task::recording::EXECUTION_LOG_CHANNEL
                    && channel.as_str() != task::recording::EXECUTION_EVENT_CHANNEL
            }),
            options,
        )?)
    }
    pub fn channels(&self) -> &[String] {
        self.plan.channels()
    }
    pub fn excluded_channels(&self) -> &BTreeSet<String> {
        &self.excluded
    }
    pub fn bind<'a>(
        self,
        bindings: &GraphBindings<'a>,
    ) -> Result<Vec<crate::ReplaySource<'a>>, BuildError> {
        self.plan.bind(bindings)
    }
    /// Carry planning exclusions into the shared simulation/live replay feed.
    #[cfg(feature = "serde")]
    pub fn bind_feed<'a>(
        self,
        bindings: &GraphBindings<'a>,
        reader: crate::SortedLogStreamReader,
    ) -> Result<crate::ReplayFeed<'a>, crate::BoxedLogError> {
        let excluded = self.excluded.iter().cloned().collect();
        crate::ReplayFeed::new(reader, self.bind(bindings)?, excluded)
    }
}
