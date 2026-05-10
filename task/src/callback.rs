use crate::executor::TaskEnqueuer;
use crate::generic_publisher::GenericPublisher;
use crate::generic_subscriber::GenericSubscriber;
use crate::log_types::ExecutionLogger;
use crate::pub_sub::{CallbackName, ChannelName};
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

pub trait GenericCallback {
    // Generic interface for calling the task. Used by the framework to trigger things
    // Can provide information about inputs/outputs per index
    // - what inputs request execution
    // - input queue capacity
    // returns number of times the task was run
    fn run_generic(
        &mut self,
        subscribers: &mut [Box<dyn GenericSubscriber>],
        publishers: &mut [Box<dyn GenericPublisher>],
        ctx: &crate::context::Context,
    ) -> Run;

    fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>>;

    fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>>;

    fn able_to_run(&self, inputs: &[Box<dyn GenericSubscriber>]) -> bool {
        inputs.iter().all(|input| input.able_to_run())
    }

    fn requests_execution(&self, inputs: &[Box<dyn GenericSubscriber>]) -> bool {
        inputs.iter().any(|input| input.requests_execution())
    }
}

pub struct MismatchTypeError {
    channel_name: ChannelName,
    publisher_callback: CallbackName,
    subscriber_callback: CallbackName,
}

impl std::fmt::Display for MismatchTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Callback '{}' publishes on '{}' but subscriber '{}' has a different type for that channel",
            self.publisher_callback, self.channel_name, self.subscriber_callback
        )
    }
}

/// Returns a mapping of the forwarded channel to the depth of subscriber queues listening to it
fn find_forwarded_channel_usage(callbacks: &[ConnectedCallback]) -> HashMap<ChannelName, usize> {
    let mut channel_to_usage = HashMap::<ChannelName, usize>::new();

    // Find all channels that are forwarded
    for callback in callbacks.iter() {
        for publisher in callback.publishers.iter() {
            for forwarded_channel in publisher.get_forwarded_channels() {
                channel_to_usage.insert(forwarded_channel.clone(), 0);
            }
        }
    }

    // Find all subscribers of the forwarded channel set and bump the usage accordingly
    for callback in callbacks.iter() {
        for subscriber in callback.subscribers.iter() {
            let subscriber_channel_name = &subscriber.get_config().channel_name;
            match channel_to_usage.get_mut(subscriber_channel_name) {
                Some(usage) => *usage += subscriber.get_config().capacity,
                None => {
                    // Subscriber doesn't use this channel
                }
            };
        }
    }

    channel_to_usage
}

