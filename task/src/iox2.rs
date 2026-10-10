//! Typed iceoryx2 channels. All processes must agree on service-wide settings.
use crate::{
    EndpointError, LoanError, StorageError, StorageLayout,
    message::{Message, MessageHeader},
    time::FrameworkTime,
    wake::{Wake, WakeHandle},
};
use iceoryx2::{
    node::Node,
    port::{
        listener::Listener, notifier::Notifier, publisher::Publisher as DataPublisher,
        subscriber::Subscriber as DataSubscriber,
    },
    prelude::*,
    sample::Sample,
    sample_mut::SampleMut,
    service::{
        ipc_threadsafe,
        port_factory::{
            event::PortFactory as EventService, publish_subscribe::PortFactory as DataService,
        },
    },
};
use std::{
    cell::{RefCell, RefMut},
    collections::{HashSet, VecDeque},
    fmt::Debug,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

/// Owns the graph's transport node. Ports and readiness registrations retain it.
pub struct Iox2Runtime {
    node: Node<ipc_threadsafe::Service>,
}
impl Iox2Runtime {
    pub fn new() -> Result<Arc<Self>, StorageError> {
        Self::with_config(Config::global_config())
    }
    pub fn with_config(config: &Config) -> Result<Arc<Self>, StorageError> {
        Ok(Arc::new(Self {
            node: NodeBuilder::new()
                .config(config)
                .signal_handling_mode(SignalHandlingMode::Disabled)
                .create::<ipc_threadsafe::Service>()
                .map_err(transport)?,
        }))
    }
}
fn transport(error: impl std::fmt::Display) -> StorageError {
    StorageError::Transport(error.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Iox2ChannelConfig {
    pub buffer_capacity: usize,
    pub max_borrowed_samples: usize,
    pub max_publishers: usize,
    pub max_subscribers: usize,
    pub max_nodes: usize,
    pub max_listeners: usize,
    pub max_notifiers: usize,
    pub event_id_max_value: usize,
}
impl Default for Iox2ChannelConfig {
    fn default() -> Self {
        Self {
            buffer_capacity: 1024,
            max_borrowed_samples: 2048,
            max_publishers: 8,
            max_subscribers: 16,
            max_nodes: 8,
            max_listeners: 16,
            max_notifiers: 8,
            event_id_max_value: 0,
        }
    }
}

struct Key {
    channel: Arc<str>,
    index: usize,
}
pub struct Iox2PublisherKey<T>(Key, PhantomData<fn(T) -> T>);
pub struct Iox2SubscriberKey<T>(Key, PhantomData<fn(T) -> T>);
impl<T> Iox2PublisherKey<T> {
    pub(crate) fn index(&self) -> usize {
        self.0.index
    }
}
impl<T> Iox2SubscriberKey<T> {
    pub(crate) fn index(&self) -> usize {
        self.0.index
    }
}
pub struct Iox2EventKey(Key);
pub struct Iox2NotifierKey(Key);

pub trait Iox2EventPlan {
    fn name(&self) -> &str;
    fn events(&mut self, capacity: usize) -> Iox2EventKey;
    fn notifier(&mut self) -> Iox2NotifierKey;
}
pub trait Iox2EventBindings {
    fn take_event(&self, key: &Iox2EventKey) -> Result<Iox2EventSubscriber, EndpointError>;
    fn take_notifier(&self, key: &Iox2NotifierKey) -> Result<Iox2Notifier, EndpointError>;
}

/// Notification accompanying each committed data sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Iox2Notification {
    Silent,
    Event(usize),
}

struct PublisherSpec {
    capacity: usize,
    notification: Iox2Notification,
}

/// One named IPC channel, with data and optional event-trigger endpoints.
pub struct Iox2ChannelPlan<T> {
    name: Arc<str>,
    runtime: Arc<Iox2Runtime>,
    config: Iox2ChannelConfig,
    publishers: Vec<PublisherSpec>,
    subscribers: Vec<usize>,
    subscriber_sources: Vec<Option<Vec<u32>>>,
    events: Vec<usize>,
    notifiers: Vec<usize>,
    payload: PhantomData<fn(T) -> T>,
}
impl<T> Iox2ChannelPlan<T> {
    pub fn topology(&self) -> crate::automatic::topology::ChannelTopology {
        crate::automatic::topology::ChannelTopology {
            name: self.name.to_string(),
            payload_type: std::any::type_name::<T>(),
            transport: crate::recording::Transport::Ipc,
            publishers: self.publishers.iter().map(|port| port.capacity).collect(),
            subscribers: self.subscribers.clone(),
            sources: self
                .subscriber_sources
                .iter()
                .map(|sources| {
                    sources
                        .as_ref()
                        .map(|ids| ids.iter().map(|id| *id as usize).collect())
                })
                .collect(),
        }
    }
    pub fn config(&self) -> &Iox2ChannelConfig {
        &self.config
    }
    pub fn set_config(&mut self, config: Iox2ChannelConfig) {
        self.config = config;
    }
    /// Minimum local service limits from the completed endpoint declarations.
    pub fn required_config(&self) -> Result<Iox2ChannelConfig, StorageError> {
        let buffer_capacity = self.subscribers.iter().copied().max().unwrap_or(1);
        let event_ids = self.publishers.iter().filter_map(|p| match p.notification {
            Iox2Notification::Silent => None,
            Iox2Notification::Event(id) => Some(id),
        });
        Ok(Iox2ChannelConfig {
            buffer_capacity,
            max_borrowed_samples: buffer_capacity
                .checked_mul(2)
                .ok_or(StorageError::CapacityOverflow)?,
            max_publishers: self.publishers.len().max(1),
            max_subscribers: self.subscribers.len().max(1),
            max_nodes: 1,
            max_listeners: self.events.len(),
            max_notifiers: self
                .publishers
                .iter()
                .filter(|p| matches!(p.notification, Iox2Notification::Event(_)))
                .count()
                .checked_add(self.notifiers.len())
                .ok_or(StorageError::CapacityOverflow)?,
            event_id_max_value: event_ids
                .chain(self.notifiers.iter().copied())
                .max()
                .unwrap_or(0),
        })
    }
    pub(crate) fn publisher_key(&self, index: usize) -> Iox2PublisherKey<T> {
        assert!(index < self.publishers.len());
        Iox2PublisherKey(
            Key {
                channel: self.name.clone(),
                index,
            },
            PhantomData,
        )
    }
    pub(crate) fn subscriber_key(&self, index: usize) -> Iox2SubscriberKey<T> {
        assert!(index < self.subscribers.len());
        Iox2SubscriberKey(
            Key {
                channel: self.name.clone(),
                index,
            },
            PhantomData,
        )
    }
    pub fn new(name: impl Into<String>, runtime: &Arc<Iox2Runtime>) -> Self {
        Self {
            name: name.into().into(),
            runtime: runtime.clone(),
            config: Iox2ChannelConfig::default(),
            publishers: Vec::new(),
            subscribers: Vec::new(),
            subscriber_sources: Vec::new(),
            events: Vec::new(),
            notifiers: Vec::new(),
            payload: PhantomData,
        }
    }
    pub fn with_config(mut self, config: Iox2ChannelConfig) -> Self {
        self.config = config;
        self
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn publisher(&mut self, capacity: usize) -> Iox2PublisherKey<T> {
        self.publisher_with_notification(capacity, Iox2Notification::Event(0))
    }
    /// Event IDs must fit the channel's configured event_id_max_value.
    pub fn publisher_with_notification(
        &mut self,
        capacity: usize,
        notification: Iox2Notification,
    ) -> Iox2PublisherKey<T> {
        let index = self.publishers.len();
        self.publishers.push(PublisherSpec {
            capacity,
            notification,
        });
        Iox2PublisherKey(
            Key {
                channel: self.name.clone(),
                index,
            },
            PhantomData,
        )
    }
    /// Configure a declared publisher before allocating storage (including keys
    /// obtained from a generated task declaration).
    pub fn set_publisher_notification(
        &mut self,
        key: &Iox2PublisherKey<T>,
        notification: Iox2Notification,
    ) -> Result<(), EndpointError> {
        if self.name != key.0.channel {
            return Err(EndpointError::WrongChannel);
        }
        self.publishers
            .get_mut(key.0.index)
            .ok_or(EndpointError::InvalidIndex(key.0.index))?
            .notification = notification;
        Ok(())
    }
    pub fn subscriber(&mut self, capacity: usize) -> Iox2SubscriberKey<T> {
        let index = self.subscribers.len();
        self.subscribers.push(capacity);
        self.subscriber_sources.push(None);
        Iox2SubscriberKey(
            Key {
                channel: self.name.clone(),
                index,
            },
            PhantomData,
        )
    }
    /// Filter publications by plan-local publisher identity before retention.
    pub fn restrict_subscriber_sources(
        &mut self,
        subscriber: &Iox2SubscriberKey<T>,
        publishers: &[Iox2PublisherKey<T>],
    ) -> Result<(), EndpointError> {
        if subscriber.0.channel != self.name
            || publishers.iter().any(|key| key.0.channel != self.name)
        {
            return Err(EndpointError::WrongChannel);
        }
        for key in publishers {
            if key.0.index >= self.publishers.len() {
                return Err(EndpointError::InvalidIndex(key.0.index));
            }
        }
        *self
            .subscriber_sources
            .get_mut(subscriber.0.index)
            .ok_or(EndpointError::InvalidIndex(subscriber.0.index))? =
            Some(publishers.iter().map(|key| key.0.index as u32).collect());
        Ok(())
    }
    pub fn events(&mut self, capacity: usize) -> Iox2EventKey {
        let index = self.events.len();
        self.events.push(capacity);
        Iox2EventKey(Key {
            channel: self.name.clone(),
            index,
        })
    }
    pub fn notifier(&mut self) -> Iox2NotifierKey {
        self.notifier_with_id(0)
    }
    pub fn notifier_with_id(&mut self, event_id: usize) -> Iox2NotifierKey {
        let index = self.notifiers.len();
        self.notifiers.push(event_id);
        Iox2NotifierKey(Key {
            channel: self.name.clone(),
            index,
        })
    }
}
impl<T> Iox2EventPlan for Iox2ChannelPlan<T> {
    fn name(&self) -> &str {
        self.name()
    }
    fn events(&mut self, capacity: usize) -> Iox2EventKey {
        self.events(capacity)
    }
    fn notifier(&mut self) -> Iox2NotifierKey {
        self.notifier()
    }
}
impl<T> crate::storage::private::Sealed for Iox2ChannelPlan<T> {}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> StorageLayout for Iox2ChannelPlan<T> {
    type Storage = Iox2ChannelStorage<T>;
    fn validate(&self) -> Result<(), StorageError> {
        u32::try_from(self.publishers.len()).map_err(|_| StorageError::CapacityOverflow)?;
        for publisher in &self.publishers {
            u32::try_from(publisher.capacity).map_err(|_| StorageError::CapacityOverflow)?;
        }
        if self
            .publishers
            .iter()
            .map(|p| &p.capacity)
            .chain(&self.subscribers)
            .chain(&self.events)
            .any(|&n| n == 0)
            || self
                .subscribers
                .iter()
                .any(|&n| n > self.config.buffer_capacity)
            || self.config.buffer_capacity == 0
            || self.config.max_borrowed_samples < self.config.buffer_capacity.saturating_mul(2)
            || self.publishers.len() > self.config.max_publishers
            || self.subscribers.len() > self.config.max_subscribers
            || self.events.len() > self.config.max_listeners
            || self.publishers.iter().filter(|p| matches!(p.notification, Iox2Notification::Event(_))).count()
                + self.notifiers.len() > self.config.max_notifiers
            || self.publishers.iter().any(|p| matches!(p.notification, Iox2Notification::Event(id) if id > self.config.event_id_max_value))
            || self
                .notifiers
                .iter()
                .any(|&id| id > self.config.event_id_max_value)
        {
            return Err(transport(format!(
                "invalid endpoint/service capacities on '{}': required {:?}, configured {:?} (borrowed samples must cover twice the service buffer)",
                self.name, self.required_config()?, self.config
            )));
        }
        Ok(())
    }
    fn validate_channel_names(&self, names: &mut HashSet<String>) -> Result<(), StorageError> {
        if !names.insert(self.name.to_string()) {
            return Err(StorageError::DuplicateChannel(self.name.to_string()));
        }
        Ok(())
    }
    fn allocate(self) -> Result<Self::Storage, StorageError> {
        let name = ServiceName::new(&self.name).map_err(transport)?;
        let data = if self.publishers.is_empty() && self.subscribers.is_empty() {
            None
        } else {
            Some(
                self.runtime
                    .node
                    .service_builder(&name)
                    .publish_subscribe::<Message<T>>()
                    .subscriber_max_buffer_size(self.config.buffer_capacity)
                    .subscriber_max_borrowed_samples(self.config.max_borrowed_samples)
                    .history_size(0)
                    .enable_safe_overflow(true)
                    .max_publishers(self.config.max_publishers)
                    .max_subscribers(self.config.max_subscribers)
                    .max_nodes(self.config.max_nodes)
                    .open_or_create()
                    .map_err(|error| {
                        transport(format!(
                            "IPC data service '{}', requested {:?}: {error:?}",
                            self.name, self.config
                        ))
                    })?,
            )
        };
        let event = self
            .runtime
            .node
            .service_builder(&name)
            .event()
            .max_listeners(self.config.max_listeners)
            .max_notifiers(self.config.max_notifiers)
            .max_nodes(self.config.max_nodes)
            .event_id_max_value(self.config.event_id_max_value)
            .open_or_create()
            .map_err(|error| {
                transport(format!(
                    "IPC event service '{}', requested {:?}: {error:?}",
                    self.name, self.config
                ))
            })?;
        Ok(Iox2ChannelStorage {
            plan: self,
            data,
            event,
        })
    }
}

pub struct Iox2ChannelStorage<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    data: Option<DataService<ipc_threadsafe::Service, Message<T>, ()>>,
    event: EventService<ipc_threadsafe::Service>,
    // Keep the node alive until both service factories have been released.
    plan: Iox2ChannelPlan<T>,
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2ChannelStorage<T> {
    pub fn build(&self) -> Result<Iox2Bindings<T>, StorageError> {
        let mut publishers = Vec::new();
        for (index, spec) in self.plan.publishers.iter().enumerate() {
            let capacity = spec.capacity;
            publishers.push(RefCell::new(Some(Iox2Publisher {
                channel: self.plan.name.to_string(),
                publisher_index: u32::try_from(index).map_err(transport)?,
                capacity,
                port: self
                    .data
                    .as_ref()
                    .expect("publisher has a data service")
                    .publisher_builder()
                    .max_loaned_samples(capacity)
                    .create()
                    .map_err(transport)?,
                notifier: match spec.notification {
                    Iox2Notification::Silent => None,
                    Iox2Notification::Event(id) => Some(
                        self.event
                            .notifier_builder()
                            .default_event_id(EventId::new(id))
                            .create()
                            .map_err(transport)?,
                    ),
                },
                pending: Vec::with_capacity(capacity),
                observers: Vec::new(),
                replay_only: false,
                runtime: self.plan.runtime.clone(),
            })));
        }
        let mut subscribers = Vec::new();
        for (index, &capacity) in self.plan.subscribers.iter().enumerate() {
            subscribers.push(RefCell::new(Some(Iox2Subscriber {
                channel: self.plan.name.to_string(),
                capacity,
                port: self
                    .data
                    .as_ref()
                    .expect("subscriber has a data service")
                    .subscriber_builder()
                    .buffer_size(capacity)
                    .create()
                    .map_err(transport)?,
                read: RefCell::new(VecDeque::with_capacity(capacity)),
                receive_errors: AtomicUsize::new(0),
                replay: None,
                sources: self.plan.subscriber_sources[index].clone(),
                _runtime: self.plan.runtime.clone(),
            })));
        }
        let mut events = Vec::new();
        for &capacity in &self.plan.events {
            events.push(RefCell::new(Some(Iox2EventSubscriber {
                channel: self.plan.name.to_string(),
                capacity,
                listener: Some(self.event.listener_builder().create().map_err(transport)?),
                staging: Arc::new(base::mpsc_queue::MpscQueue::new(capacity)),
                read: RefCell::new(VecDeque::with_capacity(capacity)),
                wake: None,
                runtime: self.plan.runtime.clone(),
            })));
        }
        let mut notifiers = Vec::new();
        for &id in &self.plan.notifiers {
            notifiers.push(RefCell::new(Some(Iox2Notifier {
                channel: self.plan.name.to_string(),
                pending: false,
                event_id: id,
                replay_only: false,
                port: self
                    .event
                    .notifier_builder()
                    .default_event_id(EventId::new(id))
                    .create()
                    .map_err(transport)?,
                _runtime: self.plan.runtime.clone(),
            })));
        }
        Ok(Iox2Bindings {
            channel: self.plan.name.clone(),
            publishers,
            subscribers,
            events,
            notifiers,
        })
    }
}

pub struct Iox2Bindings<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    channel: Arc<str>,
    publishers: Vec<RefCell<Option<Iox2Publisher<T>>>>,
    subscribers: Vec<RefCell<Option<Iox2Subscriber<T>>>>,
    events: Vec<RefCell<Option<Iox2EventSubscriber>>>,
    notifiers: Vec<RefCell<Option<Iox2Notifier>>>,
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2Bindings<T> {
    pub fn configure_publisher<R>(
        &self,
        key: &Iox2PublisherKey<T>,
        configure: impl FnOnce(&mut Iox2Publisher<T>) -> R,
    ) -> Result<R, EndpointError> {
        if self.channel != key.0.channel {
            return Err(EndpointError::WrongChannel);
        }
        let mut slot = self
            .publishers
            .get(key.0.index)
            .ok_or(EndpointError::InvalidIndex(key.0.index))?
            .borrow_mut();
        Ok(configure(slot.as_mut().ok_or(EndpointError::AlreadyTaken)?))
    }
    pub fn replay_input(
        &self,
        key: &Iox2SubscriberKey<T>,
    ) -> Result<Iox2ReplayInput<T>, EndpointError> {
        if self.channel != key.0.channel {
            return Err(EndpointError::WrongChannel);
        }
        let mut slot = self
            .subscribers
            .get(key.0.index)
            .ok_or(EndpointError::InvalidIndex(key.0.index))?
            .borrow_mut();
        let subscriber = slot.as_mut().ok_or(EndpointError::AlreadyTaken)?;
        if subscriber.replay.is_some() {
            return Err(EndpointError::AlreadyTaken);
        }
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        subscriber.replay = Some(queue.clone());
        Ok(Iox2ReplayInput {
            channel: self.channel.to_string(),
            capacity: subscriber.capacity,
            queue,
        })
    }
    pub fn take_publisher(
        &self,
        key: &Iox2PublisherKey<T>,
    ) -> Result<Iox2Publisher<T>, EndpointError> {
        if self.channel != key.0.channel {
            return Err(EndpointError::WrongChannel);
        }
        self.publishers
            .get(key.0.index)
            .ok_or(EndpointError::InvalidIndex(key.0.index))?
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }
    pub fn take_subscriber(
        &self,
        key: &Iox2SubscriberKey<T>,
    ) -> Result<Iox2Subscriber<T>, EndpointError> {
        if self.channel != key.0.channel {
            return Err(EndpointError::WrongChannel);
        }
        self.subscribers
            .get(key.0.index)
            .ok_or(EndpointError::InvalidIndex(key.0.index))?
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2EventBindings for Iox2Bindings<T> {
    fn take_notifier(&self, key: &Iox2NotifierKey) -> Result<Iox2Notifier, EndpointError> {
        if self.channel != key.0.channel {
            return Err(EndpointError::WrongChannel);
        }
        self.notifiers
            .get(key.0.index)
            .ok_or(EndpointError::InvalidIndex(key.0.index))?
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }
    fn take_event(&self, key: &Iox2EventKey) -> Result<Iox2EventSubscriber, EndpointError> {
        if self.channel != key.0.channel {
            return Err(EndpointError::WrongChannel);
        }
        self.events
            .get(key.0.index)
            .ok_or(EndpointError::InvalidIndex(key.0.index))?
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;
    #[test]
    fn ipc_keys_check_names_and_all_four_endpoint_indices_without_transport_io() {
        let bindings = Iox2Bindings::<u64> {
            channel: Arc::from("channel"),
            publishers: vec![],
            subscribers: vec![],
            events: vec![],
            notifiers: vec![],
        };
        for (name, expected) in [
            ("channel", EndpointError::InvalidIndex(3)),
            ("other", EndpointError::WrongChannel),
        ] {
            let key = || Key {
                channel: Arc::from(name),
                index: 3,
            };
            assert!(
                matches!(bindings.take_publisher(&Iox2PublisherKey(key(), PhantomData)), Err(error) if error == expected)
            );
            assert!(
                matches!(bindings.take_subscriber(&Iox2SubscriberKey(key(), PhantomData)), Err(error) if error == expected)
            );
            assert!(
                matches!(bindings.take_event(&Iox2EventKey(key())), Err(error) if error == expected)
            );
            assert!(
                matches!(bindings.take_notifier(&Iox2NotifierKey(key())), Err(error) if error == expected)
            );
        }
    }
}

pub struct Iox2Publisher<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    channel: String,
    publisher_index: u32,
    capacity: usize,
    port: DataPublisher<ipc_threadsafe::Service, Message<T>, ()>,
    notifier: Option<Notifier<ipc_threadsafe::Service>>,
    pending: Vec<SampleMut<ipc_threadsafe::Service, Message<T>, ()>>,
    observers: Vec<IpcPublishObserver<T>>,
    replay_only: bool,
    runtime: Arc<Iox2Runtime>,
}
type IpcPublishObserver<T> = Box<dyn FnMut(&Message<T>) + Send>;
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2Publisher<T> {
    pub fn publisher_index(&self) -> u32 {
        self.publisher_index
    }
    pub fn observe(&mut self, observer: impl FnMut(&Message<T>) + Send + 'static) {
        self.observers.push(Box::new(observer));
    }
    /// Exact replay observes outputs locally without notifying external services.
    pub fn suppress_transport(&mut self) {
        self.replay_only = true;
    }
    pub fn visit_pending_headers(&self, mut visit: impl FnMut(MessageHeader)) {
        for (index, sample) in self.pending.iter().enumerate() {
            let mut header = sample.header;
            header.publisher_index = self.publisher_index;
            header.batch_index = u32::try_from(index).expect("publisher batch index overflow");
            visit(header);
        }
    }
    /// Publish timestamped data without an event notification, for deterministic
    /// simulation/replay that schedules counted events independently.
    pub fn publish_with_header(&self, header: MessageHeader, value: T) -> Result<(), LoanError> {
        if self.replay_only {
            return Err(LoanError::Transport(
                "immediate IPC publication is disabled during exact replay".into(),
            ));
        }
        self.port
            .loan_uninit()
            .map_err(|e| LoanError::Transport(e.to_string()))?
            .write_payload(Message {
                header,
                message: value,
            })
            .send()
            .map_err(|e| LoanError::Transport(e.to_string()))?;
        Ok(())
    }
    pub fn channel_name(&self) -> &str {
        &self.channel
    }
    pub fn discard_pending(&mut self) {
        self.pending.clear();
    }
    pub fn flush(&mut self, timestamp: FrameworkTime) {
        for (index, mut sample) in self.pending.drain(..).enumerate() {
            sample.header.published_at = timestamp;
            sample.header.publisher_index = self.publisher_index;
            sample.header.batch_index =
                u32::try_from(index).expect("publisher batch index overflow");
            for observer in &mut self.observers {
                observer(&sample);
            }
            if self.replay_only {
                continue;
            }
            sample.send().expect("iceoryx2 send failed");
            if let Some(notifier) = &self.notifier {
                notifier.notify().expect("iceoryx2 notification failed");
            }
        }
    }
    pub fn loan(&mut self, value: T) -> Result<Iox2Output<'_, T>, LoanError> {
        if self.pending.len() >= self.capacity {
            return Err(LoanError::LoanCapacityReached);
        }
        let sample = self
            .port
            .loan_uninit()
            .map_err(|e| LoanError::Transport(e.to_string()))?
            .write_payload(Message {
                header: MessageHeader::default(),
                message: value,
            });
        Ok(Iox2Output {
            publisher: self,
            sample,
        })
    }
}
pub struct Iox2Output<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> {
    publisher: &'a mut Iox2Publisher<T>,
    sample: SampleMut<ipc_threadsafe::Service, Message<T>, ()>,
}
impl<'a, T: Debug + ZeroCopySend + Send + Sync + Default + 'static> Iox2Output<'a, T> {
    pub fn new_default(publisher: &'a mut Iox2Publisher<T>) -> Result<Self, LoanError> {
        publisher.loan(T::default())
    }
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2Output<'_, T> {
    pub fn send(self) {
        self.publisher.pending.push(self.sample);
    }
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Deref for Iox2Output<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.sample.message
    }
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> DerefMut for Iox2Output<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.sample.message
    }
}

pub struct Iox2Subscriber<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    sources: Option<Vec<u32>>,
    channel: String,
    capacity: usize,
    port: DataSubscriber<ipc_threadsafe::Service, Message<T>, ()>,
    read: RefCell<VecDeque<Received<T>>>,
    replay: Option<ReplayQueue<T>>,
    receive_errors: AtomicUsize,
    _runtime: Arc<Iox2Runtime>,
}
type ReplayQueue<T> = Arc<Mutex<VecDeque<Box<Message<T>>>>>;
enum Received<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    Transport(Sample<ipc_threadsafe::Service, Message<T>, ()>),
    Replay(Box<Message<T>>),
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Deref for Received<T> {
    type Target = Message<T>;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Transport(value) => value,
            Self::Replay(value) => value,
        }
    }
}
/// An endpoint-local replay queue. It bypasses middleware delivery so multiple
/// callback inputs on one IPC channel can restore different recorded snapshots.
pub struct Iox2ReplayInput<T> {
    channel: String,
    capacity: usize,
    queue: ReplayQueue<T>,
}
impl<T> Iox2ReplayInput<T> {
    pub fn channel_name(&self) -> &str {
        &self.channel
    }
    pub fn inject(&mut self, header: MessageHeader, value: T) -> Result<(), LoanError> {
        let mut queue = self.queue.lock().unwrap();
        if queue.len() >= self.capacity {
            return Err(LoanError::LoanCapacityReached);
        }
        queue.push_back(Box::new(Message {
            header,
            message: value,
        }));
        Ok(())
    }
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2Subscriber<T> {
    pub fn clear(&self) {
        self.read.borrow_mut().clear();
        if let Some(queue) = &self.replay {
            queue.lock().unwrap().clear();
        }
    }
    pub fn visit_headers(&self, mut visit: impl FnMut(MessageHeader)) {
        for sample in self.read.borrow().iter() {
            visit(sample.header);
        }
    }
    pub fn channel_name(&self) -> &str {
        &self.channel
    }
    pub fn receive_errors(&self) -> usize {
        self.receive_errors.load(Ordering::Relaxed)
    }
    /// Inspect and consume the prepared batch without requiring Clone. References
    /// cannot escape the visitor, and unwinding releases all remaining samples.
    pub fn inspect_messages(&self, mut inspect: impl FnMut(usize, &Message<T>)) -> usize {
        let batch = std::mem::take(&mut *self.read.borrow_mut());
        let count = batch.len();
        for (index, sample) in batch.into_iter().enumerate() {
            inspect(index, &sample);
        }
        count
    }
    pub fn update(&self) {
        let mut read = self.read.borrow_mut();
        if let Some(queue) = &self.replay {
            for message in std::mem::take(&mut *queue.lock().unwrap()) {
                if read.len() == self.capacity {
                    read.pop_front();
                }
                read.push_back(Received::Replay(message));
            }
            return;
        }
        for _ in 0..self.capacity {
            match self.port.receive() {
                Ok(Some(sample)) => {
                    if self
                        .sources
                        .as_ref()
                        .is_some_and(|sources| !sources.contains(&sample.header.publisher_index))
                    {
                        continue;
                    }
                    if read.len() == self.capacity {
                        read.pop_front();
                    }
                    read.push_back(Received::Transport(sample));
                }
                Ok(None) => break,
                Err(_) => {
                    self.receive_errors.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
    }
}
pub struct Iox2OptionalInput<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> {
    read: RefMut<'a, VecDeque<Received<T>>>,
}
impl<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2OptionalInput<'a, T> {
    pub fn new(subscriber: &'a Iox2Subscriber<T>) -> Self {
        Self {
            read: subscriber.read.borrow_mut(),
        }
    }
    pub fn value(&self) -> Option<&T> {
        self.read.back().map(|s| &s.message)
    }
    pub fn header(&self) -> Option<&MessageHeader> {
        self.read.back().map(|s| &s.header)
    }
    pub fn clear(&mut self) {
        self.read.pop_front();
    }
}
pub struct Iox2SpanInput<'a, T: Debug + ZeroCopySend + Send + Sync + 'static>(
    Iox2OptionalInput<'a, T>,
);
impl<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2SpanInput<'a, T> {
    pub fn new(subscriber: &'a Iox2Subscriber<T>) -> Self {
        Self(Iox2OptionalInput::new(subscriber))
    }
    pub fn inputs(&self) -> impl Iterator<Item = &Message<T>> {
        self.0.read.iter().map(|s| &**s)
    }
    pub fn len(&self) -> usize {
        self.0.read.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.read.is_empty()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EventRecord {
    pub event_id: EventId,
    pub count: u64,
}
pub struct Iox2EventSubscriber {
    channel: String,
    capacity: usize,
    listener: Option<Listener<ipc_threadsafe::Service>>,
    staging: Arc<base::mpsc_queue::MpscQueue<EventRecord>>,
    read: RefCell<VecDeque<EventRecord>>,
    wake: Option<WakeHandle>,
    runtime: Arc<Iox2Runtime>,
}
impl Iox2EventSubscriber {
    pub fn clear(&self) {
        self.read.borrow_mut().clear();
        while self.staging.pop().is_some() {}
    }
    pub fn stage_replay(&self, event_id: usize, count: u64) -> Result<(), LoanError> {
        if count != 0 {
            if self.staging.len() >= self.capacity {
                return Err(LoanError::LoanCapacityReached);
            }
            self.staging.push(EventRecord {
                event_id: EventId::new(event_id),
                count,
            });
        }
        Ok(())
    }
    pub fn visit_records(&self, mut visit: impl FnMut(EventRecord)) {
        for record in self.read.borrow().iter() {
            visit(*record);
        }
    }
    pub fn channel_name(&self) -> &str {
        &self.channel
    }
    pub fn set_waker(&mut self, wake: WakeHandle) {
        self.wake = Some(wake);
    }
    pub fn has_pending(&self) -> bool {
        !self.staging.is_empty() || !self.read.borrow().is_empty()
    }
    pub fn update(&self) {
        let mut read = self.read.borrow_mut();
        for _ in 0..self.capacity {
            if let Some(event) = self.staging.pop() {
                if read.len() == self.capacity {
                    read.pop_front();
                }
                read.push_back(event);
            } else {
                break;
            }
        }
    }
    pub fn take_registration(&mut self) -> Option<Iox2EventRegistration> {
        Some(Iox2EventRegistration {
            observer: None,
            channel: self.channel.clone(),
            listener: self.listener.take()?,
            staging: self.staging.clone(),
            wake: self
                .wake
                .clone()
                .expect("executor must bind wake before taking events"),
            _runtime: self.runtime.clone(),
        })
    }
}
pub struct Iox2Event<'a> {
    read: RefMut<'a, VecDeque<EventRecord>>,
}
impl Drop for Iox2Event<'_> {
    fn drop(&mut self) {
        // A gated callback never constructs this view, so its events remain
        // available until the required inputs permit an actual invocation.
        self.read.clear();
    }
}
impl<'a> Iox2Event<'a> {
    pub fn new(subscriber: &'a Iox2EventSubscriber) -> Self {
        Self {
            read: subscriber.read.borrow_mut(),
        }
    }
    pub fn events(&self) -> impl Iterator<Item = &EventRecord> {
        self.read.iter()
    }
    pub fn count(&self) -> u64 {
        self.read
            .iter()
            .fold(0_u64, |count, record| count.saturating_add(record.count))
    }
    pub fn records(&self) -> impl Iterator<Item = (EventId, u64)> + '_ {
        self.read
            .iter()
            .map(|record| (record.event_id, record.count))
    }
}
pub struct Iox2EventRegistration {
    pub observer: Option<EventObserver>,
    pub channel: String,
    pub listener: Listener<ipc_threadsafe::Service>,
    pub staging: Arc<base::mpsc_queue::MpscQueue<EventRecord>>,
    pub wake: WakeHandle,
    _runtime: Arc<Iox2Runtime>,
}
pub type EventObserver = Arc<dyn Fn(FrameworkTime, EventRecord) + Send + Sync>;

pub struct Iox2Notifier {
    channel: String,
    port: Notifier<ipc_threadsafe::Service>,
    pending: bool,
    event_id: usize,
    replay_only: bool,
    _runtime: Arc<Iox2Runtime>,
}
impl Iox2Notifier {
    pub fn suppress_transport(&mut self) {
        self.replay_only = true;
    }
    pub fn pending_event(&self) -> Option<EventRecord> {
        self.pending.then_some(EventRecord {
            event_id: EventId::new(self.event_id),
            count: 1,
        })
    }
    pub fn channel_name(&self) -> &str {
        &self.channel
    }
    pub fn discard_pending(&mut self) {
        self.pending = false;
    }
    pub fn flush(&mut self, _timestamp: FrameworkTime) {
        if std::mem::take(&mut self.pending) && !self.replay_only {
            self.port.notify().expect("iceoryx2 notification failed");
        }
    }
}
pub struct Iox2NotifyOutput<'a> {
    notifier: &'a mut Iox2Notifier,
}
impl<'a> Iox2NotifyOutput<'a> {
    pub fn new(notifier: &'a mut Iox2Notifier) -> Self {
        Self { notifier }
    }
    pub fn send(self) {
        self.notifier.pending = true;
    }
}

pub struct Iox2Shutdown {
    pub listener: Listener<ipc_threadsafe::Service>,
    pub wake: WakeHandle,
}
impl Iox2Shutdown {
    pub fn new() -> Result<Self, StorageError> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let runtime = Iox2Runtime::new()?;
        let name = ServiceName::new(&format!(
            "cfw_stop_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
        .map_err(transport)?;
        let service = runtime
            .node
            .service_builder(&name)
            .event()
            .event_id_max_value(0)
            .open_or_create()
            .map_err(transport)?;
        let listener = service.listener_builder().create().map_err(transport)?;
        let notifier = service.notifier_builder().create().map_err(transport)?;
        Ok(Self {
            listener,
            wake: Arc::new(ShutdownWake {
                notifier,
                _runtime: runtime,
            }),
        })
    }
}
struct ShutdownWake {
    notifier: Notifier<ipc_threadsafe::Service>,
    _runtime: Arc<Iox2Runtime>,
}
impl Wake for ShutdownWake {
    fn wake(&self) {
        let _ = self.notifier.notify();
    }
}
