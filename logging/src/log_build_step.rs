use std::collections::HashSet;

use task::callback::CallbackNode;
use task::task_graph_builder::{TaskGraphBuildStep, TaskGraphBuildStepError};

use crate::log_task::{ChannelLogRequest, ChannelLogger, LogTask, LogTaskConfiguration};

/// Hard error returned when a `ChannelLogRequest` references a channel for
/// which no publisher can supply a matching subscriber.
#[derive(Debug)]
struct NoMatchingSubscriber {
    channel_name: String,
}

impl std::fmt::Display for NoMatchingSubscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "channel '{}' has no publisher that supports logging \
             (no `build_matching_subscriber` override returned Some)",
            self.channel_name,
        )
    }
}

impl std::error::Error for NoMatchingSubscriber {}

/// Hard error returned when no publisher exists on a requested channel.
#[derive(Debug)]
struct NoPublisherForChannel {
    channel_name: String,
}

impl std::fmt::Display for NoPublisherForChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "channel '{}' was requested for logging but no callback publishes on it",
            self.channel_name,
        )
    }
}

impl std::error::Error for NoPublisherForChannel {}

/// Adds a `LogTask` to the task graph that subscribes to a user-specified set
/// of channels and serializes their messages to disk. The build step runs
/// once per task graph build; subscribers are created here via
/// [`task::publisher::GenericPublisher::build_matching_subscriber`] and
/// injected into the `LogTask` via `CallbackNode::new_with`.
pub struct LoggingBuildStep {
    config: LogTaskConfiguration,
    requests: Vec<ChannelLogRequest>,
}

impl LoggingBuildStep {
    pub fn new(config: LogTaskConfiguration, requests: Vec<ChannelLogRequest>) -> Self {
        LoggingBuildStep { config, requests }
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
        // No requests → no logging node to add.
        if self.requests.is_empty() {
            return Ok(vec![]);
        }

        let mut channel_loggers: Vec<ChannelLogger> = Vec::with_capacity(self.requests.len());
        let mut subscribers = Vec::with_capacity(self.requests.len());

        // Names of channels that some publisher declares it forwards *from*.
        // We don't skip them silently — the user explicitly listed them, so
        // honoring that would either double-log the source (redundant) or
        // collide on connection (`connect_to_subscriber` would reject the
        // mismatched `Subscriber<SourceT>` against a `ForwardingPublisher`).
        // For now we don't have a robust way to distinguish forwarded-source
        // conflicts, so we let `build_matching_subscriber` validate the
        // remaining constraints below; the forwarded-channel awareness
        // reserved for future event-logging work.
        let _forwarded_channels: HashSet<task::pub_sub::ChannelName> = nodes
            .iter()
            .flat_map(|n| {
                n.get_publishers()
                    .iter()
                    .flat_map(|p| p.get_forwarded_channels().to_vec())
            })
            .collect();

        for request in &self.requests {
            let channel_name = request.channel_name.clone();

            // Find the first publisher on this channel — used to construct a
            // matching subscriber. All publishers on the same channel must
            // share a type (enforced by `connect_callback_nodes`), so any
            // will do.
            let first_matching_publisher = nodes.iter().find_map(|node| {
                node.get_publishers()
                    .iter()
                    .find(|p| p.get_config().channel_name == channel_name)
            });

            let Some(publisher) = first_matching_publisher else {
                return Err(Box::new(NoPublisherForChannel { channel_name })
                    as Box<dyn std::error::Error + Send + Sync>);
            };

            let sub_config = request.make_subscriber_config();
            let Some(subscriber) = publisher.build_matching_subscriber(sub_config) else {
                return Err(Box::new(NoMatchingSubscriber { channel_name })
                    as Box<dyn std::error::Error + Send + Sync>);
            };

            channel_loggers.push(ChannelLogger::new(
                channel_name.clone(),
                std::sync::Arc::clone(&request.serialize),
            ));
            subscribers.push(subscriber);
        }

        // `CallbackNode::new_with` runs `starting_subscriber_bitmask` over the
        // subscribers — that needs `CallbackNodeReadiness` Arc, which is
        // created internally. We let the readiness mechanism decide when
        // LogTask runs based on the `is_trigger`/`is_optional` config on
        // each subscriber (we set both above).
        let log_task = LogTask::new(&self.config, channel_loggers);
        let mut log_node = CallbackNode::new_with(
            Box::new(log_task),
            subscribers,
            // build_publishers is called inside new_with, producing our
            // `log_task_diagnostics` publisher.
            vec![],
            "LogTask".into(),
        );
        // The simulation executor queries every running node's duration — give
        // LogTask a no-op so it doesn't panic. Logging should be invisible to
        // scheduling, so we occupy zero sim-time.
        log_node.set_execution_duration_callback(Box::new(|| std::time::Duration::ZERO));

        Ok(vec![log_node])
    }
}
