//! iceoryx2 channel views and graph-scoped service configuration.
//!
//! Processes sharing an iceoryx2 channel must agree on all service-wide
//! settings. The first process creating a service establishes those settings;
//! later openers must request compatible values. Safe overflow is enabled so
//! a full subscriber buffer drops its oldest sample instead of blocking senders.

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    fmt::Debug,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use iceoryx2::{
    config::Config,
    node::{Node, NodeBuilder},
    port::{
        listener::Listener, notifier::Notifier, publisher::Publisher as IoxPublisher,
        subscriber::Subscriber as IoxSubscriber,
    },
    prelude::*,
    sample::Sample,
    sample_mut::SampleMut,
    service::{
        ipc_threadsafe, port_factory::event::PortFactory as EventPortFactory,
        service_name::ServiceName,
    },
};

use crate::{
    generic_subscriber::{GenericSubscriber, QueueInfo},
    message::{Message, MessageHeader},
    pub_sub_factory::{EndpointKind, Iox2EndpointInfo},
    publisher::PublisherConfig,
    subscriber::SubscriberConfig,
    task_graph_builder::TaskGraphBuildError,
};

/// One event-id/count pair deposited for an event-driven callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventRecord {
    /// Identifier attached to this event.
    pub event_id: EventId,
    /// Number of occurrences represented by this record.
    pub count: u64,
}

/// Listener and staging state transferred to the live readiness thread.
pub struct Iox2EventRegistration {
    /// Channel whose event service this listener receives.
    pub channel: String,
    /// Event listener owned by the readiness thread.
    pub listener: Listener<ipc_threadsafe::Service>,
    /// Bounded queue receiving folded event activations.
    pub staging: Arc<base::mpsc_queue::MpscQueue<EventRecord>>,
    /// Readiness signal for the target optional-trigger subscriber.
    pub readiness: Option<crate::callback::SubscriberReadiness>,
}

/// Type-erased publisher for injecting real data samples into simulation or replay.
pub trait Iox2SyntheticPublisher: Send {
    /// The channel this publisher injects into.
    fn channel(&self) -> &str;

    /// The concrete payload type accepted by this publisher.
    fn payload_type_id(&self) -> std::any::TypeId;

    /// Publish an owned payload with its requested simulation timestamp.
    fn publish(
        &mut self,
        header: MessageHeader,
        payload: Box<dyn std::any::Any + Send>,
    ) -> Result<(), String>;

    /// Publish a deserialized payload produced on the replay worker thread.
    fn publish_replay(
        &mut self,
        header: MessageHeader,
        payload: Box<dyn std::any::Any>,
    ) -> Result<(), String>;
}

/// Atomic counters describing the live executor's event-readiness thread.
#[derive(Default)]
pub struct Iox2ReadinessMetrics {
    /// Number of waitset wake batches processed.
    pub wake_batches: std::sync::atomic::AtomicU64,
    /// Number of individual iceoryx2 event activations drained.
    pub activations: std::sync::atomic::AtomicU64,
    /// Saturating sum of activation counts.
    pub total_counts: std::sync::atomic::AtomicU64,
    /// Number of folded staging records displaced by overflow.
    pub dropped_records: std::sync::atomic::AtomicU64,
    /// Saturating sum of counts represented by displaced records.
    pub dropped_counts: std::sync::atomic::AtomicU64,
    /// Maximum observed wake-to-last-schedule latency in nanoseconds.
    pub max_drain_latency_ns: std::sync::atomic::AtomicU64,
    /// Number of readiness-thread loop passes.
    pub loop_iterations: std::sync::atomic::AtomicU64,
}

impl Iox2ReadinessMetrics {
    /// Snapshot all counters in a stable named tuple.
    pub fn snapshot(&self) -> (u64, u64, u64, u64, u64, u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.wake_batches.load(Ordering::Relaxed),
            self.activations.load(Ordering::Relaxed),
            self.total_counts.load(Ordering::Relaxed),
            self.dropped_records.load(Ordering::Relaxed),
            self.dropped_counts.load(Ordering::Relaxed),
            self.max_drain_latency_ns.load(Ordering::Relaxed),
            self.loop_iterations.load(Ordering::Relaxed),
        )
    }
}

/// Event input staging queue shared with its injector and readiness producer.
pub struct Iox2EventSubscriber {
    config: SubscriberConfig,
    staging: Arc<base::mpsc_queue::MpscQueue<EventRecord>>,
    read: RefCell<VecDeque<EventRecord>>,
    readiness_state: Option<crate::callback::SubscriberReadiness>,
}

impl Iox2EventSubscriber {
    /// Declare an optional-trigger input for events on `config.channel_name`.
    /// `config.capacity` sizes the staging queue (drop-oldest under burst).
    pub fn new(mut config: SubscriberConfig) -> Self {
        assert!(
            config.capacity > 0,
            "iox2 event subscriber capacity must be positive for channel {}",
            config.channel_name
        );
        config.is_optional = true;
        config.is_trigger = true;
        Self {
            staging: Arc::new(base::mpsc_queue::MpscQueue::new(config.capacity)),
            config,
            read: RefCell::new(VecDeque::new()),
            readiness_state: None,
        }
    }

    /// Make an injector for tests that need to stage events without the live executor.
    #[cfg(feature = "testing")]
    pub fn injector(&self) -> Iox2EventInjector {
        Iox2EventInjector {
            staging: Arc::clone(&self.staging),
            readiness: self.readiness_state.clone(),
        }
    }

    /// Create a synchronous test injector for simulation's step-boundary scheduler.
    /// Staging a record here does not invoke live-executor readiness scheduling.
    #[cfg(feature = "testing")]
    pub fn staging_injector(&self) -> Iox2StagingInjector {
        Iox2StagingInjector {
            staging: Arc::clone(&self.staging),
        }
    }

    pub(crate) fn inject_events(
        &self,
        records: impl IntoIterator<Item = EventRecord>,
        sink: &mut dyn crate::scheduling::ReadyNodeSink,
    ) {
        for record in records {
            self.staging.push(record);
        }
        if let Some(crate::callback::SubscriberReadiness::OptionalTrigger(readiness)) =
            &self.readiness_state
            && let Some(node) = readiness.optional_trigger_arrived()
        {
            sink.schedule(node);
        }
    }
}

/// Test-only handle for injecting event records into a graph input.
#[cfg(feature = "testing")]
pub struct Iox2EventInjector {
    staging: Arc<base::mpsc_queue::MpscQueue<EventRecord>>,
    readiness: Option<crate::callback::SubscriberReadiness>,
}

/// Test handle for depositing counted events without invoking live readiness scheduling.
#[cfg(feature = "testing")]
pub struct Iox2StagingInjector {
    staging: Arc<base::mpsc_queue::MpscQueue<EventRecord>>,
}

#[cfg(feature = "testing")]
impl Iox2StagingInjector {
    /// Immediately stage one event record for the next simulation step.
    pub fn notify(&self, event_id: EventId, count: u64) {
        let _ = self.staging.push(EventRecord { event_id, count });
    }
}

#[cfg(feature = "testing")]
impl Iox2EventInjector {
    /// Stage one counted event and notify the node when its required inputs are ready.
    pub fn notify(
        &self,
        event_id: EventId,
        count: u64,
        sink: &mut dyn crate::scheduling::ReadyNodeSink,
    ) {
        self.staging.push(EventRecord { event_id, count });
        if let Some(crate::callback::SubscriberReadiness::OptionalTrigger(readiness)) =
            &self.readiness
            && let Some(node) = readiness.optional_trigger_arrived()
        {
            sink.schedule(node);
        }
    }
}

/// Borrowed view of the event records available to one callback run.
pub struct Iox2EventGuard<'a> {
    records: std::cell::RefMut<'a, VecDeque<EventRecord>>,
}

impl Iox2EventGuard<'_> {
    /// The oldest event record, if any.
    pub fn front(&self) -> Option<&EventRecord> {
        self.records.front()
    }
    /// Number of records in this run's event batch.
    pub fn len(&self) -> usize {
        self.records.len()
    }
    /// Whether this run has no event records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
    /// Remove the oldest record.
    pub fn pop_front(&mut self) {
        self.records.pop_front();
    }
}

/// Event batch exposed to a callback. An empty rerun is possible when a
/// producer deposits before the worker drains but fires readiness afterward;
/// this has the same shape as native optional triggers, whose readiness is
/// fired per publish while the subscriber drain consumes the message queue.
pub struct Iox2Event<'a> {
    guard: Iox2EventGuard<'a>,
}

impl<'a> Iox2Event<'a> {
    /// Borrow the event batch retained by a subscriber.
    pub fn new(subscriber: &'a Iox2EventSubscriber) -> Self {
        Self {
            guard: Iox2EventGuard {
                records: subscriber.read.borrow_mut(),
            },
        }
    }
    /// Sum of all event counts in the batch.
    pub fn count(&self) -> u64 {
        self.guard
            .records
            .iter()
            .fold(0, |sum, record| sum.saturating_add(record.count))
    }
    /// Iterate event identifiers and their counts in arrival order.
    pub fn records(&self) -> impl Iterator<Item = (EventId, u64)> + '_ {
        self.guard
            .records
            .iter()
            .map(|record| (record.event_id, record.count))
    }
}

impl GenericSubscriber for Iox2EventSubscriber {
    fn iox2_is_event_input(&self) -> bool {
        true
    }

