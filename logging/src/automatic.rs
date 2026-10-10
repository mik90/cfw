//! Automatic capture planning for named graphs, before arena allocation.
//!
//! ```
//! use logging::{AutomaticCapturePlan, CaptureOptions};
//! use task::automatic::NamedPlan;
//!
//! let mut plan = NamedPlan::default();
//! // Register tasks and their channel overrides here.
//! let captures = AutomaticCapturePlan::declare(
//!     &mut plan, &CaptureOptions::new(16).exclude("diagnostics"),
//! )?;
//! let storage = plan.allocate()?;
//! let bindings = storage.bind()?;
//! let captures = captures.bind(&bindings)?;
//! // Pass captures to LogSession; retain storage until execution and logging end.
//! # Ok::<(), task::automatic::BuildError>(())
//! ```
use crate::Capture;
use std::collections::BTreeSet;
use task::automatic::{BuildError, NamedBindings, NamedPlan, capture::NamedCapturePlan};

#[derive(Clone, Debug)]
pub struct CaptureOptions {
    pub capacity: usize,
    pub excluded_channels: BTreeSet<String>,
}
impl CaptureOptions {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            excluded_channels: BTreeSet::new(),
        }
    }
    /// Exclude a resolved channel name, including any task-instance override.
    pub fn exclude(mut self, channel: impl Into<String>) -> Self {
        self.excluded_channels.insert(channel.into());
        self
    }
}

pub struct AutomaticCapturePlan(NamedCapturePlan);
impl AutomaticCapturePlan {
    /// Register all tasks first. Each discovered data channel gets one capture.
    pub fn declare(plan: &mut NamedPlan, options: &CaptureOptions) -> Result<Self, BuildError> {
        Ok(Self(plan.declare_captures(
            options.capacity,
            &options.excluded_channels,
        )?))
    }
    pub fn channels(&self) -> &[String] {
        self.0.channels()
    }
    pub fn bind<'a>(self, bindings: &NamedBindings<'a>) -> Result<Vec<Capture<'a>>, BuildError> {
        Ok(self
            .0
            .bind(bindings)?
            .into_iter()
            .map(Capture::from_serialized)
            .collect())
    }
}

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
/// use logging::{AutomaticReplayPlan, ReplayOptions};
/// use task::{automatic::NamedPlan, SubscriberPolicy};
/// let mut plan = NamedPlan::default();
/// // Task macros discover codecs automatically. Manual declarations opt in:
/// let input = plan.subscriber::<u64>("input", 4, SubscriberPolicy::default())?;
/// # #[cfg(feature = "serde")]
/// plan.register_replay_native::<u64>("input")?;
/// # #[cfg(feature = "serde")]
/// let replay = AutomaticReplayPlan::declare(&mut plan, ["input"], &ReplayOptions::new(1))?;
/// let storage = plan.allocate()?;
/// let bindings = storage.bind()?;
/// # #[cfg(feature = "serde")]
/// let sources = replay.bind(&bindings)?;
/// // Pass sources to the ordinary replay executor. For log-driven selection,
/// // use from_log before allocation; bind_feed also carries exclusions forward.
/// # Ok::<(), task::automatic::BuildError>(())
/// ```
pub struct AutomaticReplayPlan {
    plan: task::automatic::replay::NamedReplayPlan,
    excluded: BTreeSet<String>,
}
impl AutomaticReplayPlan {
    pub fn declare(
        plan: &mut NamedPlan,
        channels: impl IntoIterator<Item = impl AsRef<str>>,
        options: &ReplayOptions,
    ) -> Result<Self, BuildError> {
        let channels = channels
            .into_iter()
            .map(|c| c.as_ref().to_owned())
            .filter(|c| !options.excluded_channels.contains(c))
            .collect();
        Ok(Self {
            plan: plan.declare_replay_sources(&channels, options.capacity)?,
            excluded: options.excluded_channels.clone(),
        })
    }
    /// Select data channels in this log; recorded execution/event channels are
    /// interpreted by ReplayFeed, not injected as ordinary payloads.
    #[cfg(feature = "serde")]
    pub fn from_log(
        plan: &mut NamedPlan,
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
        bindings: &NamedBindings<'a>,
    ) -> Result<Vec<crate::ReplaySource<'a>>, BuildError> {
        Ok(self
            .plan
            .bind(bindings)?
            .into_iter()
            .map(crate::ReplaySource::from_serialized)
            .collect())
    }
    /// Carry planning exclusions into the shared simulation/live replay feed.
    #[cfg(feature = "serde")]
    pub fn bind_feed<'a>(
        self,
        bindings: &NamedBindings<'a>,
        reader: crate::SortedLogStreamReader,
    ) -> Result<crate::ReplayFeed<'a>, crate::BoxedLogError> {
        let excluded = self.excluded.iter().cloned().collect();
        crate::ReplayFeed::new(reader, self.bind(bindings)?, excluded)
    }
}
