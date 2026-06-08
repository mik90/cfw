use crate::{
    arena::ArenaReaderPtr,
    callback::CallbackReadiness,
    generic_subscriber::QueueInfo,
    message::Message,
    mpsc_queue::MpscQueue,
    pub_sub::ChannelName,
    subscriber::{GenericSubscriber, SubscriberConfig},
};
use std::sync::Arc;

/// Subscriber that captures messages published on a channel for inspection in tests.
/// Connects to a real `Publisher` as a "foreign" (zero-copy, non-arena) subscriber —
/// see `Publisher::add_foreign_subscriber`. Holds `ArenaReaderPtr`s in a small bounded
/// queue; once full, the oldest entry is displaced (and its arena slot released).
pub struct TestSubscriber<T> {
    /// Shared with the publisher's foreign-subscriber sink list, so messages can be
    /// pushed in directly without going through the `Subscriber`/`DoubleBuffer` machinery.
    /// Must be MPSC since, even in unit test, tasks may run in separate threads.
    queue: Arc<MpscQueue<ArenaReaderPtr<Message<T>>>>,

    config: SubscriberConfig,
}

fn make_test_config(channel: ChannelName, capacity: usize) -> SubscriberConfig {
    SubscriberConfig {
        is_optional: true,
        capacity,
        is_trigger: false,
        keep_across_runs: true,
        channel_name: channel,
    }
}

impl<T> TestSubscriber<T> {
    pub fn new(channel_name: ChannelName, capacity: usize) -> Self {
        TestSubscriber {
            queue: Arc::new(MpscQueue::new(capacity)),
            config: make_test_config(channel_name, capacity),
        }
    }

    /// How many messages are in the queue
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Handle to the underlying queue, for the publisher to push zero-copy
    /// `ArenaReaderPtr`s into directly during connection.
    pub(crate) fn queue_handle(&self) -> Arc<MpscQueue<ArenaReaderPtr<Message<T>>>> {
        self.queue.clone()
    }
}

impl<T: 'static + Clone> TestSubscriber<T> {
    /// Consumes all queued messages and clones them as boxed
    pub fn messages(&'_ mut self) -> Vec<Box<Message<T>>> {
        let mut messages = vec![];
        while let Some(buffer_message) = self.queue.pop() {
            let message: &Message<T> = &buffer_message;
            let boxed_message = Box::<Message<T>>::new(message.clone());
            messages.push(boxed_message);
        }
        messages
    }
}

impl<T: 'static> GenericSubscriber for TestSubscriber<T> {
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn get_config(&self) -> &SubscriberConfig {
        &self.config
    }

    fn get_config_mut(&mut self) -> &mut SubscriberConfig {
        &mut self.config
    }

    fn able_to_run(&self) -> bool {
        false
    }

    fn requests_execution(&self) -> bool {
        false
    }

    fn drain_writer_to_reader(&self) {
        // No-op, we always have a single buffer
    }

    fn get_queue_info(&self) -> QueueInfo {
        QueueInfo {
            reader_size: self.queue.len(),
            writer_size: 0, // Single buffer, so this is always 0
        }
    }

    fn cleanup_buffers(&self) {
        self.queue.clear();
    }

    fn set_readiness_state(&mut self, _state: Arc<CallbackReadiness>, _bit_index: usize) {
        // No-op
    }

    fn get_readiness_state(&self) -> Option<(Arc<CallbackReadiness>, usize)> {
        None
    }
}
