//! Named construction for owned payloads and the storage-borrowing ForwardedMessage family.
//! Storage remains externally owned; only the construction metadata is erased.
use crate::{
    Callback, ChannelPlan, ChannelStorage, EndpointBindings, GraphBuilder, StorageLayout,
    SubscriberPolicy,
};
use std::{
    any::{Any, TypeId},
    collections::BTreeMap,
    sync::Arc,
};
pub mod capture;
pub mod exact;
pub mod forwarding;
pub mod port_capture;
pub mod replay;
pub mod topology;
pub use topology::Topology;

#[derive(Debug)]
pub struct BuildError(pub String);
impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for BuildError {}
impl From<crate::EndpointError> for BuildError {
    fn from(e: crate::EndpointError) -> Self {
        Self(e.to_string())
    }
}
impl From<crate::StorageError> for BuildError {
    fn from(e: crate::StorageError) -> Self {
        Self(format!("{e:?}"))
    }
}

pub trait Task {
    fn register(
        self: Box<Self>,
        channels: &mut NamedPlan,
    ) -> Result<Box<dyn TaskFactory>, BuildError>;
    fn register_with(
        self: Box<Self>,
        channels: &mut NamedPlan,
        overrides: &ChannelOverrides,
    ) -> Result<Box<dyn TaskFactory>, BuildError> {
        if !overrides.is_empty() {
            return Err(BuildError("task does not support channel overrides".into()));
        }
        self.register(channels)
    }
}
pub trait TaskFactory {
    fn build<'a>(
        self: Box<Self>,
        bindings: &NamedBindings<'a>,
    ) -> Result<Box<dyn Callback + 'a>, BuildError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PortDirection {
    Input,
    Output,
}
impl PortDirection {
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
    channels: BTreeMap<(PortDirection, String), String>,
}
impl ChannelOverrides {
    pub fn input_channel(
        &mut self,
        port: impl Into<String>,
        channel: impl Into<String>,
    ) -> &mut Self {
        self.channels
            .insert((PortDirection::Input, port.into()), channel.into());
        self
    }
    pub fn output_channel(
        &mut self,
        port: impl Into<String>,
        channel: impl Into<String>,
    ) -> &mut Self {
        self.channels
            .insert((PortDirection::Output, port.into()), channel.into());
        self
    }
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
    pub fn validate(&self, ports: &[(&str, PortDirection)]) -> Result<(), BuildError> {
        for ((direction, name), channel) in &self.channels {
            let (_, actual) = ports
                .iter()
                .find(|(port, _)| *port == name)
                .ok_or_else(|| BuildError(format!("unknown {} port '{name}'", direction.name())))?;
            if actual != direction {
                return Err(BuildError(format!(
                    "port '{name}' is an {}, not an {}",
                    actual.name(),
                    direction.name()
                )));
            }
            if channel.is_empty() {
                return Err(BuildError(format!(
                    "port '{name}' has an empty channel override"
                )));
            }
        }
        Ok(())
    }
    pub fn resolve(
        &self,
        port: &str,
        direction: PortDirection,
        default: impl FnOnce() -> String,
    ) -> Result<String, BuildError> {
        let name = self
            .channels
            .get(&(direction, port.into()))
            .cloned()
            .unwrap_or_else(default);
        if name.is_empty() {
            return Err(BuildError(format!(
                "port '{port}' has an empty channel name"
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
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn input_channel(
        &mut self,
        port: impl Into<String>,
        channel: impl Into<String>,
    ) -> &mut Self {
        self.channels.input_channel(port, channel);
        self
    }
    pub fn output_channel(
        &mut self,
        port: impl Into<String>,
        channel: impl Into<String>,
    ) -> &mut Self {
        self.channels.output_channel(port, channel);
        self
    }
    pub fn register(self, plan: &mut NamedPlan) -> Result<RegisteredTask, BuildError> {
        if plan.tasks.contains_key(&self.name) {
            return Err(BuildError(format!("duplicate task '{}'", self.name)));
        }
        let start = plan.ports.len();
        plan.active_task = Some(self.name.clone());
        let factory = self
            .task
            .register_with(plan, &self.channels)
            .map_err(|e| BuildError(format!("task '{}': {e}", self.name)));
        plan.active_task = None;
        let factory = factory?;
        plan.tasks
            .insert(self.name.clone(), plan.ports[start..].to_vec());
        Ok(RegisteredTask {
            name: self.name,
            schedule: self.schedule,
            factory,
        })
    }
}
pub struct RegisteredTask {
    pub name: String,
    pub schedule: crate::CallbackSchedule,
    pub factory: Box<dyn TaskFactory>,
}
impl RegisteredTask {
    pub fn add_to_graph<'build, 'storage>(
        self,
        graph: &mut GraphBuilder<'build, 'storage>,
        bindings: &'build NamedBindings<'storage>,
    ) {
        graph.add_boxed_callback(self.name, self.schedule, move || {
            self.factory
                .build(bindings)
                .map_err(|e| Box::new(e) as crate::FactoryError)
        });
    }
}

trait Plan {
    fn source_budget(&self) -> Result<Option<(TypeId, usize)>, BuildError> {
        Ok(None)
    }
    fn set_source_budget(&mut self, _: &BTreeMap<TypeId, usize>) {}
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
    fn ipc_requirements(&self) -> Result<Option<crate::iox2::Iox2ChannelConfig>, BuildError> {
        Ok(None)
    }
    #[cfg(feature = "iceoryx2")]
    fn set_ipc_config(&mut self, _: crate::iox2::Iox2ChannelConfig) {}
}
trait Stored {
    fn bind(&self) -> Result<Box<dyn Binding<'_> + '_>, BuildError>;
}
// This private, closed family provides lifetime-preserving type erasure. Any is
// used for payload identity, never for borrowed endpoints or lifetime extension.
trait Binding<'a> {
    fn payload(&self) -> TypeId;
    fn pointer(&self) -> Option<*const ()> {
        None
    }
    #[cfg(feature = "iceoryx2")]
    fn ipc_any(&self) -> Option<&dyn Any> {
        None
    }
}
impl<T: Send + Sync + 'static> Plan for ChannelPlan<T> {
    fn set_source_budget(&mut self, budgets: &BTreeMap<TypeId, usize>) {
        self.set_forwarding_retention(budgets.get(&TypeId::of::<T>()).copied().unwrap_or(0));
    }
    fn topology(&self) -> topology::ChannelTopology {
        self.topology()
    }
    fn validate(&self) -> Result<(), BuildError> {
        StorageLayout::validate(self).map_err(Into::into)
    }
    fn payload_type(&self) -> &'static str {
        std::any::type_name::<T>()
    }
    fn any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn allocate(self: Box<Self>) -> Result<Box<dyn Stored>, BuildError> {
        StorageLayout::validate(self.as_ref())?;
        Ok(Box::new(StorageLayout::allocate(*self)?))
    }
}
impl<T: Send + Sync + 'static> Stored for ChannelStorage<T> {
    fn bind(&self) -> Result<Box<dyn Binding<'_> + '_>, BuildError> {
        Ok(Box::new(self.build()))
    }
}
impl<'a, T: 'static> Binding<'a> for EndpointBindings<'a, T> {
    fn payload(&self) -> TypeId {
        TypeId::of::<T>()
    }
    fn pointer(&self) -> Option<*const ()> {
        Some((self as *const Self).cast())
    }
}

struct Channel {
    origin: Option<String>,
    plan: Box<dyn Plan>,
    publishers: usize,
    subscribers: usize,
}
#[derive(Default)]
pub struct NamedPlan {
    active_task: Option<String>,
    channels: BTreeMap<String, Channel>,
    captures: BTreeMap<String, Box<dyn capture::CaptureDeclaration>>,
    replay_sources: BTreeMap<String, Box<dyn replay::SourceDeclaration>>,
    ports: Vec<exact::PlannedPort>,
    tasks: BTreeMap<String, Vec<exact::PlannedPort>>,
    exact_planned: bool,
    #[cfg(feature = "iceoryx2")]
    events: BTreeMap<String, (crate::iox2::Iox2ChannelPlan<()>, usize)>,
    #[cfg(feature = "iceoryx2")]
    runtime: Option<Arc<crate::iox2::Iox2Runtime>>,
    #[cfg(feature = "iceoryx2")]
    ipc_limits: BTreeMap<String, crate::iox2::Iox2ChannelConfig>,
}
impl NamedPlan {
    /// Generated registration annotates each endpoint after its checked declaration.
    pub fn name_last_port(&mut self, name: &str) {
        if let Some(port) = self.ports.last_mut() {
            port.name = Some(name.into());
        }
    }
    pub fn topology(&self) -> Topology {
        let mut snapshot = Topology {
            callbacks: self.tasks.keys().cloned().collect(),
            channels: self
                .channels
                .values()
                .map(|channel| channel.plan.topology())
                .collect(),
            ports: vec![],
        };
        #[cfg(feature = "iceoryx2")]
        for (name, (plan, _)) in &self.events {
            if !self.channels.contains_key(name) {
                let mut channel = plan.topology();
                channel.transport = crate::recording::Transport::Event;
                snapshot.channels.push(channel);
            }
        }
        snapshot.channels.sort_by(|a, b| a.name.cmp(&b.name));
        for (callback, ports) in &self.tasks {
            let mut ordinals = [0, 0];
            for port in ports {
                let ordinal = ordinals[usize::from(port.output)];
                ordinals[usize::from(port.output)] += 1;
                snapshot.ports.push(topology::PortTopology {
                    callback: callback.clone(),
                    port: port.name.clone().unwrap_or_else(|| ordinal.to_string()),
                    ordinal,
                    output: port.output,
                    channel: port.channel.clone(),
                    transport: port.transport,
                    payload_type: port.payload_type,
                    capacity: port.capacity,
                    endpoint_index: port.index,
                });
            }
        }
        snapshot
    }
    pub fn native_workload_publishers<T: Send + Sync + 'static>(
        &mut self,
        name: &str,
    ) -> Result<Vec<crate::PublisherKey<T>>, BuildError> {
        let indices: Vec<_> = self
            .ports
            .iter()
            .filter(|port| {
                port.channel == name
                    && port.output
                    && port.transport == crate::recording::Transport::Native
            })
            .map(|port| port.index)
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
    ) -> Result<crate::PublisherKey<T>, BuildError> {
        let key = self.native::<T>(name)?.publisher(capacity);
        self.ports.push(exact::PlannedPort::new::<T>(
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
    ) -> Result<crate::SubscriberKey<T>, BuildError> {
        let key = self
            .native::<T>(name)?
            .subscriber_with_policy(capacity, policy);
        self.channels.get_mut(name).unwrap().subscribers += 1;
        self.ports.push(exact::PlannedPort::new::<T>(
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
    pub fn allocate(mut self) -> Result<NamedStorage, BuildError> {
        self.prepare()?;
        let topology = self.topology();
        let mut names = crate::string_interner::ChannelNameInterner::new();
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
        Ok(NamedStorage {
            topology,
            channels,
            names: Arc::new(names),
            #[cfg(feature = "iceoryx2")]
            events,
        })
    }
    /// Validate and finalize service budgets before any channel allocation.
    pub fn prepare(&mut self) -> Result<(), BuildError> {
        let mut source_budgets = BTreeMap::<TypeId, usize>::new();
        for channel in self.channels.values() {
            if let Some((source, capacity)) = channel.plan.source_budget()? {
                let total = source_budgets.entry(source).or_default();
                *total = total
                    .checked_add(capacity)
                    .ok_or(crate::StorageError::CapacityOverflow)?;
            }
        }
        for channel in self.channels.values_mut() {
            channel.plan.set_source_budget(&source_budgets);
        }
        #[cfg(feature = "iceoryx2")]
        for (name, config) in self.ipc_service_limits()? {
            if let Some(channel) = self.channels.get_mut(&name) {
                channel.plan.set_ipc_config(config.clone());
            }
            if let Some((plan, _)) = self.events.get_mut(&name) {
                plan.set_config(config);
                StorageLayout::validate(plan)?;
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
pub struct NamedStorage {
    topology: Topology,
    channels: BTreeMap<String, Box<dyn Stored>>,
    names: Arc<crate::string_interner::ChannelNameInterner>,
    #[cfg(feature = "iceoryx2")]
    events: BTreeMap<String, Box<dyn Stored>>,
}
impl NamedStorage {
    pub fn topology(&self) -> &Topology {
        &self.topology
    }
    pub fn bind(&self) -> Result<NamedBindings<'_>, BuildError> {
        Ok(NamedBindings {
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
    pub fn graph_builder<'build, 'storage>(&self) -> GraphBuilder<'build, 'storage> {
        GraphBuilder::with_channel_names(self.names.clone())
    }
}
pub struct NamedBindings<'a> {
    channels: BTreeMap<String, Box<dyn Binding<'a> + 'a>>,
    #[cfg(feature = "iceoryx2")]
    events: BTreeMap<String, Box<dyn Binding<'a> + 'a>>,
}
impl<'a> NamedBindings<'a> {
    pub fn native<T: 'static>(&self, name: &str) -> Result<&EndpointBindings<'a, T>, BuildError> {
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
        // from EndpointBindings<'a, T>. The checked TypeId establishes T. The map
        // owns the boxed value, and this reference borrows the map. 'a is preserved
        // from Stored::bind through the map; no reference is extended to 'static.
        Ok(unsafe { &*pointer.cast::<EndpointBindings<'a, T>>() })
    }
}

#[cfg(feature = "iceoryx2")]
mod ipc {
    use super::*;
    use crate::iox2::*;
    use iceoryx2::prelude::ZeroCopySend;
    use std::fmt::Debug;
    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Plan for Iox2ChannelPlan<T> {
        fn topology(&self) -> topology::ChannelTopology {
            self.topology()
        }
        fn validate(&self) -> Result<(), BuildError> {
            StorageLayout::validate(self).map_err(Into::into)
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
            StorageLayout::validate(self.as_ref())?;
            Ok(Box::new(StorageLayout::allocate(*self)?))
        }
    }
    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Stored for Iox2ChannelStorage<T> {
        fn bind(&self) -> Result<Box<dyn Binding<'_> + '_>, BuildError> {
            Ok(Box::new(self.build()?))
        }
    }
    impl<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> Binding<'a> for Iox2Bindings<T> {
        fn payload(&self) -> TypeId {
            TypeId::of::<T>()
        }
        fn ipc_any(&self) -> Option<&dyn Any> {
            Some(self)
        }
    }
    impl NamedPlan {
        pub fn ipc_workload_publishers<T: Debug + ZeroCopySend + Send + Sync + 'static>(
            &mut self,
            name: &str,
        ) -> Result<Vec<Iox2PublisherKey<T>>, BuildError> {
            let indices: Vec<_> = self
                .ports
                .iter()
                .filter(|port| {
                    port.channel == name
                        && port.output
                        && port.transport == crate::recording::Transport::Ipc
                })
                .map(|port| port.index)
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
                    .ok_or(crate::StorageError::CapacityOverflow)?;
                config.max_notifiers = config
                    .max_notifiers
                    .checked_add(events.max_notifiers)
                    .ok_or(crate::StorageError::CapacityOverflow)?;
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
                if limits.max_borrowed_samples < limits.buffer_capacity.checked_mul(2).ok_or(crate::StorageError::CapacityOverflow)? { insufficient.push("max_borrowed_samples must cover twice buffer_capacity".into()); }
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
            self.ports.push(exact::PlannedPort::new::<T>(
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
            let key = self.ipc::<T>(name)?.subscriber(capacity);
            self.ports.push(exact::PlannedPort::new::<T>(
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
            self.ports.push(exact::PlannedPort::new::<()>(
                name, false, true, true, 0, capacity,
            ));
            Ok(key)
        }
        pub fn notifier(&mut self, name: &str) -> Result<Iox2NotifierKey, BuildError> {
            let key = self.event_plan(name)?.0.notifier();
            self.ports
                .push(exact::PlannedPort::new::<()>(name, true, true, true, 0, 1));
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
    impl NamedBindings<'_> {
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
