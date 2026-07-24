use crate::executor::CallbackNodeEnqueuer;
use crate::generic_publisher::GenericPublisher;
use crate::generic_subscriber::GenericSubscriber;
use crate::pub_sub::{CallbackNodeName, ChannelName};
use crate::publisher::PublisherConfig;
use crate::subscriber::SubscriberConfig;
use crate::time::FrameworkTime;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum InputKind {
    Required,
    Optional,
    Span,
}

impl From<InputKind> for SubscriberConfig {
    fn from(val: InputKind) -> Self {
        match val {
            InputKind::Required => SubscriberConfig {
                is_optional: false,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                // TODO, dont default this
                channel_name: "".into(),
            },
            InputKind::Optional => SubscriberConfig {
                is_optional: true,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                // TODO, dont default this
                channel_name: "".into(),
            },
            InputKind::Span => SubscriberConfig {
                is_optional: true,
                capacity: 4, // TODO dont default this
                is_trigger: true,
                keep_across_runs: true,
                // TODO, dont default this
                channel_name: "".into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputKind {
    Default,
    Span,
}
impl From<OutputKind> for PublisherConfig {
    fn from(_val: OutputKind) -> Self {
        PublisherConfig {
            capacity: 1,
            // TODO, dont default this
            channel_name: "".into(),
        }
    }
}

pub struct CallbackSignature {
    pub inputs: Vec<InputKind>,
    pub outputs: Vec<OutputKind>,
}

#[derive(Debug)]
pub struct Run {
    pub num_iterations: usize,
}

impl Run {
    pub fn new(num_iterations: usize) -> Run {
        Run { num_iterations }
    }
}

pub trait Callback {
    // Generic interface for calling the callback. Used by the framework to trigger things
    // Can provide information about inputs/outputs per index
    // - what inputs request execution
    // - input queue capacity
    // returns number of times the callback node was run
    fn run_generic(
        &mut self,
        subscribers: &mut [Box<dyn GenericSubscriber>],
        publishers: &mut [Box<dyn GenericPublisher>],
        ctx: &crate::context::Context,
    ) -> Run;

    /// Builds subscribers with some default configuration values that are appropriate for the type (e.g. RequiredInput).
    fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>>;

    /// Builds publisher with some default configuration values that are appropriate for the type (e.g. OutputSpan).
    fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>>;

    fn able_to_run(&self, inputs: &[Box<dyn GenericSubscriber>]) -> bool {
        inputs.iter().all(|input| input.able_to_run())
    }

    fn requests_execution(&self, inputs: &[Box<dyn GenericSubscriber>]) -> bool {
        inputs.iter().any(|input| input.requests_execution())
    }
}

#[derive(Debug)]
pub struct MismatchTypeError {
    channel_name: ChannelName,
    publisher_callback_node: CallbackNodeName,
    subscriber_callback_node: CallbackNodeName,
}

impl std::fmt::Display for MismatchTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Callback node '{}' publishes on '{}' but subscriber callback node '{}' has a different type for that channel",
            self.publisher_callback_node, self.channel_name, self.subscriber_callback_node
        )
    }
}

impl std::error::Error for MismatchTypeError {}

/// Returns a mapping of the forwarded channel to the depth of subscriber queues listening to it
fn find_forwarded_channel_usage(callbacks: &[CallbackNode]) -> HashMap<ChannelName, usize> {
    let mut channel_to_usage = HashMap::<ChannelName, usize>::new();

    // Find all channels that are forwarded
    for callback in callbacks.iter() {
        for publisher in callback.publishers.iter() {
            for forwarded_channel in publisher.forwarded_channels() {
                channel_to_usage.insert(forwarded_channel.clone(), 0);
            }
        }
    }

    // Find all subscribers of the forwarded channel set and bump the usage
    // accordingly. Each subscriber contributes its `arena_footprint()`
    // (write-queue + read-buffer slots) — see Publisher::add_typed_subscriber
    // for the underlying sizing rationale.
    for callback in callbacks.iter() {
        for subscriber in callback.subscribers.iter() {
            let subscriber_channel_name = &subscriber.config().channel_name;
            match channel_to_usage.get_mut(subscriber_channel_name) {
                Some(usage) => *usage += subscriber.config().arena_footprint(),
                None => {
                    // Subscriber doesn't use this channel
                }
            };
        }
    }

    channel_to_usage
}

/// Connects publishers to subscribers and sizes arenas accordingly
pub fn connect_callback_nodes(callbacks: &mut [CallbackNode]) -> Result<(), MismatchTypeError> {
    let forwarded_channel_to_usage = find_forwarded_channel_usage(callbacks);

    // Connect publishers to everyone who is subscribing to their output
    for callback_idx in 0..callbacks.len() {
        for other_callback_idx in 0..callbacks.len() {
            let callback_name = callbacks[callback_idx].name().to_string();
            let other_callback_name = callbacks[other_callback_idx].name().to_string();

            for publisher in callbacks[callback_idx].publishers.iter_mut() {
                // Find subscribers to this publisher
                for other_callback_subscriber in
                    callbacks[other_callback_idx].subscribers.iter_mut()
                {
                    if publisher.config().channel_name
                        == other_callback_subscriber.config().channel_name
                    {
                        println!(
                            "Connecting callback node '{}' to callback node '{}' on channel '{}'",
                            callback_name,
                            other_callback_name,
                            publisher.config().channel_name
                        );
                        if let Err(_e) =
                            publisher.connect_to_subscriber(other_callback_subscriber.as_mut())
                        {
                            return Err(MismatchTypeError {
                                channel_name: publisher.config().channel_name.clone(),
                                publisher_callback_node: callbacks[callback_idx].name().into(),
                                subscriber_callback_node: callbacks[other_callback_idx]
                                    .name()
                                    .into(),
                            });
                        }
                    }
                }

                // This callback node has its channel forwarded to a bunch of subscriber slots, so we must bump its size accordingly
                match forwarded_channel_to_usage.get(&publisher.config().channel_name) {
                    Some(usage) => {
                        publisher.increase_arena_size(*usage);
                    }
                    None => {
                        // Channel not forwaded anywhere
                    }
                }
            }
        }
    }

    // Allocate all arenas for all publishers
    for callback in callbacks.iter_mut() {
        for publisher in callback.publishers.iter_mut() {
            publisher.allocate_arena();
        }
    }

    Ok(())
}

/// Tracks readiness of the gating (required / non-optional) subscribers in a
/// CallbackNode via an atomic bitmask. Each gating subscriber's bit is 0 (not
/// ready) or 1 (ready); optional subscribers and unused high bits are always 1.
/// When the bitmask reaches usize::MAX, every required input has data and the
/// callback node can be enqueued. A trigger input's bit means "new event
/// pending" and is cleared when the node drains it; a non-trigger input's bit
/// means "has a value" and stays set while its read buffer retains one.
pub struct CallbackNodeReadiness {
    bitmask: AtomicUsize,
    node_index: OnceLock<usize>,
    enqueuer: OnceLock<Arc<dyn CallbackNodeEnqueuer>>,
}

impl CallbackNodeReadiness {
    fn new(initial_bitmask: usize) -> Arc<Self> {
        Arc::new(CallbackNodeReadiness {
            bitmask: AtomicUsize::new(initial_bitmask),
            node_index: OnceLock::new(),
            enqueuer: OnceLock::new(),
        })
    }

