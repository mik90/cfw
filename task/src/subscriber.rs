use base::arena::ArenaReaderPtr;
use base::double_buffer::{DoubleBuffer, ReadBufferGuard, WriteBufferHandle};

use crate::message::Message;
use crate::wake::{WakeHandle, WakeRegistration};
use std::sync::{Arc, OnceLock};

/// A typed subscriber retaining messages for the storage lifetime.
pub struct Subscriber<'storage, T> {
    buffer: DoubleBuffer<'storage, Message<T>>,
    channel: String,
    wake: WakeRegistration,
    policy: SubscriberPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriberPolicy {
    pub trigger: bool,
    pub keep_across_runs: bool,
}
impl Default for SubscriberPolicy {
    fn default() -> Self {
        Self {
            trigger: true,
            keep_across_runs: true,
        }
    }
}

impl<'storage, T> Subscriber<'storage, T> {
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

    pub(crate) fn writer(&self) -> SubscriberWriter<'storage, T> {
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

    pub fn has_pending(&self) -> bool {
        !self.is_empty() || !self.buffer.write_buffer().is_empty()
    }
    pub fn requests_execution(&self) -> bool {
        self.policy.trigger && !self.buffer.write_buffer().is_empty()
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

    pub fn input(&self) -> Input<'_, 'storage, T> {
        Input {
            guard: self.buffer.read_buffer(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.read_buffer().is_empty()
    }

    pub fn writer_drops(&self) -> usize {
        self.buffer.writer_drops()
    }

    pub fn reader_drops(&self) -> usize {
        self.buffer.read_buffer().drops()
    }
}

pub(crate) struct SubscriberWriter<'storage, T> {
    queue: WriteBufferHandle<'storage, Message<T>>,
    wake: WakeRegistration,
    trigger: bool,
}

impl<'storage, T> SubscriberWriter<'storage, T> {
    pub(crate) fn write(&self, ptr: base::arena::ArenaPtr<'storage, Message<T>>) {
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
pub struct Input<'input, 'storage, T> {
    guard: ReadBufferGuard<'input, 'storage, Message<T>>,
}

impl<'storage, T> Input<'_, 'storage, T> {
    pub fn value(&self) -> Option<&T> {
        self.guard.front().map(|message| &message.message)
    }

    pub fn clear(&mut self) {
        self.guard.pop_front();
    }

    pub fn pop(&mut self) -> Option<ArenaReaderPtr<'storage, Message<T>>> {
        self.guard.pop_front_ptr().map(ArenaReaderPtr::new)
    }

    pub fn drain(&mut self) -> impl Iterator<Item = ArenaReaderPtr<'storage, Message<T>>> + '_ {
        self.guard.drain_contiguous()
    }
}
