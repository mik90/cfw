use crate::arena::{ArenaPtr, ArenaReaderPtr};
use crate::mpsc_queue::MpscQueue;
use std::cell::{RefCell, RefMut};
use std::collections::VecDeque;
use std::sync::Arc;

pub(crate) struct Buffer<'arena, T> {
    pub storage: VecDeque<ArenaPtr<'arena, T>>,
    pub drops: usize,
}

pub struct WriteBufferHandle<'arena, T> {
    queue: Arc<MpscQueue<ArenaPtr<'arena, T>>>,
}

impl<'arena, T> WriteBufferHandle<'arena, T> {
    pub fn write(&self, element: ArenaPtr<'arena, T>) {
        self.queue.push(element);
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }
}

pub struct ReadBufferGuard<'a, 'arena, T> {
    buffer: RefMut<'a, Buffer<'arena, T>>,
}

// TODO should this be partially specialized on MaybeUninit and auto-handle safety?
impl<'a, 'arena, T> ReadBufferGuard<'a, 'arena, T> {
    pub fn pop_front(&mut self) {
        self.buffer.storage.pop_front();
    }

    pub fn pop_front_ptr(&mut self) -> Option<ArenaPtr<'arena, T>> {
        self.buffer.storage.pop_front()
    }

    pub fn pop_back(&mut self) {
        self.buffer.storage.pop_back();
    }

    pub fn front(&'a self) -> Option<&'a T> {
        self.buffer.storage.front().map(|ptr|
            // SAFETY: We can assume that read buffers are only viewing already-initialized data
            unsafe {
                ptr.assume_init_ref()
            })
    }

    pub fn len(&self) -> usize {
        self.buffer.storage.len()
    }

    /// Mut because it makes the slice contiguous
    pub fn as_slice(&mut self) -> impl Iterator<Item = &T> {
        self.buffer.storage.make_contiguous().iter().map(|ptr|
                // SAFETY: We can assume that messages in read buffers are already initialized
                unsafe { ptr.assume_init_ref() })
    }

    /// Mut because it makes the slice contiguous
    pub fn drain_contiguous(&mut self) -> impl Iterator<Item = ArenaReaderPtr<'arena, T>> {
        // Shuffle to get things in order before we give everything to the consumer
        self.buffer.storage.make_contiguous();

        self.buffer
            .storage
            .drain(..)
            .map(|ptr| ArenaReaderPtr::new(ptr))
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.storage.is_empty()
    }

    /// How many entries have been displaced from this read buffer (due to overflow)
    /// since it was created.
    pub fn drops(&self) -> usize {
        self.buffer.drops
    }
}

pub struct DoubleBuffer<'arena, T> {
    write_queue: Arc<MpscQueue<ArenaPtr<'arena, T>>>,
    // No lock needed: read_buffer is only accessed during drain (before task runs)
    // or by the task itself — never concurrently.
    read_buffer: RefCell<Buffer<'arena, T>>,
}

impl<'arena, T> DoubleBuffer<'arena, T> {
    pub fn new(capacity: usize) -> Self {
        DoubleBuffer {
            write_queue: Arc::new(MpscQueue::new(capacity)),
            read_buffer: RefCell::new(Buffer {
                storage: VecDeque::with_capacity(capacity),
                drops: 0,
            }),
        }
    }

    /// How many elements have been displaced from the write queue (due to overflow —
    /// the consumer didn't drain often enough to keep up) since it was created.
    pub fn writer_drops(&self) -> usize {
        self.write_queue.dropped()
    }

    pub fn write_buffer(&self) -> WriteBufferHandle<'arena, T> {
        WriteBufferHandle {
            queue: self.write_queue.clone(),
        }
    }

    pub fn read_buffer(&self) -> ReadBufferGuard<'_, 'arena, T> {
        ReadBufferGuard {
            buffer: self.read_buffer.borrow_mut(),
        }
    }

    pub fn drain_writer_to_reader(&self) {
        // Snapshot the count before draining so items pushed during drain
        // are left for the next cycle rather than causing an infinite loop.
        let n = self.write_queue.len();
        let mut read = self.read_buffer.borrow_mut();
        for _ in 0..n {
            if let Some(v) = self.write_queue.pop() {
                while read.storage.len() >= read.storage.capacity() {
                    read.drops += 1;
                    read.storage.pop_front();
                }
                read.storage.push_back(v);
            }
        }
    }

    /// Clear both queues. Must be called before Arenas are dropped to ensure
    /// ArenaPtrs don't outlive their Arena.
    pub fn clear(&self) {
        while self.write_queue.pop().is_some() {}
        self.read_buffer.borrow_mut().storage.clear();
    }
}
