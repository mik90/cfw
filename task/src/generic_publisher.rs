use crate::pub_sub::ChannelName;
use crate::time::FrameworkTime;
use crate::{generic_subscriber::GenericSubscriber, publisher::PublisherConfig};

pub struct ConnectionTypeMismatch {}

pub trait GenericPublisher {
    fn as_any(&mut self) -> &mut dyn std::any::Any;

    fn get_config(&self) -> &PublisherConfig;

    fn get_config_mut(&mut self) -> &mut PublisherConfig;

    fn get_forwarded_channels(&self) -> &[ChannelName];

    fn flush_loaned_values(&mut self, timestamp: FrameworkTime);

    fn allocate_arena(&mut self);

    fn increase_arena_size(&mut self, additional_capacity: usize);

    fn connect_to_subscriber(
        &mut self,
        subscriber: &mut dyn GenericSubscriber,
    ) -> Result<(), ConnectionTypeMismatch>;

    /// Iterate over sent-but-not-yet-flushed outputs.
    /// The default no-op impl is used by publishers that don't participate in logging.
    fn for_each_pending_output(&self, _f: &mut dyn FnMut(&dyn std::any::Any)) {}
}
