use task::{ArenaReaderPtr, message::Message};
use task_macros::ForwardedSources;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Value(pub u64);

#[derive(ForwardedSources)]
pub struct Sources<'s> {
    pub source: ArenaReaderPtr<'s, Message<Value>>,
}
