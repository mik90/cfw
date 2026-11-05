use base::string_interner::{InternId, StringInterner};

/// Marker type distinguishing interned callback names from interned channel
/// names, so [`StringInterner`] lookups stay strongly typed.
#[derive(Clone, Debug)]
pub struct CallbackNameTag {}
pub type CallbackNameInterner = StringInterner<CallbackNameTag>;
pub type CallbackId = InternId<CallbackNameTag>;

/// Marker type distinguishing interned channel names from interned callback
/// names, so [`StringInterner`] lookups stay strongly typed.
#[derive(Clone, Debug)]
pub struct ChannelNameTag {}
pub type ChannelNameInterner = StringInterner<ChannelNameTag>;
pub type ChannelId = InternId<ChannelNameTag>;