    /// Set bit `index`, then enqueue the callback node if this was the transition to usize::MAX.
    /// Only enqueues when the bitmask was not already MAX before this call.
    pub fn set_bit(&self, index: usize) {
        let bit = 1usize << index;
        let prev = self.bitmask.fetch_or(bit, Ordering::AcqRel);
        // Only enqueue on the transition: previous was not MAX but now is
        if prev != usize::MAX
            && prev | bit == usize::MAX
            && let (Some(enqueuer), Some(&node_index)) =
                (self.enqueuer.get(), self.node_index.get())
        {
            enqueuer.enqueue_node(node_index);
        }
    }

    /// Clear bit `index` (called when the callback node drains write→read for this subscriber).
    pub fn clear_bit(&self, index: usize) {
        let mask = !(1usize << index);
        self.bitmask.fetch_and(mask, Ordering::AcqRel);
    }

    /// Register the executor enqueuer and node index. If the bitmask is already MAX
    /// (e.g., startup with pre-loaded data), immediately enqueue.
    pub fn register(&self, node_index: usize, enqueuer: Arc<dyn CallbackNodeEnqueuer>) {
        let _ = self.node_index.set(node_index);
        let _ = self.enqueuer.set(enqueuer);
        // Startup case: if already ready, enqueue now
        self.enqueue_if_ready();
    }

