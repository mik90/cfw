use crate::string_interner::{CallbackNameInterner, ChannelNameInterner};
use crate::time::FrameworkTime;

/// Executor-provided time for one callback invocation.
#[derive(Clone, Debug)]
pub struct Context<'execution> {
    pub now: FrameworkTime,
    pub channel_names: &'execution ChannelNameInterner,
    pub callback_names: &'execution CallbackNameInterner,
}

impl<'execution> Context<'execution> {
    pub fn new(
        now: FrameworkTime,
        channel_names: &'execution ChannelNameInterner,
        callback_names: &'execution CallbackNameInterner,
    ) -> Self {
        Self {
            now,
            channel_names,
            callback_names,
        }
    }
    pub fn now(&self) -> FrameworkTime {
        self.now
    }
    pub fn channel_names(&self) -> &ChannelNameInterner {
        self.channel_names
    }
    pub fn callback_names(&self) -> &CallbackNameInterner {
        self.callback_names
    }
}
