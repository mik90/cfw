//! Named construction for tasks with owned (`'static`) payload types.
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
pub mod replay;

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
        let factory = self
            .task
            .register_with(plan, &self.channels)
            .map_err(|e| BuildError(format!("task '{}': {e}", self.name)))?;
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
    fn any_mut(&mut self) -> &mut dyn Any;
    fn allocate(self: Box<Self>) -> Result<Box<dyn Stored>, BuildError>;
    #[cfg(feature = "iceoryx2")]
    fn is_ipc(&self) -> bool {
        false
    }
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
    fn any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn allocate(self: Box<Self>) -> Result<Box<dyn Stored>, BuildError> {
        self.validate()?;
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
    plan: Box<dyn Plan>,
    publishers: usize,
    subscribers: usize,
}
#[derive(Default)]
pub struct NamedPlan {
    channels: BTreeMap<String, Channel>,
    captures: BTreeMap<String, Box<dyn capture::CaptureDeclaration>>,
    replay_sources: BTreeMap<String, Box<dyn replay::SourceDeclaration>>,
    #[cfg(feature = "iceoryx2")]
    events: BTreeMap<String, (crate::iox2::Iox2ChannelPlan<()>, usize)>,
    #[cfg(feature = "iceoryx2")]
    runtime: Option<Arc<crate::iox2::Iox2Runtime>>,
}
impl NamedPlan {
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
            plan: Box::new(ChannelPlan::<T>::new(name)),
            publishers: 0,
            subscribers: 0,
        });
        entry.plan.any_mut().downcast_mut().ok_or_else(|| {
            BuildError(format!(
                "channel '{name}': incompatible payload type or transport (requested {})",
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
    pub fn allocate(self) -> Result<NamedStorage, BuildError> {
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
            channels,
            names: Arc::new(names),
            #[cfg(feature = "iceoryx2")]
            events,
        })
    }
}
pub struct NamedStorage {
    channels: BTreeMap<String, Box<dyn Stored>>,
    names: Arc<crate::string_interner::ChannelNameInterner>,
    #[cfg(feature = "iceoryx2")]
    events: BTreeMap<String, Box<dyn Stored>>,
}
impl NamedStorage {
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
        fn any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn is_ipc(&self) -> bool {
            true
        }
        fn allocate(self: Box<Self>) -> Result<Box<dyn Stored>, BuildError> {
            self.validate()?;
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
                plan: Box::new(Iox2ChannelPlan::<T>::new(name, &runtime)),
                publishers: 0,
                subscribers: 0,
            });
            channel.plan.any_mut().downcast_mut().ok_or_else(|| {
                BuildError(format!(
                    "channel '{name}': incompatible payload type or transport"
                ))
            })
        }
        pub fn ipc_publisher<T: Debug + ZeroCopySend + Send + Sync + 'static>(
            &mut self,
            name: &str,
            capacity: usize,
        ) -> Result<Iox2PublisherKey<T>, BuildError> {
            let key = self.ipc::<T>(name)?.publisher(capacity);
            self.channels.get_mut(name).unwrap().publishers += 1;
            Ok(key)
        }
        pub fn ipc_subscriber<T: Debug + ZeroCopySend + Send + Sync + 'static>(
            &mut self,
            name: &str,
            capacity: usize,
        ) -> Result<Iox2SubscriberKey<T>, BuildError> {
            let key = self.ipc::<T>(name)?.subscriber(capacity);
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
            Ok(entry.0.events(capacity))
        }
        pub fn notifier(&mut self, name: &str) -> Result<Iox2NotifierKey, BuildError> {
            Ok(self.event_plan(name)?.0.notifier())
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
