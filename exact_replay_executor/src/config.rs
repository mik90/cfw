//! Configuration for the exact replay executor.

use logging::log_file::LogFileReader;
use task::channel_registry::ChannelRegistry;
use task::executor::ThreadPoolConfig;

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
    /// The thread pools to replay. The callback nodes must be fresh copies of
    /// the original ones (not already wired into a live executor), arranged in
    /// the same global order the original graph used so node indices match the
    /// execution-log descriptor.
    pub pools: Vec<ThreadPoolConfig>,
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
        pools: Vec<ThreadPoolConfig>,
        registry: ChannelRegistry,
        log_reader: Box<dyn LogFileReader>,
    ) -> Self {
        ExactReplayConfig {
            pools,
            registry,
            log_reader,
            divergence_policy: DEFAULT_DIVERGENCE_POLICY,
        }
    }

    /// Override the divergence policy (e.g. for a `--best-effort` CLI flag).
    pub fn with_divergence_policy(mut self, policy: DivergencePolicy) -> Self {
        self.divergence_policy = policy;
        self
    }
}