    fn iox2_stage_replay_event(&self, event_id: usize, count: u64) -> Result<(), String> {
        let _ = self.staging.push(EventRecord {
            event_id: EventId::new(event_id),
            count,
        });
        Ok(())
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn config(&self) -> &SubscriberConfig {
        &self.config
    }
    fn config_mut(&mut self) -> &mut SubscriberConfig {
        &mut self.config
    }
    fn able_to_run(&self) -> bool {
        true
    }
    fn requests_execution(&self) -> bool {
        !self.staging.is_empty()
    }
    fn drain_writer_to_reader(&self) {
        let mut read = self.read.borrow_mut();
        read.clear();
        while let Some(record) = self.staging.pop() {
            read.push_back(record);
        }
    }
    fn queue_info(&self) -> QueueInfo {
        QueueInfo {
            reader_size: self.read.borrow().len(),
            writer_size: self.staging.len(),
        }
    }
    fn cleanup_buffers(&self) {
        self.read.borrow_mut().clear();
        self.staging.clear();
    }
    fn readiness_state(&self) -> Option<crate::callback::SubscriberReadiness> {
        self.readiness_state.clone()
    }
    fn set_readiness_state(&mut self, state: crate::callback::SubscriberReadiness) {
        self.readiness_state = Some(state);
    }
    /// Event records are not message payloads; they are intentionally not
    /// channel-logged yet, hence this explicit no-op.
    fn drain_queued_inputs(
        &mut self,
        _f: &mut dyn FnMut(
            &MessageHeader,
            &dyn std::any::Any,
        ) -> Result<(), crate::channel_registry::BoxedError>,
    ) -> Result<(), crate::channel_registry::BoxedError> {
        Ok(())
    }
    fn iox2_find_endpoints(
        &self,
        add: &mut dyn FnMut(
            crate::pub_sub_factory::Iox2EndpointInfo,
        ) -> Result<(), TaskGraphBuildError>,
    ) -> Result<(), TaskGraphBuildError> {
        add(crate::pub_sub_factory::Iox2EndpointInfo {
            channel: self.config.channel_name.clone(),
            kind: EndpointKind::Iox2EventSub,
            payload_type: None,
        })
    }
    fn iox2_open(&mut self, ctx: &mut dyn Iox2OpenCtx) -> Result<(), TaskGraphBuildError> {
        let channel = self.config.channel_name.clone();
        ctx.event_service(&channel).map(|_| ())
    }
    fn iox2_take_event_registration(
        &mut self,
        ctx: &mut dyn Iox2OpenCtx,
    ) -> Result<Option<Iox2EventRegistration>, String> {
        let channel = self.config.channel_name.clone();
        let readiness = self.readiness_state.clone();
        if !matches!(
            readiness,
            Some(crate::callback::SubscriberReadiness::OptionalTrigger(_))
        ) {
            return Err(format!(
                "event input channel {channel} has no optional-trigger readiness"
            ));
        }
        let listener = ctx
            .event_service(&channel)
            .map_err(|error| error.to_string())?
            .listener_builder()
            .create()
            .map_err(|error| format!("unable to create listener for channel {channel}: {error}"))?;
        Ok(Some(Iox2EventRegistration {
            channel,
            listener,
            staging: Arc::clone(&self.staging),
            readiness,
        }))
    }
}

/// Event output endpoint that emits a notification when flushed.
pub struct Iox2Notifier {
    config: PublisherConfig,
    /// Event identifier; can be overridden before graph construction.
    pub event_id: EventId,
    notifier: Option<Notifier<ipc_threadsafe::Service>>,
    notify_pending: bool,
}

impl Iox2Notifier {
    /// Declare an event notifier on the configured channel.
    pub fn new(config: PublisherConfig) -> Self {
        Self {
            config,
            event_id: EventId::new(0),
            notifier: None,
            notify_pending: false,
        }
    }
}

/// Mutable one-shot notification output.
pub struct Iox2NotifyOutput<'a> {
    notifier: &'a mut Iox2Notifier,
}
impl<'a> Iox2NotifyOutput<'a> {
    /// Prepare a notification for the next publisher flush.
    pub fn new(notifier: &'a mut Iox2Notifier) -> Self {
        notifier.notify_pending = false;
        Self { notifier }
    }
    /// Mark the notification pending.
    pub fn send(self) {
        self.notifier.notify_pending = true;
    }
}

impl crate::generic_publisher::GenericPublisher for Iox2Notifier {
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn config(&self) -> &PublisherConfig {
        &self.config
    }
    fn config_mut(&mut self) -> &mut PublisherConfig {
        &mut self.config
    }
    fn forwarded_channels(&self) -> &[String] {
        &[]
    }
    fn allocate_arena(&mut self) {}
    fn increase_arena_size(&mut self, _additional_capacity: usize) {}
    fn flush_loaned_values(
        &mut self,
        _timestamp: crate::time::FrameworkTime,
        _sink: &mut dyn crate::scheduling::ReadyNodeSink,
    ) {
        if self.notify_pending {
            self.notifier
                .as_ref()
                .unwrap_or_else(|| {
                    panic!(
                        "iox2 notifier for channel {} is not open",
                        self.config.channel_name
                    )
                })
                .notify()
                .unwrap_or_else(|error| {
                    panic!(
                        "iox2 notify failed for channel {}: {error}",
                        self.config.channel_name
                    )
                });
            self.notify_pending = false;
        }
    }
    fn connect_to_subscriber(
        &mut self,
        subscriber: &mut dyn GenericSubscriber,
    ) -> Result<(), crate::generic_publisher::ConnectionTypeMismatch> {
        if subscriber
            .as_any()
            .downcast_mut::<Iox2EventSubscriber>()
            .is_some()
        {
            return Ok(());
        }
        let mut is_data_input = false;
        let report = subscriber.iox2_find_endpoints(&mut |endpoint| {
            is_data_input |= matches!(endpoint.kind, EndpointKind::Iox2DataSub { .. });
            Ok(())
        });
        if report.is_ok() && is_data_input {
            return Ok(());
        }
        Err(crate::generic_publisher::ConnectionTypeMismatch::new(
            self.config.channel_name.clone(),
            "iox2 event",
            "native",
        ))
    }
    fn iox2_find_endpoints(
        &self,
        add: &mut dyn FnMut(
            crate::pub_sub_factory::Iox2EndpointInfo,
        ) -> Result<(), TaskGraphBuildError>,
    ) -> Result<(), TaskGraphBuildError> {
        add(crate::pub_sub_factory::Iox2EndpointInfo {
            channel: self.config.channel_name.clone(),
            kind: EndpointKind::Iox2Notifier {
                event_id: self.event_id.as_value(),
            },
            payload_type: None,
        })
    }
    fn iox2_open(&mut self, ctx: &mut dyn Iox2OpenCtx) -> Result<(), TaskGraphBuildError> {
        let channel = self.config.channel_name.clone();
        self.notifier = Some(
            ctx.event_service(&channel)?
                .notifier_builder()
                .default_event_id(self.event_id)
                .create()
                .map_err(|error| TaskGraphBuildError::Iox2ServiceOpen {
                    channel,
                    source: error.to_string(),
                })?,
        );
        Ok(())
    }
}

/// iceoryx2 graph-wide node and runtime configuration.
#[derive(Clone, Debug, Default)]
pub struct Iox2GraphConfig {
    /// Optional iceoryx2 node name.
    pub node_name: Option<String>,
    /// Optional runtime configuration for the graph node and service defaults.
    /// When absent, iceoryx2's normal global configuration is used.
    pub config: Option<Config>,
}

/// A publisher loan retained until the framework flushes it.
pub struct LoanedIox2Message<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    sample: SampleMut<ipc_threadsafe::Service, Message<T>, ()>,
    sent: bool,
}

/// Native-config-compatible iox2 subscriber field type.
///
/// Receive failures are counted in `receive_errors`; a failed receive ends
/// that drain attempt and leaves remaining samples for a later call.
pub struct Iox2Subscriber<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    config: SubscriberConfig,
    port: Option<IoxSubscriber<ipc_threadsafe::Service, Message<T>, ()>>,
    read: RefCell<VecDeque<Sample<ipc_threadsafe::Service, Message<T>, ()>>>,
    receive_errors: std::sync::atomic::AtomicU64,
}

impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2Subscriber<T> {
    /// Create an iox2 data input; it is always optional and non-triggering.
    pub fn new(mut config: SubscriberConfig) -> Self {
        assert!(
            config.capacity > 0,
            "iox2 subscriber capacity must be positive for channel {}",
            config.channel_name
        );
        config.is_optional = true;
        config.is_trigger = false;
        Self {
            config,
            port: None,
            read: RefCell::new(VecDeque::new()),
            receive_errors: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl<T: Debug + ZeroCopySend + Send + Sync + 'static> GenericSubscriber for Iox2Subscriber<T> {
    fn iox2_is_data_input(&self) -> bool {
        true
    }

    fn iox2_replay_publisher_factory(
        &self,
    ) -> Option<crate::channel_registry::Iox2ReplayPublisherFactory> {
        Some(std::sync::Arc::new(|channel_name| {
            let mut publisher = Iox2Publisher::<T>::new(PublisherConfig {
                capacity: 1,
                channel_name,
            });
            publisher.notify_on_send = false;
            let writer: crate::channel_registry::ChannelReplayWriter =
                std::sync::Arc::new(|publisher, header, value| {
                    let publisher = publisher
                        .as_any()
                        .downcast_mut::<Iox2Publisher<T>>()
                        .expect("iox2 replay publisher type mismatch");
                    let value = value
                        .downcast::<T>()
                        .expect("iox2 replay payload type mismatch");
                    publisher
                        .publish_with_header(header, *value)
                        .unwrap_or_else(|error| panic!("iox2 replay publish failed: {error}"));
                });
            (Box::new(publisher), writer)
        }))
    }

    fn iox2_find_endpoints(
        &self,
        add: &mut dyn FnMut(
            crate::pub_sub_factory::Iox2EndpointInfo,
        ) -> Result<(), crate::task_graph_builder::TaskGraphBuildError>,
    ) -> Result<(), crate::task_graph_builder::TaskGraphBuildError> {
        add(crate::pub_sub_factory::Iox2EndpointInfo {
            channel: self.config.channel_name.clone(),
            kind: crate::pub_sub_factory::EndpointKind::Iox2DataSub {
                capacity: self.config.capacity,
            },
            payload_type: Some(std::any::TypeId::of::<T>()),
        })
    }
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn config(&self) -> &SubscriberConfig {
        &self.config
    }
    fn config_mut(&mut self) -> &mut SubscriberConfig {
        &mut self.config
    }
    fn able_to_run(&self) -> bool {
        true
    }
    fn requests_execution(&self) -> bool {
        false
    }
    fn drain_writer_to_reader(&self) {
        let Some(port) = &self.port else { return };
        let mut read = self.read.borrow_mut();
        loop {
            match port.receive() {
                Ok(Some(sample)) => {
                    if read.len() == self.config.capacity {
                        read.pop_front();
                    }
                    read.push_back(sample);
                }
                Ok(None) => break,
                Err(_) => {
                    self.receive_errors
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            }
        }
    }
    fn queue_info(&self) -> QueueInfo {
        let writer_size = self
            .port
            .as_ref()
            .map_or(0, |p| usize::from(p.has_samples().unwrap_or(false)));
        QueueInfo {
            reader_size: self.read.borrow().len(),
            writer_size,
        }
    }
    fn cleanup_buffers(&self) {
        self.read.borrow_mut().clear();
    }
    fn readiness_state(&self) -> Option<crate::callback::SubscriberReadiness> {
        None
    }
    fn iox2_open(&mut self, ctx: &mut dyn Iox2OpenCtx) -> Result<(), TaskGraphBuildError> {
        let channel = self.config.channel_name.clone();
        let name = ctx.service_name(&channel)?;
        let settings = ctx.pubsub_settings(&channel)?;
        let service = ctx
            .node()
            .service_builder(&name)
            .publish_subscribe::<Message<T>>()
            .subscriber_max_buffer_size(settings.subscriber_max_buffer_size)
            .subscriber_max_borrowed_samples(settings.subscriber_max_borrowed_samples)
            .history_size(settings.history_size)
            .enable_safe_overflow(settings.enable_safe_overflow)
            .max_publishers(settings.max_publishers)
            .max_subscribers(settings.max_subscribers)
            .max_nodes(settings.max_nodes)
            .open_or_create()
            .map_err(|error| TaskGraphBuildError::Iox2ServiceOpen {
                channel: channel.clone(),
                source: error.to_string(),
            })?;
        let port = service
            .subscriber_builder()
            .buffer_size(self.config.capacity)
            .create()
            .map_err(|error| TaskGraphBuildError::Iox2ServiceOpen {
                channel,
                source: error.to_string(),
            })?;
        self.port = Some(port);
        Ok(())
    }
    fn iox2_create_simulation_publisher(
        &self,
        ctx: &mut dyn Iox2OpenCtx,
    ) -> Result<Option<Box<dyn Iox2SyntheticPublisher>>, String> {
        let mut publisher = Iox2Publisher::<T>::new(PublisherConfig {
            capacity: 1,
            channel_name: self.config.channel_name.clone(),
        });
        publisher.notify_on_send = false;
        crate::generic_publisher::GenericPublisher::iox2_open(&mut publisher, ctx)
            .map_err(|error| error.to_string())?;
        Ok(Some(Box::new(TypedIox2SyntheticPublisher { publisher })))
    }
    fn for_each_queued_input(&self, f: &mut dyn FnMut(&MessageHeader, &dyn std::any::Any)) {
        for sample in self.read.borrow().iter() {
            f(&sample.header, &sample.message as &dyn std::any::Any);
        }
    }
    fn drain_queued_inputs(
        &mut self,
        f: &mut dyn FnMut(
            &MessageHeader,
            &dyn std::any::Any,
        ) -> Result<(), crate::channel_registry::BoxedError>,
    ) -> Result<(), crate::channel_registry::BoxedError> {
        for sample in self.read.get_mut().drain(..) {
            f(&sample.header, &sample.message)?;
        }
        Ok(())
    }
}

/// Read-only queue guard for samples retained by an iox2 input.
pub struct Iox2SampleGuard<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> {
    samples: std::cell::RefMut<'a, VecDeque<Sample<ipc_threadsafe::Service, Message<T>, ()>>>,
}

impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2SampleGuard<'_, T> {
    /// Return the newest retained sample.
    pub fn back(&self) -> Option<&Message<T>> {
        self.samples.back().map(|s| &**s)
    }
    /// Return the oldest retained sample.
    pub fn front(&self) -> Option<&Message<T>> {
        self.samples.front().map(|s| &**s)
    }
    /// Remove the oldest retained sample.
    pub fn pop_front(&mut self) {
        self.samples.pop_front();
    }
    /// Number of retained samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }
    /// Whether no samples are retained.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Optional, non-triggering view of an iox2 input. With capacity greater than
/// one, `value()` selects the newest sample; use `clear()` to consume the oldest.
pub struct Iox2OptionalInput<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> {
    guard: Iox2SampleGuard<'a, T>,
}
impl<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2OptionalInput<'a, T> {
    /// Borrow an iox2 subscriber as an input view.
    pub fn new(subscriber: &'a Iox2Subscriber<T>) -> Self {
        Self {
            guard: Iox2SampleGuard {
                samples: subscriber.read.borrow_mut(),
            },
        }
    }
    /// Borrow the newest payload, if any.
    pub fn value(&self) -> Option<&T> {
        self.guard.back().map(|m| &m.message)
    }
    /// Borrow the newest message header, if any.
    pub fn header(&self) -> Option<&MessageHeader> {
        self.guard.back().map(|m| &m.header)
    }
    /// Remove the oldest retained sample.
    pub fn clear(&mut self) {
        self.guard.pop_front();
    }
}

/// Windowed view over an iox2 input's retained samples, oldest to newest.
/// Capacity greater than one provides the lookback window used by event-data
/// recorder patterns.
pub struct Iox2SpanInput<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> {
    guard: Iox2SampleGuard<'a, T>,
}

impl<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2SpanInput<'a, T> {
    /// Borrow the retained samples of an iox2 subscriber.
    pub fn new(subscriber: &'a Iox2Subscriber<T>) -> Self {
        Self {
            guard: Iox2SampleGuard {
                samples: subscriber.read.borrow_mut(),
            },
        }
    }

    /// Iterate retained samples, oldest to newest.
    pub fn inputs(&self) -> impl Iterator<Item = &Message<T>> + '_ {
        self.guard.samples.iter().map(|sample| &**sample)
    }

    /// Number of retained samples.
    pub fn len(&self) -> usize {
        self.guard.len()
    }

    /// Whether no samples are retained.
    pub fn is_empty(&self) -> bool {
        self.guard.is_empty()
    }
}

/// Native-config-compatible iox2 publisher field type.
pub struct Iox2Publisher<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    config: PublisherConfig,
    /// Whether a send also notifies the same-named event service. Users may
    /// mutate this generated `PubOrSubs` field before building the graph.
    pub notify_on_send: bool,
    /// Event identifier associated with notifications. Users may mutate this
    /// generated `PubOrSubs` field before building the graph.
    pub event_id: EventId,
    port: Option<IoxPublisher<ipc_threadsafe::Service, Message<T>, ()>>,
    notifier: Option<Notifier<ipc_threadsafe::Service>>,
    loans: Vec<LoanedIox2Message<T>>,
    marker: PhantomData<T>,
}

struct TypedIox2SyntheticPublisher<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    publisher: Iox2Publisher<T>,
}

impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2SyntheticPublisher
    for TypedIox2SyntheticPublisher<T>
{
    fn channel(&self) -> &str {
        &self.publisher.config.channel_name
    }

    fn payload_type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<T>()
    }

    fn publish(
        &mut self,
        header: MessageHeader,
        payload: Box<dyn std::any::Any + Send>,
    ) -> Result<(), String> {
        let payload = payload.downcast::<T>().map_err(|_| {
            format!(
                "simulation publisher payload type mismatch on {}",
                self.channel()
            )
        })?;
        self.publisher.publish_with_header(header, *payload)
    }

    fn publish_replay(
        &mut self,
        header: MessageHeader,
        payload: Box<dyn std::any::Any>,
    ) -> Result<(), String> {
        let payload = payload.downcast::<T>().map_err(|_| {
            format!(
                "replay publisher payload type mismatch on {}",
                self.channel()
            )
        })?;
        self.publisher.publish_with_header(header, *payload)
    }
}

impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Iox2Publisher<T> {
    /// Create an iox2 output declaration.
    pub fn new(config: PublisherConfig) -> Self {
        assert!(
            config.capacity > 0,
            "iox2 publisher capacity must be positive for channel {}",
            config.channel_name
        );
        Self {
            config,
            notify_on_send: true,
            event_id: EventId::new(0),
            port: None,
            notifier: None,
            loans: Vec::new(),
            marker: PhantomData,
        }
    }

    /// Access the native publisher configuration.
    pub fn config(&self) -> &PublisherConfig {
        &self.config
    }
    /// Mutably access the native publisher configuration.
    pub fn config_mut(&mut self) -> &mut PublisherConfig {
        &mut self.config
    }

    /// Publish a payload with its supplied header without emitting an event notification.
    /// Intended for inputs controlled by simulation and unit tests.
    pub fn publish_with_header(&self, header: MessageHeader, payload: T) -> Result<(), String> {
        let channel = self.config.channel_name.as_str();
        let port = self
            .port
            .as_ref()
            .ok_or_else(|| format!("iox2 simulation publisher for {channel} is not open"))?;
        port.loan_uninit()
            .map_err(|error| format!("iox2 simulation loan failed on {channel}: {error}"))?
            .write_payload(Message {
                header,
                message: payload,
            })
            .send()
            .map(|_| ())
            .map_err(|error| format!("iox2 simulation send failed on {channel}: {error}"))
    }
}

