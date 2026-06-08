use crate::{
    generic_publisher::ConnectionTypeMismatch,
    generic_subscriber::GenericSubscriber,
    output::Output,
    pub_sub::ChannelName,
    publisher::{GenericPublisher, Publisher, PublisherConfig},
    time::FrameworkTime,
};

use std::any::Any;
use std::sync::{Arc, Mutex};

/// Publisher that can send messages to a ConnectedCallback
pub struct TestPublisher<T> {
    publisher: Publisher<T>,

    /// Callback for getting time from the executor
    executor_time_source: Arc<Mutex<Box<dyn Fn() -> FrameworkTime>>>,
}

impl<T> TestPublisher<T> {
    pub fn new(
        channel_name: ChannelName,
        capacity: usize,
        time_source: Arc<Mutex<Box<dyn Fn() -> FrameworkTime>>>,
    ) -> Self {
        TestPublisher {
            publisher: Publisher::new(PublisherConfig {
                capacity,
                channel_name,
            }),
            executor_time_source: time_source,
        }
    }
}

impl<T: 'static> GenericPublisher for TestPublisher<T> {
    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn get_config(&self) -> &PublisherConfig {
        self.publisher.get_config()
    }

    fn get_config_mut(&mut self) -> &mut PublisherConfig {
        self.publisher.get_config_mut()
    }

    fn get_forwarded_channels(&self) -> &[ChannelName] {
        self.publisher.get_forwarded_channels()
    }

    fn flush_loaned_values(&mut self, timestamp: FrameworkTime) {
        self.publisher.flush_loaned_values(timestamp);
    }

    fn allocate_arena(&mut self) {
        self.publisher.allocate_arena();
    }

    fn increase_arena_size(&mut self, additional_capacity: usize) {
        self.publisher.increase_arena_size(additional_capacity);
    }

    fn connect_to_subscriber(
        &mut self,
        subscriber: &mut dyn GenericSubscriber,
    ) -> Result<(), ConnectionTypeMismatch> {
        self.publisher.connect_to_subscriber(subscriber)
    }
}

impl<T: Default + 'static> TestPublisher<T> {
    /// Sends a message, immediately flushing loaned values
    pub fn send(&mut self, message: T) {
        let mut output = Output::new_default(&mut self.publisher);
        *output = message;
        output.send();

        self.flush_loaned_values();
    }

    /// Sends a message, immediately flushing loaned values
    /// Avoids putting a large type on the heap.
    pub fn send_boxed(&mut self, message: Box<T>) {
        let mut output = Output::new_default(&mut self.publisher);
        *output = *message;
        output.send();

        self.flush_loaned_values();
    }

    fn flush_loaned_values(&mut self) {
        let callback_guard = self.executor_time_source.lock().unwrap();

        let timestamp = callback_guard();
        self.publisher.flush_loaned_values(timestamp);
    }
}
