use crate::{
    arena::ArenaReaderPtr,
    callback::CallbackReadiness,
    generic_subscriber::QueueInfo,
    message::Message,
    mpsc_queue::MpscQueue,
    subscriber::{GenericSubscriber, SubscriberConfig},
};
use std::sync::Arc;

/// Subscriber that can listen to messages. Has 'infinite' heap buffer.
pub struct TestSubscriber<T> {
    /// Must be MPSC since, even in unit test, tasks may run in separate threads
    queue: MpscQueue<ArenaReaderPtr<Message<T>>>,

    config: SubscriberConfig,
}

fn make_test_config(channel: String) -> SubscriberConfig {
    SubscriberConfig {
        is_optional: true,
        capacity: usize::MAX,
        is_trigger: false,
        keep_across_runs: true,
        channel_name: channel,
    }
}

impl<T> TestSubscriber<T> {
    /// How many messages are in the queue
    pub fn queue_len(&self) -> usize {
        self.queue.len()
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
