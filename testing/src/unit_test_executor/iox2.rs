use std::any::TypeId;
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use iceoryx2::prelude::{EventId, ZeroCopySend};
use task::callback::CallbackViews;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::iox2::{
    Iox2EventSubscriber, Iox2OpenCtx, Iox2Publisher, Iox2SpanInput, Iox2StagingInjector,
    Iox2Subscriber,
};
use task::message::{Message, MessageHeader};
use task::pub_sub_factory::{EndpointKind, Iox2EndpointInfo};
use task::publisher::PublisherConfig;
use task::task_graph_builder::TaskGraphBuildError;
use task::testing_time::TimeSource;

use super::UnitTestExecutorBuilder;

pub(super) trait Iox2FixturePort: Send {
    fn endpoint(&self) -> Iox2EndpointInfo;
    fn open(&self, context: &mut dyn Iox2OpenCtx) -> Result<(), TaskGraphBuildError>;
    fn close(&self);
}

struct TypedIox2Capture<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    channel: String,
    capacity: usize,
    subscriber: Arc<Mutex<Option<Iox2Subscriber<T>>>>,
}

impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2FixturePort for TypedIox2Capture<T> {
    fn endpoint(&self) -> Iox2EndpointInfo {
        Iox2EndpointInfo {
            channel: self.channel.clone(),
            kind: EndpointKind::Iox2DataSub {
                capacity: self.capacity,
            },
            payload_type: Some(TypeId::of::<T>()),
        }
    }

    fn open(&self, context: &mut dyn Iox2OpenCtx) -> Result<(), TaskGraphBuildError> {
        self.subscriber
            .lock()
            .expect("capture lock poisoned")
            .as_mut()
            .expect("capture subscriber missing")
            .iox2_open(context)
    }

    fn close(&self) {
        self.subscriber
            .lock()
            .expect("capture lock poisoned")
            .take();
    }
}

impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Drop for TypedIox2Capture<T> {
    fn drop(&mut self) {
        self.close();
    }
}

struct TypedIox2InputPublisher<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    channel: String,
    publisher: Arc<Mutex<Option<Iox2Publisher<T>>>>,
}

impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2FixturePort
    for TypedIox2InputPublisher<T>
{
    fn endpoint(&self) -> Iox2EndpointInfo {
        Iox2EndpointInfo {
            channel: self.channel.clone(),
            kind: EndpointKind::Iox2DataPub {
                capacity: 1,
                notify_on_send: false,
                event_id: 0,
            },
            payload_type: Some(TypeId::of::<T>()),
        }
    }

    fn open(&self, context: &mut dyn Iox2OpenCtx) -> Result<(), TaskGraphBuildError> {
        self.publisher
            .lock()
            .expect("iox2 publisher fixture lock poisoned")
            .as_mut()
            .expect("iox2 publisher fixture missing")
            .iox2_open(context)
    }

    fn close(&self) {
        self.publisher
            .lock()
            .expect("iox2 publisher fixture lock poisoned")
            .take();
    }
}

impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Drop for TypedIox2InputPublisher<T> {
    fn drop(&mut self) {
        self.close();
    }
}

/// Handle for capturing real iox2 output samples, including their headers.
pub struct Iox2TestSubscriber<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    subscriber: Arc<Mutex<Option<Iox2Subscriber<T>>>>,
    activation: ActivationCell,
}

impl<T> Iox2TestSubscriber<T>
where
    T: Clone + Debug + ZeroCopySend + Send + Sync + 'static,
{
    /// Drain available real iox2 samples and return cloned messages with headers.
    pub fn messages(&self) -> Vec<Message<T>> {
        self.try_messages()
            .unwrap_or_else(|error| panic!("{error}"))
    }

    /// Drain available samples, returning an error if the executor has been dropped.
    pub fn try_messages(&self) -> Result<Vec<Message<T>>, &'static str> {
        if !self.activation.load(Ordering::Acquire) {
            return Err("iox2 capture subscriber is not open until build() completes");
        }
        let mut subscriber = self.subscriber.lock().expect("capture lock poisoned");
        let subscriber = subscriber
            .as_mut()
            .ok_or("iox2 capture subscriber is closed")?;
        subscriber.drain_writer_to_reader();
        Ok(Iox2SpanInput::new(subscriber).inputs().cloned().collect())
    }
}

