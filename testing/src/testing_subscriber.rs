use task::{Subscriber, message::Message};

pub const DEFAULT_TEST_SUBSCRIBER_CAPACITY: usize = 10;

/// Cumulative overflow counts on each side of the subscriber's double buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DroppedMessages {
    pub writer: usize,
    pub reader: usize,
}

impl DroppedMessages {
    /// Whether any writer or reader data has been lost. Unlike normal callback execution, we can treat these
    /// as errors by default since we can expect test subscribers to try consuming all data emitted in a test.
    /// This may not be suitable for _all_ tests, so we have both `try_*` and non-try variants in the test subscriber
    /// for when drops are fine.
    pub fn any(&self) -> bool {
        self.writer > 0 || self.reader > 0
    }
}

/// Capture endpoint whose messages may outlive both the fixture and executor.
/// The application must retain graph storage until all messages are dropped.
pub struct TestSubscriber<'graph_storage, T> {
    subscriber: Subscriber<'graph_storage, T>,
}
impl<'graph_storage, T> TestSubscriber<'graph_storage, T> {
    pub fn new(subscriber: Subscriber<'graph_storage, T>) -> Self {
        Self { subscriber }
    }

    /// Inspect the next batch and consume it. Message references do not escape
    /// the callback, so payloads need not be Clone.
    pub fn try_messages(
        &self,
        mut inspect: impl FnMut(usize, &Message<T>),
    ) -> (usize, DroppedMessages) {
        self.subscriber.update();
        let dropped = self.dropped();
        let mut input = self.subscriber.input_readonly();
        let count = input.len();
        for (index, message) in input.drain().enumerate() {
            inspect(index, &message);
        }
        (count, dropped)
    }

    pub fn messages(&self, inspect: impl FnMut(usize, &Message<T>)) -> usize {
        let (count, dropped) = self.try_messages(inspect);
        assert!(!dropped.any(), "test subscriber drops: {dropped:?}");
        count
    }

    /// Inspect the next batch without consuming it. Pending writer-side
    /// publications are moved into the reader buffer first, so this matches the
    /// batch a later `messages` call would return.
    pub fn try_peek(
        &self,
        mut inspect: impl FnMut(usize, &Message<T>),
    ) -> (usize, DroppedMessages) {
        self.subscriber.update();
        let dropped = self.dropped();
        let input = self.subscriber.input_readonly();
        for (index, message) in input.inputs().enumerate() {
            inspect(index, message);
        }
        (input.len(), dropped)
    }

    pub fn peek(&self, inspect: impl FnMut(usize, &Message<T>)) -> usize {
        let (count, dropped) = self.try_peek(inspect);
        assert!(!dropped.any(), "test subscriber drops: {dropped:?}");
        count
    }

    /// Messages currently available to `messages`. Writer-side publications are
    /// moved into the reader buffer first; the result is bounded by the reader
    /// capacity, so overflow is reported by the consuming calls instead.
    pub fn message_count(&self) -> usize {
        self.subscriber.update();
        self.subscriber.reader_len()
    }

    fn dropped(&self) -> DroppedMessages {
        DroppedMessages {
            writer: self.subscriber.writer_drops(),
            reader: self.subscriber.reader_drops(),
        }
    }
}
