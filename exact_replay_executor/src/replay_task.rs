//! Replay worker: manages persistent hydration publishers, capture
//! subscribers, and per-execution hydrate/run/compare logic.

use std::collections::HashMap;

use crate::error::ReplayError;
use crate::log_reader::ReplayExecution;
use logging::log_capture::PublisherCapture;
use task::callback::{CallbackNode, CallbackViews};
use task::channel_registry::ChannelRegistry;
use task::context::Context;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::loggable::ForwardedMessageContext;
use task::message::MessageHeader;

/// Policy for handling output mismatches (actual vs. expected).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DivergencePolicy {
    /// Stop replay on the first mismatch.
    Strict,
    /// Log the mismatch and continue with best-effort replay.
    BestEffort,
}

/// Either a plain type-erased deserializer or a forwarded-message deserializer
/// paired with its source-channel context. `hydrate_subscribers` picks one per
/// channel and calls [`deserialize`](Self::deserialize) uniformly for each
/// logged body.
enum DeserializerForHydration {
    Plain(task::channel_registry::DeserializerFn),
    Forwarded(
        task::channel_registry::ForwardedDeserializerFn,
        ForwardedMessageContext,
    ),
}

impl DeserializerForHydration {
    fn deserialize(
        &self,
        body: &[u8],
    ) -> Result<Box<dyn std::any::Any>, task::loggable::DeserializeError> {
        match self {
            DeserializerForHydration::Plain(deserializer) => deserializer(body),
            DeserializerForHydration::Forwarded(deserializer, context) => {
                deserializer(body, context)
            }
        }
    }
}

/// Per-node persistent state for replay: hydration publishers keyed by
/// subscriber ordinal, and capture subscribers keyed by publisher ordinal.
pub(crate) struct ReplayNodeState {
    /// Hydration publishers: one per subscriber ordinal that receives data.
    /// Created once, connected to the subscriber, kept alive for the entire
    /// replay. Maps subscriber ordinal -> (publisher, writer, serializer).
    pub(crate) hydration_publishers: HashMap<
        u16,
        (
            Box<dyn GenericPublisher>,
            task::channel_registry::ChannelPublisherWriter,
        ),
    >,
    /// Capture subscribers: one per publisher ordinal. Created once during
    /// the first execution, connected to the publisher, kept alive.
    /// All publishers get a capture subscriber so unexpected outputs are
    /// detected.
    pub(crate) capture_subscribers: HashMap<u16, PublisherCapture>,
    /// Whether capture subscribers have been initialised for this node.
    capture_subscribers_initialised: bool,
}

impl ReplayNodeState {
    pub fn new() -> Self {
        ReplayNodeState {
            hydration_publishers: HashMap::new(),
            capture_subscribers: HashMap::new(),
            capture_subscribers_initialised: false,
        }
    }
}

