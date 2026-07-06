use crate::log_task::LogTaskConfiguration;
use std::collections::HashSet;
use task::pub_sub::ChannelName;
use task::{
    callback::CallbackNode,
    task_graph_builder::{TaskGraphBuildStep, TaskGraphBuildStepError},
};

pub enum ChannelLogStrategy {
    AllChannels,
    Subset(Vec<ChannelName>),
    None,
}

struct LoggingBuildStep {
    config: LogTaskConfiguration,
    channel_log_strategy: ChannelLogStrategy,
}

impl LoggingBuildStep {
    pub fn new(config: LogTaskConfiguration, channel_log_strategy: ChannelLogStrategy) -> Self {
        Self {
            config,
            channel_log_strategy,
        }
    }
}

impl TaskGraphBuildStep for LoggingBuildStep {
    fn name(&self) -> &str {
        "LoggingBuildStep"
    }

    fn build_step(
        &self,
        nodes: &[CallbackNode],
    ) -> Result<Vec<CallbackNode>, TaskGraphBuildStepError> {
        let mut channel_set = HashSet::new();
        for node in nodes {
            for publisher in node.get_publishers() {
                channel_set.insert(&publisher.get_config().channel_name);
            }
        }

        // TODO create subscribers from the publisher's type

        // TODO Handle forwarded channels

        todo!("Need to add channel loggers")
    }
}