/// Mutable output view over a single iox2 loan.
pub struct Iox2Output<'a, T: Debug + ZeroCopySend + Send + Sync + Default + 'static> {
    publisher: &'a mut Iox2Publisher<T>,
    index: usize,
}
impl<'a, T: Debug + ZeroCopySend + Send + Sync + Default + 'static> Iox2Output<'a, T> {
    /// Acquire and initialize a default payload loan.
    pub fn new_default(publisher: &'a mut Iox2Publisher<T>) -> Self {
        let channel = publisher.config.channel_name.clone();
        let sample = publisher
            .port
            .as_ref()
            .unwrap_or_else(|| panic!("iox2 publisher for channel {channel} is not open"))
            .loan_uninit()
            .unwrap_or_else(|error| panic!("iox2 loan failed for channel {channel}: {error}"))
            .write_payload(Message {
                header: MessageHeader::default(),
                message: T::default(),
            });
        publisher.loans.push(LoanedIox2Message {
            sample,
            sent: false,
        });
        let index = publisher.loans.len() - 1;
        Self { publisher, index }
    }
    /// Mark this loan for publication at the next flush.
    pub fn send(self) {
        self.publisher.loans[self.index].sent = true;
    }
}

impl<T: Debug + ZeroCopySend + Send + Sync + 'static> crate::generic_publisher::GenericPublisher
    for Iox2Publisher<T>
{
    fn iox2_find_endpoints(
        &self,
        add: &mut dyn FnMut(
            crate::pub_sub_factory::Iox2EndpointInfo,
        ) -> Result<(), crate::task_graph_builder::TaskGraphBuildError>,
    ) -> Result<(), crate::task_graph_builder::TaskGraphBuildError> {
        add(crate::pub_sub_factory::Iox2EndpointInfo {
            channel: self.config.channel_name.clone(),
            kind: crate::pub_sub_factory::EndpointKind::Iox2DataPub {
                capacity: self.config.capacity,
                notify_on_send: self.notify_on_send,
                event_id: self.event_id.as_value(),
            },
            payload_type: Some(std::any::TypeId::of::<T>()),
        })
    }
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn config(&self) -> &PublisherConfig {
        &self.config
    }
    fn config_mut(&mut self) -> &mut PublisherConfig {
        &mut self.config
    }
    fn forwarded_channels(&self) -> &[String] {
        &[]
    }
    fn flush_loaned_values(
        &mut self,
        timestamp: crate::time::FrameworkTime,
        _sink: &mut dyn crate::scheduling::ReadyNodeSink,
    ) {
        for mut loan in self.loans.drain(..) {
            if loan.sent {
                loan.sample.header.published_at = timestamp;
                loan.sample.send().unwrap_or_else(|error| {
                    panic!(
                        "iox2 send failed for channel {}: {error}",
                        self.config.channel_name
                    )
                });
                if self.notify_on_send {
                    self.notifier
                        .as_ref()
                        .unwrap_or_else(|| {
                            panic!(
                                "iox2 notifier for channel {} is not open",
                                self.config.channel_name
                            )
                        })
                        .notify()
                        .unwrap_or_else(|error| {
                            panic!(
                                "iox2 notify failed for channel {}: {error}",
                                self.config.channel_name
                            )
                        });
                }
            }
        }
    }
    fn allocate_arena(&mut self) {}
    fn increase_arena_size(&mut self, _additional_capacity: usize) {}
    fn connect_to_subscriber(
        &mut self,
        subscriber: &mut dyn GenericSubscriber,
    ) -> Result<(), crate::generic_publisher::ConnectionTypeMismatch> {
        if subscriber
            .as_any()
            .downcast_mut::<Iox2Subscriber<T>>()
            .is_some()
        {
            return Ok(());
        }
        if subscriber
            .as_any()
            .downcast_mut::<Iox2EventSubscriber>()
            .is_some()
        {
            return Ok(());
        }
        Err(crate::generic_publisher::ConnectionTypeMismatch::new(
            self.config.channel_name.clone(),
            "iox2",
            "native",
        ))
    }
    fn for_each_pending_output(&self, f: &mut dyn FnMut(&MessageHeader, &dyn std::any::Any)) {
        for loan in self.loans.iter().filter(|loan| loan.sent) {
            f(
                &loan.sample.header,
                &loan.sample.message as &dyn std::any::Any,
            );
        }
    }
    fn build_matching_subscriber(
        &self,
        config: SubscriberConfig,
    ) -> Option<Box<dyn GenericSubscriber>> {
        Some(Box::new(Iox2Subscriber::<T>::new(config)))
    }
    fn value_type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<T>()
    }
    fn iox2_open(&mut self, ctx: &mut dyn Iox2OpenCtx) -> Result<(), TaskGraphBuildError> {
        let channel = self.config.channel_name.clone();
        let name = ctx.service_name(&channel)?;
        let settings = ctx.pubsub_settings(&channel)?;
        let service = ctx
            .node()
            .service_builder(&name)
            .publish_subscribe::<Message<T>>()
            .subscriber_max_buffer_size(settings.subscriber_max_buffer_size)
            .subscriber_max_borrowed_samples(settings.subscriber_max_borrowed_samples)
            .history_size(settings.history_size)
            .enable_safe_overflow(settings.enable_safe_overflow)
            .max_publishers(settings.max_publishers)
            .max_subscribers(settings.max_subscribers)
            .max_nodes(settings.max_nodes)
            .open_or_create()
            .map_err(|error| TaskGraphBuildError::Iox2ServiceOpen {
                channel: channel.clone(),
                source: error.to_string(),
            })?;
        let port = service
            .publisher_builder()
            .max_loaned_samples(self.config.capacity)
            .create()
            .map_err(|error| TaskGraphBuildError::Iox2ServiceOpen {
                channel: channel.clone(),
                source: error.to_string(),
            })?;
        self.port = Some(port);
        if self.notify_on_send {
            let event_service = ctx.event_service(&channel)?;
            let notifier = event_service
                .notifier_builder()
                .default_event_id(self.event_id)
                .create()
                .map_err(|error| TaskGraphBuildError::Iox2ServiceOpen {
                    channel,
                    source: error.to_string(),
                })?;
            self.notifier = Some(notifier);
        }
        Ok(())
    }
}
impl<T: Debug + ZeroCopySend + Send + Sync + Default + 'static> Deref for Iox2Output<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.publisher.loans[self.index].sample.message
    }
}
impl<T: Debug + ZeroCopySend + Send + Sync + Default + 'static> DerefMut for Iox2Output<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.publisher.loans[self.index].sample.message
    }
}

/// Service-wide publish-subscribe settings for one channel, aggregated from
/// every endpoint the scan found. All processes opening the channel must agree
/// on these values; the first creator establishes them and later openers may
/// not request anything larger.
#[derive(Clone, Debug)]
pub struct PubSubServiceSettings {
    /// Per-subscriber-port receive buffer ceiling.
    pub subscriber_max_buffer_size: usize,
    /// How many received samples a consumer may hold at once.
    pub subscriber_max_borrowed_samples: usize,
    /// Historical samples delivered to late subscribers.
    pub history_size: usize,
    /// Drop-oldest overflow instead of blocking senders.
    pub enable_safe_overflow: bool,
    pub max_publishers: usize,
    pub max_subscribers: usize,
    pub max_nodes: usize,
}

/// Service-wide event settings for one channel.
#[derive(Clone, Debug)]
pub struct EventServiceSettings {
    pub max_notifiers: usize,
    pub max_listeners: usize,
    pub max_nodes: usize,
    /// Largest notification event id accepted by the service (inclusive).
    /// Keep this small: listeners scan the whole id range on every drain.
    pub event_id_max_value: usize,
}

/// Supplies endpoints with the graph's node, service names, and the settings
/// established during the endpoint scan. Endpoints perform their own typed
/// service opens (an object-safe context cannot name a payload type); the
/// context caches the untyped event services.
pub trait Iox2OpenCtx {
    /// The graph's iceoryx2 node.
    fn node(&self) -> &Node<ipc_threadsafe::Service>;
    /// Service name for a channel.
    fn service_name(&self, channel: &str) -> Result<ServiceName, TaskGraphBuildError>;
    /// Aggregated publish-subscribe settings for a channel.
    fn pubsub_settings(&self, channel: &str)
    -> Result<&PubSubServiceSettings, TaskGraphBuildError>;
    /// Get or create the channel's event service.
    fn event_service(
        &mut self,
        channel: &str,
    ) -> Result<&EventPortFactory<ipc_threadsafe::Service>, TaskGraphBuildError>;
}

/// Graph-scoped iceoryx2 resources: one node, per-channel service settings
/// from the endpoint scan, and a cache of opened event services. Settings are
/// immutable after construction; only the event-service cache grows.
#[derive(Debug)]
pub struct Iox2Context {
    node: Node<ipc_threadsafe::Service>,
    pubsub_settings: HashMap<String, PubSubServiceSettings>,
    event_settings: HashMap<String, EventServiceSettings>,
    event_services: HashMap<String, EventPortFactory<ipc_threadsafe::Service>>,
}

/// Per-channel endpoint counts gathered while aggregating scan results.
#[derive(Default)]
struct ChannelAggregates {
    max_sub_capacity: usize,
    data_sub_count: usize,
    data_pub_count: usize,
    event_sub_count: usize,
    notifier_count: usize,
    max_event_id: usize,
}

