use base::arena::ArenaReaderPtr;
use base::double_buffer::{DoubleBuffer, ReadBufferGuard, WriteBufferHandle};

use crate::message::{Message, MessageHeader};
use crate::wake::{WakeHandle, WakeRegistration};
use std::sync::{Arc, OnceLock};

/// A typed subscriber retaining messages for the storage lifetime.
pub struct Subscriber<'graph_storage, MessageType> {
    buffer: DoubleBuffer<'graph_storage, Message<MessageType>>,
    channel: String,
    wake: WakeRegistration,
    policy: SubscriberPolicy,
}

impl<'graph_storage, MessageType> std::fmt::Debug for Subscriber<'graph_storage, MessageType> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscriber")
            .field(
                "each_buffer_capacity",
                &self.buffer.single_buffer_capacity(),
            )
            .field("channel", &self.channel)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriberPolicy {
    pub trigger: bool,
    pub keep_across_runs: bool,
}
impl Default for SubscriberPolicy {
    fn default() -> Self {
        Self {
            trigger: false,
            keep_across_runs: true,
        }
    }
}

impl<'graph_storage, MessageType> Subscriber<'graph_storage, MessageType> {
    pub fn new(capacity: usize) -> Self {
        Self::with_policy(capacity, SubscriberPolicy::default())
    }
    pub fn with_policy(capacity: usize, policy: SubscriberPolicy) -> Self {
        Self {
            buffer: DoubleBuffer::new(capacity),
            channel: String::new(),
            wake: Arc::new(OnceLock::new()),
            policy,
        }
    }

    pub(crate) fn writer(&self) -> SubscriberWriter<'graph_storage, MessageType> {
        SubscriberWriter {
            queue: self.buffer.write_buffer(),
            wake: self.wake.clone(),
            trigger: self.policy.trigger,
        }
    }

    pub(crate) fn set_channel_name(&mut self, name: &str) {
        self.channel = name.into();
    }

    pub fn channel_name(&self) -> &str {
        &self.channel
    }

    pub fn set_waker(&mut self, wake: WakeHandle) {
        assert!(
            self.wake.set(wake).is_ok(),
            "subscriber already attached to an executor"
        );
    }

    /// Whether the read buffer or write buffer has elements
    pub fn has_pending(&self) -> bool {
        !self.buffer.read_buffer().is_empty() || !self.buffer.write_buffer().is_empty()
    }

    pub fn requests_execution(&self) -> bool {
        self.policy.trigger && !self.buffer.write_buffer().is_empty()
    }

    /// Whether this input can request execution when new messages arrive.
    pub fn triggers_execution(&self) -> bool {
        self.policy.trigger
    }

    pub fn finish_iteration(&self) {
        if !self.policy.keep_across_runs {
            let mut guard = self.buffer.read_buffer();
            while !guard.is_empty() {
                guard.pop_front();
            }
        }
    }

    /// Move pending publications into the bounded read buffer.
    pub fn update(&self) {
        self.buffer.drain_writer_to_reader();
    }

    pub fn clear(&self) {
        self.buffer.clear();
    }

    pub fn input_readonly(&self) -> Input<'_, 'graph_storage, MessageType, crate::input::ReadOnly> {
        Input {
            guard: self.buffer.read_buffer(),
            capability: std::marker::PhantomData,
        }
    }

    pub fn input_forwardable(
        &self,
    ) -> Input<'_, 'graph_storage, MessageType, crate::input::Forwardable> {
        Input {
            guard: self.buffer.read_buffer(),
            capability: std::marker::PhantomData,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.read_buffer().is_empty()
    }

    /// How many messages were dropped on the writer side and never got a chance to be
    /// exposed to the callback.
    /// These may occur if a callback isn't able to be run enough due to either triggering semantics
    /// or a lack of available CPU time.
    pub fn writer_drops(&self) -> usize {
        self.buffer.writer_drops()
    }

    /// How many messages were dropped on the reader buffer but had a chance to be exposed to the callback.
    /// These are not generally concerning if `keep_across_runs` is false, since writer buffer data will overwrite
    /// reader buffer data whenever the callback comes in.
    pub fn reader_drops(&self) -> usize {
        self.buffer.read_buffer().drops()
    }

    pub fn visit_headers(&self, mut visit: impl FnMut(crate::message::MessageHeader)) {
        for message in self.buffer.read_buffer().as_slice() {
            visit(message.header);
        }
    }
}

