//! Closed payload-family erasure for storage-borrowing forwarded messages.
use super::*;
use crate::{ForwardedMessage, PublisherKey, SubscriberKey};
use std::marker::PhantomData;

struct Family<M, S>(PhantomData<fn() -> (M, S)>);
struct ForwardPlan<M: 'static, S: 'static>(ChannelPlan<ForwardedMessage<'static, M, S>>);
struct ForwardStorage<M: 'static, S: 'static>(ChannelStorage<ForwardedMessage<'static, M, S>>);
struct ForwardBinding<'a, M, S>(EndpointBindings<'a, ForwardedMessage<'a, M, S>>);

/// Declaration keys own only name/index metadata and never carry message values.
pub fn publisher_key<'a, M, S>(
    key: PublisherKey<ForwardedMessage<'static, M, S>>,
) -> PublisherKey<ForwardedMessage<'a, M, S>> {
    key.retype()
}
pub fn subscriber_key<'a, M, S>(
    key: SubscriberKey<ForwardedMessage<'static, M, S>>,
) -> SubscriberKey<ForwardedMessage<'a, M, S>> {
    key.retype()
}

impl<M: Send + Sync + 'static, S: Send + Sync + 'static> Plan for ForwardPlan<M, S> {
    fn payload_type(&self) -> &'static str {
        std::any::type_name::<ForwardedMessage<'static, M, S>>()
    }
    fn topology(&self) -> topology::ChannelTopology {
        self.0.topology()
    }
    fn any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn validate(&self) -> Result<(), BuildError> {
        StorageLayout::validate(&self.0).map_err(Into::into)
    }
    fn source_budget(&self) -> Result<Option<(TypeId, usize)>, BuildError> {
        let mut total = 0usize;
        for index in 0..self.0.topology().publishers.len() {
            total = total
                .checked_add(self.0.publisher_capacity(&self.0.publisher_key(index))?)
                .ok_or(crate::StorageError::CapacityOverflow)?;
        }
        Ok(Some((TypeId::of::<S>(), total)))
    }
    fn allocate(self: Box<Self>) -> Result<Box<dyn Stored>, BuildError> {
        self.validate()?;
        Ok(Box::new(ForwardStorage(StorageLayout::allocate(self.0)?)))
    }
}
impl<M: Send + Sync + 'static, S: Send + Sync + 'static> Stored for ForwardStorage<M, S> {
    fn bind(&self) -> Result<Box<dyn Binding<'_> + '_>, BuildError> {
        fn bind<'a, M: 'static, S: 'static>(
            storage: &'a ChannelStorage<ForwardedMessage<'static, M, S>>,
        ) -> EndpointBindings<'a, ForwardedMessage<'a, M, S>> {
            // SAFETY: The closed family owns raw MaybeUninit arena slots and
            // endpoint metadata. Payloads are initialized and destroyed exclusively
            // by lifetime-bound loans/messages; ChannelStorage never reads/drops a
            // payload. Each build connects only its own endpoints. Reusing arena
            // slots across builds requires the preceding message to release them.
            // No accessor exposes the canonical 'static payload view. Family's
            // distinct TypeId also prevents native::<ForwardedMessage<'static,..>>
            // from extracting a binding with an extended source lifetime.
            let storage = unsafe {
                &*std::ptr::from_ref(storage).cast::<ChannelStorage<ForwardedMessage<'a, M, S>>>()
            };
            storage.build()
        }
        Ok(Box::new(ForwardBinding(bind(&self.0))))
    }
}
impl<'a, M: 'static, S: 'static> Binding<'a> for ForwardBinding<'a, M, S> {
    fn payload(&self) -> TypeId {
        TypeId::of::<Family<M, S>>()
    }
    fn pointer(&self) -> Option<*const ()> {
        Some((&self.0 as *const EndpointBindings<'a, ForwardedMessage<'a, M, S>>).cast())
    }
}
impl NamedPlan {
    pub fn forwarded_input_key<M: Send + Sync + 'static, S: Send + Sync + 'static>(
        &mut self,
        callback: &str,
        ordinal: usize,
    ) -> Result<SubscriberKey<ForwardedMessage<'static, M, S>>, BuildError> {
        let port = self.port(callback, ordinal, false)?;
        Ok(self
            .forwarded::<M, S>(&port.channel)?
            .subscriber_key(port.index))
    }
    /// Plans forwarded storage; the returned keys describe a payload family,
    /// while binding instantiates the payload with the graph-storage lifetime.
    pub fn forwarded<M: Send + Sync + 'static, S: Send + Sync + 'static>(
        &mut self,
        name: &str,
    ) -> Result<&mut ChannelPlan<ForwardedMessage<'static, M, S>>, BuildError> {
        #[cfg(feature = "iceoryx2")]
        if self.events.contains_key(name) {
            return Err(BuildError(format!(
                "channel '{name}' mixes forwarded native and IPC endpoints"
            )));
        }
        let entry = self.channels.entry(name.into()).or_insert_with(|| Channel {
            origin: self.active_task.clone(),
            plan: Box::new(ForwardPlan::<M, S>(ChannelPlan::new(name))),
            publishers: 0,
            subscribers: 0,
        });
        let existing = entry.plan.payload_type();
        let origin = entry.origin.as_deref().unwrap_or("direct declaration");
        entry.plan.any_mut().downcast_mut::<ForwardPlan<M,S>>().map(|plan| &mut plan.0).ok_or_else(|| BuildError(format!("channel '{name}': incompatible forwarded payload family (first declared by '{origin}' as {existing}, requested {})", std::any::type_name::<ForwardedMessage<'static,M,S>>())))
    }
    pub fn forwarded_publisher<M: Send + Sync + 'static, S: Send + Sync + 'static>(
        &mut self,
        name: &str,
        capacity: usize,
    ) -> Result<PublisherKey<ForwardedMessage<'static, M, S>>, BuildError> {
        let key = self.forwarded::<M, S>(name)?.publisher(capacity);
        self.ports
            .push(exact::PlannedPort::new::<ForwardedMessage<'static, M, S>>(
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
    pub fn forwarded_subscriber<M: Send + Sync + 'static, S: Send + Sync + 'static>(
        &mut self,
        name: &str,
        capacity: usize,
        policy: SubscriberPolicy,
    ) -> Result<SubscriberKey<ForwardedMessage<'static, M, S>>, BuildError> {
        let key = self
            .forwarded::<M, S>(name)?
            .subscriber_with_policy(capacity, policy);
        self.ports
            .push(exact::PlannedPort::new::<ForwardedMessage<'static, M, S>>(
                name,
                false,
                false,
                false,
                key.index(),
                capacity,
            ));
        self.channels.get_mut(name).unwrap().subscribers += 1;
        Ok(key)
    }
}
impl<'a> NamedBindings<'a> {
    /// The source borrow cannot be promoted to static, even when the declaration
    /// key uses the family's canonical planning type.
    /// ```compile_fail
    /// use task::{automatic::{NamedBindings, forwarding::subscriber_key}, ForwardedMessage, Subscriber, SubscriberKey};
    /// fn extend<'a>(bindings: &NamedBindings<'a>, key: SubscriberKey<ForwardedMessage<'static,bool,u64>>)
    ///     -> Subscriber<'a,ForwardedMessage<'static,bool,u64>> {
    ///     bindings.forwarded::<bool,u64>("forwarded").unwrap().take_subscriber(&subscriber_key(key)).unwrap()
    /// }
    /// ```
    pub fn forwarded<M: Send + Sync + 'static, S: Send + Sync + 'static>(
        &self,
        name: &str,
    ) -> Result<&EndpointBindings<'a, ForwardedMessage<'a, M, S>>, BuildError> {
        let binding = self
            .channels
            .get(name)
            .ok_or_else(|| BuildError(format!("unknown channel '{name}'")))?;
        if binding.payload() != TypeId::of::<Family<M, S>>() {
            return Err(BuildError(format!(
                "channel '{name}': forwarded payload family mismatch"
            )));
        }
        // SAFETY: Family identity is private and implemented only by ForwardBinding
        // carrying exactly these payload types and the bindings' storage lifetime.
        Ok(unsafe {
            &*binding
                .pointer()
                .unwrap()
                .cast::<EndpointBindings<'a, ForwardedMessage<'a, M, S>>>()
        })
    }
}

