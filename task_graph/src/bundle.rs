//! Heterogeneous forwarding families and explicit retention dependencies.
use crate::plan::{Binding, Channel, Plan, Stored};
use crate::{BuildError, GraphBindings, GraphPlan, capture, endpoints, topology};
use std::marker::PhantomData;
use std::{
    any::{Any, TypeId},
    collections::BTreeMap,
};
use task::{ChannelBindings, ChannelPlan, ChannelStorage, SubscriberPolicy};
use task::{Forwarded, ForwardedSources, PublisherKey, SubscriberKey};

type Value<'a, M, S> = Forwarded<M, <S as ForwardedSources>::Bound<'a>>;
struct Family<M, S>(PhantomData<fn() -> (M, S)>);
struct BundlePlan<M, S: ForwardedSources>(ChannelPlan<Value<'static, M, S>>);
struct BundleStorage<M, S: ForwardedSources>(ChannelStorage<Value<'static, M, S>>);
struct BundleBinding<'a, M, S: ForwardedSources>(ChannelBindings<'a, Value<'a, M, S>>);

pub use task::forwarded_message::{publisher_key, subscriber_key};

impl<M: Send + Sync + 'static, S: ForwardedSources> Plan for BundlePlan<M, S> {
    fn payload_type(&self) -> &'static str {
        std::any::type_name::<Value<'static, M, S>>()
    }
    fn topology(&self) -> topology::ChannelTopology {
        self.0.topology()
    }
    fn any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn validate(&self) -> Result<(), BuildError> {
        self.0.validate().map_err(Into::into)
    }
    fn forwarding_slots(&self) -> Result<usize, BuildError> {
        self.0.total_publisher_capacity().map_err(Into::into)
    }
    fn reserve_forwarding(&mut self, capacity: usize) -> Result<(), BuildError> {
        self.0.set_forwarding_retention(capacity);
        Ok(())
    }
    fn allocate(self: Box<Self>) -> Result<Box<dyn Stored>, BuildError> {
        Ok(Box::new(BundleStorage::<M, S>(self.0.allocate()?)))
    }
}
impl<M: Send + Sync + 'static, S: ForwardedSources> Stored for BundleStorage<M, S> {
    fn bind(&self) -> Result<Box<dyn Binding<'_> + '_>, BuildError> {
        fn bind<'a, M: 'static, S: ForwardedSources>(
            storage: &'a ChannelStorage<Value<'static, M, S>>,
        ) -> ChannelBindings<'a, Value<'a, M, S>> {
            // SAFETY: ForwardedSources guarantees layout/drop equivalence under
            // lifetime substitution, including nested source payloads. Each build
            // creates fresh queues: it cannot recover publications from an older
            // binding. Arenas only allocate free slots and never traverse payloads
            // on drop. A publication is accessed/destroyed through its bound typed
            // handles, which carry both destination and source storage lifetimes.
            // Forgotten handles leak occupied slots; a later binding cannot read
            // them. The private Family identity prevents canonical static access.
            let storage =
                unsafe { &*std::ptr::from_ref(storage).cast::<ChannelStorage<Value<'a, M, S>>>() };
            storage.bind()
        }
        Ok(Box::new(BundleBinding::<M, S>(bind::<M, S>(&self.0))))
    }
}
impl<'a, M: 'static, S: ForwardedSources> Binding<'a> for BundleBinding<'a, M, S> {
    fn payload(&self) -> TypeId {
        TypeId::of::<Family<M, S>>()
    }
    fn pointer(&self) -> Option<*const ()> {
        Some(std::ptr::from_ref(&self.0).cast())
    }
}

