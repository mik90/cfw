use crate::{output::Output, publisher::Publisher, testing_time::TimeSource};

use std::sync::MutexGuard;
use std::sync::{Arc, Mutex};

/// Publisher that can send messages to a CallbackNode
pub struct TestPublisher<T> {
    publisher: Arc<Mutex<Publisher<T>>>,

    /// Callback for getting time from the executor
    executor_time_source: Arc<TimeSource>,
}

impl<T> TestPublisher<T> {
    pub(crate) fn publisher_guard<'a>(&'a self) -> MutexGuard<'a, Publisher<T>> {
        self.publisher.lock().expect("publisher lock failed")
    }
}

// This real channel fixture moves values/final drops between workers (`Send`),
// fans out immutable reads (`Sync`), and retains queued values (`'static`).
impl<T: Send + Sync + 'static> TestPublisher<T> {
    pub fn new(publisher: Arc<Mutex<Publisher<T>>>, time_source: Arc<TimeSource>) -> Self {
        TestPublisher {
            publisher,
            executor_time_source: time_source,
        }
    }
}

// Sending moves values/final drops to workers (`Send`), fans out shared reads
// (`Sync`), and may retain values in queues (`'static`).
impl<T: Default + Send + Sync + 'static> TestPublisher<T> {
    /// Sends a message, immediately flushing loaned values
    pub fn send(&mut self, message: T) {
        let mut publisher_guard = self.publisher_guard();
        let mut output = Output::new_default(&mut publisher_guard);
        *output = message;
        output.send();

        let timestamp = self.executor_time_source.get();
        publisher_guard.flush_loaned_values(timestamp);
    }
}

// Copied sends have the same cross-worker (`Send`), shared-read (`Sync`), and
// queue-retention (`'static`) requirements.
impl<T: Default + Send + Sync + 'static + Clone> TestPublisher<T> {
    /// Sends a message, immediately flushing loaned values
    /// Avoids putting a large type on the heap.
    pub fn send_copied(&mut self, message: &T) {
        let mut publisher_guard = self.publisher_guard();
        // TODO: This still dumps a type on the stack. How can we use OutputUninit here?
        // We should be able to clone the message into the OutputUninit.
        // I think we need the message type to impl ToOwned!!! and not Clone
        let mut output = Output::new_default(&mut publisher_guard);
        *output = message.clone();
        output.send();

        let timestamp = self.executor_time_source.get();
        publisher_guard.flush_loaned_values(timestamp);
    }
}