    /// Enqueue the node if every required input currently has a value
    /// (bitmask == usize::MAX). Called when an optional+trigger subscriber
    /// receives data: the data is a trigger, but the node may only run once
    /// its required inputs are ready. The executor dedups duplicate enqueues.
    pub fn enqueue_if_ready(&self) {
        if self.bitmask.load(Ordering::Acquire) == usize::MAX
            && let (Some(enqueuer), Some(&idx)) = (self.enqueuer.get(), self.node_index.get())
        {
            enqueuer.enqueue_node(idx);
        }
    }
}

/// How a subscriber participates in its node's readiness tracking.
#[derive(Clone)]
pub enum SubscriberReadiness {
    /// Gating (required / non-optional) input: owns the bit at the given
    /// index. Data arrival sets the bit; the node enqueues when all gating
    /// bits are set.
    Gating(Arc<CallbackNodeReadiness>, usize),
    /// Optional trigger input: owns no bit (it never gates). Data arrival
    /// enqueues the node if all required inputs are ready.
    OptionalTrigger(Arc<CallbackNodeReadiness>),
}

/// Whether a subscriber gates its node's data-triggered execution: every
/// required (non-optional) input must have data before the node is enqueued.
/// Trigger and non-trigger required inputs differ only in what their bit
/// *means* — a trigger bit is an event consumed by the next run, a
/// non-trigger bit is a retained "has a value" gate — see
/// `Subscriber::drain_writer_to_reader`. These are exactly the subscribers
/// publishers track readiness for — see `Publisher::add_typed_subscriber`.
fn is_gating(subscriber: &dyn GenericSubscriber) -> bool {
    !subscriber.config().is_optional
}

/// Compute the initial bitmask for a set of subscribers.
/// Only gating (required / non-optional) subscribers consume bits, packed
/// densely from bit 0; their bits start at 0 (must receive data). All other
/// bits start at 1.
fn starting_subscriber_bitmask(subscribers: &[Box<dyn GenericSubscriber>]) -> usize {
    const MAX_GATING_SUBSCRIBER_COUNT: usize = std::mem::size_of::<usize>() * 8;
    let gating_count = subscribers.iter().filter(|s| is_gating(s.as_ref())).count();
    if gating_count > MAX_GATING_SUBSCRIBER_COUNT {
        // 64 isn't much for most use-cases, but we may have some diagnostic or logging callbacks that want more.
        // We could either have some non-triggering subscribers (so, poll only) or have those callbacks decompose themselves into smaller
        // callbacks that publish intermediate results.
        panic!(
            "We cannot support callbacks with more than {} required (non-optional) subscribers, try splitting out your callback into multiple callbacks.",
            MAX_GATING_SUBSCRIBER_COUNT
        )
    }

    let mut bitmask = usize::MAX;
    for bit_index in 0..gating_count {
        // Clear this bit — subscriber must receive data before triggering
        bitmask &= !(1usize << bit_index);
    }
    bitmask
}

pub struct CallbackNode {
    subscribers: Vec<Box<dyn GenericSubscriber>>,
    publishers: Vec<Box<dyn GenericPublisher>>,
    callback: Box<dyn Callback>,

    // Ideally this would be apart of Callback, but I dont have a good way to store it
    next_execution_time_callback: Box<dyn Fn(FrameworkTime) -> Option<FrameworkTime>>,
    execution_duration_callback: Option<Box<dyn Fn() -> Duration>>,

    name: CallbackNodeName,

    readiness: Arc<CallbackNodeReadiness>,

    /// Whether an executor should record this node's executions in its
    /// execution log. Default off; each executor decides whether it honors
    /// this (and whether it was configured with execution logging at all).
    log_executions: bool,
}

impl std::fmt::Debug for CallbackNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackNode")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// SAFETY: Callbacks may run on any thread, users cannot make thread assumptions
unsafe impl Sync for CallbackNode {}
/// SAFETY: Callbacks may run on any thread, users cannot make thread assumptions
unsafe impl Send for CallbackNode {}

impl CallbackNode {
    pub fn new_named(callback: Box<dyn Callback>, name: CallbackNodeName) -> Self {
        let subscribers = callback.build_subscribers();
        let publishers = callback.build_publishers();
        CallbackNode::new_with(callback, subscribers, publishers, name)
    }