pub(super) type ActivationCell = Arc<AtomicBool>;

fn assert_active(activation: &ActivationCell) {
    assert!(
        activation.load(Ordering::Acquire),
        "iox2 test fixture cannot send until build() completes or after executor drop"
    );
}

/// Typed publisher that immediately emits real iox2 data after the executor is built.
pub struct Iox2TestPublisher<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    channel: String,
    activation: ActivationCell,
    time_source: Arc<TimeSource>,
    publisher: Arc<Mutex<Option<Iox2Publisher<T>>>>,
}

impl<T> Iox2TestPublisher<T>
where
    T: Debug + ZeroCopySend + Send + Sync + 'static,
{
    /// Immediately publish at the executor's current simulation time.
    /// Panics before build completes or after executor drop.
    pub fn send(&self, value: T) {
        self.send_with_header(MessageHeader::new(self.time_source.get()), value);
    }

    /// Immediately publish with a chosen header, without advancing simulation time.
    /// Panics before build completes or after executor drop.
    pub fn send_with_header(&self, header: MessageHeader, value: T) {
        assert_active(&self.activation);
        self.publisher
            .lock()
            .expect("iox2 publisher fixture lock poisoned")
            .as_ref()
            .expect("iox2 test fixture publisher is closed")
            .publish_with_header(header, value)
            .unwrap_or_else(|error| panic!("{error}"));
    }

    /// Channel receiving these samples.
    pub fn channel(&self) -> &str {
        &self.channel
    }
}

/// Handle for immediately staging counted event activations after the executor is built.
pub struct Iox2TestNotifier {
    channel: String,
    activation: ActivationCell,
    subscribers: Vec<Iox2StagingInjector>,
}

impl Iox2TestNotifier {
    /// Immediately stage one counted activation for every subscriber on the channel.
    /// Panics before build completes or after executor drop.
    pub fn notify(&self, event_id: EventId, count: u64) {
        assert_active(&self.activation);
        for subscriber in &self.subscribers {
            subscriber.notify(event_id, count);
        }
    }

    /// Channel receiving these event activations.
    pub fn channel(&self) -> &str {
        &self.channel
    }
}

impl UnitTestExecutorBuilder {
    /// Create a typed sender for a declared iox2 data input channel.
    pub fn add_iox2_test_publisher<T>(&mut self, channel: &str) -> Iox2TestPublisher<T>
    where
        T: Debug + ZeroCopySend + Send + Sync + 'static,
    {
        let mut found = false;
        let mut wrong_type = false;
        let mut native = false;
        for node in &mut self.nodes {
            for subscriber in node.callback_mut().collect_subscribers_mut() {
                if subscriber.config().channel_name != channel {
                    continue;
                }
                let mut info = None;
                let _ = subscriber.iox2_find_endpoints(&mut |endpoint| {
                    info = Some(endpoint);
                    Ok(())
                });
                match info {
                    Some(info) if matches!(info.kind, EndpointKind::Iox2DataSub { .. }) => {
                        if info.payload_type == Some(TypeId::of::<T>()) {
                            found = true;
                        } else {
                            wrong_type = true;
                        }
                    }
                    Some(info) if info.kind.is_iox2() => {}
                    _ => native = true,
                }
            }
        }
        assert!(
            !native,
            "Channel '{channel}' has a native subscriber; iox2 and native transports cannot be mixed"
        );
        assert!(
            found,
            "{}",
            if wrong_type {
                format!("Type mismatch connecting iox2 test publisher to channel '{channel}'")
            } else {
                format!("No iox2 data subscriber for channel '{channel}'")
            }
        );
        let mut publisher = Iox2Publisher::<T>::new(PublisherConfig {
            capacity: 1,
            channel_name: channel.to_owned(),
        });
        publisher.notify_on_send = false;
        let publisher = Arc::new(Mutex::new(Some(publisher)));
        self.iox2_fixtures.push(Box::new(TypedIox2InputPublisher {
            channel: channel.to_owned(),
            publisher: Arc::clone(&publisher),
        }));
        Iox2TestPublisher {
            channel: channel.to_owned(),
            activation: Arc::clone(&self.iox2_activation),
            time_source: Arc::clone(&self.time_source),
            publisher,
        }
    }

