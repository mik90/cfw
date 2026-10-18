//! Graph input sources that decode recorded payloads and publish them through
//! bound endpoints. Replay drivers control when each source injects a message.
use crate::BoxedLogError;
use task::{ChannelBindings, ChannelPlan, PublisherKey, loggable::Loggable};

pub struct ReplaySourcePlan<T> {
    key: PublisherKey<T>,
}
impl<T> ReplaySourcePlan<T> {
    pub fn declare(plan: &mut ChannelPlan<T>, capacity: usize) -> Self {
        Self {
            key: plan.publisher(capacity),
        }
    }
    pub fn bind<'a>(
        self,
        bindings: &ChannelBindings<'a, T>,
    ) -> Result<ReplaySource<'a>, task::EndpointError>
    where
        T: Send + Sync + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        Ok(ReplaySource::native(bindings.take_publisher(&self.key)?))
    }
    pub fn bind_with_decoder<'a>(
        self,
        bindings: &ChannelBindings<'a, T>,
        decode: impl FnMut(&[u8]) -> Result<T, BoxedLogError> + Send + 'a,
    ) -> Result<ReplaySource<'a>, task::EndpointError>
    where
        T: Send + Sync + 'a,
    {
        Ok(ReplaySource::with_decoder(
            bindings.take_publisher(&self.key)?,
            decode,
        ))
    }
}
pub use task_graph::replay::ReplaySource;
