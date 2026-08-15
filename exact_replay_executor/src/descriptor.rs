//! Descriptor and descriptor-less execution validation.
//!
//! Validates that the execution log descriptor matches the supplied callback
//! nodes, and that every channel referenced by an **actively replayed** port
//! has the required registrations in the [`ChannelRegistry`].

use std::collections::{HashMap, HashSet};

use task::callback_storage::CallbackStorage;
use task::channel_registry::ChannelRegistry;
use task::time::FrameworkTime;

use crate::error::ReplayError;
use crate::log_reader::ReplayLog;

/// Validate the parsed `ReplayLog` against the rebuilt callback nodes:
///
/// - **Eagerly** (for every descriptor entry): check that every node/port/
///   channel referenced in the descriptor exists in the nodes and that the
///   channel names match the node's ports.
/// - **Lazily** (only for ports that actually appear in a parsed
///   [`ReplayExecution`]): require registry capabilities — a deserializer and
///   publisher factory for subscriber ports, a serializer for publisher ports.
///
/// Descriptor-only channels that are never referenced by a parsed replay
/// execution may remain unregistered (lazy). Actively replayed ports must be
/// registered — the replay needs to deserialize incoming messages (subscriber)
/// or capture outgoing messages (publisher) for them.
///
/// [`ReplayExecution`]: crate::log_reader::ReplayExecution
pub(crate) fn validate_descriptor(
    replay_log: &ReplayLog,
    nodes: &CallbackStorage,
    registry: &ChannelRegistry,
) -> Result<(), ReplayError> {
    use task::callback::CallbackViews;

    let descriptor = &replay_log.descriptor;

    // Active ports per node: the union of all subscriber/publisher ordinals
    // that appear in any parsed execution for that node.
    let mut active_received: HashMap<usize, HashSet<u16>> = HashMap::new();
    let mut active_published: HashMap<usize, HashSet<u16>> = HashMap::new();
    for execution in &replay_log.executions {
        active_received
            .entry(execution.callback_node_index)
            .or_default()
            .extend(execution.received.keys().copied());
        active_published
            .entry(execution.callback_node_index)
            .or_default()
            .extend(execution.published.keys().copied());
    }

    for (&node_idx, cd) in &descriptor.index_to_callbacks {
        // Check node index is valid.
        if node_idx >= nodes.len() {
            return Err(ReplayError::InvalidCallbackNodeIndex {
                index: node_idx,
                node_count: nodes.len(),
            });
        }
        // Build time / validation runs on the main thread before the replay
        // thread starts, so exclusive access cannot conflict.
        nodes[node_idx].access(|node| {
            let node_name = node.name().to_owned();

            let active_subs = active_received.get(&node_idx);
            let active_pubs = active_published.get(&node_idx);

            // Check subscriber ordinals and channel registrations.
            let subs = node.callback().collect_subscribers();
            for (&ordinal, desc_ch) in &cd.subscriber_index_to_channel_name {
                if ordinal >= subs.len() {
                    return Err(ReplayError::InvalidSubscriberOrdinal {
                        node: node_name.clone(),
                        ordinal: ordinal as u16,
                        subscriber_count: subs.len(),
                    });
                }
                let actual_ch = &subs[ordinal].config().channel_name;
                if desc_ch != actual_ch {
                    return Err(ReplayError::OutputMismatch {
                        node: node_name.clone(),
                        channel: desc_ch.clone(),
                        details: format!(
                            "descriptor subscriber ordinal {ordinal} channel '{desc_ch}' \
                             does not match node channel '{actual_ch}'"
                        ),
                    });
                }
                // Only actively replayed subscriber channels MUST be registered.
                let is_active = active_subs
                    .map(|set| set.contains(&(ordinal as u16)))
                    .unwrap_or(false);
                if is_active {
                    let type_id = registry.channel_type(desc_ch).ok_or_else(|| {
                        ReplayError::UnregisteredChannel {
                            channel: desc_ch.clone(),
                            node: node_name.clone(),
                        }
                    })?;
                    // Forwarded channels resolve their deserializer from the
                    // forwarded-deserializer map; plain channels use the regular one.
                    let has_deserializer = if registry.forwarded_channel_info(desc_ch).is_some() {
                        registry.forwarded_deserializer_for(type_id).is_some()
                    } else {
                        registry.deserializer_for(type_id).is_some()
                    };
                    if !has_deserializer {
                        return Err(ReplayError::UnregisteredDeserializer {
                            channel: desc_ch.clone(),
                            node: node_name.clone(),
                        });
                    }
                    if registry.channel_publisher_factory(type_id).is_none() {
                        return Err(ReplayError::UnregisteredDeserializer {
                            channel: desc_ch.clone(),
                            node: node_name.clone(),
                        });
                    }
                }
            }

            // Check publisher ordinals and channel registrations.
            let pubs = node.callback().collect_publishers();
            for (&ordinal, desc_ch) in &cd.publisher_index_to_channel_name {
                if ordinal >= pubs.len() {
                    return Err(ReplayError::InvalidPublisherOrdinal {
                        node: node_name.clone(),
                        ordinal: ordinal as u16,
                        publisher_count: pubs.len(),
                    });
                }
                let actual_ch = &pubs[ordinal].config().channel_name;
                if desc_ch != actual_ch {
                    return Err(ReplayError::OutputMismatch {
                        node: node_name.clone(),
                        channel: desc_ch.clone(),
                        details: format!(
                            "descriptor publisher ordinal {ordinal} channel '{desc_ch}' \
                             does not match node channel '{actual_ch}'"
                        ),
                    });
                }
                // Only actively replayed publisher channels MUST be registered
                // with a serializer (needed for output capture).
                let is_active = active_pubs
                    .map(|set| set.contains(&(ordinal as u16)))
                    .unwrap_or(false);
                if is_active {
                    let type_id = registry.channel_type(desc_ch).ok_or_else(|| {
                        ReplayError::UnregisteredChannel {
                            channel: desc_ch.clone(),
                            node: node_name.clone(),
                        }
                    })?;
                    if registry.serializer_for(type_id).is_none() {
                        return Err(ReplayError::UnregisteredOutputCapture {
                            channel: desc_ch.clone(),
                            node: node_name.clone(),
                        });
                    }
                }
            }

            Ok(())
        })?;
    }

    Ok(())
}