    /// Create an injector for a declared iox2 event input channel.
    pub fn add_iox2_test_notifier(&mut self, channel: &str) -> Iox2TestNotifier {
        let mut subscribers = Vec::new();
        let mut native_found = false;
        for node in &mut self.nodes {
            for subscriber in node.callback_mut().collect_subscribers_mut() {
                if subscriber.config().channel_name == channel {
                    if let Some(event) = subscriber.as_any().downcast_mut::<Iox2EventSubscriber>() {
                        subscribers.push(event.staging_injector());
                        continue;
                    }
                    let mut info = None;
                    let _ = subscriber.iox2_find_endpoints(&mut |endpoint| {
                        info = Some(endpoint);
                        Ok(())
                    });
                    match info {
                        Some(info) if info.kind.is_iox2() => {}
                        _ => native_found = true,
                    }
                }
            }
        }
        assert!(
            !native_found,
            "Channel '{channel}' has a native subscriber; iox2 and native transports cannot be mixed"
        );
        assert!(
            !subscribers.is_empty(),
            "No iox2 event subscriber for channel '{channel}'"
        );
        Iox2TestNotifier {
            channel: channel.to_owned(),
            activation: Arc::clone(&self.iox2_activation),
            subscribers,
        }
    }

    /// Attach a real iox2 subscriber fixture for output capture.
    pub fn add_iox2_test_subscriber<T>(&mut self, channel: &str) -> Iox2TestSubscriber<T>
    where
        T: Clone + Debug + ZeroCopySend + Send + Sync + 'static,
    {
        self.add_iox2_test_subscriber_with_capacity(channel, 8)
    }

    /// Attach a real iox2 output capture fixture with an explicit queue capacity.
    pub fn add_iox2_test_subscriber_with_capacity<T>(
        &mut self,
        channel: &str,
        capacity: usize,
    ) -> Iox2TestSubscriber<T>
    where
        T: Clone + Debug + ZeroCopySend + Send + Sync + 'static,
    {
        assert!(
            capacity > 0,
            "iox2 capture capacity must be positive for channel {channel}"
        );
        let publishers = self
            .nodes
            .iter_mut()
            .flat_map(|node| node.callback_mut().collect_publishers_mut())
            .filter(|publisher| publisher.config().channel_name == channel)
            .collect::<Vec<_>>();
        assert!(
            !publishers.is_empty(),
            "No publisher for iox2 capture channel '{channel}'"
        );
        assert!(
            publishers
                .iter()
                .all(|publisher| publisher.value_type_id() == TypeId::of::<T>()),
            "Type mismatch connecting iox2 test subscriber to channel '{channel}'"
        );
        let subscriber = Arc::new(Mutex::new(Some(Iox2Subscriber::<T>::new(
            task::subscriber::SubscriberConfig {
                is_optional: true,
                capacity,
                is_trigger: false,
                keep_across_runs: true,
                channel_name: channel.to_owned(),
            },
        ))));
        self.iox2_fixtures.push(Box::new(TypedIox2Capture {
            channel: channel.to_owned(),
            capacity,
            subscriber: Arc::clone(&subscriber),
        }));
        Iox2TestSubscriber {
            subscriber,
            activation: Arc::clone(&self.iox2_activation),
        }
    }
}

pub(super) fn activate(cell: &ActivationCell) {
    cell.store(true, Ordering::Release);
}

pub(super) fn inactive_cell() -> ActivationCell {
    Arc::new(AtomicBool::new(false))
}

pub(super) fn closed(cell: &ActivationCell) {
    cell.store(false, Ordering::Release);
}