#[derive(Clone, Debug)]
pub struct ForwardingEdge {
    pub callback: String,
    pub output: String,
    pub field: String,
    pub input: String,
    pub source_channel: String,
    pub output_channel: String,
}
impl GraphPlan {
    pub fn bundle_input_key<M: Send + Sync + 'static, S: ForwardedSources>(
        &mut self,
        callback: &str,
        ordinal: usize,
    ) -> Result<SubscriberKey<Value<'static, M, S>>, BuildError> {
        let endpoint = self.endpoint(callback, ordinal, false)?;
        Ok(self
            .bundle::<M, S>(&endpoint.channel)?
            .subscriber_key(endpoint.index))
    }
    pub fn bundle<M: Send + Sync + 'static, S: ForwardedSources>(
        &mut self,
        name: &str,
    ) -> Result<&mut ChannelPlan<Value<'static, M, S>>, BuildError> {
        #[cfg(feature = "iceoryx2")]
        if self.events.contains_key(name) {
            return Err(BuildError(format!(
                "channel '{name}' mixes forwarding and IPC"
            )));
        }
        let entry = self.channels.entry(name.into()).or_insert_with(|| Channel {
            capture: None,
            replay: None,
            origin: self.active_task.clone(),
            plan: Box::new(BundlePlan::<M, S>(ChannelPlan::new(name))),
            publishers: 0,
            subscribers: 0,
        });
        entry
            .plan
            .any_mut()
            .downcast_mut::<BundlePlan<M, S>>()
            .map(|plan| &mut plan.0)
            .ok_or_else(|| {
                BuildError(format!(
                    "channel '{name}': incompatible forwarding bundle {}",
                    std::any::type_name::<S>()
                ))
            })
    }
    pub fn bundle_publisher<M: Send + Sync + 'static, S: ForwardedSources>(
        &mut self,
        name: &str,
        capacity: usize,
    ) -> Result<PublisherKey<Value<'static, M, S>>, BuildError> {
        let key = self.bundle::<M, S>(name)?.publisher(capacity);
        self.endpoints
            .push(endpoints::PlannedEndpoint::new::<Value<'static, M, S>>(
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
    pub fn bundle_subscriber<M: Send + Sync + 'static, S: ForwardedSources>(
        &mut self,
        name: &str,
        capacity: usize,
        policy: SubscriberPolicy,
    ) -> Result<SubscriberKey<Value<'static, M, S>>, BuildError> {
        let key = self
            .bundle::<M, S>(name)?
            .subscriber_with_policy(capacity, policy);
        self.endpoints
            .push(endpoints::PlannedEndpoint::new::<Value<'static, M, S>>(
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
    /// Map every source field to a resolved forwardable input channel. Generated
    /// registration validates capabilities; payload identities are checked here.
    pub fn map_forwarding<S: ForwardedSources>(
        &mut self,
        output: &str,
        output_channel: &str,
        fields: &[(&str, &str, &str, TypeId)],
    ) -> Result<(), BuildError> {
        let expected = S::fields();
        if expected.len() != fields.len() {
            return Err(BuildError(format!(
                "output '{output}': every source field requires a forwarding mapping"
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for &(field, input, _, ty) in fields {
            if !seen.insert(field)
                || !expected
                    .iter()
                    .any(|(name, id)| *name == field && *id == ty)
            {
                return Err(BuildError(format!(
                    "output '{output}', field '{field}', input '{input}': unknown, duplicate or incompatible source field"
                )));
            }
        }
        for &(field, input, source_channel, _) in fields {
            self.forwarding_edges.push(ForwardingEdge {
                callback: self.active_task.clone().unwrap_or_default(),
                output: output.into(),
                field: field.into(),
                input: input.into(),
                source_channel: source_channel.into(),
                output_channel: output_channel.into(),
            });
        }
        Ok(())
    }
    pub(crate) fn prepare_forwarding(&mut self) -> Result<(), BuildError> {
        fn budget(
            name: &str,
            plan: &mut GraphPlan,
            visiting: &mut std::collections::BTreeSet<String>,
            done: &mut BTreeMap<String, usize>,
        ) -> Result<usize, BuildError> {
            if let Some(value) = done.get(name) {
                return Ok(*value);
            }
            if !visiting.insert(name.into()) {
                return Err(BuildError(format!(
                    "cyclic forwarding retention dependency at channel '{name}'"
                )));
            }
            let outputs: Vec<_> = plan
                .forwarding_edges
                .iter()
                .filter(|edge| edge.source_channel == name)
                .map(|edge| edge.output_channel.clone())
                .collect();
            let mut extra = 0usize;
            for output in outputs {
                extra = extra
                    .checked_add(budget(&output, plan, visiting, done)?)
                    .ok_or(task::StorageError::CapacityOverflow)?;
            }
            let channel = plan
                .channels
                .get_mut(name)
                .ok_or_else(|| BuildError(format!("unknown forwarding channel '{name}'")))?;
            channel.plan.reserve_forwarding(extra)?;
            let capacity = channel.plan.forwarding_slots()?;
            visiting.remove(name);
            done.insert(name.into(), capacity);
            Ok(capacity)
        }
        let sources: Vec<_> = self
            .forwarding_edges
            .iter()
            .map(|edge| edge.source_channel.clone())
            .collect();
        let mut done = BTreeMap::new();
        for source in sources {
            budget(&source, self, &mut Default::default(), &mut done)?;
        }
        Ok(())
    }
}
impl<'a> GraphBindings<'a> {
    pub fn bundle<M: Send + Sync + 'static, S: ForwardedSources>(
        &self,
        name: &str,
    ) -> Result<&ChannelBindings<'a, Value<'a, M, S>>, BuildError> {
        let binding = self
            .channels
            .get(name)
            .ok_or_else(|| BuildError(format!("unknown channel '{name}'")))?;
        if binding.payload() != TypeId::of::<Family<M, S>>() {
            return Err(BuildError(format!(
                "channel '{name}': forwarding bundle mismatch"
            )));
        }
        // SAFETY: Private family identity certifies both payload types and the
        // storage lifetime carried by BundleBinding. No static view is exposed.
        Ok(unsafe {
            &*binding
                .pointer()
                .unwrap()
                .cast::<ChannelBindings<'a, Value<'a, M, S>>>()
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
    fn register(&self, _: &mut GraphPlan, _: &str) -> Result<(), BuildError> {
        Ok(())
    }
}
impl<M, S> MaybeCapture for CaptureProbe<M, S> {}
impl<M: Send + Sync + 'static, S: ForwardedSources> CaptureProbe<M, S>
where
    for<'a> Value<'a, M, S>: task::loggable::Loggable,
{
    pub fn register(&self, plan: &mut GraphPlan, channel: &str) -> Result<(), BuildError> {
        plan.bundle::<M, S>(channel)?;
        plan.channels
            .get_mut(channel)
            .unwrap()
            .capture
            .get_or_insert(capture::CaptureDeclaration {
                declare: declare_capture::<M, S>,
                declare_output: declare_output::<M, S>,
            });
        Ok(())
    }
}
fn declare_capture<M: Send + Sync + 'static, S: ForwardedSources>(
    plan: &mut GraphPlan,
    channel: &str,
    capacity: usize,
) -> Result<capture::CaptureBinding, BuildError>
where
    for<'a> Value<'a, M, S>: task::loggable::Loggable,
{
    let key = plan.bundle::<M, S>(channel)?.subscriber_with_policy(
        capacity,
        SubscriberPolicy {
            trigger: false,
            keep_across_runs: true,
        },
    );
    let channel = channel.to_owned();
    Ok(Box::new(move |bindings| {
        Ok(Box::new(capture::NativeCapture::new(
            bindings
                .bundle::<M, S>(&channel)?
                .take_subscriber(&subscriber_key::<M, S>(key))?,
        )))
    }))
}
fn declare_output<M: Send + Sync + 'static, S: ForwardedSources>(
    plan: &mut GraphPlan,
    endpoint: &endpoints::PlannedEndpoint,
) -> Result<capture::OutputBinding, BuildError>
where
    for<'a> Value<'a, M, S>: task::loggable::Loggable,
{
    let key = plan
        .bundle::<M, S>(&endpoint.channel)?
        .publisher_key(endpoint.index);
    let channel = endpoint.channel.clone();
    Ok(capture::OutputBinding::new(move |bindings, sink| {
        bindings.bundle::<M, S>(&channel)?.configure_publisher(
            &publisher_key::<M, S>(key),
            |publisher| {
                capture::output::observe_native(publisher, sink);
            },
        )?;
        Ok(())
    }))
}
