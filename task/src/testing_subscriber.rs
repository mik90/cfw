use crate::{Subscriber, message::Message};
use base::arena::ArenaReaderPtr;

pub const DEFAULT_TEST_SUBSCRIBER_CAPACITY: usize = 10;

/// Cumulative overflow counts on each side of the subscriber's double buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DroppedMessages {
    pub writer: usize,
    pub reader: usize,
}
impl DroppedMessages {
    pub fn any(&self) -> bool {
        self.writer > 0 || self.reader > 0
    }
}

/// Capture endpoint whose messages may outlive both the fixture and executor.
/// The application must retain graph storage until all messages are dropped.
pub struct TestSubscriber<'storage, T> {
    subscriber: Subscriber<'storage, T>,
}
impl<'storage, T> TestSubscriber<'storage, T> {
    pub fn new(subscriber: Subscriber<'storage, T>) -> Self {
        Self { subscriber }
    }

    pub fn try_messages(
        &mut self,
        mut inspect: impl FnMut(usize, &Message<T>),
    ) -> (usize, DroppedMessages) {
        let (messages, dropped) = self.try_take_messages();
        for (index, message) in messages.iter().enumerate() {
            inspect(index, message);
        }
        (messages.len(), dropped)
    }

    pub fn messages(&mut self, inspect: impl FnMut(usize, &Message<T>)) -> usize {
        let (count, dropped) = self.try_messages(inspect);
        assert!(!dropped.any(), "test subscriber overflow: {dropped:?}");
        count
    }

    pub fn try_take_messages(
        &mut self,
    ) -> (Vec<ArenaReaderPtr<'storage, Message<T>>>, DroppedMessages) {
        self.subscriber.update();
        let dropped = DroppedMessages {
            writer: self.subscriber.writer_drops(),
            reader: self.subscriber.reader_drops(),
        };
        (self.subscriber.input().drain().collect(), dropped)
    }

    pub fn take_messages(&mut self) -> Vec<ArenaReaderPtr<'storage, Message<T>>> {
        let (messages, dropped) = self.try_take_messages();
        assert!(!dropped.any(), "test subscriber overflow: {dropped:?}");
        messages
    }
}
