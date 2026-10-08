use crate::{LoanError, Publisher, testing_time::TimeSource};
use std::sync::Arc;

/// A typed fixture endpoint borrowing the application's graph storage.
/// Sends use the executor's current simulated time, including after an idle step.
pub struct TestPublisher<'storage, T> {
    publisher: Publisher<'storage, T>,
    time: Arc<TimeSource>,
}

impl<'storage, T> TestPublisher<'storage, T> {
    pub fn new(publisher: Publisher<'storage, T>, time: Arc<TimeSource>) -> Self {
        Self { publisher, time }
    }

    pub fn try_send(&mut self, value: T) -> Result<(), LoanError> {
        self.publisher.publish(value)?;
        self.publisher.flush(self.time.get());
        Ok(())
    }

    pub fn send(&mut self, value: T) {
        self.try_send(value).expect("test publication failed");
    }

    pub fn send_copied(&mut self, value: &T)
    where
        T: Clone,
    {
        self.send(value.clone());
    }
}
