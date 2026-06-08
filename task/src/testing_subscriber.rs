use crate::{
    callback::CallbackReadiness,
    generic_subscriber::QueueInfo,
    message::Message,
    pub_sub::ChannelName,
    subscriber::{GenericSubscriber, Subscriber, SubscriberConfig},
};
use std::sync::Arc;

/// Queue depth used when no explicit capacity is given — generous enough for typical
/// single-step unit tests without forcing every test to think about sizing.
pub const DEFAULT_TEST_SUBSCRIBER_CAPACITY: usize = 10;

fn make_test_config(channel: ChannelName, capacity: usize) -> SubscriberConfig {
    SubscriberConfig {
        is_optional: true,
        capacity,
        is_trigger: false,
        keep_across_runs: true,
        channel_name: channel,
    }
}

/// Subscriber that captures messages published on a channel for inspection in tests.
/// Wraps a real `Subscriber<T>` — connecting to a publisher works exactly like any
/// other internal subscriber, so there's no special-casing anywhere in `Publisher<T>`.
pub struct TestSubscriber<T> {
    subscriber: Subscriber<T>,
}

impl<T> TestSubscriber<T> {
    /// Creates a `TestSubscriber` with the default queue depth
    /// ([`DEFAULT_TEST_SUBSCRIBER_CAPACITY`]).
    pub fn new(channel_name: ChannelName) -> Self {
        Self::with_capacity(channel_name, DEFAULT_TEST_SUBSCRIBER_CAPACITY)
    }

    /// Creates a `TestSubscriber` with a caller-chosen queue depth — use this when a
    /// test sends through more messages than the default comfortably holds.
    pub fn with_capacity(channel_name: ChannelName, capacity: usize) -> Self {
        TestSubscriber {
            subscriber: Subscriber::new(make_test_config(channel_name, capacity)),
        }
    }
}

impl<T: 'static + Clone> TestSubscriber<T> {
    /// Drains and returns all queued messages, cloned out as owned values.
    ///
    /// Panics if any messages were ever dropped due to the queue overflowing — that
    /// means the test capacity is too small for what was actually published. Use
    /// [`TestSubscriber::try_messages`] if dropped messages are expected and you'd
    /// rather inspect the situation than panic.
    pub fn messages(&mut self) -> Vec<Box<Message<T>>> {
        let (messages, dropped) = self.try_messages();
        assert_eq!(
            dropped,
            0,
            "TestSubscriber on channel '{}' dropped {dropped} message(s) — queue capacity ({}) \
             was exceeded; use `with_capacity` to size it for what this test actually sends, or \
             call `try_messages` if drops are expected",
            self.subscriber.get_config().channel_name,
            self.subscriber.get_config().capacity,
        );
        messages
    }

    /// Like [`TestSubscriber::messages`], but never panics on drops — instead returns
    /// the drained messages alongside the total number of messages ever displaced due
    /// to overflow, for tests that want to assert on drop behavior directly.
    pub fn try_messages(&mut self) -> (Vec<Box<Message<T>>>, usize) {
        self.subscriber.drain_writer_to_reader();
        let mut guard = self.subscriber.get_read_buffer();
        let messages = guard
            .drain_contiguous()
            .map(|ptr| Box::new((*ptr).clone()))
            .collect();
        drop(guard);
        (messages, self.subscriber.get_reader_queue_drops())
    }
}

impl<T: 'static> GenericSubscriber for TestSubscriber<T> {
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        // Expose the inner `Subscriber<T>` rather than `self`: this is what lets
        // `Publisher<T>::connect_to_subscriber` recognize and wire up a `TestSubscriber`
        // through the exact same path as any other internal subscriber.
        &mut self.subscriber
    }

    fn get_config(&self) -> &SubscriberConfig {
        self.subscriber.get_config()
    }

    fn get_config_mut(&mut self) -> &mut SubscriberConfig {
        self.subscriber.get_config_mut()
    }

    fn able_to_run(&self) -> bool {
        self.subscriber.able_to_run()
    }

    fn requests_execution(&self) -> bool {
        self.subscriber.requests_execution()
    }

    fn drain_writer_to_reader(&self) {
        self.subscriber.drain_writer_to_reader();
    }

    fn get_queue_info(&self) -> QueueInfo {
        self.subscriber.get_queue_info()
    }

    fn cleanup_buffers(&self) {
        self.subscriber.cleanup_buffers();
    }

    fn set_readiness_state(&mut self, state: Arc<CallbackReadiness>, bit_index: usize) {
        self.subscriber.set_readiness_state(state, bit_index)
    }

    fn get_readiness_state(&self) -> Option<(Arc<CallbackReadiness>, usize)> {
        self.subscriber.get_readiness_state()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        generic_publisher::GenericPublisher,
        output::Output,
        publisher::{Publisher, PublisherConfig},
        time::FrameworkTime,
    };

    fn connected_publisher(channel: &str, subscriber: &mut TestSubscriber<i32>) -> Publisher<i32> {
        let mut publisher = Publisher::<i32>::new(PublisherConfig {
            capacity: 1,
            channel_name: channel.into(),
        });
        assert!(publisher.connect_to_subscriber(subscriber).is_ok());
        publisher.allocate_arena();
        publisher
    }

    fn send(publisher: &mut Publisher<i32>, value: i32, at: i64) {
        let mut output = Output::new_default(publisher);
        *output = value;
        output.send();
        publisher.flush_loaned_values(FrameworkTime::from_nanoseconds(at));
    }

    #[test]
    fn connects_like_a_normal_subscriber_and_receives_messages() {
        let mut subscriber = TestSubscriber::<i32>::new("channel".into());
        let mut publisher = connected_publisher("channel", &mut subscriber);

        send(&mut publisher, 42, 99);

        let messages = subscriber.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message, 42);
        assert_eq!(
            messages[0].header.published_at,
            FrameworkTime::from_nanoseconds(99)
        );
    }

    #[test]
    fn default_capacity_matches_constant() {
        let subscriber = TestSubscriber::<i32>::new("channel".into());
        assert_eq!(
            subscriber.get_config().capacity,
            DEFAULT_TEST_SUBSCRIBER_CAPACITY
        );
    }

    #[test]
    fn with_capacity_overrides_default() {
        let subscriber = TestSubscriber::<i32>::with_capacity("channel".into(), 2);
        assert_eq!(subscriber.get_config().capacity, 2);
    }

    #[test]
    #[should_panic(expected = "dropped 1 message")]
    fn messages_panics_on_overflow() {
        let mut subscriber = TestSubscriber::<i32>::with_capacity("channel".into(), 1);
        let mut publisher = connected_publisher("channel", &mut subscriber);

        send(&mut publisher, 1, 1);
        subscriber.drain_writer_to_reader();
        send(&mut publisher, 2, 2);

        subscriber.messages();
    }

    #[test]
    fn try_messages_reports_drops_without_panicking() {
        let mut subscriber = TestSubscriber::<i32>::with_capacity("channel".into(), 1);
        let mut publisher = connected_publisher("channel", &mut subscriber);

        send(&mut publisher, 1, 1);
        subscriber.drain_writer_to_reader();
        send(&mut publisher, 2, 2);

        let (messages, dropped) = subscriber.try_messages();
        assert_eq!(dropped, 1);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message, 2);
    }
}
