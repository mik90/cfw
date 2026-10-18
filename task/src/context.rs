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

#[cfg(test)]
pub mod testing {
    use super::*;

    /// Test utility for creating contexts
    pub struct ContextBuilder {
        pub now: FrameworkTime,
        pub channel_names: ChannelNameInterner,
        pub callback_names: CallbackNameInterner,
    }

    impl Default for ContextBuilder {
        fn default() -> Self {
            Self {
                now: FrameworkTime::INVALID,
                channel_names: ChannelNameInterner::default(),
                callback_names: CallbackNameInterner::default(),
            }
        }
    }

    impl ContextBuilder {
        pub fn with_time(time: FrameworkTime) -> Self {
            Self {
                now: time,
                ..Default::default()
            }
        }
        pub fn build(&'_ self) -> Context<'_> {
            Context::new(self.now, &self.channel_names, &self.callback_names)
        }
    }
}