impl Iox2Context {
    /// Aggregate scan results into per-channel service settings and create the
    /// graph's node. Returns `Ok(None)` when the graph declares no iox2
    /// endpoints, so iceoryx2-free graphs never create a node.
    pub fn from_endpoints(
        graph_config: &Iox2GraphConfig,
        endpoints: &[Iox2EndpointInfo],
    ) -> Result<Option<Self>, TaskGraphBuildError> {
        if !endpoints.iter().any(|endpoint| endpoint.kind.is_iox2()) {
            return Ok(None);
        }
        let config: &Config = match graph_config.config.as_ref() {
            Some(config) => config,
            None => Config::global_config(),
        };
        let defaults = config;
        let pubsub_defaults = &defaults.defaults.publish_subscribe;
        let event_defaults = &defaults.defaults.event;

        let mut aggregates: HashMap<&str, ChannelAggregates> = HashMap::new();
        for endpoint in endpoints {
            let channel = aggregates.entry(endpoint.channel.as_str()).or_default();
            match endpoint.kind {
                EndpointKind::Iox2DataSub { capacity } => {
                    channel.max_sub_capacity = channel.max_sub_capacity.max(capacity);
                    channel.data_sub_count += 1;
                }
                EndpointKind::Iox2DataPub {
                    notify_on_send,
                    event_id,
                    ..
                } => {
                    channel.data_pub_count += 1;
                    if notify_on_send {
                        channel.notifier_count += 1;
                        channel.max_event_id = channel.max_event_id.max(event_id);
                    }
                }
                EndpointKind::Iox2EventSub => channel.event_sub_count += 1,
                EndpointKind::Iox2Notifier { event_id } => {
                    channel.notifier_count += 1;
                    channel.max_event_id = channel.max_event_id.max(event_id);
                }
                _ => {}
            }
        }

        let mut pubsub_settings = HashMap::new();
        let mut event_settings = HashMap::new();
        for (channel, agg) in aggregates {
            if agg.data_sub_count > 0 || agg.data_pub_count > 0 {
                let buffer_size = agg.max_sub_capacity.max(1);
                pubsub_settings.insert(
                    channel.to_string(),
                    PubSubServiceSettings {
                        subscriber_max_buffer_size: buffer_size,
                        subscriber_max_borrowed_samples: 2 * buffer_size,
                        history_size: 0,
                        enable_safe_overflow: true,
                        max_publishers: pubsub_defaults
                            .max_publishers
                            .max(agg.data_pub_count + usize::from(agg.data_sub_count > 0)),
                        max_subscribers: pubsub_defaults.max_subscribers.max(agg.data_sub_count),
                        max_nodes: pubsub_defaults.max_nodes,
                    },
                );
            }
            if agg.event_sub_count > 0 || agg.notifier_count > 0 {
                event_settings.insert(
                    channel.to_string(),
                    EventServiceSettings {
                        max_notifiers: event_defaults.max_notifiers.max(agg.notifier_count),
                        max_listeners: event_defaults.max_listeners.max(agg.event_sub_count),
                        max_nodes: event_defaults.max_nodes,
                        event_id_max_value: agg.max_event_id,
                    },
                );
            }
        }

        let mut builder = NodeBuilder::new();
        if let Some(config) = &graph_config.config {
            builder = builder.config(config);
        }
        if let Some(node_name) = &graph_config.node_name {
            let node_name = NodeName::new(node_name).map_err(|error| {
                TaskGraphBuildError::Iox2NodeCreation {
                    source: error.to_string(),
                }
            })?;
            builder = builder.name(&node_name);
        }
        let node = builder
            .create::<ipc_threadsafe::Service>()
            .map_err(|error| TaskGraphBuildError::Iox2NodeCreation {
                source: error.to_string(),
            })?;
        Ok(Some(Self {
            node,
            pubsub_settings,
            event_settings,
            event_services: HashMap::new(),
        }))
    }

    /// Build the service name for a channel.
    fn build_service_name(&self, channel: &str) -> Result<ServiceName, TaskGraphBuildError> {
        ServiceName::new(channel).map_err(|error| TaskGraphBuildError::InvalidServiceName {
            channel: channel.to_string(),
            reason: error.to_string(),
        })
    }
}

impl Iox2OpenCtx for Iox2Context {
    fn node(&self) -> &Node<ipc_threadsafe::Service> {
        &self.node
    }
    fn service_name(&self, channel: &str) -> Result<ServiceName, TaskGraphBuildError> {
        self.build_service_name(channel)
    }
    fn pubsub_settings(
        &self,
        channel: &str,
    ) -> Result<&PubSubServiceSettings, TaskGraphBuildError> {
        self.pubsub_settings
            .get(channel)
            .ok_or_else(|| TaskGraphBuildError::Iox2ServiceOpen {
                channel: channel.to_string(),
                source: "endpoint scan recorded no publish-subscribe settings for this channel"
                    .to_string(),
            })
    }
    fn event_service(
        &mut self,
        channel: &str,
    ) -> Result<&EventPortFactory<ipc_threadsafe::Service>, TaskGraphBuildError> {
        if !self.event_services.contains_key(channel) {
            let name = self.build_service_name(channel)?;
            let mut builder = self.node.service_builder(&name).event();
            if let Some(settings) = self.event_settings.get(channel) {
                builder = builder
                    .max_notifiers(settings.max_notifiers)
                    .max_listeners(settings.max_listeners)
                    .event_id_max_value(settings.event_id_max_value)
                    .max_nodes(settings.max_nodes);
            }
            let service =
                builder
                    .open_or_create()
                    .map_err(|error| TaskGraphBuildError::Iox2ServiceOpen {
                        channel: channel.to_string(),
                        source: error.to_string(),
                    })?;
            self.event_services.insert(channel.to_string(), service);
        }
        Ok(self
            .event_services
            .get(channel)
            .expect("event service was just inserted"))
    }
}

#[cfg(all(test, feature = "iceoryx2"))]
mod event_staging_tests {
    use super::*;
    use crate::callback::CallbackViews;
    use crate::generic_subscriber::GenericSubscriber;

    fn subscriber(name: &str) -> Iox2EventSubscriber {
        Iox2EventSubscriber::new(SubscriberConfig {
            is_optional: true,
            capacity: 4,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: name.into(),
        })
    }

    /// Staging drain moves the complete batch and clears the previous run's records.
    #[test]
    fn iox2_event_staging_drains_all_and_clears() {
        let sub = subscriber("staging_clear");
        sub.inject_events(
            (0..3).map(|id| EventRecord {
                event_id: EventId::new(id),
                count: 1,
            }),
            &mut crate::scheduling::NoopReadyNodeSink,
        );
        assert_eq!(sub.queue_info().writer_size, 3);
        sub.drain_writer_to_reader();
        assert_eq!(sub.queue_info().reader_size, 3);
        sub.drain_writer_to_reader();
        assert_eq!(sub.queue_info().reader_size, 0);
    }

    /// Event views sum counts and preserve record order.
    #[test]
    fn iox2_event_staging_counts_and_records() {
        let sub = subscriber("staging_records");
        sub.inject_events(
            [(EventId::new(2), 3), (EventId::new(7), 5)]
                .into_iter()
                .map(|(event_id, count)| EventRecord { event_id, count }),
            &mut crate::scheduling::NoopReadyNodeSink,
        );
        sub.drain_writer_to_reader();
        let view = Iox2Event::new(&sub);
        assert_eq!(view.count(), 8);
        assert_eq!(
            view.records().collect::<Vec<_>>(),
            vec![(EventId::new(2), 3), (EventId::new(7), 5)]
        );
    }

    /// Event batch count saturates instead of overflowing when records sum past u64::MAX.
    #[test]
    fn iox2_event_staging_count_saturates() {
        let sub = subscriber("staging_count_saturates");
        sub.inject_events(
            [
                EventRecord {
                    event_id: EventId::new(1),
                    count: u64::MAX,
                },
                EventRecord {
                    event_id: EventId::new(2),
                    count: 1,
                },
            ],
            &mut crate::scheduling::NoopReadyNodeSink,
        );
        sub.drain_writer_to_reader();
        assert_eq!(Iox2Event::new(&sub).count(), u64::MAX);
    }

    /// Deposits made by distinct producer passes are visible together in one run.
    #[test]
    fn iox2_event_staging_multi_injection_aggregates() {
        let sub = subscriber("staging_multi");
        sub.inject_events(
            [EventRecord {
                event_id: EventId::new(1),
                count: 4,
            }],
            &mut crate::scheduling::NoopReadyNodeSink,
        );
        sub.inject_events(
            [EventRecord {
                event_id: EventId::new(2),
                count: 6,
            }],
            &mut crate::scheduling::NoopReadyNodeSink,
        );
        sub.drain_writer_to_reader();
        assert_eq!(Iox2Event::new(&sub).count(), 10);
    }

    /// Staged events request startup execution; draining clears the pending request.
    #[test]
    fn iox2_event_staging_requests_execution_tracks_queue() {
        let sub = subscriber("staging_startup");
        assert!(!sub.requests_execution());
        sub.inject_events(
            [EventRecord {
                event_id: EventId::new(0),
                count: 1,
            }],
            &mut crate::scheduling::NoopReadyNodeSink,
        );
        assert!(sub.requests_execution());
        sub.drain_writer_to_reader();
        assert!(!sub.requests_execution());
    }

    struct StagingGateCallback {
        required: crate::subscriber::Subscriber<u64>,
        event: Iox2EventSubscriber,
    }

    impl crate::callback::Callback for StagingGateCallback {
        fn run(&mut self, _ctx: &crate::context::Context) {}
        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(crate::callback::PubOrSub<'a>)) {
            f(crate::callback::PubOrSub::Subscriber(&self.required));
            f(crate::callback::PubOrSub::Subscriber(&self.event));
        }
        fn for_each_pub_or_sub_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(crate::callback::PubOrSubMut<'a>),
        ) {
            f(crate::callback::PubOrSubMut::Subscriber(&mut self.required));
            f(crate::callback::PubOrSubMut::Subscriber(&mut self.event));
        }
    }

    struct CountingSink(usize);
    impl crate::scheduling::ReadyNodeSink for CountingSink {
        fn schedule(&mut self, _node: crate::scheduling::CallbackNodeId) {
            self.0 += 1;
        }
    }