/// Validate descriptor-less execution records.
///
/// Descriptor-less executions come from infrastructure nodes that were added
/// by build steps *after* the descriptor was generated (e.g. `LogTask`). In
/// the original graph those nodes are appended after the application nodes,
/// so their indices may be at or beyond `nodes.len()` in the replay graph
/// (which omits them). Such out-of-range records are treated as
/// infrastructure and skipped.
///
/// An in-range descriptor-less execution is only valid for an explicit
/// infrastructure node (`LogTask`); anything else indicates a graph mismatch.
pub(crate) fn validate_descriptor_less_executions(
    descriptor_less: &[(usize, FrameworkTime)],
    nodes: &CallbackStorage,
) -> Result<(), ReplayError> {
    for (node_idx, _time) in descriptor_less {
        if *node_idx >= nodes.len() {
            // Appended infrastructure nodes are not part of the replay graph.
            continue;
        }
        let node_name = nodes[*node_idx].access(|n| n.name().to_owned());
        if !node_name.starts_with("LogTask") {
            return Err(ReplayError::DescriptorlessApplicationNode {
                index: *node_idx,
                node_name,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use task::callback::{Callback, CallbackNode, PortMut, Run};
    use task::context::Context;
    use task::execution_log::ExecutionLogDescriptor;
    use task::generic_publisher::GenericPublisher;
    use task::generic_subscriber::GenericSubscriber;
    use task::publisher::{Publisher, PublisherConfig};
    use task::subscriber::{Subscriber, SubscriberConfig};

    use crate::log_reader::{ReplayExecution, ReplayLog};

    /// A simple passthrough callback for testing.
    struct PassthroughCallback {
        sub: Subscriber<u64>,
        pub_: Publisher<u64>,
    }

    impl Callback for PassthroughCallback {
        fn run(&mut self, _ctx: &Context) -> Run {
            Run::new(1)
        }
        fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
            f(&self.sub);
        }
        fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
            f(&self.pub_);
        }
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
            f(&mut self.sub);
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
            f(&mut self.pub_);
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            f(PortMut::Subscriber(&mut self.sub));
            f(PortMut::Publisher(&mut self.pub_));
        }
    }

    fn make_passthrough_node(name: &str) -> CallbackNode {
        CallbackNode::new_named(
            Box::new(PassthroughCallback {
                sub: Subscriber::<u64>::new(SubscriberConfig {
                    is_optional: true,
                    capacity: 1,
                    is_trigger: true,
                    keep_across_runs: true,
                    channel_name: "input".into(),
                }),
                pub_: Publisher::<u64>::new(PublisherConfig {
                    capacity: 1,
                    channel_name: "output".into(),
                }),
            }),
            name.into(),
        )
    }

    /// Build a `ReplayLog` with the given descriptor and executions.
    fn make_replay_log(
        desc: ExecutionLogDescriptor,
        executions: Vec<ReplayExecution>,
    ) -> ReplayLog {
        ReplayLog {
            descriptor: desc,
            executions,
            descriptor_less_executions: Vec::new(),
            source_messages: HashMap::new(),
        }
    }

    fn make_execution(node: usize, received: Vec<u16>, published: Vec<u16>) -> ReplayExecution {
        ReplayExecution {
            callback_node_index: node,
            execution_time: FrameworkTime::from_nanoseconds(100),
            execution_duration_ns: 0,
            received: received.into_iter().map(|o| (o, Vec::new())).collect(),
            published: published.into_iter().map(|o| (o, Vec::new())).collect(),
        }
    }

    /// Descriptor with subscriber ordinal 0 → "input" and publisher ordinal
    /// 0 → "output".
    fn make_descriptor() -> ExecutionLogDescriptor {
        let mut desc = ExecutionLogDescriptor::new(&[]);
        let mut sub_map = HashMap::new();
        sub_map.insert(0usize, "input".to_string());
        let mut pub_map = HashMap::new();
        pub_map.insert(0usize, "output".to_string());
        desc.index_to_callbacks.insert(
            0usize,
            task::execution_log::CallbackDescriptor {
                subscriber_index_to_channel_name: sub_map,
                publisher_index_to_channel_name: pub_map,
            },
        );
        desc
    }

    #[test]
    fn valid_descriptor_passes() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());
        registry.register_channel::<u64>("output".into());

        let desc = make_descriptor();
        let log = make_replay_log(desc, vec![make_execution(0, vec![0], vec![0])]);
        let node = make_passthrough_node("Test");
        let result = validate_descriptor(&log, &CallbackStorage::from_nodes(vec![node]), &registry);
        assert!(result.is_ok(), "valid descriptor should pass: {:?}", result);
    }

    #[test]
    fn descriptor_only_port_may_be_unregistered() {
        // No channels registered at all. The descriptor references "input"
        // and "output", but NO execution replays any port, so the channels
        // may remain lazy/unregistered.
        let registry = ChannelRegistry::new();
        let desc = make_descriptor();
        let log = make_replay_log(desc, Vec::new());
        let node = make_passthrough_node("Lazy");
        let result = validate_descriptor(&log, &CallbackStorage::from_nodes(vec![node]), &registry);
        assert!(
            result.is_ok(),
            "descriptor-only ports may remain unregistered: {:?}",
            result
        );
    }

    #[test]
    fn descriptor_only_publisher_may_be_unregistered() {
        // Only the subscriber port is actively replayed. The publisher port
        // is referenced by the descriptor but never replayed, so "output"
        // may remain unregistered.
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());
        let desc = make_descriptor();
        let log = make_replay_log(desc, vec![make_execution(0, vec![0], vec![])]);
        let node = make_passthrough_node("LazyPub");
        let result = validate_descriptor(&log, &CallbackStorage::from_nodes(vec![node]), &registry);
        assert!(
            result.is_ok(),
            "descriptor-only publisher may remain unregistered: {:?}",
            result
        );
    }

    #[test]
    fn descriptor_only_subscriber_may_be_unregistered() {
        // Only the publisher port is actively replayed. The subscriber port
        // is referenced by the descriptor but never replayed, so "input"
        // may remain unregistered.
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("output".into());
        let desc = make_descriptor();
        let log = make_replay_log(desc, vec![make_execution(0, vec![], vec![0])]);
        let node = make_passthrough_node("LazySub");
        let result = validate_descriptor(&log, &CallbackStorage::from_nodes(vec![node]), &registry);
        assert!(
            result.is_ok(),
            "descriptor-only subscriber may remain unregistered: {:?}",
            result
        );
    }

    #[test]
    fn actively_replayed_unregistered_channel_is_rejected() {
        let registry = ChannelRegistry::new();
        // registry does NOT have "input" registered, but an execution
        // replays subscriber ordinal 0.

        let desc = make_descriptor();
        let log = make_replay_log(desc, vec![make_execution(0, vec![0], vec![])]);
        let node = make_passthrough_node("Test");
        let result = validate_descriptor(&log, &CallbackStorage::from_nodes(vec![node]), &registry);
        assert!(
            result.is_err(),
            "actively replayed unregistered channel should be rejected: {:?}",
            result
        );
        assert!(matches!(
            result.unwrap_err(),
            ReplayError::UnregisteredChannel { .. }
        ));
    }

    #[test]
    fn actively_replayed_unregistered_publisher_is_rejected() {
        let registry = ChannelRegistry::new();
        // registry does NOT have "output" registered, but an execution
        // replays publisher ordinal 0.

        let desc = make_descriptor();
        let log = make_replay_log(desc, vec![make_execution(0, vec![], vec![0])]);
        let node = make_passthrough_node("Test");
        let result = validate_descriptor(&log, &CallbackStorage::from_nodes(vec![node]), &registry);
        assert!(
            result.is_err(),
            "actively replayed unregistered publisher should be rejected: {:?}",
            result
        );
        assert!(matches!(
            result.unwrap_err(),
            ReplayError::UnregisteredChannel { .. }
        ));
    }

    #[test]
    fn descriptor_node_ordinal_channel_mismatch_still_eager() {
        // Even with zero executions, descriptor node/ordinal/channel shape
        // mismatches are still validated eagerly.
        let registry = ChannelRegistry::new();
        let mut desc = make_descriptor();
        // Subscriber channel name does not match the node's "input" channel.
        desc.index_to_callbacks
            .get_mut(&0)
            .unwrap()
            .subscriber_index_to_channel_name =
            HashMap::from([(0usize, "wrong_input".to_string())]);
        let log = make_replay_log(desc, Vec::new());
        let node = make_passthrough_node("Mismatch");
        let result = validate_descriptor(&log, &CallbackStorage::from_nodes(vec![node]), &registry);
        assert!(
            result.is_err(),
            "descriptor channel mismatch must be eager: {:?}",
            result
        );
        assert!(matches!(
            result.unwrap_err(),
            ReplayError::OutputMismatch { .. }
        ));
    }

    #[test]
    fn descriptor_less_logtask_passes() {
        let nodes = vec![
            make_passthrough_node("App"),
            CallbackNode::new_named(
                Box::new(PassthroughCallback {
                    sub: Subscriber::<u64>::new(SubscriberConfig {
                        is_optional: true,
                        capacity: 1,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: "log".into(),
                    }),
                    pub_: Publisher::<u64>::new(PublisherConfig {
                        capacity: 1,
                        channel_name: "log_out".into(),
                    }),
                }),
                "LogTask_0".into(),
            ),
        ];
        let desc_less = vec![(1usize, FrameworkTime::from_nanoseconds(100))];
        let result =
            validate_descriptor_less_executions(&desc_less, &CallbackStorage::from_nodes(nodes));
        assert!(result.is_ok());
    }

    #[test]
    fn descriptor_less_application_node_rejected() {
        let nodes = vec![make_passthrough_node("App")];
        let desc_less = vec![(0usize, FrameworkTime::from_nanoseconds(100))];
        let result =
            validate_descriptor_less_executions(&desc_less, &CallbackStorage::from_nodes(nodes));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ReplayError::DescriptorlessApplicationNode { .. }
        ));
    }

    #[test]
    fn descriptor_less_out_of_range_infrastructure_is_skipped() {
        // LogTask nodes are appended after the application nodes by build
        // steps, so their execution records carry indices at or beyond
        // `nodes.len()`. The replay graph omits them; these records should
        // be treated as infrastructure and skipped, not rejected.
        let nodes = vec![make_passthrough_node("App")];
        let desc_less = vec![
            (3usize, FrameworkTime::from_nanoseconds(100)),
            (7usize, FrameworkTime::from_nanoseconds(200)),
        ];
        let result =
            validate_descriptor_less_executions(&desc_less, &CallbackStorage::from_nodes(nodes));
        assert!(result.is_ok());
    }
}