/// Connects publishers to subscribers and sizes arenas accordingly
pub fn connect_callbacks(callbacks: &mut [ConnectedCallback]) -> Result<(), MismatchTypeError> {
    let forwarded_channel_to_usage = find_forwarded_channel_usage(callbacks);

    // Connect publishers to everyone who is subscribing to their output
    for callback_idx in 0..callbacks.len() {
        for other_callback_idx in 0..callbacks.len() {
            let callback_name = callbacks[callback_idx].get_name().to_string();
            let other_callback_name = callbacks[other_callback_idx].get_name().to_string();

            for publisher in callbacks[callback_idx].publishers.iter_mut() {
                // Find subscribers to this publisher
                for other_callback_subscriber in
                    callbacks[other_callback_idx].subscribers.iter_mut()
                {
                    if publisher.get_config().channel_name
                        == other_callback_subscriber.get_config().channel_name
                    {
                        println!(
                            "Connecting task '{}' to task '{}' on channel '{}'",
                            callback_name,
                            other_callback_name,
                            publisher.get_config().channel_name
                        );
                        if let Err(_e) =
                            publisher.connect_to_subscriber(other_callback_subscriber.as_mut())
                        {
                            return Err(MismatchTypeError {
                                channel_name: publisher.get_config().channel_name.clone(),
                                publisher_callback: callbacks[callback_idx].get_name().into(),
                                subscriber_callback: callbacks[other_callback_idx]
                                    .get_name()
                                    .into(),
                            });
                        }
                    }
                }

                // This task has its channel forwarded to a bunch of subscriber slots, so we must bump its size accordingly
                match forwarded_channel_to_usage.get(&publisher.get_config().channel_name) {
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

/// Tracks readiness of all subscribers in a ConnectedCallback via an atomic bitmask.
/// Each subscriber bit is 0 (not ready) or 1 (ready). Unused high bits are always 1.
/// When the bitmask reaches usize::MAX, all trigger subscribers have data and the task
/// can be enqueued.
pub struct CallbackReadiness {
    bitmask: AtomicUsize,
    task_index: OnceLock<usize>,
    enqueuer: OnceLock<Arc<dyn TaskEnqueuer>>,
}

impl CallbackReadiness {
    fn new(initial_bitmask: usize) -> Arc<Self> {
        Arc::new(CallbackReadiness {
            bitmask: AtomicUsize::new(initial_bitmask),
            task_index: OnceLock::new(),
            enqueuer: OnceLock::new(),
        })
    }

    /// Set bit `index`, then enqueue the task if this was the transition to usize::MAX.
    /// Only enqueues when the bitmask was not already MAX before this call.
    pub fn set_bit(&self, index: usize) {
        let bit = 1usize << index;
        let prev = self.bitmask.fetch_or(bit, Ordering::AcqRel);
        // Only enqueue on the transition: previous was not MAX but now is
        if prev != usize::MAX
            && prev | bit == usize::MAX
            && let (Some(enqueuer), Some(&task_index)) =
                (self.enqueuer.get(), self.task_index.get())
        {
            enqueuer.enqueue_task(task_index);
        }
    }

    /// Clear bit `index` (called when the task drains write→read for this subscriber).
    pub fn clear_bit(&self, index: usize) {
        let mask = !(1usize << index);
        self.bitmask.fetch_and(mask, Ordering::AcqRel);
    }

    /// Register the executor enqueuer and task index. If the bitmask is already MAX
    /// (e.g., startup with pre-loaded data), immediately enqueue.
    pub fn register(&self, task_index: usize, enqueuer: Arc<dyn TaskEnqueuer>) {
        let _ = self.task_index.set(task_index);
        let _ = self.enqueuer.set(enqueuer);
        // Startup case: if already ready, enqueue now
        if self.bitmask.load(Ordering::Acquire) == usize::MAX
            && let (Some(enqueuer), Some(&idx)) = (self.enqueuer.get(), self.task_index.get())
        {
            enqueuer.enqueue_task(idx);
        }
    }
}

/// Compute the initial bitmask for a set of subscribers.
/// Bits for trigger + non-optional subscribers start at 0 (must receive data).
/// All other subscriber bits and unused bits start at 1.
fn starting_subscriber_bitmask(subscribers: &[Box<dyn GenericSubscriber>]) -> usize {
    const MAX_SUBSCRIBER_COUNT: usize = std::mem::size_of::<usize>() * 8;
    if subscribers.len() > MAX_SUBSCRIBER_COUNT {
        // 64 isn't much for most use-cases, but we may have some diagnostic or logging callbacks that want more.
        // We could either have some non-triggering subscribers (so, poll only) or have those callbacks decompose themselves into smaller
        // callbacks that publish intermediate results.
        panic!(
            "We cannot support callbacks with more than {} subscribers, try splitting out your callback into multiple callbacks.",
            MAX_SUBSCRIBER_COUNT
        )
    }

    let mut bitmask = usize::MAX;
    for (index, subscriber) in subscribers.iter().enumerate() {
        let config = subscriber.get_config();
        if config.is_trigger && !config.is_optional {
            // Clear this bit — subscriber must receive data before triggering
            bitmask &= !(1usize << index);
        }
    }
    bitmask
}

pub struct ConnectedCallback {
    subscribers: Vec<Box<dyn GenericSubscriber>>,
    publishers: Vec<Box<dyn GenericPublisher>>,
    callback: Box<dyn GenericCallback>,

    // Ideally this would be apart of GenericCallback, but I dont have a good way to store it
    next_execution_time_callback: Box<dyn Fn(FrameworkTime) -> Option<FrameworkTime>>,
    execution_duration_callback: Option<Box<dyn Fn() -> Duration>>,

    name: CallbackName,

    readiness: Arc<CallbackReadiness>,

    logger: Option<Box<dyn ExecutionLogger>>,
}
unsafe impl Sync for ConnectedCallback {}
unsafe impl Send for ConnectedCallback {}

impl ConnectedCallback {
    // TODO remove this constructor, users should always provide names
    pub fn new(callback: Box<dyn GenericCallback>) -> Self {
        ConnectedCallback::new_named(callback, "CallbackName".into())
    }

    pub fn new_named(callback: Box<dyn GenericCallback>, name: CallbackName) -> Self {
        let subscribers = callback.build_subscribers();
        let publishers = callback.build_publishers();
        ConnectedCallback::new_with(callback, subscribers, publishers, name)
    }

    pub fn new_with(
        callback: Box<dyn GenericCallback>,
        mut subscribers: Vec<Box<dyn GenericSubscriber>>,
        publishers: Vec<Box<dyn GenericPublisher>>,
        name: CallbackName,
    ) -> Self {
        let initial_bitmask = starting_subscriber_bitmask(&subscribers);
        let readiness = CallbackReadiness::new(initial_bitmask);

        // Inject bitmask Arc and bit index into each subscriber
        for (index, subscriber) in subscribers.iter_mut().enumerate() {
            subscriber.set_readiness_state(readiness.clone(), index);
        }

        ConnectedCallback {
            subscribers,
            publishers,
            callback,
            next_execution_time_callback: Box::new(|_| None),
            execution_duration_callback: None,
            name,
            readiness,
            logger: None,
        }
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }

    pub fn set_execution_duration_callback(&mut self, callback: Box<dyn Fn() -> Duration>) {
        self.execution_duration_callback = Some(callback);
    }

    pub fn get_execution_duration(&self) -> Duration {
        (self
            .execution_duration_callback
            .as_ref()
            .expect("execution_duration_callback not set on this ConnectedCallback"))()
    }

    pub fn set_execution_time_callback(
        &mut self,
        callback: Box<dyn Fn(FrameworkTime) -> Option<FrameworkTime>>,
    ) {
        self.next_execution_time_callback = callback;
    }

    /// The next requested execution time relevant to a current execution time.
    /// 'Instant' is assumed to be provided via a monotonic clock as per rust docs.
    pub fn get_next_requested_execution_time(&self, now: FrameworkTime) -> Option<FrameworkTime> {
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

    pub fn run(&mut self, ctx: &crate::context::Context) -> Run {
        if let Some(mut logger) = self.logger.take() {
            logger.log_before_run(ctx, &self.subscribers);
            let result =
                self.callback
                    .run_generic(&mut self.subscribers, &mut self.publishers, ctx);
            logger.log_after_run(ctx, &self.publishers);
            self.logger = Some(logger);
            result
        } else {
            self.callback
                .run_generic(&mut self.subscribers, &mut self.publishers, ctx)
        }
    }

    pub fn set_execution_logger(&mut self, logger: Box<dyn ExecutionLogger>) {
        self.logger = Some(logger);
    }

    pub fn subscribers_request_execution(&self) -> bool {
        self.callback.requests_execution(&self.subscribers)
    }

    pub fn able_to_run(&self) -> bool {
        self.callback.able_to_run(&self.subscribers)
    }

    pub fn get_publishers(&self) -> &[Box<dyn GenericPublisher>] {
        &self.publishers
    }

    pub fn get_subscribers(&self) -> &[Box<dyn GenericSubscriber>] {
        &self.subscribers
    }

    /// Called by the executor after construction to wire up the enqueue mechanism.
    pub fn register_with_executor(&self, task_index: usize, enqueuer: Arc<dyn TaskEnqueuer>) {
        self.readiness.register(task_index, enqueuer);
    }
}

#[cfg(test)]
mod test {

    use std::sync::atomic::AtomicUsize;
    use std::usize;

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

    /// A TaskEnqueuer that counts how many times it has been called.
    struct CountingEnqueuer(Arc<AtomicUsize>);
    impl TaskEnqueuer for CountingEnqueuer {
        fn enqueue_task(&self, _task_index: usize) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn test_two_trigger_subscribers_both_must_be_set() {
        // Build a CallbackReadiness for two required-trigger subscribers.
        // Bits 0 and 1 start at 0; all other bits are 1. The enqueuer should
        // only fire once BOTH bits are set (i.e., bitmask == usize::MAX).

        let enqueue_count = Arc::new(AtomicUsize::new(0));
        let enqueuer = Arc::new(CountingEnqueuer(enqueue_count.clone())) as Arc<dyn TaskEnqueuer>;

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
        let readiness = CallbackReadiness::new(initial);
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

        // Non-trigger required subscriber → bit stays at 1 (doesn't gate triggering)
        compare_bitmask(vec![make_subscriber(false, false)], usize::MAX);
    }
}