    /// Event notifications wait for required inputs, and later event arrivals nudge a ready node.
    #[test]
    fn iox2_event_staging_gating_respects_required_inputs() {
        let required = crate::subscriber::Subscriber::new(SubscriberConfig {
            is_optional: false,
            capacity: 1,
            is_trigger: false,
            keep_across_runs: true,
            channel_name: "required_native".into(),
        });
        let event = subscriber("optional_event");
        let mut node = crate::callback::CallbackNode::new_named(
            Box::new(StagingGateCallback { required, event }),
            "gated_event".into(),
        );
        node.bind_id(crate::scheduling::CallbackNodeId(3));
        let mut sink = CountingSink(0);
        node.callback_mut().for_each_subscriber_mut(&mut |sub| {
            if sub.config().channel_name == "optional_event" {
                let event = sub.as_any().downcast_mut::<Iox2EventSubscriber>().unwrap();
                event.inject_events(
                    [EventRecord {
                        event_id: EventId::new(1),
                        count: 1,
                    }],
                    &mut sink,
                );
            }
        });
        assert_eq!(
            sink.0, 0,
            "event alone cannot bypass the required input gate"
        );
        let readiness = node.callback().collect_subscribers()[0]
            .readiness_state()
            .unwrap();
        if let crate::callback::SubscriberReadiness::Gating(readiness, bit) = readiness {
            assert_eq!(
                readiness.gating_input_arrived(bit),
                Some(crate::scheduling::CallbackNodeId(3))
            );
        } else {
            panic!("required input must own a gating bit");
        }
        assert_eq!(
            sink.0, 0,
            "the gating arrival only returns a node id to its producer"
        );
        node.callback_mut().for_each_subscriber_mut(&mut |sub| {
            if sub.config().channel_name == "optional_event" {
                let event = sub.as_any().downcast_mut::<Iox2EventSubscriber>().unwrap();
                event.inject_events(
                    [EventRecord {
                        event_id: EventId::new(2),
                        count: 1,
                    }],
                    &mut sink,
                );
                assert!(event.requests_execution());
                event.drain_writer_to_reader();
                assert!(!event.requests_execution());
            }
        });
        assert_eq!(sink.0, 1);
    }
}

#[cfg(all(test, feature = "iceoryx2", not(miri)))]
mod tests {
    use super::*;
    use crate::{
        callback::{Callback, PubOrSub, PubOrSubMut},
        context::Context,
        time::FrameworkTime,
    };
    use crate::{
        pub_sub_factory::{EndpointKind, Iox2EndpointInfo},
        task_graph_builder::TaskGraphBuildError,
    };