    pub fn new_with(
        callback: Box<dyn Callback>,
        mut subscribers: Vec<Box<dyn GenericSubscriber>>,
        publishers: Vec<Box<dyn GenericPublisher>>,
        name: CallbackNodeName,
    ) -> Self {
        let initial_bitmask = starting_subscriber_bitmask(&subscribers);
        let readiness = CallbackNodeReadiness::new(initial_bitmask);

        // Inject readiness state into each subscriber that participates in
        // data-triggered scheduling: gating (required) subscribers get a
        // densely-packed bit; optional+trigger subscribers get a
        // bit-less OptionalTrigger handle so their data arrival can enqueue
        // the node whenever the required inputs are ready. Optional
        // non-trigger subscribers get nothing — they never gate or trigger.
        let mut bit_index = 0;
        for subscriber in subscribers.iter_mut() {
            if is_gating(subscriber.as_ref()) {
                subscriber
                    .set_readiness_state(SubscriberReadiness::Gating(readiness.clone(), bit_index));
                bit_index += 1;
            } else if subscriber.config().is_trigger {
                subscriber
                    .set_readiness_state(SubscriberReadiness::OptionalTrigger(readiness.clone()));
            }
        }

        CallbackNode {
            subscribers,
            publishers,
            callback,
            next_execution_time_callback: Box::new(|_| None),
            execution_duration_callback: None,
            name,
            readiness,
            log_executions: false,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_execution_duration_callback(&mut self, callback: Box<dyn Fn() -> Duration>) {
        self.execution_duration_callback = Some(callback);
    }

    pub fn execution_duration(&self) -> Duration {
        (self
            .execution_duration_callback
            .as_ref()
            .expect("execution_duration_callback not set on this CallbackNode"))()
    }

    pub fn set_execution_time_callback(
        &mut self,
        callback: Box<dyn Fn(FrameworkTime) -> Option<FrameworkTime>>,
    ) {
        self.next_execution_time_callback = callback;
    }

    /// The next requested execution time relevant to a current execution time.
    /// 'Instant' is assumed to be provided via a monotonic clock as per rust docs.
    pub fn next_requested_execution_time(&self, now: FrameworkTime) -> Option<FrameworkTime> {
        (self.next_execution_time_callback)(now)
    }

    pub fn drain_subscribers(&mut self) {
        for subscriber in self.subscribers.iter_mut() {
            subscriber.drain_writer_to_reader();
        }
    }

    pub fn flush_publishers(&mut self, timestamp: FrameworkTime) {
        for publisher in self.publishers.iter_mut() {
            publisher.flush_loaned_values(timestamp);
        }
    }

    /// Flush publishers, invoking `hook` with each published message's header
    /// (per publisher, in `publishers()` order). Used by executors that record
    /// published headers in their execution log.
    pub fn flush_publishers_logged(
        &mut self,
        timestamp: FrameworkTime,
        hook: &mut dyn FnMut(usize, &crate::message::MessageHeader),
    ) {
        for (ordinal, publisher) in self.publishers.iter_mut().enumerate() {
            publisher.flush_loaned_values_logged(timestamp, &mut |header| {
                hook(ordinal, header);
            });
        }
    }

    /// Whether this node wants its executions recorded in an executor's
    /// execution log. Executors decide whether they honor this.
    pub fn log_executions(&self) -> bool {
        self.log_executions
    }

    /// Toggle execution logging participation for this node. Default is off.
    pub fn set_log_executions(&mut self, enabled: bool) {
        self.log_executions = enabled;
    }

    pub fn run(&mut self, ctx: &crate::context::Context) -> Run {
        self.callback
            .run_generic(&mut self.subscribers, &mut self.publishers, ctx)
    }

    pub fn subscribers_request_execution(&self) -> bool {
        self.callback.requests_execution(&self.subscribers)
    }

    /// Whether every required (non-optional) input will have data for the
    /// callback once the next write→read drain completes: pending data in the
    /// write queue, or a retained value in the read buffer. Polling executors
    /// pair this with `subscribers_request_execution` so a node only runs
    /// when a trigger fired *and* all required inputs have values.
    pub fn required_inputs_ready(&self) -> bool {
        self.subscribers
            .iter()
            .all(|s| s.config().is_optional || s.has_data_available())
    }

    pub fn able_to_run(&self) -> bool {
        self.callback.able_to_run(&self.subscribers)
    }

    pub fn publishers(&self) -> &[Box<dyn GenericPublisher>] {
        &self.publishers
    }

    pub fn publishers_mut(&mut self) -> &mut [Box<dyn GenericPublisher>] {
        &mut self.publishers
    }

    pub fn subscribers(&self) -> &[Box<dyn GenericSubscriber>] {
        &self.subscribers
    }

    pub fn subscribers_mut(&mut self) -> &mut [Box<dyn GenericSubscriber>] {
        &mut self.subscribers
    }

    /// Finds the subscriber connected to the given channel, by name.
    pub fn find_subscriber_mut(
        &mut self,
        channel_name: &str,
    ) -> Option<&mut Box<dyn GenericSubscriber>> {
        self.subscribers
            .iter_mut()
            .find(|subscriber| subscriber.config().channel_name == channel_name)
    }

    /// Finds the publisher connected to the given channel, by name.
    pub fn find_publisher_mut(
        &mut self,
        channel_name: &str,
    ) -> Option<&mut Box<dyn GenericPublisher>> {
        self.publishers
            .iter_mut()
            .find(|publisher| publisher.config().channel_name == channel_name)
    }

    /// Called by the executor after construction to wire up the enqueue mechanism.
    pub fn register_with_executor(
        &self,
        node_index: usize,
        enqueuer: Arc<dyn CallbackNodeEnqueuer>,
    ) {
        self.readiness.register(node_index, enqueuer);
    }
}

impl Drop for CallbackNode {
    /// Empty subscriber queues before the node is destroyed. In standalone
    /// tests where a publisher's arena outlives this node, leaving ArenaPtrs
    /// in the subscriber buffers would be a use-after-free when the
    /// subscriber's queue drop glue dereferences the stale arena slots.
    /// Executors should still call cleanup on all nodes before tearing down
    /// arenas, but this is a cheap safety net for simple cases.
    fn drop(&mut self) {
        for subscriber in self.subscribers.iter() {
            subscriber.cleanup_buffers();
        }
    }
}

#[cfg(test)]
mod test {

    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::subscriber::{Subscriber, SubscriberConfig};

    fn make_subscriber(is_trigger: bool, is_optional: bool) -> Box<dyn GenericSubscriber> {
        Box::new(Subscriber::<u64>::new(SubscriberConfig {
            is_optional,
            capacity: 1,
            is_trigger,
            keep_across_runs: true,
            channel_name: "".into(),
        }))
    }

    fn compare_bitmask(subscribers: Vec<Box<dyn GenericSubscriber>>, expected: usize) {
        let actual = starting_subscriber_bitmask(&subscribers);
        assert_eq!(
            actual, expected,
            "Actual:{:064b} vs Expected:{:064b}",
            actual, expected
        );
    }

    /// A CallbackNodeEnqueuer that counts how many times it has been called.
    struct CountingEnqueuer(Arc<AtomicUsize>);
    impl CallbackNodeEnqueuer for CountingEnqueuer {
        fn enqueue_node(&self, _node_index: usize) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A callback that does nothing when run.
    struct NoopCallback;
    impl Callback for NoopCallback {
        fn run_generic(
            &mut self,
            _: &mut [Box<dyn GenericSubscriber>],
            _: &mut [Box<dyn GenericPublisher>],
            _: &crate::context::Context,
        ) -> Run {
            Run::new(0)
        }
        fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
            vec![]
        }
        fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
            vec![]
        }
    }

    #[test]
    fn test_two_trigger_subscribers_both_must_be_set() {
        // Build a CallbackNodeReadiness for two required-trigger subscribers.
        // Bits 0 and 1 start at 0; all other bits are 1. The enqueuer should
        // only fire once BOTH bits are set (i.e., bitmask == usize::MAX).

        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let enqueuer =
            Arc::new(CountingEnqueuer(enqueue_count.clone())) as Arc<dyn CallbackNodeEnqueuer>;

        let initial = starting_subscriber_bitmask(&[
            Box::new(Subscriber::<u64>::new(SubscriberConfig {
                is_optional: false,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: "a".into(),
            })),
            Box::new(Subscriber::<u64>::new(SubscriberConfig {
                is_optional: false,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: "b".into(),
            })),
        ]);
        let readiness = CallbackNodeReadiness::new(initial);
        readiness.register(0, enqueuer);

        // Only subscriber 0 is ready — should NOT enqueue yet
        readiness.set_bit(0);
        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            0,
            "should not enqueue after only one subscriber is ready"
        );

        // Now subscriber 1 is also ready — bitmask becomes MAX, should enqueue
        readiness.set_bit(1);
        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            1,
            "should enqueue once both subscribers are ready"
        );