pub struct CaptureProbe<M, S>(PhantomData<fn() -> (M, S)>);
impl<M, S> Default for CaptureProbe<M, S> {
    fn default() -> Self {
        Self(PhantomData)
    }
}
pub trait MaybeCapture {
    fn register(&self, _: &mut NamedPlan, _: &str) -> Result<(), BuildError> {
        Ok(())
    }
}
impl<M, S> MaybeCapture for CaptureProbe<M, S> {}
impl<M: Send + Sync + 'static, S: Send + Sync + 'static> CaptureProbe<M, S>
where
    for<'a> ForwardedMessage<'a, M, S>: crate::loggable::Loggable,
{
    pub fn register(&self, plan: &mut NamedPlan, channel: &str) -> Result<(), BuildError> {
        plan.forwarded::<M, S>(channel)?;
        plan.captures
            .entry(channel.into())
            .or_insert_with(|| Box::new(ForwardCapture::<M, S>(PhantomData)));
        Ok(())
    }
}
struct ForwardCapture<M, S>(PhantomData<fn() -> (M, S)>);
struct ForwardCaptureFactory<M: 'static, S: 'static> {
    channel: String,
    subscriber: SubscriberKey<ForwardedMessage<'static, M, S>>,
}
struct ForwardOutputFactory<M: 'static, S: 'static> {
    channel: String,
    publisher: PublisherKey<ForwardedMessage<'static, M, S>>,
    capacity: usize,
}
impl<M: Send + Sync + 'static, S: Send + Sync + 'static> capture::CaptureDeclaration
    for ForwardCapture<M, S>
