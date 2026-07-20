use std::collections::HashSet;

use task::callback::{Callback, CallbackNode};
use task::channel_registry::ChannelRegistry;
use task::generic_subscriber::GenericSubscriber;
use task::task_graph_builder::{TaskGraphBuildStep, TaskGraphBuildStepError};

use crate::log_file::SharedLogFileWriter;
use crate::log_task::{
    ChannelLogger, LogTask, LogTaskConfiguration, log_task_diagnostics_channel, log_task_name,
    open_writer,
};

/// Default queue depth for log-task subscribers. Logging is meant to keep up
/// with the publisher's steady rate; matches the existing
/// `DEFAULT_TEST_SUBSCRIBER_CAPACITY` to avoid surprises during tests.
const DEFAULT_LOG_QUEUE_CAPACITY: usize = 10;

/// One log task's share of the loggable channels: the `ChannelLogger`s and
/// their matching subscribers, kept in lockstep (subscriber slot i drains
/// channel logger i).
#[derive(Default)]
struct Shard(Vec<ChannelLogger>, Vec<Box<dyn GenericSubscriber>>);

/// Adds `LogTask`s to the task graph. The build step walks every existing
/// `CallbackNode`'s publishers, asks `registry` for a matching serializer by
/// `GenericPublisher::value_type_id`, and — when one exists — builds a
/// `ChannelLogger` + matching subscriber and injects them into one of the new
/// `LogTask` `CallbackNode`s via `CallbackNode::new_with`. Channels whose
/// types aren't registered as loggable are silently skipped.
///
/// The collected channels are spread round-robin across
/// `LogTaskConfiguration::num_tasks` log tasks so an executor with a
/// multi-threaded pool can drain them in parallel. All tasks write to the
/// same log file through a `SharedLogFileWriter` and publish errors on their
/// own diagnostics channel (`log_task_diagnostics_channel`).
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
                n.publishers()
                    .iter()
                    .flat_map(|p| p.forwarded_channels().to_vec())
            })
            .collect();

        for node in nodes {
            for publisher in node.publishers() {
                let channel_name = publisher.config().channel_name.clone();

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

        // No loggable channels found → skip adding LogTask nodes entirely.
        if channel_loggers.is_empty() {
            return Ok(vec![]);
        }

        // Spread the channels round-robin across the requested number of log
        // tasks (clamped so every task gets at least one channel).
        // Round-robin rather than contiguous chunks so channels from the same
        // publisher node — adjacent in the collection above — land on
        // different log tasks.
        let num_tasks = self.config.num_tasks.max(1).min(channel_loggers.len());
        let mut shards: Vec<Shard> = (0..num_tasks).map(|_| Shard::default()).collect();
        for (index, (logger, subscriber)) in
            channel_loggers.into_iter().zip(subscribers).enumerate()
        {
            let shard = &mut shards[index % num_tasks];
            shard.0.push(logger);
            shard.1.push(subscriber);
        }

        // All log tasks share a single log file. The writer is created once
        // here and cloned per task — `open_writer` panics if the file can't
        // be created, so a bad path fails the build rather than the first run.
        let shared_writer = SharedLogFileWriter::new(open_writer(&self.config.output_path));

        let mut log_nodes = Vec::with_capacity(num_tasks);
        for (index, Shard(shard_loggers, shard_subscribers)) in shards.into_iter().enumerate() {
            let log_task = LogTask::new(
                Box::new(shared_writer.clone()),
                log_task_diagnostics_channel(index),
                shard_loggers,
            );

            // Per the build-step contract, we drive `build_subscribers()` and
            // `build_publishers()` on the callback to get its initial set, then
            // extend with the subscribers we created above. LogTask's
            // `build_subscribers` returns `vec![]` (subscribers are entirely
            // build-step-driven); `build_publishers` returns the diagnostics
            // publisher.
            let mut all_subscribers = log_task.build_subscribers();
            all_subscribers.extend(shard_subscribers);
            let publishers = log_task.build_publishers();

            let mut log_node = CallbackNode::new_with(
                Box::new(log_task),
                all_subscribers,
                publishers,
                log_task_name(index),
            );
            // The simulation executor queries every running node's duration — give
            // LogTask a no-op so it doesn't panic. Logging should be invisible to
            // scheduling, so we occupy zero sim-time.
            log_node.set_execution_duration_callback(Box::new(|| std::time::Duration::ZERO));
            let period = self.config.period;
            log_node.set_execution_time_callback(Box::new(move |now| Some(now + period)));

            log_nodes.push(log_node);
        }

        Ok(log_nodes)
    }
}
