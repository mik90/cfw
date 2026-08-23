pub type ChannelName = String;
pub type ChannelNameStr<'a> = &'a str;

// TODO define tags in string_interner.rs
#[derive(Clone, Debug)]
pub struct ChannelNameTag {}

pub type CallbackNodeName = String;

// TODO define tags in string_interner.rs
#[derive(Clone, Debug)]
pub struct CallbackNameTag {}
