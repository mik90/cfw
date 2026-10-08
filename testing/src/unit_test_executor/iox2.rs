use super::BoundUnitTestExecutorBuilder;
use iceoryx2::prelude::{EventId, ZeroCopySend};
use std::{
    fmt::Debug,
    sync::{Arc, Mutex, Weak},
};
use task::{
    LoanError,
    iox2::{Iox2OptionalInput, Iox2Publisher, Iox2SpanInput, Iox2Subscriber},
    message::{Message, MessageHeader},
    testing_time::TimeSource,
};

type Events = Mutex<Vec<(String, EventId, u64)>>;
pub(super) type PendingEvents = Arc<Events>;

/// Publishes real middleware samples without implicit event notifications.
/// Bound fixture ports own their middleware resources independently of execution.
pub struct Iox2TestPublisher<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    publisher: Iox2Publisher<T>,
    time: Arc<TimeSource>,
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2TestPublisher<T> {
    pub fn channel(&self) -> &str {
        self.publisher.channel_name()
    }
    pub fn try_send(&self, value: T) -> Result<(), LoanError> {
        self.publisher
            .publish_with_header(MessageHeader::new(self.time.get()), value)
    }
    pub fn send(&self, value: T) {
        self.try_send(value)
            .expect("iox2 fixture publication failed");
    }
    pub fn send_with_header(&self, header: MessageHeader, value: T) {
        self.publisher
            .publish_with_header(header, value)
            .expect("iox2 fixture publication failed");
    }
}

/// Counted events are staged for the next step, in notification order.
/// They do not generate duplicate kernel notifications.
pub struct Iox2TestNotifier {
    channel: String,
    events: Weak<Events>,
}
impl Iox2TestNotifier {
    pub fn channel(&self) -> &str {
        &self.channel
    }
    pub fn notify(&self, id: EventId, count: u64) {
        let events = self.events.upgrade().expect("unit test executor is closed");
        events
            .lock()
            .unwrap()
            .push((self.channel.clone(), id, count));
    }
}

/// Capture endpoint with middleware-owned samples copied into test-owned values.
pub struct Iox2TestSubscriber<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    subscriber: Iox2Subscriber<T>,
}
impl<T: Debug + ZeroCopySend + Send + Sync + Clone + 'static> Iox2TestSubscriber<T> {
    pub fn try_messages(&self) -> Result<Vec<Message<T>>, &'static str> {
        self.subscriber.update();
        if self.subscriber.receive_errors() != 0 {
            return Err("iox2 fixture receive failed");
        }
        let messages: Vec<_> = Iox2SpanInput::new(&self.subscriber)
            .inputs()
            .cloned()
            .collect();
        let mut input = Iox2OptionalInput::new(&self.subscriber);
        for _ in 0..messages.len() {
            input.clear();
        }
        Ok(messages)
    }
    pub fn messages(&self) -> Vec<Message<T>> {
        self.try_messages().expect("iox2 fixture capture failed")
    }
}

impl BoundUnitTestExecutorBuilder<'_> {
    pub fn add_iox2_test_publisher<T: Debug + ZeroCopySend + Send + Sync + 'static>(
        &self,
        publisher: Iox2Publisher<T>,
    ) -> Iox2TestPublisher<T> {
        Iox2TestPublisher {
            publisher,
            time: self.time.clone(),
        }
    }
    /// Unknown event channels are reported by `try_step`.
    pub fn add_iox2_test_notifier(&self, channel: &str) -> Iox2TestNotifier {
        Iox2TestNotifier {
            channel: channel.into(),
            events: Arc::downgrade(&self.events),
        }
    }
    pub fn add_iox2_test_subscriber<T: Debug + ZeroCopySend + Send + Sync + 'static>(
        &self,
        subscriber: Iox2Subscriber<T>,
    ) -> Iox2TestSubscriber<T> {
        Iox2TestSubscriber { subscriber }
    }
}