/// Replay a single execution step: hydrate subscribers, run the callback,
/// flush publishers, drain capture subscribers, and compare outputs.
pub(crate) fn replay_execution(
    node: &mut CallbackNode,
    state: &mut ReplayNodeState,
    execution: &ReplayExecution,
    registry: &ChannelRegistry,
    source_messages: &HashMap<task::pub_sub::ChannelName, Vec<(MessageHeader, Vec<u8>)>>,
    policy: DivergencePolicy,
    errors: &mut Vec<ReplayError>,
) {
    let node_name = node.name().to_owned();

    // ── Ensure capture subscribers exist for ALL publishers ───────────
    if !state.capture_subscribers_initialised {
        if let Err(e) = bind_capture_subscribers(node, state, registry, &node_name) {
            errors.push(e);
            if policy == DivergencePolicy::Strict {
                return;
            }
        }
        state.capture_subscribers_initialised = true;
    }

    // ── Hydrate subscribers ────────────────────────────────────────────
    if let Err(e) = hydrate_subscribers(
        node,
        state,
        &execution.received,
        registry,
        source_messages,
        &node_name,
    ) {
        errors.push(e);
        if policy == DivergencePolicy::Strict {
            return;
        }
    }

    // ── Run the callback ───────────────────────────────────────────────
    let ctx = Context::new(execution.execution_time);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        node.run(&ctx);
    }));
    if result.is_err() {
        errors.push(ReplayError::CallbackPanic {
            node: node_name.clone(),
        });
        return;
    }

    // ── Flush publishers (stamps headers, writes to capture subscribers) ─
    node.flush_publishers(execution.execution_time);

    // ── Drain ALL capture subscribers and compare outputs ──────────────
    for (ordinal, capture) in state.capture_subscribers.iter_mut() {
        let channel_name = capture.logger.channel_name.clone();
        let expected = execution.published.get(ordinal);

        let mut actual_outputs: Vec<(MessageHeader, Vec<u8>)> = Vec::new();
        if let Err(e) = capture.drain_to_vec(&mut actual_outputs) {
            errors.push(ReplayError::OutputMismatch {
                node: node_name.clone(),
                channel: channel_name.clone(),
                details: format!("failed to capture outputs: {e}"),
            });
            if policy == DivergencePolicy::Strict {
                return;
            }
            continue;
        }

        match expected {
            Some(expected_msgs) => {
                // Compare counts
                if actual_outputs.len() != expected_msgs.len() {
                    errors.push(ReplayError::OutputMismatch {
                        node: node_name.clone(),
                        channel: channel_name.clone(),
                        details: format!(
                            "expected {} output(s), got {}",
                            expected_msgs.len(),
                            actual_outputs.len()
                        ),
                    });
                    if policy == DivergencePolicy::Strict {
                        return;
                    }
                    continue;
                }

                // Compare each output header and body
                for (i, ((actual_header, actual_body), (expected_header, expected_body))) in
                    actual_outputs.iter().zip(expected_msgs.iter()).enumerate()
                {
                    if actual_header.published_at != expected_header.published_at {
                        errors.push(ReplayError::OutputMismatch {
                            node: node_name.clone(),
                            channel: channel_name.clone(),
                            details: format!(
                                "output {i}: header time mismatch: actual={:?}, expected={:?}",
                                actual_header.published_at, expected_header.published_at
                            ),
                        });
                        if policy == DivergencePolicy::Strict {
                            return;
                        }
                    }
                    if actual_body != expected_body {
                        errors.push(ReplayError::OutputMismatch {
                            node: node_name.clone(),
                            channel: channel_name.clone(),
                            details: format!("output {i}: body mismatch"),
                        });
                        if policy == DivergencePolicy::Strict {
                            return;
                        }
                    }
                }
            }
            None => {
                // No expected output for this ordinal — detect unexpected.
                if !actual_outputs.is_empty() {
                    errors.push(ReplayError::OutputMismatch {
                        node: node_name.clone(),
                        channel: channel_name.clone(),
                        details: format!(
                            "unexpected {} output(s) with no expected entry",
                            actual_outputs.len()
                        ),
                    });
                    if policy == DivergencePolicy::Strict {
                        return;
                    }
                }
            }
        }
    }
}

/// Create a `PublisherCapture` for every publisher on `node`. This must be
/// called before `node.run()` so the first execution's outputs are captured.
/// The capture subscriber is connected, the publisher's arena is re-allocated
/// to include the capture subscriber's footprint, and the pair is stored in
/// `state.capture_subscribers`.
///
/// SAFETY: This assumes the publisher's arena has NOT yet been allocated.
/// For replay graphs built from fresh `CallbackNode`s (no prior
/// `connect_callback_nodes`), this is the case.  If the arena has already
/// been allocated, `update_capacity` (called by `connect_to_subscriber`)
/// will re-allocate the Vec and destroy existing slots, which would break
/// existing connections.
fn bind_capture_subscribers(
    node: &mut CallbackNode,
    state: &mut ReplayNodeState,
    registry: &ChannelRegistry,
    node_name: &str,
) -> Result<(), ReplayError> {
    let mut pubs = node.callback_mut().collect_publishers_mut();
    for (ordinal, pub_mut) in pubs.iter_mut().enumerate() {
        let ordinal = ordinal as u16;
        let channel_name = pub_mut.config().channel_name.clone();
        let type_id = pub_mut.value_type_id();

        let Some(serializer) = registry.serializer_for(type_id) else {
            return Err(ReplayError::UnregisteredOutputCapture {
                channel: channel_name.clone(),
                node: node_name.to_owned(),
            });
        };

        let capture =
            PublisherCapture::connect_from(channel_name.clone(), serializer, &mut **pub_mut)
                .map_err(|_e| ReplayError::UnregisteredOutputCapture {
                    channel: channel_name.clone(),
                    node: node_name.to_owned(),
                })?;

        // After connecting the capture subscriber, the publisher's arena
        // capacity has been increased.  Re-allocate the arena so the new
        // slots are materialised.  This is safe only because the arena
        // was NOT already allocated (see SAFETY doc above).
        pub_mut.allocate_arena();

        state.capture_subscribers.insert(ordinal, capture);
    }
    Ok(())
}

