//! Graph planning for owned payloads and storage-borrowing Forwarded source bundles.
//! Storage remains externally owned; only the construction metadata is erased.
use crate::{BuiltGraph, ScheduledCallback, bundle, capture, endpoints, replay, topology};
use std::{
    any::{Any, TypeId},
    cell::RefCell,
    collections::BTreeMap,
    ops::Range,
    sync::Arc,
};
use task::{Callback, ChannelBindings, ChannelPlan, ChannelStorage, SubscriberPolicy};
use topology::Topology;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "iceoryx2")]
    #[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
    fn native_and_ipc_channels_cannot_claim_the_same_name() {
        let mut plan = GraphPlan::default();
        plan.native::<u64>("shared").unwrap();
        assert!(plan.ipc::<u64>("shared").is_err());
    }

    #[test]
    fn all_channels_are_validated_before_any_arena_is_allocated() {
        let mut plan = GraphPlan::default();
        // This valid channel would allocate an enormous arena if validation were interleaved.
        plan.native::<u64>("a-large")
            .unwrap()
            .publisher(u32::MAX as usize);
        plan.native::<u64>("z-invalid").unwrap().subscriber(0);
        let error = plan.allocate().err().expect("invalid subscriber capacity");
        assert!(error.to_string().contains("ZeroSubscriberCapacity"));
        assert!(GraphPlan::default().allocate().unwrap().build().is_ok());
    }

    #[test]
    fn repeated_channel_names_share_declarations_and_reject_conflicting_types() {
        let mut plan = GraphPlan::default();
        let publisher = plan.native::<u64>("shared").unwrap().publisher(1);
        let subscriber = plan.native::<u64>("shared").unwrap().subscriber(1);
        assert!(plan.native::<String>("shared").is_err());
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let channel = bindings.native::<u64>("shared").unwrap();
        let mut publisher = channel.take_publisher(&publisher).unwrap();
        let subscriber = channel.take_subscriber(&subscriber).unwrap();
        publisher.publish(42).unwrap();
        publisher.flush(task::time::FrameworkTime::from_nanoseconds(1));
        subscriber.update();
        assert_eq!(subscriber.input_forwardable().pop().unwrap().message, 42);
    }
}

#[derive(Debug)]
pub struct BuildError(pub String);
impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for BuildError {}
impl From<task::EndpointError> for BuildError {
    fn from(e: task::EndpointError) -> Self {
        Self(e.to_string())
    }
}
impl From<task::StorageError> for BuildError {
    fn from(e: task::StorageError) -> Self {
        Self(format!("{e:?}"))
    }
}

pub trait Task {
    fn register(
        self: Box<Self>,
        channels: &mut GraphPlan,
        overrides: &ChannelOverrides,
    ) -> Result<TaskBinding, BuildError>;
}
/// A single-use binding operation valid for any borrow of the allocated storage.
pub type TaskBinding = Box<
    dyn for<'storage> FnOnce(
        &GraphBindings<'storage>,
    ) -> Result<Box<dyn Callback + 'storage>, BuildError>,
