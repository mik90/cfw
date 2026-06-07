use task::{
    output::Output,
    publisher::{GenericPublisher, Publisher},
    time::FrameworkTime,
};

use std::sync::{Arc, Mutex};

/// Publisher that can send messages to a ConnectedCallback
pub struct TestPublisher<T> {
    publisher: Publisher<T>,

    /// Callback for getting time from the executor
    executor_time_source: Arc<Mutex<Box<dyn Fn() -> FrameworkTime>>>,
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