        // Setting an already-set bit again should not double-enqueue
        readiness.set_bit(0);
        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            1,
            "should not enqueue again when bit is already set"
        );
    }

    #[test]
    fn test_loopback_self_subscribe() {
        use crate::context::Context;
        use crate::input::OptionalInput;
        use crate::output::Output;
        use crate::publisher::{Publisher, PublisherConfig};
        use crate::time::FrameworkTime;

        struct LoopbackCallback {
            value_to_publish: u64,
            received: Vec<u64>,
        }

        impl Callback for LoopbackCallback {
            fn run_generic(
                &mut self,
                subscribers: &mut [Box<dyn GenericSubscriber>],
                publishers: &mut [Box<dyn GenericPublisher>],
                _ctx: &Context,
            ) -> Run {
                // Read any looped-back messages
                let input = OptionalInput::<u64>::new_downcasted(&mut *subscribers[0]);
                if let Some(msg) = input.value() {
                    self.received.push(*msg);
                }

                // Publish a new value
                let mut output = Output::<u64>::new_downcasted(&mut *publishers[0]);
                *output = self.value_to_publish;
                output.send();
                self.value_to_publish += 1;

                Run::new(1)
            }

            fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
                vec![Box::new(Subscriber::<u64>::new(SubscriberConfig {
                    is_optional: true,
                    capacity: 1,
                    is_trigger: false,
                    keep_across_runs: true,
                    channel_name: "loopback".into(),
                }))]
            }

            fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
                vec![Box::new(Publisher::<u64>::new(PublisherConfig {
                    capacity: 1,
                    channel_name: "loopback".into(),
                }))]
            }
        }

        let callback = LoopbackCallback {
            value_to_publish: 10,
            received: vec![],
        };

        let mut nodes = vec![CallbackNode::new_named(
            Box::new(callback),
            "LoopbackCallback".into(),
        )];

        connect_callback_nodes(&mut nodes).expect("loopback connection should succeed");

        let ctx = Context {
            now: FrameworkTime::from_nanoseconds(1),
        };

        // Run 1: publishes 10, no loopback data yet (first run)
        nodes[0].run(&ctx);
        nodes[0].flush_publishers(ctx.now);

        // The published message should be in the subscriber's write buffer now
        nodes[0].drain_subscribers();

        // Run 2: should receive 10 from the loopback, publishes 11
        nodes[0].run(&ctx);
        nodes[0].flush_publishers(ctx.now);

        // After flush, the published message from run 2 is in the write buffer
        let sub_info = nodes[0].subscribers()[0].queue_info();
        assert_eq!(
            sub_info.writer_size, 1,
            "loopback: published message should be in subscriber's write buffer"
        );

        nodes[0].drain_subscribers();

        let sub_info = nodes[0].subscribers()[0].queue_info();
        assert_eq!(
            sub_info.reader_size, 1,
            "loopback: message should have drained to read buffer"
        );
    }

    #[test]
    fn test_subscriber_bitmask() {
        // No subscribers → all bits 1 (nothing blocks us)
        compare_bitmask(vec![], usize::MAX);

        // One required trigger subscriber → bit 0 cleared
        compare_bitmask(vec![make_subscriber(true, false)], usize::MAX - 1);

        // Two required trigger subscribers → bits 0 and 1 cleared
        compare_bitmask(
            vec![make_subscriber(true, false), make_subscriber(true, false)],
            usize::MAX - 3,
        );

        // Optional trigger subscriber → bit stays at 1 (doesn't block)
        compare_bitmask(vec![make_subscriber(true, true)], usize::MAX);

        // Non-trigger required subscriber → bit 0 cleared (required inputs
        // gate whether or not they trigger)
        compare_bitmask(vec![make_subscriber(false, false)], usize::MAX - 1);
    }

    #[test]
    fn test_gating_bits_pack_densely_past_optional_subscribers() {
        // Optional subscribers interleaved with gating ones consume no bits:
        // two gating subscribers → bits 0 and 1 cleared regardless of position.
        compare_bitmask(
            vec![
                make_subscriber(true, true),  // optional trigger — no bit
                make_subscriber(true, false), // gating → bit 0
                make_subscriber(false, true), // optional non-trigger — no bit
                make_subscriber(true, false), // gating → bit 1
            ],
            usize::MAX - 3,
        );
    }

    #[test]
    fn test_more_than_bitwidth_optional_subscribers_is_allowed() {
        // Only gating subscribers count against the bit-width cap. A node made
        // entirely of optional subscribers (e.g. a logging task draining many
        // channels) is fine at any count.
        let subscribers = (0..(std::mem::size_of::<usize>() * 8 + 1))
            .map(|_| make_subscriber(false, true))
            .collect::<Vec<_>>();
        compare_bitmask(subscribers, usize::MAX);
    }

    #[test]
    #[should_panic(expected = "required (non-optional) subscribers")]
    fn test_more_than_bitwidth_gating_subscribers_panics() {
        let subscribers = (0..(std::mem::size_of::<usize>() * 8 + 1))
            .map(|_| make_subscriber(true, false))
            .collect::<Vec<_>>();
        starting_subscriber_bitmask(&subscribers);
    }

    /// Regression test: an optional subscriber's drain must not clear a
    /// readiness bit (its bit is never set by publishers, so a cleared bit
    /// would permanently keep the bitmask below usize::MAX and the node would
    /// never be data-triggered again).
    #[test]
    fn test_optional_subscriber_drain_does_not_block_retriggering() {
        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let enqueuer =
            Arc::new(CountingEnqueuer(enqueue_count.clone())) as Arc<dyn CallbackNodeEnqueuer>;

        let subscribers: Vec<Box<dyn GenericSubscriber>> = vec![
            Box::new(Subscriber::<u64>::new(SubscriberConfig {
                is_optional: false,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: "required".into(),
            })),
            Box::new(Subscriber::<u64>::new(SubscriberConfig {
                is_optional: true,
                capacity: 1,
                is_trigger: false,
                keep_across_runs: true,
                channel_name: "optional".into(),
            })),
        ];

        let mut node =
            CallbackNode::new_with(Box::new(NoopCallback), subscribers, vec![], "mixed".into());
        node.register_with_executor(0, enqueuer);

        // Simulate a full cycle: publisher data sets the gating bit, the node
        // is enqueued, then both subscribers drain (clearing their bits, if
        // they have them).
        let Some(SubscriberReadiness::Gating(readiness, bit_index)) =
            node.subscribers()[0].readiness_state()
        else {
            panic!("gating subscriber should have gating readiness state")
        };
        assert!(
            node.subscribers()[1].readiness_state().is_none(),
            "optional non-trigger subscriber should have no readiness state"
        );

        readiness.set_bit(bit_index);
        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            1,
            "first data arrival should enqueue the node"
        );

        node.drain_subscribers();

        // Second data arrival: the node must be enqueued again. Before dense
        // gating-only bit assignment, the optional subscriber's drain had
        // cleared a bit nobody would re-set, blocking this.
        readiness.set_bit(bit_index);
        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            2,
            "node must re-trigger after optional subscriber drains"
        );
    }

    /// A required non-trigger input must gate the node: the callback runs
    /// only once every required input has a value, and a trigger arrival
    /// while the gate is empty must not run (or panic on) the node. Once the
    /// gate has a value it is retained, so later trigger data re-runs the
    /// node — while gate data alone never triggers.
    #[test]
    fn test_required_non_trigger_input_gates_until_it_has_a_value() {
        use crate::output::Output;
        use crate::publisher::{Publisher, PublisherConfig};
        use crate::time::FrameworkTime;

        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let enqueuer =
            Arc::new(CountingEnqueuer(enqueue_count.clone())) as Arc<dyn CallbackNodeEnqueuer>;

        // Publishers are declared before the CallbackNode so the node drops
        // first; CallbackNode::drop cleans subscriber queues before the
        // publisher arenas are freed.
        let mut trigger_pub = Publisher::<u64>::new(PublisherConfig {
            capacity: 1,
            channel_name: "trigger".into(),
        });
        let mut gate_pub = Publisher::<u64>::new(PublisherConfig {
            capacity: 1,
            channel_name: "gate".into(),
        });

        let subscribers: Vec<Box<dyn GenericSubscriber>> = vec![
            Box::new(Subscriber::<u64>::new(SubscriberConfig {
                is_optional: false,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: "trigger".into(),
            })),
            Box::new(Subscriber::<u64>::new(SubscriberConfig {
                is_optional: false,
                capacity: 1,
                is_trigger: false,
                keep_across_runs: true,
                channel_name: "gate".into(),
            })),
        ];

        let mut node =
            CallbackNode::new_with(Box::new(NoopCallback), subscribers, vec![], "gated".into());
        node.register_with_executor(0, enqueuer);

        assert!(
            trigger_pub
                .connect_to_subscriber(node.subscribers_mut()[0].as_mut())
                .is_ok(),
            "trigger publisher connects"
        );
        assert!(
            gate_pub
                .connect_to_subscriber(node.subscribers_mut()[1].as_mut())
                .is_ok(),
            "gate publisher connects"
        );
        trigger_pub.allocate_arena();
        gate_pub.allocate_arena();

        let t = FrameworkTime::from_nanoseconds(1);

        // Trigger fires while the gate has no value: no enqueue.
        {
            let mut out = Output::new_default(&mut trigger_pub);
            *out = 1;
            out.send();
        }
        trigger_pub.flush_loaned_values(t);
        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            0,
            "trigger alone must not run the node"
        );

        // The gate gets a value: everyone was ready and the callback was
        // triggered → enqueue.
        {
            let mut out = Output::new_default(&mut gate_pub);
            *out = 10;
            out.send();
        }
        gate_pub.flush_loaned_values(t);
        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            1,
            "gate value arriving after the trigger must run the node"
        );

        // The node runs: draining clears the trigger bit (event consumed) but
        // keeps the gate bit (its value is retained in the read buffer).
        node.drain_subscribers();

        // New trigger data while the gate value is retained: runs again.
        {
            let mut out = Output::new_default(&mut trigger_pub);
            *out = 2;
            out.send();
        }
        trigger_pub.flush_loaned_values(t);
        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            2,
            "new trigger data must re-run the node while the gate retains its value"
        );

        node.drain_subscribers();

        // Gate data alone never re-triggers the node.
        {
            let mut out = Output::new_default(&mut gate_pub);
            *out = 11;
            out.send();
        }
        gate_pub.flush_loaned_values(t);
        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            2,
            "gate data alone must not trigger the node"
        );
    }

    /// An optional+trigger input must data-trigger its node even though it
    /// owns no readiness bit: with no required inputs the bitmask is always
    /// MAX, so every arrival nudges an enqueue.
    #[test]
    fn test_optional_trigger_input_fires_node() {
        use crate::output::Output;
        use crate::publisher::{Publisher, PublisherConfig};
        use crate::time::FrameworkTime;

        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let enqueuer =
            Arc::new(CountingEnqueuer(enqueue_count.clone())) as Arc<dyn CallbackNodeEnqueuer>;

        // Publisher declared before CallbackNode so the node drops first and
        // CallbackNode::drop cleans subscriber queues before the arena is freed.
        let mut publisher = Publisher::<u64>::new(PublisherConfig {
            capacity: 2,
            channel_name: "opt_trig".into(),
        });

        let subscribers: Vec<Box<dyn GenericSubscriber>> =
            vec![Box::new(Subscriber::<u64>::new(SubscriberConfig {
                is_optional: true,
                capacity: 2,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: "opt_trig".into(),
            }))];

        let mut node = CallbackNode::new_with(
            Box::new(NoopCallback),
            subscribers,
            vec![],
            "optional_triggered".into(),
        );
        node.register_with_executor(0, enqueuer);

        // Registration itself enqueues once (bitmask starts at MAX) — the
        // startup kick. Reset so the counts below reflect data arrivals.
        enqueue_count.store(0, Ordering::Relaxed);
        assert!(
            publisher
                .connect_to_subscriber(node.subscribers_mut()[0].as_mut())
                .is_ok(),
            "publisher connects"
        );
        publisher.allocate_arena();

        let t = FrameworkTime::from_nanoseconds(1);
        for i in 0..3u64 {
            {
                let mut out = Output::new_default(&mut publisher);
                *out = i;
                out.send();
            }
            publisher.flush_loaned_values(t);
            assert_eq!(
                enqueue_count.load(Ordering::Relaxed) as u64,
                i + 1,
                "every optional-trigger arrival must enqueue the node"
            );
            node.drain_subscribers();
        }
    }

    /// An optional+trigger input may only fire the node when its required
    /// input has a value — otherwise the run would hit an empty required
    /// input. While the gate is empty the nudge is swallowed; once the gate
    /// has a value (retained), optional-trigger data enqueues again.
    #[test]
    fn test_optional_trigger_respects_required_gate() {
        use crate::output::Output;
        use crate::publisher::{Publisher, PublisherConfig};
        use crate::time::FrameworkTime;

        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let enqueuer =
            Arc::new(CountingEnqueuer(enqueue_count.clone())) as Arc<dyn CallbackNodeEnqueuer>;

        // Publishers declared before CallbackNode so the node drops first and
        // CallbackNode::drop cleans subscriber queues before the arenas are freed.
        let mut gate_pub = Publisher::<u64>::new(PublisherConfig {
            capacity: 1,
            channel_name: "gate".into(),
        });
        let mut opt_pub = Publisher::<u64>::new(PublisherConfig {
            capacity: 1,
            channel_name: "opt_trig".into(),
        });

        let subscribers: Vec<Box<dyn GenericSubscriber>> = vec![
            Box::new(Subscriber::<u64>::new(SubscriberConfig {
                is_optional: false,
                capacity: 1,
                is_trigger: false,
                keep_across_runs: true,
                channel_name: "gate".into(),
            })),
            Box::new(Subscriber::<u64>::new(SubscriberConfig {
                is_optional: true,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: "opt_trig".into(),
            })),
        ];

        let mut node = CallbackNode::new_with(
            Box::new(NoopCallback),
            subscribers,
            vec![],
            "gated_optional_trigger".into(),
        );
        node.register_with_executor(0, enqueuer);

        assert!(
            gate_pub
                .connect_to_subscriber(node.subscribers_mut()[0].as_mut())
                .is_ok()
        );
        assert!(
            opt_pub
                .connect_to_subscriber(node.subscribers_mut()[1].as_mut())
                .is_ok()
        );
        gate_pub.allocate_arena();
        opt_pub.allocate_arena();

        let t = FrameworkTime::from_nanoseconds(1);

        // Optional-trigger data while the required gate is empty: no fire.
        {
            let mut out = Output::new_default(&mut opt_pub);
            *out = 1;
            out.send();
        }
        opt_pub.flush_loaned_values(t);
        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            0,
            "optional trigger must not fire while a required input is empty"
        );

        // Gate value arrives → enqueue (required-side MAX transition).
        {
            let mut out = Output::new_default(&mut gate_pub);
            *out = 10;
            out.send();
        }
        gate_pub.flush_loaned_values(t);
        assert_eq!(enqueue_count.load(Ordering::Relaxed), 1);

        node.drain_subscribers();

        // Gate value is retained (non-trigger) → optional trigger fires again.
        {
            let mut out = Output::new_default(&mut opt_pub);
            *out = 2;
            out.send();
        }
        opt_pub.flush_loaned_values(t);
        assert_eq!(
            enqueue_count.load(Ordering::Relaxed),
            2,
            "optional trigger must fire once the gate retains a value"
        );
    }

    /// `find_forwarded_channel_usage` drives how much the forwarding publisher's
    /// arena is grown by `connect_callback_nodes`. It must account for clones
    /// held simultaneously in each downstream subscriber's write queue *and*
    /// its read buffer; otherwise a forwarder that re-publishes before drains
    /// exhausts the arena and panics (and, via cleanup-ordering fallout, trips
    /// a use-after-free under miri).
    #[test]
    fn test_forwarded_channel_usage_accounts_for_write_and_read_buffers() {
        use crate::forwarded_message::ForwardedMessage;
        use crate::publisher::{ForwardingPublisher, PublisherConfig};
        use crate::subscriber::{Subscriber, SubscriberConfig};

        const FORWARDED: &str = "forwarded";

        struct Forwarder;
        impl Callback for Forwarder {
            fn run_generic(
                &mut self,
                _: &mut [Box<dyn GenericSubscriber>],
                _: &mut [Box<dyn GenericPublisher>],
                _: &crate::context::Context,
            ) -> Run {
                Run::new(0)
            }
            fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
                vec![]
            }
            fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
                vec![Box::new(ForwardingPublisher::<bool, u64>::new(
                    PublisherConfig {
                        capacity: 1,
                        channel_name: FORWARDED.into(),
                    },
                    vec![FORWARDED.into()],
                ))]
            }
        }

        struct Receiver {
            capacity: usize,
        }
        impl Callback for Receiver {
            fn run_generic(
                &mut self,
                _: &mut [Box<dyn GenericSubscriber>],
                _: &mut [Box<dyn GenericPublisher>],
                _: &crate::context::Context,
            ) -> Run {
                Run::new(0)
            }
            fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
                vec![Box::new(Subscriber::<ForwardedMessage<bool, u64>>::new(
                    SubscriberConfig {
                        is_optional: true,
                        capacity: self.capacity,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: FORWARDED.into(),
                    },
                ))]
            }
            fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
                vec![]
            }
        }

        let nodes = vec![
            CallbackNode::new_named(Box::new(Forwarder), "Forwarder".into()),
            CallbackNode::new_named(Box::new(Receiver { capacity: 4 }), "Receiver4".into()),
            CallbackNode::new_named(Box::new(Receiver { capacity: 3 }), "Receiver3".into()),
        ];

        let usage = super::find_forwarded_channel_usage(&nodes);
        assert_eq!(
            usage.get(&String::from(FORWARDED)).copied(),
            Some(2 * 4 + 2 * 3),
            "each subscriber contributes 2 * capacity (write + read buffers)"
        );
    }
}
