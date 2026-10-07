use crate::string_interner::{CallbackNameTag, ChannelNameTag, StringInterner};
use crate::time::FrameworkTime;

#[derive(Clone, Debug)]
pub struct Context<'a> {
    /// Current framework time, frozen at time of execution
    pub now: FrameworkTime,
    /// Interned channel names, frozen at graph build time
    pub channel_names: &'a StringInterner<ChannelNameTag>,
    /// Interned callback names, frozen at graph build time
    pub callback_names: &'a StringInterner<CallbackNameTag>,
}

impl<'a> Context<'a> {
    pub fn new(
        now: FrameworkTime,
        channel_names: &'a StringInterner<ChannelNameTag>,
        callback_names: &'a StringInterner<CallbackNameTag>,
    ) -> Self {
        Context {
            now,
            channel_names,
            callback_names,
        }
    }

    pub fn now(&self) -> FrameworkTime {
        self.now
    }

    pub fn channel_names(&'a self) -> &'a StringInterner<ChannelNameTag> {
        self.channel_names
    }

    pub fn callback_names(&'a self) -> &'a StringInterner<CallbackNameTag> {
        self.callback_names
    }
}
