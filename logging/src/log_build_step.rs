use std::collections::HashSet;

use task::callback::{Callback, CallbackNode};
use task::channel_registry::ChannelRegistry;
use task::task_graph_builder::{TaskGraphBuildStep, TaskGraphBuildStepError};

use crate::log_task::{ChannelLogger, LogTask, LogTaskConfiguration};

/// Default queue depth for log-task subscribers. Logging is meant to keep up
/// with the publisher's steady rate; matches the existing
/// `DEFAULT_TEST_SUBSCRIBER_CAPACITY` to avoid surprises during tests.
const DEFAULT_LOG_QUEUE_CAPACITY: usize = 10;

/// Adds a `LogTask` to the task graph. The build step walks every existing
/// `CallbackNode`'s publishers, asks `registry` for a matching serializer by
/// `GenericPublisher::value_type_id`, and — when one exists — builds a
/// `ChannelLogger` + matching subscriber and injects them into the new
/// `LogTask` `CallbackNode` via `CallbackNode::new_with`. Channels whose
/// types aren't registered as loggable are silently skipped.
pub struct LoggingBuildStep {
    config: LogTaskConfiguration,
    registry: ChannelRegistry,
}

impl LoggingBuildStep {
    pub fn new(config: LogTaskConfiguration, registry: ChannelRegistry) -> Self {
        LoggingBuildStep { config, registry }
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
        // Build a `ChannelLogger` for each (publisher, matching-serializer)
        // pair found across all existing callback nodes. Subscribers are
        // collected in lockstep so the LogTask's subscriber slots line up
        // 1:1 with its `channel_loggers`.
        let mut channel_loggers: Vec<ChannelLogger> = Vec::new();
        let mut subscribers = Vec::new();

        // Forwarded-source channels are covered by their `ForwardingPublisher`'s
        // own `ForwardedMessage<T, F>` logger — which serializes only the
        // forwarded message's header, avoiding double-logging. Skip them
        // here so we don't subscribe to a channel whose subscriber type won't
        // match a `ForwardingPublisher`'s expected `Subscriber<ForwardedMessage<T, F>>`.
        let forwarded_channels: HashSet<task::pub_sub::ChannelName> = nodes
            .iter()
            .flat_map(|n| {
                n.get_publishers()
                    .iter()
                    .flat_map(|p| p.get_forwarded_channels().to_vec())
            })
            .collect();

        for node in nodes {
            for publisher in node.get_publishers() {
                let channel_name = publisher.get_config().channel_name.clone();

                if forwarded_channels.contains(&channel_name) {
                    continue;
                }

                let Some(serializer) = self.registry.serializer_for(publisher.value_type_id())
                else {
                    // No serializer registered for this type — silently skip.
                    // The user either didn't register this type with the
                    // `ChannelRegistry`, or the type isn't `Loggable`.
                    continue;
                };

                let sub_config = task::subscriber::SubscriberConfig {
                    // Non-triggering optional span: the LogTask runs on its
                    // periodic schedule and drains whatever has accumulated.
                    is_optional: true,
                    capacity: DEFAULT_LOG_QUEUE_CAPACITY,
                    is_trigger: false,
                    keep_across_runs: true,
                    channel_name: channel_name.clone(),
                };

                let Some(subscriber) = publisher.build_matching_subscriber(sub_config) else {
                    return Err(format!(
                        "LoggingBuildStep: publisher for channel '{}' does not support build_matching_subscriber",
                        channel_name
                    ).into());
                };

                channel_loggers.push(ChannelLogger::new(channel_name, serializer));
                subscribers.push(subscriber);
            }
        }

        // No loggable channels found → skip adding a LogTask node entirely.
        if channel_loggers.is_empty() {
            return Ok(vec![]);
        }

        let log_task = LogTask::new(&self.config, channel_loggers);

        // Per the build-step contract, we drive `build_subscribers()` and
        // `build_publishers()` on the callback to get its initial set, then
        // extend with the subscribers we created above. LogTask's
        // `build_subscribers` returns `vec![]` (subscribers are entirely
        // build-step-driven); `build_publishers` returns the
        // `log_task_diagnostics` publisher.
        let mut all_subscribers = log_task.build_subscribers();
        all_subscribers.extend(subscribers);
        let publishers = log_task.build_publishers();

        let mut log_node = CallbackNode::new_with(
            Box::new(log_task),
            all_subscribers,
            publishers,
            "LogTask".into(),
        );
        // The simulation executor queries every running node's duration — give
        // LogTask a no-op so it doesn't panic. Logging should be invisible to
        // scheduling, so we occupy zero sim-time.
        log_node.set_execution_duration_callback(Box::new(|| std::time::Duration::ZERO));
        let period = self.config.period;
        log_node.set_execution_time_callback(Box::new(move |now| Some(now + period)));

        Ok(vec![log_node])
    }
}
