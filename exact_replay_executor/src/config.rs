//! Configuration for the exact replay executor.

use logging::log_file::LogFileReader;
use task::callback::CallbackNode;
use task::channel_registry::ChannelRegistry;

use crate::replay_task::DivergencePolicy;

/// Default divergence policy: strict (stop on first mismatch).
pub(crate) const DEFAULT_DIVERGENCE_POLICY: DivergencePolicy = DivergencePolicy::Strict;

/// Configuration for the exact replay executor.
///
/// # Defaults
///
/// | Field | Default |
/// |---|---|
/// | `divergence_policy` | [`DivergencePolicy::Strict`] |
pub struct ExactReplayConfig {
    /// The callback nodes to replay. These should be fresh copies of the
    /// original nodes (not the ones already wired into a live executor).
    pub nodes: Vec<CallbackNode>,
    /// Channel registry containing serializers, deserializers, and publisher
    /// factories for every channel referenced in the log. Output serialization
    /// is performed by the logging crate using the registry's serializers.
    pub registry: ChannelRegistry,
    /// Log file reader populated with the recorded log data.
    pub log_reader: Box<dyn LogFileReader>,
    /// Divergence policy: `Strict` stops on first mismatch, `BestEffort`
    /// continues collecting errors.
    pub divergence_policy: DivergencePolicy,
}

impl ExactReplayConfig {
    /// Create a new configuration with default divergence policy.
    pub fn new(
        nodes: Vec<CallbackNode>,
        registry: ChannelRegistry,
        log_reader: Box<dyn LogFileReader>,
    ) -> Self {
        ExactReplayConfig {
            nodes,
            registry,
            log_reader,
            divergence_policy: DEFAULT_DIVERGENCE_POLICY,
        }
    }
}