>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EndpointDirection {
    Input,
    Output,
}
impl EndpointDirection {
    fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

/// Overrides refer to callback argument names, not to default channel names or
/// channel endpoint indices. Resolution precedes all endpoint registration.
#[derive(Debug, Clone, Default)]
pub struct ChannelOverrides {
    channels: BTreeMap<(EndpointDirection, String), String>,
}
impl ChannelOverrides {
    pub fn input_channel(
        &mut self,
        endpoint: impl Into<String>,
        channel: impl Into<String>,
    ) -> &mut Self {
        self.channels
            .insert((EndpointDirection::Input, endpoint.into()), channel.into());
        self
    }
    pub fn output_channel(
        &mut self,
        endpoint: impl Into<String>,
        channel: impl Into<String>,
    ) -> &mut Self {
        self.channels
            .insert((EndpointDirection::Output, endpoint.into()), channel.into());
        self
    }
    pub fn validate(&self, endpoints: &[(&str, EndpointDirection)]) -> Result<(), BuildError> {
        for ((direction, name), channel) in &self.channels {
            let (_, actual) = endpoints
                .iter()
                .find(|(endpoint, _)| *endpoint == name)
                .ok_or_else(|| {
                    BuildError(format!("unknown {} endpoint '{name}'", direction.name()))
                })?;
            if actual != direction {
                return Err(BuildError(format!(
                    "endpoint '{name}' is an {}, not an {}",
                    actual.name(),
                    direction.name()
                )));
            }
            if channel.is_empty() {
                return Err(BuildError(format!(
                    "endpoint '{name}' has an empty channel override"
                )));
            }
        }
        Ok(())
    }
    pub fn resolve(
        &self,
        endpoint: &str,
        direction: EndpointDirection,
        default: impl FnOnce() -> String,
    ) -> Result<String, BuildError> {
        let name = self
            .channels
            .get(&(direction, endpoint.into()))
            .cloned()
            .unwrap_or_else(default);
        if name.is_empty() {
            return Err(BuildError(format!(
                "endpoint '{endpoint}' has an empty channel name"
            )));
        }
        Ok(name)
    }
}

/// An unbound task and its per-instance configuration, reusable by executor and
/// test graph construction. Channel wiring becomes fixed during registration.
pub struct TaskRegistration {
    name: String,
    schedule: crate::CallbackSchedule,
    task: Box<dyn Task>,
    channels: ChannelOverrides,
}
impl TaskRegistration {
    pub fn new(
        name: impl Into<String>,
        task: impl Task + 'static,
        schedule: crate::CallbackSchedule,
    ) -> Self {
        Self {
            name: name.into(),
            task: Box::new(task),
            schedule,
            channels: ChannelOverrides::default(),
        }
    }
    pub fn input_channel(
        &mut self,
        endpoint: impl Into<String>,
        channel: impl Into<String>,
    ) -> &mut Self {
        self.channels.input_channel(endpoint, channel);
        self
    }
    pub fn output_channel(
        &mut self,
        endpoint: impl Into<String>,
        channel: impl Into<String>,
    ) -> &mut Self {
        self.channels.output_channel(endpoint, channel);
        self
    }
    pub fn register(self, plan: &mut GraphPlan) -> Result<(), BuildError> {
        if plan.tasks.contains_key(&self.name) {
            return Err(BuildError(format!("duplicate task '{}'", self.name)));
        }
        let start = plan.endpoints.len();
        plan.active_task = Some(self.name.clone());
        let binding = self
            .task
            .register(plan, &self.channels)
            .map_err(|e| BuildError(format!("task '{}': {e}", self.name)));
        plan.active_task = None;
        let binding = binding?;
        plan.tasks
            .insert(self.name.clone(), start..plan.endpoints.len());
        plan.callbacks.push((self.name, self.schedule, binding));
        Ok(())
    }
}

type TaskDefinition = (String, crate::CallbackSchedule, TaskBinding);

pub(crate) trait Plan {
    fn forwarding_slots(&self) -> Result<usize, BuildError> {
        Err(BuildError("forwarding requires native storage".into()))
    }
    fn reserve_forwarding(&mut self, _: usize) -> Result<(), BuildError> {
        Err(BuildError("forwarding requires native storage".into()))
    }
    fn topology(&self) -> topology::ChannelTopology;
    fn validate(&self) -> Result<(), BuildError>;
    fn payload_type(&self) -> &'static str;
    fn any_mut(&mut self) -> &mut dyn Any;
    fn allocate(self: Box<Self>) -> Result<Box<dyn Stored>, BuildError>;
    #[cfg(feature = "iceoryx2")]
    fn is_ipc(&self) -> bool {
        false
    }
    #[cfg(feature = "iceoryx2")]
    fn ipc_requirements(&self) -> Result<Option<task::iox2::Iox2ChannelConfig>, BuildError> {
        Ok(None)
    }
    #[cfg(feature = "iceoryx2")]
    fn set_ipc_config(&mut self, _: task::iox2::Iox2ChannelConfig) {}
}
pub(crate) trait Stored {
    fn bind(&self) -> Result<Box<dyn Binding<'_> + '_>, BuildError>;
}
// This private, closed family provides lifetime-preserving type erasure. Any is
// used for payload identity, never for borrowed endpoints or lifetime extension.
pub(crate) trait Binding<'a> {
    fn payload(&self) -> TypeId;
    fn pointer(&self) -> Option<*const ()> {
        None
    }
    #[cfg(feature = "iceoryx2")]
    fn ipc_any(&self) -> Option<&dyn Any> {
        None
    }
    #[cfg(feature = "iceoryx2")]
    fn set_output_delivery(
        &self,
        _index: usize,
        _notifier: bool,
        _delivery: task::iox2::OutputDelivery,
    ) -> Result<(), BuildError> {
        Err(BuildError("expected IPC output binding".into()))
    }
}
impl<T: Send + Sync + 'static> Plan for ChannelPlan<T> {
    fn forwarding_slots(&self) -> Result<usize, BuildError> {
        self.total_publisher_capacity().map_err(Into::into)
    }
    fn reserve_forwarding(&mut self, capacity: usize) -> Result<(), BuildError> {
        self.set_forwarding_retention(capacity);
        Ok(())
    }
    fn topology(&self) -> topology::ChannelTopology {
        self.topology()
    }
    fn validate(&self) -> Result<(), BuildError> {
        ChannelPlan::validate(self).map_err(Into::into)
    }
    fn payload_type(&self) -> &'static str {
        std::any::type_name::<T>()
    }
    fn any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn allocate(self: Box<Self>) -> Result<Box<dyn Stored>, BuildError> {
        Ok(Box::new(ChannelPlan::allocate(*self)?))
    }
}
impl<T: Send + Sync + 'static> Stored for ChannelStorage<T> {
    fn bind(&self) -> Result<Box<dyn Binding<'_> + '_>, BuildError> {
        Ok(Box::new(self.bind()))
    }
}
impl<'a, T: 'static> Binding<'a> for ChannelBindings<'a, T> {
    fn payload(&self) -> TypeId {
        TypeId::of::<T>()
    }
    fn pointer(&self) -> Option<*const ()> {
        Some((self as *const Self).cast())
    }
}

