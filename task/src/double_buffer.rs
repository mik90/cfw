use crate::mpsc_queue::MpscQueue;
use std::cell::{RefCell, RefMut};
use std::collections::VecDeque;
use std::sync::Arc;

pub(crate) struct Buffer<T> {
    pub storage: VecDeque<T>,
    pub drops: usize,
}

pub struct WriteBufferHandle<T> {
    queue: Arc<MpscQueue<T>>,
}

impl<T> WriteBufferHandle<T> {
    pub fn write(&self, element: T) {
        self.queue.push(element);
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }
}

pub struct ReadBufferGuard<'a, T> {
    buffer: RefMut<'a, Buffer<T>>,
}

impl<'a, T> ReadBufferGuard<'a, T> {
    pub fn pop_front(&mut self) {
        self.buffer.storage.pop_front();
    }

    pub fn pop_back(&mut self) {
        self.buffer.storage.pop_back();
    }

    pub fn front(&'a self) -> Option<&'a T> {
        self.buffer.storage.front()
    }

    pub fn len(&self) -> usize {
        self.buffer.storage.len()
    }

    pub fn as_slice(&mut self) -> &[T] {
        self.buffer.storage.make_contiguous()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.storage.is_empty()
    }
}

pub(crate) struct DoubleBuffer<T> {
    write_queue: Arc<MpscQueue<T>>,
    // No lock needed: read_buffer is only accessed during drain (before task runs)
    // or by the task itself — never concurrently.
    read_buffer: RefCell<Buffer<T>>,
    overwrite_reader_buffer_values: bool,
}

impl<T> DoubleBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        DoubleBuffer {
            write_queue: Arc::new(MpscQueue::new(capacity)),
            read_buffer: RefCell::new(Buffer {
                storage: VecDeque::with_capacity(capacity),
                drops: 0,
            }),
            overwrite_reader_buffer_values: true,
        }
    }

    pub fn set_should_overwrite_reader_buffer(&mut self, overwrite_reader_buffer_values: bool) {
        self.overwrite_reader_buffer_values = overwrite_reader_buffer_values;
    }

    pub fn get_write_buffer(&self) -> WriteBufferHandle<T> {
        WriteBufferHandle {
            queue: self.write_queue.clone(),
        }
    }

    pub fn get_read_buffer(&self) -> ReadBufferGuard<'_, T> {
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