    struct EventProducer(Iox2Notifier);
    impl Callback for EventProducer {
        fn run(&mut self, _ctx: &Context) {
            Iox2NotifyOutput::new(&mut self.0).send();
        }
        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Publisher(&self.0));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Publisher(&mut self.0));
        }
    }

    struct EventConsumer(Iox2EventSubscriber);
    impl Callback for EventConsumer {
        fn run(&mut self, _ctx: &Context) {}
        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Subscriber(&self.0));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Subscriber(&mut self.0));
        }
    }

    /// An event-only graph opens its declared event service and graph notifier.
    #[test]
    fn iox2_event_subscriber_opens_service() {
        let channel = "stage3_event_sub_open";
        let built = TaskGraphBuilder::new()
            .add_pool(1, |pool| {
                pool.add_callback_builder(
                    test_callback(
                        "event_sub",
                        Box::new(EventConsumer(Iox2EventSubscriber::new(
                            iox2_subscriber_config(channel, 4),
                        ))),
                    )
                    .with_subscriber_channels(&[channel]),
                )
            })
            .build()
            .expect("event input graph builds");
        assert!(built.iox2_context.is_some());
    }

    /// Data and event ports can share a channel without cross-connecting service kinds.
    #[test]
    fn same_name_data_and_event_endpoints_build_together() {
        let channel = format!("stage4_shared_service_{}", std::process::id());
        let result = TaskGraphBuilder::new()
            .add_pool(1, |pool| {
                pool.add_callback_builder(test_callback(
                    "data_producer",
                    Box::new(CountingProducer {
                        publisher: Iox2Publisher::new(crate::publisher::PublisherConfig {
                            capacity: 1,
                            channel_name: channel.clone(),
                        }),
                        next: 1,
                    }),
                ))
                .add_callback_builder(test_callback(
                    "event_producer",
                    Box::new(EventProducer(Iox2Notifier::new(
                        crate::publisher::PublisherConfig {
                            capacity: 1,
                            channel_name: channel.clone(),
                        },
                    ))),
                ))
                .add_callback_builder(test_callback(
                    "data_consumer",
                    Box::new(Iox2Consumer {
                        subscriber: Iox2Subscriber::new(iox2_subscriber_config(&channel, 2)),
                    }),
                ))
                .add_callback_builder(test_callback(
                    "event_consumer",
                    Box::new(EventConsumer(Iox2EventSubscriber::new(
                        iox2_subscriber_config(&channel, 2),
                    ))),
                ))
            })
            .build();
        assert!(result.is_ok(), "same-name iox2 graph failed: {result:?}");
    }

    /// A graph notifier publishes its configured id to an independently attached listener.
    #[test]
    fn iox2_notifier_notifies_through_graph() {
        let channel = "stage3_notifier_graph";
        let mut built = TaskGraphBuilder::new()
            .add_pool(1, |pool| {
                pool.add_callback_builder(
                    test_callback(
                        "notifier",
                        Box::new(EventProducer(Iox2Notifier::new(
                            crate::publisher::PublisherConfig {
                                capacity: 1,
                                channel_name: channel.into(),
                            },
                        ))),
                    )
                    .with_publisher_channels(&[channel]),
                )
            })
            .build()
            .expect("notifier graph builds");
        let context = built.iox2_context.as_mut().unwrap();
        let listener = context
            .event_service(channel)
            .unwrap()
            .listener_builder()
            .create()
            .unwrap();
        let names = crate::string_interner::ChannelNameInterner::default();
        let callbacks = crate::string_interner::CallbackNameInterner::default();
        let time = crate::time::FrameworkTime::from_nanoseconds(42);
        let ctx = Context::new(time, &names, &callbacks);
        built.pools[0]
            .nodes
            .get(crate::scheduling::CallbackNodeId(0))
            .unwrap()
            .access(|node| {
                node.run(&ctx);
                node.flush_publishers(time, &mut crate::scheduling::NoopReadyNodeSink);
            });
        let mut seen = false;
        listener
            .try_wait(|activation| {
                seen |= activation.id == EventId::new(0);
            })
            .unwrap();
        assert!(seen);
    }

    fn info(
        channel: &str,
        kind: EndpointKind,
        payload_type: Option<std::any::TypeId>,
    ) -> Iox2EndpointInfo {
        Iox2EndpointInfo {
            channel: channel.into(),
            kind,
            payload_type,
        }
    }

    /// A declared data channel creates graph-scoped publish-subscribe settings and node state.
    #[test]
    fn context_aggregates_data_settings() {
        let endpoints = [
            info(
                "stage2_context_settings",
                EndpointKind::Iox2DataSub { capacity: 3 },
                Some(std::any::TypeId::of::<u64>()),
            ),
            info(
                "stage2_context_settings",
                EndpointKind::Iox2DataPub {
                    capacity: 2,
                    notify_on_send: false,
                    event_id: 0,
                },
                Some(std::any::TypeId::of::<u64>()),
            ),
        ];
        let context = Iox2Context::from_endpoints(&Iox2GraphConfig::default(), &endpoints)
            .unwrap()
            .unwrap();
        let settings = context.pubsub_settings("stage2_context_settings").unwrap();
        assert_eq!(settings.subscriber_max_buffer_size, 3);
        assert_eq!(settings.subscriber_max_borrowed_samples, 6);
        assert!(settings.enable_safe_overflow);
        assert!(
            !context
                .event_settings
                .contains_key("stage2_context_settings")
        );
    }

    /// An injected iceoryx2 config controls both the node and service defaults.
    #[test]
    fn graph_config_is_applied_to_node_and_service_settings() {
        let channel = "stage2_injected_config";
        let endpoints = [info(channel, EndpointKind::Iox2EventSub, None)];
        let mut config = Config::default();
        config.defaults.event.max_listeners = 23;
        let context = Iox2Context::from_endpoints(
            &Iox2GraphConfig {
                node_name: None,
                config: Some(config.clone()),
            },
            &endpoints,
        )
        .unwrap()
        .unwrap();

        assert_eq!(context.node().config(), &config);
        assert_eq!(context.event_settings[channel].max_listeners, 23);
    }

    /// Declared notifier ids set the inclusive event service maximum.
    #[test]
    fn notifier_event_limit_includes_declared_id() {
        let endpoints = [info(
            "stage2_event_limit",
            EndpointKind::Iox2Notifier { event_id: 17 },
            None,
        )];
        let context = Iox2Context::from_endpoints(&Iox2GraphConfig::default(), &endpoints)
            .unwrap()
            .unwrap();
        assert_eq!(
            context.event_settings["stage2_event_limit"].event_id_max_value,
            17
        );
    }

    /// Event and publish-subscribe settings are independently aggregated for the same name.
    #[test]
    fn data_and_event_settings_coexist_under_one_channel_name() {
        let endpoints = [
            info(
                "stage2_coexist",
                EndpointKind::Iox2DataPub {
                    capacity: 1,
                    notify_on_send: true,
                    event_id: 4,
                },
                Some(std::any::TypeId::of::<u64>()),
            ),
            info(
                "stage2_coexist",
                EndpointKind::Iox2DataSub { capacity: 1 },
                Some(std::any::TypeId::of::<u64>()),
            ),
        ];
        let context = Iox2Context::from_endpoints(&Iox2GraphConfig::default(), &endpoints)
            .unwrap()
            .unwrap();
        assert!(context.pubsub_settings.contains_key("stage2_coexist"));
        assert_eq!(
            context.event_settings["stage2_coexist"].event_id_max_value,
            4
        );
    }

    // ── graph-level tests ─────────────────────────────────────────────
    //
    // These drive real services: hand-written callbacks hold the iox2 field
    // types (the macro integration comes later), the graph is built through
    // `TaskGraphBuilder` (running the scan, validation, and open lifecycle),
    // and nodes are exercised through `SharedCallbackNode::access`.

    use crate::publisher::Publisher as NativePublisher;
    use crate::scheduling::{CallbackNodeId, NoopReadyNodeSink};
    use crate::string_interner::{CallbackNameInterner, ChannelNameInterner};
    use crate::subscriber::Subscriber as NativeSubscriber;
    use crate::task_graph_builder::TaskGraphBuilder;

    /// Wrap a hand-written callback the way the proc macro does: named, with a
    /// nominal execution duration (required by `CallbackBuilder::build`).
    fn test_callback(
        name: &str,
        callback: Box<dyn Callback>,
    ) -> crate::callback_builder::CallbackBuilder {
        crate::callback_builder::CallbackBuilder::new(name.into(), callback)
            .with_execution_duration_callback(|| std::time::Duration::from_millis(1))
    }

    /// Publisher callback that publishes an incrementing value each run.
    struct CountingProducer {
        publisher: Iox2Publisher<u64>,
        next: u64,
    }
    impl Callback for CountingProducer {
        fn run(&mut self, _ctx: &Context) {
            let mut output = Iox2Output::new_default(&mut self.publisher);
            *output = self.next;
            self.next += 1;
            output.send();
        }
        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Publisher(&self.publisher));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Publisher(&mut self.publisher));
        }
    }

    /// Passive consumer callback exposing an iox2 subscriber to the graph.
    struct Iox2Consumer {
        subscriber: Iox2Subscriber<u64>,
    }
    impl Callback for Iox2Consumer {
        fn run(&mut self, _ctx: &Context) {}
        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Subscriber(&self.subscriber));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Subscriber(&mut self.subscriber));
        }
    }

    /// Native producer used to exercise mixed-transport rejection.
    struct NativeProducer {
        publisher: NativePublisher<u64>,
    }
    impl Callback for NativeProducer {
        fn run(&mut self, _ctx: &Context) {}
        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Publisher(&self.publisher));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Publisher(&mut self.publisher));
        }
    }

    /// Native consumer used to exercise mixed-transport rejection.
    struct NativeConsumer {
        subscriber: NativeSubscriber<u64>,
    }
    impl Callback for NativeConsumer {
        fn run(&mut self, _ctx: &Context) {}
        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Subscriber(&self.subscriber));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Subscriber(&mut self.subscriber));
        }
    }

    fn iox2_subscriber_config(
        channel: &str,
        capacity: usize,
    ) -> crate::subscriber::SubscriberConfig {
        crate::subscriber::SubscriberConfig {
            is_optional: true,
            capacity,
            is_trigger: false,
            keep_across_runs: true,
            channel_name: channel.into(),
        }
    }

    fn iox2_publisher_config(channel: &str) -> crate::publisher::PublisherConfig {
        crate::publisher::PublisherConfig {
            capacity: 1,
            channel_name: channel.into(),
        }
    }

    fn build_producer_consumer_graph(channel: &str) -> crate::task_graph_builder::BuiltTaskGraph {
        TaskGraphBuilder::new()
            .with_iox2_config(crate::iox2::Iox2GraphConfig {
                node_name: None,
                ..Default::default()
            })
            .add_pool(1, |pool| {
                pool.add_callback_builder(test_callback(
                    "producer",
                    Box::new(CountingProducer {
                        publisher: Iox2Publisher::new(iox2_publisher_config(channel)),
                        next: 1,
                    }),
                ))
                .add_callback_builder(test_callback(
                    "consumer",
                    Box::new(Iox2Consumer {
                        subscriber: Iox2Subscriber::new(iox2_subscriber_config(channel, 1)),
                    }),
                ))
            })
            .build()
            .expect("iox2 producer/consumer graph should build")
    }

    /// Run the producer `count` times (publish + flush each time). The context
    /// and interners must be created by the caller so the borrow outlives the
    /// call.
    fn run_producer(pools: &[crate::executor::ThreadPoolConfig], count: usize, ctx: &Context<'_>) {
        for _ in 0..count {
            pools[0]
                .nodes
                .get(CallbackNodeId(0))
                .expect("producer node")
                .access(|node| {
                    node.run(ctx);
                    node.flush_publishers(ctx.now, &mut NoopReadyNodeSink);
                });
        }
    }

    /// Drain the consumer and return the newest payload and header it sees.
    fn drain_consumer(
        pools: &[crate::executor::ThreadPoolConfig],
    ) -> (Option<u64>, Option<MessageHeader>) {
        let mut value = None;
        let mut header = None;
        pools[0]
            .nodes
            .get(CallbackNodeId(1))
            .expect("consumer node")
            .access(|node| {
                node.drain_subscribers();
                node.callback_mut().for_each_subscriber_mut(&mut |sub| {
                    let typed = sub
                        .as_any()
                        .downcast_mut::<Iox2Subscriber<u64>>()
                        .expect("consumer holds an Iox2Subscriber<u64>");
                    let input = Iox2OptionalInput::new(typed);
                    value = input.value().copied();
                    header = input.header().copied();
                });
            });
        (value, header)
    }

    /// Span input exposes the retained iox2 window oldest to newest.
    #[test]
    fn iox2_span_input_iterates_oldest_to_newest() {
        struct WindowProducer(Iox2Publisher<u64>, u64);
        impl Callback for WindowProducer {
            fn run(&mut self, _ctx: &Context) {
                let mut out = Iox2Output::new_default(&mut self.0);
                *out = self.1;
                self.1 += 1;
                out.send();
            }
            fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
                f(PubOrSub::Publisher(&self.0));
            }
            fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
                f(PubOrSubMut::Publisher(&mut self.0));
            }
        }
        struct WindowConsumer(Iox2Subscriber<u64>);
        impl Callback for WindowConsumer {
            fn run(&mut self, _ctx: &Context) {}
            fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
                f(PubOrSub::Subscriber(&self.0));
            }
            fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
                f(PubOrSubMut::Subscriber(&mut self.0));
            }
        }
        let channel = "stage2_span_window";
        let built = TaskGraphBuilder::new()
            .add_pool(1, |pool| {
                pool.add_callback_builder(
                    test_callback(
                        "window_pub",
                        Box::new(WindowProducer(
                            Iox2Publisher::new(PublisherConfig {
                                capacity: 1,
                                channel_name: channel.into(),
                            }),
                            1,
                        )),
                    )
                    .with_publisher_channels(&[channel]),
                )
                .add_callback_builder(
                    test_callback(
                        "window_sub",
                        Box::new(WindowConsumer(Iox2Subscriber::new(SubscriberConfig {
                            is_optional: true,
                            capacity: 4,
                            is_trigger: false,
                            keep_across_runs: true,
                            channel_name: channel.into(),
                        }))),
                    )
                    .with_subscriber_channels(&[channel]),
                )
            })
            .build()
            .expect("window graph builds");
        let names = ChannelNameInterner::default();
        let callbacks = CallbackNameInterner::default();
        let ctx = Context::new(FrameworkTime::from_nanoseconds(9), &names, &callbacks);
        for _ in 1..=3 {
            built.pools[0]
                .nodes
                .get(CallbackNodeId(0))
                .unwrap()
                .access(|node| {
                    node.run(&ctx);
                    node.flush_publishers(ctx.now, &mut NoopReadyNodeSink);
                });
        }
        built.pools[0]
            .nodes
            .get(CallbackNodeId(1))
            .unwrap()
            .access(|node| {
                node.drain_subscribers();
                node.callback_mut()
                    .for_each_subscriber_mut(&mut |subscriber| {
                        let subscriber = subscriber
                            .as_any()
                            .downcast_mut::<Iox2Subscriber<u64>>()
                            .unwrap();
                        let span = Iox2SpanInput::new(subscriber);
                        assert_eq!(
                            span.inputs()
                                .map(|message| message.message)
                                .collect::<Vec<_>>(),
                            vec![1, 2, 3]
                        );
                    });
            });
    }

    /// Optional input value selects newest while `clear` removes the oldest retained sample.
    #[test]
    fn iox2_optional_input_clear_keeps_newest_value() {
        let channel = "stage2_optional_clear";
        let built = TaskGraphBuilder::new()
            .add_pool(1, |pool| {
                pool.add_callback_builder(
                    test_callback(
                        "producer",
                        Box::new(CountingProducer {
                            publisher: Iox2Publisher::new(iox2_publisher_config(channel)),
                            next: 1,
                        }),
                    )
                    .with_publisher_channels(&[channel]),
                )
                .add_callback_builder(
                    test_callback(
                        "consumer",
                        Box::new(Iox2Consumer {
                            subscriber: Iox2Subscriber::new(iox2_subscriber_config(channel, 4)),
                        }),
                    )
                    .with_subscriber_channels(&[channel]),
                )
            })
            .build()
            .unwrap();
        let pools = &built.pools;
        let names = ChannelNameInterner::default();
        let callbacks = CallbackNameInterner::default();
        let ctx = Context::new(FrameworkTime::from_nanoseconds(77), &names, &callbacks);
        run_producer(pools, 3, &ctx);
        pools[0]
            .nodes
            .get(CallbackNodeId(1))
            .unwrap()
            .access(|node| {
                node.drain_subscribers();
                node.callback_mut()
                    .for_each_subscriber_mut(&mut |subscriber| {
                        let subscriber = subscriber
                            .as_any()
                            .downcast_mut::<Iox2Subscriber<u64>>()
                            .unwrap();
                        let mut input = Iox2OptionalInput::new(subscriber);
                        assert_eq!(input.value(), Some(&3));
                        input.clear();
                        assert_eq!(input.value(), Some(&3));
                    });
            });
    }

    /// A publish, flush, drain, and read cycle round-trips the payload and
    /// carries the flush-time timestamp in the wire header. The publisher's
    /// default `notify_on_send` also proves a pub/sub service and an event
    /// service coexist under one channel name.
    #[test]
    fn iox2_roundtrip_publishes_and_reads() {
        let built = build_producer_consumer_graph("stage2_roundtrip");
        let pools = &built.pools;
        let channel_interner = ChannelNameInterner::default();
        let callback_interner = CallbackNameInterner::default();
        let ctx = Context::new(
            crate::time::FrameworkTime::from_nanoseconds(77),
            &channel_interner,
            &callback_interner,
        );
        run_producer(pools, 1, &ctx);
        let (value, header) = drain_consumer(pools);
        assert_eq!(value, Some(1));
        assert_eq!(
            header,
            Some(MessageHeader::new(
                crate::time::FrameworkTime::from_nanoseconds(77)
            ))
        );
    }

    /// With a capacity-one consumer, samples published between drains evict
    /// down to the newest; a following publish succeeds, proving the evicted
    /// samples were returned to the sample pool.
    #[test]
    fn iox2_eviction_keeps_newest_and_recycles_samples() {
        let built = build_producer_consumer_graph("stage2_eviction");
        let pools = &built.pools;
        let channel_interner = ChannelNameInterner::default();
        let callback_interner = CallbackNameInterner::default();
        let ctx = Context::new(
            crate::time::FrameworkTime::from_nanoseconds(77),
            &channel_interner,
            &callback_interner,
        );
        run_producer(pools, 3, &ctx);
        let (value, _) = drain_consumer(pools);
        assert_eq!(value, Some(3), "newest sample should survive eviction");
        // Publish again: the loan succeeds only if evicted samples were
        // returned to the publisher's sample pool.
        run_producer(pools, 1, &ctx);
        let (value, _) = drain_consumer(pools);
        assert_eq!(value, Some(4));
    }

    /// Zero-capacity iox2 endpoints are rejected at construction with a
    /// message naming the channel.
    #[test]
    #[should_panic(expected = "iox2 subscriber capacity must be positive")]
    fn iox2_zero_capacity_subscriber_rejected() {
        let _ = Iox2Subscriber::<u64>::new(iox2_subscriber_config("stage2_zero", 0));
    }

    #[test]
    #[should_panic(expected = "iox2 publisher capacity must be positive")]
    fn iox2_zero_capacity_publisher_rejected() {
        let _ = Iox2Publisher::<u64>::new(crate::publisher::PublisherConfig {
            capacity: 0,
            channel_name: "stage2_zero".into(),
        });
    }

    /// Native and iox2 endpoints cannot share a channel, in either direction.
    #[test]
    fn iox2_mixed_transport_rejected_both_directions() {
        let channel = "stage2_mixed";
        let native_pub_graph = TaskGraphBuilder::new()
            .add_pool(1, |pool| {
                pool.add_callback_builder(test_callback(
                    "native_producer",
                    Box::new(NativeProducer {
                        publisher: NativePublisher::new(crate::publisher::PublisherConfig {
                            capacity: 1,
                            channel_name: channel.into(),
                        }),
                    }),
                ))
                .add_callback_builder(test_callback(
                    "iox2_consumer",
                    Box::new(Iox2Consumer {
                        subscriber: Iox2Subscriber::new(iox2_subscriber_config(channel, 1)),
                    }),
                ))
            })
            .build();
        assert!(matches!(
            native_pub_graph,
            Err(TaskGraphBuildError::MixedTransport { ref channel, .. }) if channel == channel
        ));

        let iox2_pub_graph = TaskGraphBuilder::new()
            .add_pool(1, |pool| {
                pool.add_callback_builder(test_callback(
                    "iox2_producer",
                    Box::new(CountingProducer {
                        publisher: Iox2Publisher::new(iox2_publisher_config(channel)),
                        next: 1,
                    }),
                ))
                .add_callback_builder(test_callback(
                    "native_consumer",
                    Box::new(NativeConsumer {
                        subscriber: NativeSubscriber::new(crate::subscriber::SubscriberConfig {
                            is_optional: true,
                            capacity: 1,
                            is_trigger: false,
                            keep_across_runs: true,
                            channel_name: channel.into(),
                        }),
                    }),
                ))
            })
            .build();
        assert!(matches!(
            iox2_pub_graph,
            Err(TaskGraphBuildError::MixedTransport { ref channel, .. }) if channel == channel
        ));
    }

    /// Data endpoints on one channel must agree on the payload type.
    #[test]
    fn iox2_mixed_payload_type_rejected() {
        struct U32Consumer {
            subscriber: Iox2Subscriber<u32>,
        }
        impl Callback for U32Consumer {
            fn run(&mut self, _ctx: &Context) {}
            fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
                f(PubOrSub::Subscriber(&self.subscriber));
            }
            fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
                f(PubOrSubMut::Subscriber(&mut self.subscriber));
            }
        }

        let channel = "stage2_mixed_payload";
        let graph = TaskGraphBuilder::new()
            .add_pool(1, |pool| {
                pool.add_callback_builder(test_callback(
                    "producer",
                    Box::new(CountingProducer {
                        publisher: Iox2Publisher::new(iox2_publisher_config(channel)),
                        next: 1,
                    }),
                ))
                .add_callback_builder(test_callback(
                    "consumer",
                    Box::new(U32Consumer {
                        subscriber: Iox2Subscriber::<u32>::new(iox2_subscriber_config(channel, 1)),
                    }),
                ))
            })
            .build();
        assert!(matches!(
            graph,
            Err(TaskGraphBuildError::MixedPayloadType { ref channel }) if channel == channel
        ));
    }

    /// The registered serializer drains each iox2 message exactly once via
    /// the transport-neutral hook (the logging build step's code path).
    #[cfg(feature = "serde")]
    #[test]
    fn iox2_serializer_drains_each_message_once() {
        #[repr(C)]
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, ZeroCopySend)]
        struct Sample {
            value: u64,
        }

        struct SampleProducer {
            publisher: Iox2Publisher<Sample>,
        }
        impl Callback for SampleProducer {
            fn run(&mut self, _ctx: &Context) {
                let mut output = Iox2Output::new_default(&mut self.publisher);
                *output = Sample { value: 1 };
                output.send();
            }
            fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
                f(PubOrSub::Publisher(&self.publisher));
            }
            fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
                f(PubOrSubMut::Publisher(&mut self.publisher));
            }
        }

        struct SampleConsumer {
            subscriber: Iox2Subscriber<Sample>,
        }
        impl Callback for SampleConsumer {
            fn run(&mut self, _ctx: &Context) {}
            fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
                f(PubOrSub::Subscriber(&self.subscriber));
            }
            fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
                f(PubOrSubMut::Subscriber(&mut self.subscriber));
            }
        }

        let channel = "stage2_serializer";
        let mut built = TaskGraphBuilder::new()
            .add_pool(1, |pool| {
                pool.add_callback_builder(test_callback(
                    "producer",
                    Box::new(SampleProducer {
                        publisher: Iox2Publisher::new(crate::publisher::PublisherConfig {
                            capacity: 1,
                            channel_name: channel.into(),
                        }),
                    }),
                ))
                .add_callback_builder(test_callback(
                    "consumer",
                    Box::new(SampleConsumer {
                        subscriber: Iox2Subscriber::new(iox2_subscriber_config(channel, 4)),
                    }),
                ))
            })
            .build()
            .expect("serializer graph should build");

        built
            .channel_registry
            .register_channel::<Sample>(channel.into());
        let serializer = built
            .channel_registry
            .serializer_for(std::any::TypeId::of::<Sample>())
            .expect("serializer registered");

        let channel_interner = ChannelNameInterner::default();
        let callback_interner = CallbackNameInterner::default();
        let ctx = Context::new(
            crate::time::FrameworkTime::from_nanoseconds(77),
            &channel_interner,
            &callback_interner,
        );
        let pools = &built.pools;
        let mut serialized = 0usize;
        for _ in 0..3 {
            pools[0]
                .nodes
                .get(CallbackNodeId(0))
                .expect("producer node")
                .access(|node| {
                    node.run(&ctx);
                    node.flush_publishers(ctx.now, &mut NoopReadyNodeSink);
                });
            pools[0]
                .nodes
                .get(CallbackNodeId(1))
                .expect("consumer node")
                .access(|node| {
                    node.drain_subscribers();
                    node.callback_mut().for_each_subscriber_mut(&mut |sub| {
                        let mut scratch = Vec::new();
                        serializer(sub, &mut scratch, &mut |_header, _bytes| {
                            serialized += 1;
                            Ok(())
                        })
                        .expect("serialization succeeds");
                    });
                });
        }
        assert_eq!(
            serialized, 3,
            "each published message serialized exactly once"
        );
    }

    /// Notifying with the event id equal to the service's configured maximum
    /// succeeds and is observed by a listener (the boundary is inclusive).
    #[test]
    fn iox2_event_boundary_id_is_inclusive() {
        let endpoints = [info(
            "stage2_event_boundary",
            EndpointKind::Iox2Notifier { event_id: 17 },
            None,
        )];
        let mut context = Iox2Context::from_endpoints(&Iox2GraphConfig::default(), &endpoints)
            .unwrap()
            .unwrap();
        let service = context
            .event_service("stage2_event_boundary")
            .expect("event service opens");
        let notifier = service
            .notifier_builder()
            .default_event_id(EventId::new(17))
            .create()
            .expect("notifier with the maximum event id is accepted");
        let listener = service
            .listener_builder()
            .create()
            .expect("listener created");
        notifier.notify().expect("boundary notify succeeds");
        let mut activations = 0usize;
        listener
            .try_wait(|_activation| activations += 1)
            .expect("try_wait succeeds");
        assert!(
            activations >= 1,
            "listener observed the boundary notification"
        );
    }
}
