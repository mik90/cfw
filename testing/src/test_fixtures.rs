use std::sync::{Arc, Mutex};

use task::message::Message;
use task::testing_publisher as task_publisher;
use task::testing_subscriber as task_subscriber;

use crate::unit_test_executor::TestSessionStorage;

pub struct TestSubscriber<T: Send + Sync + 'static> {
    pub(crate) inner: Arc<Mutex<task_subscriber::TestSubscriber<T>>>,
    _session: Arc<TestSessionStorage>,
}

impl<T: Send + Sync + 'static> TestSubscriber<T> {
    pub(crate) fn new(
        inner: Arc<Mutex<task_subscriber::TestSubscriber<T>>>,
        session: Arc<TestSessionStorage>,
    ) -> Self {
        Self {
            inner,
            _session: session,
        }
    }
}

impl<T: Clone + Send + Sync + 'static> TestSubscriber<T> {
    pub fn messages(&mut self) -> Vec<Box<Message<T>>> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .messages()
    }

    pub fn try_messages(&mut self) -> (Vec<Box<Message<T>>>, task_subscriber::DroppedMessages) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .try_messages()
    }
}

pub struct TestPublisher<T: Send + Sync + 'static> {
    pub(crate) inner: Arc<Mutex<task_publisher::TestPublisher<T>>>,
    _session: Arc<TestSessionStorage>,
}

impl<T: Send + Sync + 'static> TestPublisher<T> {
    pub(crate) fn new(
        inner: Arc<Mutex<task_publisher::TestPublisher<T>>>,
        session: Arc<TestSessionStorage>,
    ) -> Self {
        Self {
            inner,
            _session: session,
        }
    }
}

impl<T: Default + Send + Sync + 'static> TestPublisher<T> {
    pub fn send(&mut self, message: T) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(message);
    }
}

impl<T: Default + Send + Sync + Clone + 'static> TestPublisher<T> {
    pub fn send_copied(&mut self, message: &T) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send_copied(message);
    }
}