/// Allows publishers to write to a subscriber
pub(crate) struct SubscriberWriter<'graph_storage, T> {
    queue: WriteBufferHandle<'graph_storage, Message<T>>,
    wake: WakeRegistration,
    trigger: bool,
}

impl<'graph_storage, T> std::fmt::Debug for SubscriberWriter<'graph_storage, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscriberWriter")
            .field("queue_length", &self.queue.len())
            .field("trigger", &self.trigger)
            .finish_non_exhaustive()
    }
}

impl<'graph_storage, T> SubscriberWriter<'graph_storage, T> {
    pub(crate) fn write(&self, ptr: base::arena::ArenaPtr<'graph_storage, Message<T>>) {
        self.queue.write(ptr);
        if let Some(wake) = self.wake.get() {
            if self.trigger {
                wake.wake();
            } else {
                wake.readiness_changed();
            }
        }
    }
}

/// The buffer guard's borrow is independent of retained message lifetimes.
pub struct Input<'input, 'graph_storage, MessageType, Modifiers = crate::input::ReadOnly> {
    guard: ReadBufferGuard<'input, 'graph_storage, Message<MessageType>>,
    capability: std::marker::PhantomData<Modifiers>,
}

impl<'graph_storage, MessageType, Modifiers> Input<'_, 'graph_storage, MessageType, Modifiers> {
    pub fn full_message(&self) -> Option<&Message<MessageType>> {
        self.guard.front()
    }

    pub fn value(&self) -> Option<&MessageType> {
        self.full_message().map(|message| &message.message)
    }

    pub fn header(&self) -> Option<&MessageHeader> {
        self.full_message().map(|message| &message.header)
    }

    /// Inspect all prepared messages, including headers, without consuming them.
    /// References cannot outlive this view or overlap a consuming operation.
    pub fn inputs(
        &self,
    ) -> impl ExactSizeIterator<Item = &Message<MessageType>> + DoubleEndedIterator {
        self.guard.iter()
    }

    pub fn len(&self) -> usize {
        self.guard.len()
    }

    pub fn is_empty(&self) -> bool {
        self.guard.is_empty()
    }

    pub fn clear(&mut self) {
        self.guard.pop_front();
    }
}

impl<'input, MessageType> Input<'input, '_, MessageType, crate::input::ReadOnly> {
    /// Consumed handles are limited to the input borrow. Use Forwardable to
    /// transfer ownership into a storage-lived output.
    pub fn pop(&mut self) -> Option<ArenaReaderPtr<'input, Message<MessageType>>> {
        self.guard.pop_front_ptr().map(ArenaReaderPtr::new)
    }

    #[expect(
        clippy::map_identity,
        reason = "The return annotation shortens each handle to the input borrow"
    )]
    pub fn drain(
        &mut self,
    ) -> impl Iterator<Item = ArenaReaderPtr<'input, Message<MessageType>>> + '_ {
        self.guard
            .drain_contiguous()
            .map(|ptr| -> ArenaReaderPtr<'input, Message<MessageType>> { ptr })
    }
}

impl<'graph_storage, MessageType>
    Input<'_, 'graph_storage, MessageType, crate::input::Forwardable>
{
    pub fn pop(&mut self) -> Option<ArenaReaderPtr<'graph_storage, Message<MessageType>>> {
        self.guard.pop_front_ptr().map(ArenaReaderPtr::new)
    }

    pub fn drain(
        &mut self,
    ) -> impl Iterator<Item = ArenaReaderPtr<'graph_storage, Message<MessageType>>> + '_ {
        self.guard.drain_contiguous()
    }

    /// Consume a prepared message, retaining its source arena slot.
    pub fn forward(&mut self) -> Option<ArenaReaderPtr<'graph_storage, Message<MessageType>>> {
        self.pop()
    }

    pub fn forward_all(
        &mut self,
    ) -> impl Iterator<Item = ArenaReaderPtr<'graph_storage, Message<MessageType>>> + '_ {
        self.drain()
    }
}
