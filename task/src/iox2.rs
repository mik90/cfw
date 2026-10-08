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
        Arc,
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

#[derive(Clone, Debug)]
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
            buffer_capacity: 16,
            max_borrowed_samples: 32,
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
    identity: Arc<()>,
    index: usize,
}
pub struct Iox2PublisherKey<T>(Key, PhantomData<fn(T) -> T>);
pub struct Iox2SubscriberKey<T>(Key, PhantomData<fn(T) -> T>);
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

/// One named IPC channel, with data and optional event-trigger endpoints.
pub struct Iox2ChannelPlan<T> {
    name: String,
    runtime: Arc<Iox2Runtime>,
    config: Iox2ChannelConfig,
    identity: Arc<()>,
    publishers: Vec<usize>,
    subscribers: Vec<usize>,
    events: Vec<usize>,
    notifiers: Vec<usize>,
    payload: PhantomData<fn(T) -> T>,
}
impl<T> Iox2ChannelPlan<T> {
    pub fn new(name: impl Into<String>, runtime: &Arc<Iox2Runtime>) -> Self {
        Self {
            name: name.into(),
            runtime: runtime.clone(),
            config: Iox2ChannelConfig::default(),
            identity: Arc::new(()),
            publishers: Vec::new(),
            subscribers: Vec::new(),
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
        let index = self.publishers.len();
        self.publishers.push(capacity);
        Iox2PublisherKey(
            Key {
                identity: self.identity.clone(),
                index,
            },
            PhantomData,
        )
    }
    pub fn subscriber(&mut self, capacity: usize) -> Iox2SubscriberKey<T> {
        let index = self.subscribers.len();
        self.subscribers.push(capacity);
        Iox2SubscriberKey(
            Key {
                identity: self.identity.clone(),
                index,
            },
            PhantomData,
        )
    }
    pub fn events(&mut self, capacity: usize) -> Iox2EventKey {
        let index = self.events.len();
        self.events.push(capacity);
        Iox2EventKey(Key {
            identity: self.identity.clone(),
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
            identity: self.identity.clone(),
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
        if self
            .publishers
            .iter()
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
            || self.publishers.len() + self.notifiers.len() > self.config.max_notifiers
            || self
                .notifiers
                .iter()
                .any(|&id| id > self.config.event_id_max_value)
        {
            return Err(transport(format!(
                "invalid endpoint/service capacities on {}",
                self.name
            )));
        }
        Ok(())
    }
    fn validate_channel_names(&self, names: &mut HashSet<String>) -> Result<(), StorageError> {
        if !names.insert(self.name.clone()) {
            return Err(StorageError::DuplicateChannel(self.name.clone()));
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
                    .map_err(transport)?,
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
            .map_err(transport)?;
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
        for &capacity in &self.plan.publishers {
            publishers.push(RefCell::new(Some(Iox2Publisher {
                channel: self.plan.name.clone(),
                capacity,
                port: self
                    .data
                    .as_ref()
                    .expect("publisher has a data service")
                    .publisher_builder()
                    .max_loaned_samples(capacity)
                    .create()
                    .map_err(transport)?,
                notifier: self
                    .event
                    .notifier_builder()
                    .default_event_id(EventId::new(0))
                    .create()
                    .map_err(transport)?,
                pending: Vec::with_capacity(capacity),
                runtime: self.plan.runtime.clone(),
            })));
        }
        let mut subscribers = Vec::new();
        for &capacity in &self.plan.subscribers {
            subscribers.push(RefCell::new(Some(Iox2Subscriber {
                channel: self.plan.name.clone(),
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
                _runtime: self.plan.runtime.clone(),
            })));
        }
        let mut events = Vec::new();
        for &capacity in &self.plan.events {
            events.push(RefCell::new(Some(Iox2EventSubscriber {
                channel: self.plan.name.clone(),
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
                channel: self.plan.name.clone(),
                pending: false,
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
            identity: self.plan.identity.clone(),
            publishers,
            subscribers,
            events,
            notifiers,
        })
    }
}

pub struct Iox2Bindings<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    identity: Arc<()>,
    publishers: Vec<RefCell<Option<Iox2Publisher<T>>>>,
    subscribers: Vec<RefCell<Option<Iox2Subscriber<T>>>>,
    events: Vec<RefCell<Option<Iox2EventSubscriber>>>,
    notifiers: Vec<RefCell<Option<Iox2Notifier>>>,
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2Bindings<T> {
    pub fn take_publisher(
        &self,
        key: &Iox2PublisherKey<T>,
    ) -> Result<Iox2Publisher<T>, EndpointError> {
        if !Arc::ptr_eq(&self.identity, &key.0.identity) {
            return Err(EndpointError::WrongChannel);
        }
        self.publishers[key.0.index]
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }
    pub fn take_subscriber(
        &self,
        key: &Iox2SubscriberKey<T>,
    ) -> Result<Iox2Subscriber<T>, EndpointError> {
        if !Arc::ptr_eq(&self.identity, &key.0.identity) {
            return Err(EndpointError::WrongChannel);
        }
        self.subscribers[key.0.index]
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2EventBindings for Iox2Bindings<T> {
    fn take_notifier(&self, key: &Iox2NotifierKey) -> Result<Iox2Notifier, EndpointError> {
        if !Arc::ptr_eq(&self.identity, &key.0.identity) {
            return Err(EndpointError::WrongChannel);
        }
        self.notifiers[key.0.index]
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }
    fn take_event(&self, key: &Iox2EventKey) -> Result<Iox2EventSubscriber, EndpointError> {
        if !Arc::ptr_eq(&self.identity, &key.0.identity) {
            return Err(EndpointError::WrongChannel);
        }
        self.events[key.0.index]
            .borrow_mut()
            .take()
            .ok_or(EndpointError::AlreadyTaken)
    }
}

pub struct Iox2Publisher<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    channel: String,
    capacity: usize,
    port: DataPublisher<ipc_threadsafe::Service, Message<T>, ()>,
    notifier: Notifier<ipc_threadsafe::Service>,
    pending: Vec<SampleMut<ipc_threadsafe::Service, Message<T>, ()>>,
    runtime: Arc<Iox2Runtime>,
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2Publisher<T> {
    /// Publish timestamped data without an event notification, for deterministic
    /// simulation/replay that schedules counted events independently.
    pub fn publish_with_header(&self, header: MessageHeader, value: T) -> Result<(), LoanError> {
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
        for mut sample in self.pending.drain(..) {
            sample.header.published_at = timestamp;
            sample.send().expect("iceoryx2 send failed");
            self.notifier
                .notify()
                .expect("iceoryx2 notification failed");
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
    channel: String,
    capacity: usize,
    port: DataSubscriber<ipc_threadsafe::Service, Message<T>, ()>,
    read: RefCell<VecDeque<Sample<ipc_threadsafe::Service, Message<T>, ()>>>,
    receive_errors: AtomicUsize,
    _runtime: Arc<Iox2Runtime>,
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2Subscriber<T> {
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
        for _ in 0..self.capacity {
            match self.port.receive() {
                Ok(Some(sample)) => {
                    if read.len() == self.capacity {
                        read.pop_front();
                    }
                    read.push_back(sample);
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
    read: RefMut<'a, VecDeque<Sample<ipc_threadsafe::Service, Message<T>, ()>>>,
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
    pub channel: String,
    pub listener: Listener<ipc_threadsafe::Service>,
    pub staging: Arc<base::mpsc_queue::MpscQueue<EventRecord>>,
    pub wake: WakeHandle,
    _runtime: Arc<Iox2Runtime>,
}

pub struct Iox2Notifier {
    channel: String,
    port: Notifier<ipc_threadsafe::Service>,
    pending: bool,
    _runtime: Arc<Iox2Runtime>,
}
impl Iox2Notifier {
    pub fn channel_name(&self) -> &str {
        &self.channel
    }
    pub fn discard_pending(&mut self) {
        self.pending = false;
    }
    pub fn flush(&mut self, _timestamp: FrameworkTime) {
        if std::mem::take(&mut self.pending) {
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