where
    for<'a> ForwardedMessage<'a, M, S>: crate::loggable::Loggable,
{
    fn declare(
        &self,
        plan: &mut NamedPlan,
        channel: &str,
        capacity: usize,
    ) -> Result<Box<dyn capture::CaptureFactory>, BuildError> {
        let subscriber = plan.forwarded::<M, S>(channel)?.subscriber_with_policy(
            capacity,
            SubscriberPolicy {
                trigger: false,
                keep_across_runs: true,
            },
        );
        Ok(Box::new(ForwardCaptureFactory {
            channel: channel.into(),
            subscriber,
        }))
    }
    fn declare_output(
        &self,
        plan: &mut NamedPlan,
        port: &exact::PlannedPort,
    ) -> Result<Box<dyn exact::ExactFactory>, BuildError> {
        Ok(Box::new(ForwardOutputFactory {
            channel: port.channel.clone(),
            publisher: plan
                .forwarded::<M, S>(&port.channel)?
                .publisher_key(port.index),
            capacity: port.capacity,
        }))
    }
}
impl<M: Send + Sync + 'static, S: Send + Sync + 'static> capture::CaptureFactory
    for ForwardCaptureFactory<M, S>
where
    for<'a> ForwardedMessage<'a, M, S>: crate::loggable::Loggable,
{
    fn bind<'a>(
        self: Box<Self>,
        bindings: &NamedBindings<'a>,
    ) -> Result<Box<dyn capture::SerializedCapture + 'a>, BuildError> {
        Ok(Box::new(capture::NativeCapture::new(
            bindings
                .forwarded::<M, S>(&self.channel)?
                .take_subscriber(&subscriber_key(self.subscriber))?,
        )))
    }
}
impl<M: Send + Sync + 'static, S: Send + Sync + 'static> exact::ExactFactory
    for ForwardOutputFactory<M, S>
where
    for<'a> ForwardedMessage<'a, M, S>: crate::loggable::Loggable,
{
    fn bind<'a>(
        self: Box<Self>,
        bindings: &NamedBindings<'a>,
    ) -> Result<exact::ExactPort<'a>, BuildError> {
        Ok(exact::ExactPort::Output(
            bindings
                .forwarded::<M, S>(&self.channel)?
                .configure_publisher(&publisher_key(self.publisher), |publisher| {
                    super::port_capture::PortCapture::native(publisher, self.capacity)
                })?,
        ))
    }
}