/// Hydrate subscriber inputs from the replay execution's received messages.
/// Uses persistent hydration publishers created once during setup.
///
/// Clears all subscriber buffers **once** before hydrating any ordinal, so
/// earlier injections are not wiped by later ordinals.
///
/// `source_messages` supplies the ordinary-log payloads used to build a
/// [`ForwardedMessageContext`] for forwarded channels.
#[allow(clippy::too_many_arguments)]
fn hydrate_subscribers(
    node: &mut CallbackNode,
    state: &mut ReplayNodeState,
    received: &HashMap<u16, Vec<(MessageHeader, Vec<u8>)>>,
    registry: &ChannelRegistry,
    source_messages: &HashMap<task::pub_sub::ChannelName, Vec<(MessageHeader, Vec<u8>)>>,
    node_name: &str,
) -> Result<(), ReplayError> {
    // ── Clear all subscriber buffers once before any hydration ─────────
    {
        let mut cleanup = |s: &dyn GenericSubscriber| {
            s.cleanup_buffers();
        };
        node.callback().for_each_subscriber(&mut cleanup);
    }

    for (&ordinal, messages) in received {
        // Ensure a hydration publisher exists for this ordinal.
        if let std::collections::hash_map::Entry::Vacant(e) =
            state.hydration_publishers.entry(ordinal)
        {
            let mut subs = node.callback_mut().collect_subscribers_mut();
            let Some(subscriber) = subs.get_mut(ordinal as usize) else {
                return Err(ReplayError::InvalidSubscriberOrdinal {
                    node: node_name.to_owned(),
                    ordinal,
                    subscriber_count: subs.len(),
                });
            };

            let channel_name = subscriber.config().channel_name.clone();
            let type_id = registry.channel_type(&channel_name).ok_or_else(|| {
                ReplayError::UnregisteredChannel {
                    channel: channel_name.clone(),
                    node: node_name.to_owned(),
                }
            })?;

            let factory = registry.channel_publisher_factory(type_id).ok_or_else(|| {
                ReplayError::UnregisteredDeserializer {
                    channel: channel_name.clone(),
                    node: node_name.to_owned(),
                }
            })?;

            let (mut publisher, writer) = factory(channel_name.clone());
            // The registry factory uses capacity 1 because normal replay
            // playback publishes one value at a time. Exact replay must be
            // able to reconstruct a span, so size this hydration publisher
            // to the target subscriber's queue capacity before connecting it
            // and allocating its arena.
            publisher.config_mut().capacity = subscriber.config().capacity.max(1);
            publisher
                .connect_to_subscriber(&mut **subscriber)
                .map_err(|_| ReplayError::UnsupportedForwardedMessage {
                    channel: channel_name.clone(),
                    node: node_name.to_owned(),
                })?;
            publisher.allocate_arena();
            e.insert((publisher, writer));
        }

        let (publisher, writer) = state.hydration_publishers.get_mut(&ordinal).unwrap();

        // Look up the deserializer (lazily).
        let channel_name = publisher.config().channel_name.clone();
        let type_id = registry.channel_type(&channel_name).ok_or_else(|| {
            ReplayError::UnregisteredChannel {
                channel: channel_name.clone(),
                node: node_name.to_owned(),
            }
        })?;

        // Forwarded channels deserialize their logged body through a
        // `ForwardedMessageContext` built from the source channel's logged
        // payloads, so the forwarded payload can be resolved by header.
        let deserializer: DeserializerForHydration =
            if let Some(info) = registry.forwarded_channel_info(&channel_name) {
                let source_deserializer = registry
                    .deserializer_for(info.forwarded_data_type_id)
                    .ok_or_else(|| ReplayError::UnregisteredDeserializer {
                        channel: info.source_channel.clone(),
                        node: node_name.to_owned(),
                    })?;
                let mut source_values = Vec::new();
                if let Some(source_bodies) = source_messages.get(&info.source_channel) {
                    for (header, body) in source_bodies {
                        let value = source_deserializer(body).map_err(|e| {
                            ReplayError::DeserializationFailed {
                                channel: info.source_channel.clone(),
                                details: e.to_string(),
                            }
                        })?;
                        source_values.push((*header, value));
                    }
                }
                let context = ForwardedMessageContext::new(source_values);
                let forwarded = registry
                    .forwarded_deserializer_for(type_id)
                    .ok_or_else(|| ReplayError::UnregisteredDeserializer {
                        channel: channel_name.clone(),
                        node: node_name.to_owned(),
                    })?;
                DeserializerForHydration::Forwarded(forwarded, context)
            } else {
                DeserializerForHydration::Plain(registry.deserializer_for(type_id).ok_or_else(
                    || ReplayError::UnregisteredDeserializer {
                        channel: channel_name.clone(),
                        node: node_name.to_owned(),
                    },
                )?)
            };

        // Deserialize, write, and flush each message individually with
        // its original header timestamp.
        for (_header, body) in messages {
            let value =
                deserializer
                    .deserialize(body)
                    .map_err(|e| ReplayError::DeserializationFailed {
                        channel: channel_name.clone(),
                        details: e.to_string(),
                    })?;
            writer(publisher.as_mut(), value);
            publisher.flush_loaned_values(_header.published_at);
        }
    }

    // Drain subscribers (write → read) so the callback sees the data.
    node.drain_subscribers();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use task::callback::{Callback, PortMut, Run};
    use task::output::Output;
    use task::publisher::{Publisher, PublisherConfig};
    use task::subscriber::{Subscriber, SubscriberConfig};
    use task::time::FrameworkTime;

    use std::collections::HashMap;

    struct IdentityCallback {
        subscriber: Subscriber<u64>,
        publisher: Publisher<u64>,
    }

    impl Callback for IdentityCallback {
        fn run(&mut self, _ctx: &Context) -> Run {
            let input = task::input::OptionalInput::<u64>::new_downcasted(&mut self.subscriber);
            if let Some(val) = input.value() {
                let mut output = Output::<u64>::new_downcasted(&mut self.publisher);
                *output = *val;
                output.send();
            }
            Run::new(1)
        }

        fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
            f(&self.subscriber);
        }
        fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
            f(&self.publisher);
        }
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
            f(&mut self.subscriber);
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
            f(&mut self.publisher);
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            f(PortMut::Subscriber(&mut self.subscriber));
            f(PortMut::Publisher(&mut self.publisher));
        }
    }

    /// Verify that replay_execution correctly handles the case
    /// where hydration is not needed (no received messages).
    #[test]
    fn test_replay_without_hydration() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("output".into());

        let callback = IdentityCallback {
            subscriber: Subscriber::<u64>::new(SubscriberConfig {
                is_optional: true,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: "input".into(),
            }),
            publisher: Publisher::<u64>::new(PublisherConfig {
                capacity: 1,
                channel_name: "output".into(),
            }),
        };

        let mut node = CallbackNode::new_named(Box::new(callback), "Identity".into());

        let execution = ReplayExecution {
            callback_node_index: 0,
            execution_time: FrameworkTime::from_nanoseconds(100),
            execution_duration_ns: 0,
            received: HashMap::new(),
            published: HashMap::new(),
        };

        let mut state = ReplayNodeState::new();
        let mut errors = Vec::new();
        replay_execution(
            &mut node,
            &mut state,
            &execution,
            &registry,
            &HashMap::new(),
            DivergencePolicy::Strict,
            &mut errors,
        );
        assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
    }

    /// Test that capture subscribers are bound BEFORE run, so the first
    /// execution's outputs are captured and compared.  If capture setup
    /// happened after the flush, the first execution's outputs would be
    /// invisible and the expected-count mismatch would go undetected.
    #[test]
    fn test_capture_bound_before_run() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());
        registry.register_channel::<u64>("output".into());

        // A callback that publishes with no input — always produces one
        // output.
        struct AlwaysPublish {
            publisher: Publisher<u64>,
        }
        impl Callback for AlwaysPublish {
            fn run(&mut self, _ctx: &Context) -> Run {
                let mut output = Output::new_default(&mut self.publisher);
                *output = 7u64;
                output.send();
                Run::new(1)
            }
            fn for_each_subscriber<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {}
            fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
                f(&self.publisher);
            }
            fn for_each_subscriber_mut<'a>(
                &'a mut self,
                _f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
            ) {
            }
            fn for_each_publisher_mut<'a>(
                &'a mut self,
                f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
            ) {
                f(&mut self.publisher);
            }
            fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
                f(PortMut::Publisher(&mut self.publisher));
            }
        }

        let always = AlwaysPublish {
            publisher: Publisher::<u64>::new(PublisherConfig {
                capacity: 1,
                channel_name: "output".into(),
            }),
        };
        let mut node = CallbackNode::new_named(Box::new(always), "Always".into());

        // The execution log says there should be 1 output at time 100.
        let execution = ReplayExecution {
            callback_node_index: 0,
            execution_time: FrameworkTime::from_nanoseconds(100),
            execution_duration_ns: 0,
            received: HashMap::new(),
            published: {
                let mut m = HashMap::new();
                m.insert(
                    0u16,
                    vec![(
                        MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
                        serde_json::to_vec(&7u64).unwrap(),
                    )],
                );
                m
            },
        };

        let mut state = ReplayNodeState::new();
        let mut errors = Vec::new();
        replay_execution(
            &mut node,
            &mut state,
            &execution,
            &registry,
            &HashMap::new(),
            DivergencePolicy::Strict,
            &mut errors,
        );
        assert!(
            errors.is_empty(),
            "expected matching outputs, got: {errors:?}"
        );
    }

    /// Test that unexpected outputs (no expected entry for that ordinal)
    /// are detected.
    #[test]
    fn test_unexpected_output_detected() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("output".into());

        struct AlwaysPublish {
            publisher: Publisher<u64>,
        }
        impl Callback for AlwaysPublish {
            fn run(&mut self, _ctx: &Context) -> Run {
                let mut output = Output::new_default(&mut self.publisher);
                *output = 7u64;
                output.send();
                Run::new(1)
            }
            fn for_each_subscriber<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {}
            fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
                f(&self.publisher);
            }
            fn for_each_subscriber_mut<'a>(
                &'a mut self,
                _f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
            ) {
            }
            fn for_each_publisher_mut<'a>(
                &'a mut self,
                f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
            ) {
                f(&mut self.publisher);
            }
            fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
                f(PortMut::Publisher(&mut self.publisher));
            }
        }

        let always = AlwaysPublish {
            publisher: Publisher::<u64>::new(PublisherConfig {
                capacity: 1,
                channel_name: "output".into(),
            }),
        };
        let mut node = CallbackNode::new_named(Box::new(always), "Always".into());

        // No expected published messages — the callback still produces one.
        let execution = ReplayExecution {
            callback_node_index: 0,
            execution_time: FrameworkTime::from_nanoseconds(100),
            execution_duration_ns: 0,
            received: HashMap::new(),
            published: HashMap::new(),
        };

        let mut state = ReplayNodeState::new();
        let mut errors = Vec::new();
        replay_execution(
            &mut node,
            &mut state,
            &execution,
            &registry,
            &HashMap::new(),
            DivergencePolicy::Strict,
            &mut errors,
        );
        assert!(
            !errors.is_empty(),
            "expected an error for unexpected output"
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, ReplayError::OutputMismatch { .. })),
            "expected OutputMismatch, got: {errors:?}"
        );
    }

    /// Test output capture with Header comparison.
    #[test]
    fn test_output_header_capture() {
        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("output".into());

        let mut pub_ = Publisher::<u64>::new(PublisherConfig {
            capacity: 1,
            channel_name: "output".into(),
        });

        let serialize = registry
            .serializer_for(std::any::TypeId::of::<u64>())
            .expect("serializer");
        let mut capture =
            PublisherCapture::connect_from("output".to_string(), serialize, &mut pub_)
                .expect("connect_from");
        pub_.allocate_arena();

        let mut output = Output::new_default(&mut pub_);
        *output = 99u64;
        output.send();
        pub_.flush_loaned_values(FrameworkTime::from_nanoseconds(1000));

        let mut results = Vec::new();
        capture.drain_to_vec(&mut results).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].0.published_at,
            FrameworkTime::from_nanoseconds(1000)
        );
    }

    /// Test that two hydrated messages on a single subscriber preserve
    /// order and original header timestamps.
    ///
    /// Each message is deserialized, written via the ChannelPublisherWriter,
    /// and flushed individually with its original header timestamp. The
    /// subscriber has capacity 2 so both messages fit. After hydration,
    /// the subscriber's reader buffer contains both messages with their
    /// original timestamps in order.
    ///
    /// Note: ChannelPublisherWriter writes one value per call (the registry
    /// factory creates a Publisher with capacity 1, and each write+flush
    /// cycle completes before the next message begins). This is the
    /// current invariant.
    #[test]
    fn test_two_hydrated_messages_preserve_order_and_headers() {
        use std::sync::{Arc, Mutex};

        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("input".into());

        // Shared storage for received messages (Arc+ Mutex so the callback
        // can write into it and the test can read it after replay_execution).
        let received = Arc::new(Mutex::new(Vec::new()));

        struct RecordingCallback {
            subscriber: Subscriber<u64>,
            received: Arc<Mutex<Vec<(FrameworkTime, u64)>>>,
        }
        impl Callback for RecordingCallback {
            fn run(&mut self, _ctx: &Context) -> Run {
                let mut input = task::input::InputSpan::<u64>::new_downcasted(&mut self.subscriber);
                let mut records = self.received.lock().unwrap();
                for msg in input.inputs() {
                    records.push((msg.header.published_at, msg.message));
                }
                Run::new(1)
            }
            fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
                f(&self.subscriber);
            }
            fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
            fn for_each_subscriber_mut<'a>(
                &'a mut self,
                f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
            ) {
                f(&mut self.subscriber);
            }
            fn for_each_publisher_mut<'a>(
                &'a mut self,
                _f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
            ) {
            }
            fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
                f(PortMut::Subscriber(&mut self.subscriber));
            }
        }

        let callback = RecordingCallback {
            subscriber: Subscriber::<u64>::new(SubscriberConfig {
                is_optional: true,
                capacity: 2,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: "input".into(),
            }),
            received: received.clone(),
        };

        let mut node = CallbackNode::new_named(Box::new(callback), "Recorder".into());

        // Two received messages with different timestamps.
        let execution = ReplayExecution {
            callback_node_index: 0,
            execution_time: FrameworkTime::from_nanoseconds(100),
            execution_duration_ns: 0,
            received: {
                let mut m = HashMap::new();
                m.insert(
                    0u16,
                    vec![
                        (
                            MessageHeader::new(FrameworkTime::from_nanoseconds(10)),
                            serde_json::to_vec(&42u64).unwrap(),
                        ),
                        (
                            MessageHeader::new(FrameworkTime::from_nanoseconds(20)),
                            serde_json::to_vec(&99u64).unwrap(),
                        ),
                    ],
                );
                m
            },
            published: HashMap::new(),
        };

        let mut state = ReplayNodeState::new();
        let mut errors = Vec::new();
        replay_execution(
            &mut node,
            &mut state,
            &execution,
            &registry,
            &HashMap::new(),
            DivergencePolicy::Strict,
            &mut errors,
        );
        assert!(errors.is_empty(), "expected no errors, got: {errors:?}");

        // Verify the callback received both values in order with correct
        // header timestamps (matching the original hydrated message timestamps).
        let records = received.lock().unwrap();
        assert_eq!(records.len(), 2, "should have received 2 messages");

        // First message: value 42, timestamp 10
        assert_eq!(
            records[0],
            (FrameworkTime::from_nanoseconds(10), 42),
            "first message: timestamp=10, value=42"
        );

        // Second message: value 99, timestamp 20
        assert_eq!(
            records[1],
            (FrameworkTime::from_nanoseconds(20), 99),
            "second message: timestamp=20, value=99"
        );

        // The node owns subscriber buffers containing pointers into the
        // persistent hydration publisher. Drop it first so those pointers
        // are released before the replay state drops the publisher arena.
        drop(node);
    }
}