pub(crate) struct Channel {
    pub(crate) capture: Option<capture::CaptureDeclaration>,
    pub(crate) replay: Option<replay::SourceDeclaration>,
    pub(crate) origin: Option<String>,
    pub(crate) plan: Box<dyn Plan>,
    pub(crate) publishers: usize,
    pub(crate) subscribers: usize,
}
#[derive(Default)]
pub struct GraphPlan {
    pub(crate) forwarding_edges: Vec<bundle::ForwardingEdge>,
    pub(crate) active_task: Option<String>,
    pub(crate) channels: BTreeMap<String, Channel>,
    pub(crate) endpoints: Vec<endpoints::PlannedEndpoint>,
    pub(crate) tasks: BTreeMap<String, Range<usize>>,
    callbacks: Vec<TaskDefinition>,
    #[cfg(feature = "iceoryx2")]
    pub(crate) events: BTreeMap<String, (task::iox2::Iox2ChannelPlan<()>, usize)>,
    #[cfg(feature = "iceoryx2")]
    runtime: Option<Arc<task::iox2::Iox2Runtime>>,
    #[cfg(feature = "iceoryx2")]
    ipc_limits: BTreeMap<String, task::iox2::Iox2ChannelConfig>,
}
impl GraphPlan {
    /// Generated registration annotates each endpoint after its checked declaration.
    #[doc(hidden)]
    pub fn annotate_last_endpoint(&mut self, name: &str, forwardable: bool) {
        let endpoint = self
            .endpoints
            .last_mut()
            .expect("endpoint must be declared before annotation");
        endpoint.name = Some(name.into());
        endpoint.forwardable = forwardable;
    }
    pub fn topology(&self) -> Topology {
        let mut snapshot = Topology {
            forwarding: self.forwarding_edges.clone(),
            callbacks: self.tasks.keys().cloned().collect(),
            channels: self
                .channels
                .values()
                .map(|channel| channel.plan.topology())
                .collect(),
            endpoints: vec![],
        };
        #[cfg(feature = "iceoryx2")]
        for (name, (plan, _)) in &self.events {
            if !self.channels.contains_key(name) {
                let mut channel = plan.topology();
                channel.transport = task::recording::Transport::Event;
                snapshot.channels.push(channel);
            }
        }
        snapshot.channels.sort_by(|a, b| a.name.cmp(&b.name));
        for (callback, endpoints) in &self.tasks {
            let mut ordinals = [0, 0];
            for endpoint in &self.endpoints[endpoints.clone()] {
                let ordinal = ordinals[usize::from(endpoint.output)];
                ordinals[usize::from(endpoint.output)] += 1;
                snapshot.endpoints.push(topology::EndpointTopology {
                    forwardable: endpoint.forwardable,
                    callback: callback.clone(),
                    name: endpoint.name.clone().unwrap_or_else(|| ordinal.to_string()),
                    ordinal,
                    output: endpoint.output,
                    channel: endpoint.channel.clone(),
                    transport: endpoint.transport,
                    payload_type: endpoint.payload_type,
                    capacity: endpoint.capacity,
                    endpoint_index: endpoint.index,
                });
            }
        }
        snapshot
    }
    pub fn native_workload_publishers<T: Send + Sync + 'static>(
        &mut self,
        name: &str,
    ) -> Result<Vec<task::PublisherKey<T>>, BuildError> {
        let indices: Vec<_> = self
            .endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.channel == name
                    && endpoint.output
                    && endpoint.transport == task::recording::Transport::Native
            })
            .map(|endpoint| endpoint.index)
            .collect();
        let plan = self.native::<T>(name)?;
        Ok(indices
            .into_iter()
            .map(|index| plan.publisher_key(index))
            .collect())
    }
    pub fn native<T: Send + Sync + 'static>(
        &mut self,
        name: &str,
    ) -> Result<&mut ChannelPlan<T>, BuildError> {
        #[cfg(feature = "iceoryx2")]
        if self.events.contains_key(name) {
            return Err(BuildError(format!(
                "channel '{name}': mixes native and IPC transports"
            )));
        }
        let entry = self.channels.entry(name.into()).or_insert_with(|| Channel {
            capture: None,
            replay: None,
            origin: self.active_task.clone(),
            plan: Box::new(ChannelPlan::<T>::new(name)),
            publishers: 0,
            subscribers: 0,
        });
        let existing = entry.plan.payload_type();
        let origin = entry.origin.as_deref().unwrap_or("direct declaration");
        entry.plan.any_mut().downcast_mut().ok_or_else(|| {
            BuildError(format!(
                "channel '{name}': incompatible payload type or transport (first declared by '{origin}' as {existing}, requested native {})",
                std::any::type_name::<T>()
            ))
        })
    }
    pub fn publisher<T: Send + Sync + 'static>(
        &mut self,
        name: &str,
        capacity: usize,
    ) -> Result<task::PublisherKey<T>, BuildError> {
        let key = self.native::<T>(name)?.publisher(capacity);
        self.endpoints.push(endpoints::PlannedEndpoint::new::<T>(
            name,
            true,
            false,
            false,
            key.index(),
            capacity,
        ));
        self.channels.get_mut(name).unwrap().publishers += 1;
        Ok(key)
    }
    pub fn subscriber<T: Send + Sync + 'static>(
        &mut self,
        name: &str,
        capacity: usize,
        policy: SubscriberPolicy,
    ) -> Result<task::SubscriberKey<T>, BuildError> {
        let key = self
            .native::<T>(name)?
            .subscriber_with_policy(capacity, policy);
        self.channels.get_mut(name).unwrap().subscribers += 1;
        self.endpoints.push(endpoints::PlannedEndpoint::new::<T>(
            name,
            false,
            false,
            false,
            key.index(),
            capacity,
        ));
        Ok(key)
    }
    pub fn require(&self, name: &str, publisher: bool) -> Result<(), BuildError> {
        let found = self.channels.get(name).is_some_and(|c| {
            if publisher {
                c.publishers > 0
            } else {
                c.subscribers > 0
            }
        });
        if found {
            Ok(())
        } else {
            Err(BuildError(format!(
                "channel '{name}': no task {}",
                if publisher { "publisher" } else { "subscriber" }
            )))
        }
    }
    pub fn allocate(mut self) -> Result<GraphStorage, BuildError> {
        self.prepare()?;
        let topology = self.topology();
        let mut names = task::string_interner::ChannelNameInterner::new();
        let mut channels = BTreeMap::new();
        for (name, channel) in self.channels {
            names.intern(&name);
            channels.insert(name, channel.plan.allocate()?);
        }
        #[cfg(feature = "iceoryx2")]
        let events = self
            .events
            .into_iter()
            .map(|(name, (plan, _))| {
                names.intern(&name);
                Ok((name, Box::new(plan).allocate()?))
            })
            .collect::<Result<_, BuildError>>()?;
        Ok(GraphStorage {
            callbacks: RefCell::new(Some(self.callbacks)),
            topology,
            channels,
            names: Arc::new(names),
            #[cfg(feature = "iceoryx2")]
            events,
        })
    }
    /// Validate and finalize service budgets before any channel allocation.
    pub fn prepare(&mut self) -> Result<(), BuildError> {
        crate::graph::validate_callbacks(
            self.callbacks
                .iter()
                .map(|(name, schedule, _)| (name.as_str(), schedule)),
        )
        .map_err(|error| BuildError(format!("{error:?}")))?;
        self.prepare_forwarding()?;
        #[cfg(feature = "iceoryx2")]
        for (name, config) in self.ipc_service_limits()? {
            if let Some(channel) = self.channels.get_mut(&name) {
                channel.plan.set_ipc_config(config.clone());
            }
            if let Some((plan, _)) = self.events.get_mut(&name) {
                plan.set_config(config);
                plan.validate()?;
            }
        }
        for (name, channel) in &self.channels {
            channel
                .plan
                .validate()
                .map_err(|e| BuildError(format!("channel '{name}': {e}")))?;
        }
        Ok(())
    }
}
/// Owns channel storage and the single-use task definitions. Graphs, endpoints,
/// and retained messages borrow this owner; endpoint bindings can be rebuilt.
///
/// ```compile_fail,E0597
/// use task_graph::ApplicationBuilder;
/// let graph = {
///     let storage = ApplicationBuilder::new().allocate().unwrap();
///     storage.build().unwrap()
/// };
/// drop(graph);
/// ```
pub struct GraphStorage {
    callbacks: RefCell<Option<Vec<TaskDefinition>>>,
    topology: Topology,
    channels: BTreeMap<String, Box<dyn Stored>>,
    names: Arc<task::string_interner::ChannelNameInterner>,
    #[cfg(feature = "iceoryx2")]
    events: BTreeMap<String, Box<dyn Stored>>,
}
impl GraphStorage {
    pub fn topology(&self) -> &Topology {
        &self.topology
    }
    pub fn bind(&self) -> Result<GraphBindings<'_>, BuildError> {
        Ok(GraphBindings {
            channels: self
                .channels
                .iter()
                .map(|(name, storage)| Ok((name.clone(), storage.bind()?)))
                .collect::<Result<_, BuildError>>()?,
            #[cfg(feature = "iceoryx2")]
            events: self
                .events
                .iter()
                .map(|(name, storage)| Ok((name.clone(), storage.bind()?)))
                .collect::<Result<_, BuildError>>()?,
        })
    }
    pub fn build(&self) -> Result<BuiltGraph<'_>, BuildError> {
        Ok(self.bind_and_build()?.0)
    }
    pub fn bind_and_build(&self) -> Result<(BuiltGraph<'_>, GraphBindings<'_>), BuildError> {
        let bindings = self.bind()?;
        let graph = self.build_with(&bindings)?;
        Ok((graph, bindings))
    }
    /// Construct tasks in registration order, after configuring any replay hooks.
    pub fn build_with<'a>(
        &'a self,
        bindings: &GraphBindings<'a>,
    ) -> Result<BuiltGraph<'a>, BuildError> {
        let tasks = self
            .callbacks
            .borrow_mut()
            .take()
            .ok_or_else(|| BuildError("graph has already been built".into()))?;
        let callbacks = tasks
            .into_iter()
            .map(|(name, schedule, bind)| {
                let callback =
                    bind(bindings).map_err(|e| BuildError(format!("task '{name}': {e}")))?;
                Ok(ScheduledCallback {
                    name,
                    schedule,
                    callback,
                })
            })
            .collect::<Result<_, BuildError>>()?;
        Ok(BuiltGraph::from_callbacks(callbacks, self.names.clone()))
    }
}
pub struct GraphBindings<'a> {
    pub(crate) channels: BTreeMap<String, Box<dyn Binding<'a> + 'a>>,
    #[cfg(feature = "iceoryx2")]
    events: BTreeMap<String, Box<dyn Binding<'a> + 'a>>,
}
impl<'a> GraphBindings<'a> {
    /// Select delivery for one IPC output before its endpoint is taken by a task.
    #[cfg(feature = "iceoryx2")]
    pub fn set_ipc_output_delivery(
        &self,
        channel: &str,
        index: usize,
        notifier: bool,
        delivery: task::iox2::OutputDelivery,
    ) -> Result<(), BuildError> {
        let binding = if notifier {
            self.events
                .get(channel)
                .or_else(|| self.channels.get(channel))
        } else {
            self.channels.get(channel)
        }
        .ok_or_else(|| BuildError(format!("unknown channel '{channel}'")))?;
        binding.set_output_delivery(index, notifier, delivery)
    }
    pub fn native<T: 'static>(&self, name: &str) -> Result<&ChannelBindings<'a, T>, BuildError> {
        let binding = self
            .channels
            .get(name)
            .ok_or_else(|| BuildError(format!("unknown channel '{name}'")))?;
        if binding.payload() != TypeId::of::<T>() {
            return Err(BuildError(format!("channel '{name}': payload mismatch")));
        }
        let pointer = binding
            .pointer()
            .ok_or_else(|| BuildError(format!("channel '{name}': expected native transport")))?;
        // SAFETY: Binding is private and native entries are exclusively constructed
        // from ChannelBindings<'a, T>. The checked TypeId establishes T. The map
        // owns the boxed value, and this reference borrows the map. 'a is preserved
        // from Stored::bind through the map; no reference is extended to 'static.
        Ok(unsafe { &*pointer.cast::<ChannelBindings<'a, T>>() })
    }
}

#[cfg(feature = "iceoryx2")]
mod ipc {
    use super::*;
    use iceoryx2::prelude::ZeroCopySend;
    use std::fmt::Debug;
    use task::iox2::*;
    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Plan for Iox2ChannelPlan<T> {
        fn topology(&self) -> topology::ChannelTopology {
            self.topology()
        }
        fn validate(&self) -> Result<(), BuildError> {
            Iox2ChannelPlan::validate(self).map_err(Into::into)
        }
        fn payload_type(&self) -> &'static str {
            std::any::type_name::<T>()
        }
        fn ipc_requirements(&self) -> Result<Option<Iox2ChannelConfig>, BuildError> {
            Ok(Some(self.required_config()?))
        }
        fn set_ipc_config(&mut self, config: Iox2ChannelConfig) {
            self.set_config(config);
        }
        fn any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn is_ipc(&self) -> bool {
            true
        }
        fn allocate(self: Box<Self>) -> Result<Box<dyn Stored>, BuildError> {
            Ok(Box::new(Iox2ChannelPlan::allocate(*self)?))
        }
    }
    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Stored for Iox2ChannelStorage<T> {
        fn bind(&self) -> Result<Box<dyn Binding<'_> + '_>, BuildError> {
            Ok(Box::new(self.bind()?))
        }
    }
    impl<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> Binding<'a> for Iox2Bindings<T> {
        fn set_output_delivery(
            &self,
            index: usize,
            notifier: bool,
            delivery: OutputDelivery,
        ) -> Result<(), BuildError> {
            Iox2Bindings::set_output_delivery(self, index, notifier, delivery).map_err(Into::into)
        }
        fn payload(&self) -> TypeId {
            TypeId::of::<T>()
        }
        fn ipc_any(&self) -> Option<&dyn Any> {
            Some(self)
        }
    }
    impl GraphPlan {
        pub fn ipc_workload_publishers<T: Debug + ZeroCopySend + Send + Sync + 'static>(
            &mut self,
            name: &str,
        ) -> Result<Vec<Iox2PublisherKey<T>>, BuildError> {
            let indices: Vec<_> = self
                .endpoints
                .iter()
                .filter(|endpoint| {
                    endpoint.channel == name
                        && endpoint.output
                        && endpoint.transport == task::recording::Transport::Ipc
                })
                .map(|endpoint| endpoint.index)
                .collect();
            let plan = self.ipc::<T>(name)?;
            Ok(indices
                .into_iter()
                .map(|index| plan.publisher_key(index))
                .collect())
        }
        /// Explicit limits are hard bounds; automatic limits otherwise grow from
        /// the defaults to cover every task, capture and fixture endpoint.
        pub fn set_ipc_service_limits(
            &mut self,
            channel: impl Into<String>,
            limits: Iox2ChannelConfig,
        ) {
            self.ipc_limits.insert(channel.into(), limits);
        }
        pub fn ipc_service_limits(
            &self,
        ) -> Result<BTreeMap<String, Iox2ChannelConfig>, BuildError> {
            let mut required = BTreeMap::new();
            for (name, channel) in &self.channels {
                if let Some(config) = channel.plan.ipc_requirements()? {
                    required.insert(name.clone(), config);
                }
            }
            for (name, (plan, _)) in &self.events {
                let events = plan.required_config()?;
                let config = required.entry(name.clone()).or_insert(Iox2ChannelConfig {
                    max_listeners: 0,
                    max_notifiers: 0,
                    ..events.clone()
                });
                config.max_listeners = config
                    .max_listeners
                    .checked_add(events.max_listeners)
                    .ok_or(task::StorageError::CapacityOverflow)?;
                config.max_notifiers = config
                    .max_notifiers
                    .checked_add(events.max_notifiers)
                    .ok_or(task::StorageError::CapacityOverflow)?;
                config.event_id_max_value =
                    config.event_id_max_value.max(events.event_id_max_value);
            }
            for name in self.ipc_limits.keys() {
                if !required.contains_key(name) {
                    return Err(BuildError(format!(
                        "IPC limits configured for unknown/non-IPC channel '{name}'"
                    )));
                }
            }
            required.into_iter().map(|(name, needed)| {
                let mut limits = Iox2ChannelConfig::default();
                macro_rules! grow { ($($field:ident),*) => { $(limits.$field = limits.$field.max(needed.$field);)* }; }
                grow!(buffer_capacity, max_borrowed_samples, max_publishers, max_subscribers, max_nodes, max_listeners, max_notifiers, event_id_max_value);
                if let Some(explicit) = self.ipc_limits.get(&name) { limits = explicit.clone(); }
                let mut insufficient = Vec::new();
                if limits.max_listeners == 0 || limits.max_notifiers == 0 { insufficient.push("max_listeners and max_notifiers must be positive service limits".into()); }
                macro_rules! check { ($($field:ident),*) => { $(if limits.$field < needed.$field { insufficient.push(format!("{} requires {}, configured {}", stringify!($field), needed.$field, limits.$field)); })* }; }
                check!(buffer_capacity, max_borrowed_samples, max_publishers, max_subscribers, max_nodes, max_listeners, max_notifiers, event_id_max_value);
                if limits.max_borrowed_samples < limits.buffer_capacity.checked_mul(2).ok_or(task::StorageError::CapacityOverflow)? { insufficient.push("max_borrowed_samples must cover twice buffer_capacity".into()); }
                if !insufficient.is_empty() { return Err(BuildError(format!("IPC service '{name}': {}", insufficient.join(", ")))); }
                Ok((name, limits))
            }).collect()
        }
        pub fn with_iox2_runtime(runtime: Arc<Iox2Runtime>) -> Self {
            Self {
                runtime: Some(runtime),
                ..Self::default()
            }
        }
        fn runtime(&mut self) -> Result<Arc<Iox2Runtime>, BuildError> {
            if self.runtime.is_none() {
                self.runtime = Some(Iox2Runtime::new()?);
            }
            Ok(self.runtime.as_ref().unwrap().clone())
        }
        pub fn ipc<T: Debug + ZeroCopySend + Send + Sync + 'static>(
            &mut self,
            name: &str,
        ) -> Result<&mut Iox2ChannelPlan<T>, BuildError> {
            let runtime = self.runtime()?;
            let channel = self.channels.entry(name.into()).or_insert_with(|| Channel {
                capture: None,
                replay: None,
                origin: self.active_task.clone(),
                plan: Box::new(Iox2ChannelPlan::<T>::new(name, &runtime)),
                publishers: 0,
                subscribers: 0,
            });
            let existing = channel.plan.payload_type();
            let origin = channel.origin.as_deref().unwrap_or("direct declaration");
            channel.plan.any_mut().downcast_mut().ok_or_else(|| {
                BuildError(format!(
                    "channel '{name}': incompatible payload type or transport (first declared by '{origin}' as {existing}, requested IPC {})", std::any::type_name::<T>()
                ))
            })
        }
        pub fn ipc_publisher<T: Debug + ZeroCopySend + Send + Sync + 'static>(
            &mut self,
            name: &str,
            capacity: usize,
        ) -> Result<Iox2PublisherKey<T>, BuildError> {
            let key = self.ipc::<T>(name)?.publisher(capacity);
            self.endpoints.push(endpoints::PlannedEndpoint::new::<T>(
                name,
                true,
                true,
                false,
                key.index(),
                capacity,
            ));
            self.channels.get_mut(name).unwrap().publishers += 1;
            Ok(key)
        }
        pub fn ipc_subscriber<T: Debug + ZeroCopySend + Send + Sync + 'static>(
            &mut self,
            name: &str,
            capacity: usize,
        ) -> Result<Iox2SubscriberKey<T>, BuildError> {
            self.ipc_subscriber_with_keep(name, capacity, true)
        }
        pub fn ipc_subscriber_with_keep<T: Debug + ZeroCopySend + Send + Sync + 'static>(
            &mut self,
            name: &str,
            capacity: usize,
            keep_across_runs: bool,
        ) -> Result<Iox2SubscriberKey<T>, BuildError> {
            let key = self
                .ipc::<T>(name)?
                .subscriber_with_keep(capacity, keep_across_runs);
            self.endpoints.push(endpoints::PlannedEndpoint::new::<T>(
                name,
                false,
                true,
                false,
                key.index(),
                capacity,
            ));
            self.channels.get_mut(name).unwrap().subscribers += 1;
            Ok(key)
        }
        fn event_plan(
            &mut self,
            name: &str,
        ) -> Result<&mut (Iox2ChannelPlan<()>, usize), BuildError> {
            if self.channels.get(name).is_some_and(|c| !c.plan.is_ipc()) {
                return Err(BuildError(format!(
                    "channel '{name}': mixes native and IPC transports"
                )));
            }
            let runtime = self.runtime()?;
            Ok(self
                .events
                .entry(name.into())
                .or_insert_with(|| (Iox2ChannelPlan::new(name, &runtime), 0)))
        }
        pub fn event(&mut self, name: &str, capacity: usize) -> Result<Iox2EventKey, BuildError> {
            let entry = self.event_plan(name)?;
            entry.1 += 1;
            let key = entry.0.events(capacity);
            self.endpoints.push(endpoints::PlannedEndpoint::new::<()>(
                name,
                false,
                true,
                true,
                key.index(),
                capacity,
            ));
            Ok(key)
        }
        pub fn notifier(&mut self, name: &str) -> Result<Iox2NotifierKey, BuildError> {
            let key = self.event_plan(name)?.0.notifier();
            self.endpoints.push(endpoints::PlannedEndpoint::new::<()>(
                name,
                true,
                true,
                true,
                key.index(),
                1,
            ));
            Ok(key)
        }
        pub fn require_event(&self, name: &str) -> Result<(), BuildError> {
            if self.events.get(name).is_some_and(|(_, count)| *count > 0) {
                Ok(())
            } else {
                Err(BuildError(format!(
                    "channel '{name}': no task event subscriber"
                )))
            }
        }
    }
    impl GraphBindings<'_> {
        pub fn ipc<T: Debug + ZeroCopySend + Send + Sync + 'static>(
            &self,
            name: &str,
        ) -> Result<&Iox2Bindings<T>, BuildError> {
            self.channels
                .get(name)
                .and_then(|b| b.ipc_any())
                .and_then(|a| a.downcast_ref())
                .ok_or_else(|| BuildError(format!("channel '{name}': IPC type/transport mismatch")))
        }
        pub fn events(&self, name: &str) -> Result<&Iox2Bindings<()>, BuildError> {
            self.events
                .get(name)
                .and_then(|b| b.ipc_any())
                .and_then(|a| a.downcast_ref())
                .ok_or_else(|| BuildError(format!("channel '{name}': no event bindings")))
        }
    }
}
