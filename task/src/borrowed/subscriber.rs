use base::arena::ArenaReaderPtr;
use base::double_buffer::{DoubleBuffer, ReadBufferGuard, WriteBufferHandle};

use crate::message::Message;

/// A typed subscriber retaining messages for the storage lifetime.
pub struct Subscriber<'storage, T> {
    buffer: DoubleBuffer<'storage, Message<T>>,
}

impl<'storage, T> Subscriber<'storage, T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: DoubleBuffer::new(capacity),
        }
    }

    pub(crate) fn writer(&self) -> WriteBufferHandle<'storage, Message<T>> {
        self.buffer.write_buffer()
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

    pub fn writer_drops(&self) -> usize {
        self.buffer.writer_drops()
    }
}

/// The buffer guard's borrow is independent of retained message lifetimes.
pub struct Input<'input, 'storage, T> {
    guard: ReadBufferGuard<'input, 'storage, Message<T>>,
}

impl<'storage, T> Input<'_, 'storage, T> {
    pub fn pop(&mut self) -> Option<ArenaReaderPtr<'storage, Message<T>>> {
        self.guard.pop_front_ptr().map(ArenaReaderPtr::new)
    }

    pub fn drain(&mut self) -> impl Iterator<Item = ArenaReaderPtr<'storage, Message<T>>> + '_ {
        self.guard.drain_contiguous()
    }
}
