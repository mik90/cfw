use super::*;
use crate::arena::Arena;

// A publisher borrows external storage and can be destroyed independently
// of the subscriber queues retaining its messages.
struct Publisher<'arena, T> {
    arena: &'arena Arena<T>,
    output: WriteBufferHandle<'arena, T>,
}

impl<T> Publisher<'_, T> {
    fn publish(&self, value: T) {
        let mut loan = self.arena.allocate_uninit();
        loan.payload_uninit().write(value);
        // SAFETY: The payload is fully initialized above.
        self.output.write(unsafe { loan.assume_init() });
    }
}

#[test]
fn queued_message_outlives_publisher() {
    let arena = Arena::new(1);
    let subscriber = DoubleBuffer::new(1);
    let publisher = Publisher {
        arena: &arena,
        output: subscriber.write_buffer(),
    };
    publisher.publish(42);
    drop(publisher);
    assert!(arena.try_allocate_uninit().is_none());

    subscriber.drain_writer_to_reader();
    let message = ArenaReaderPtr::new(subscriber.read_buffer().pop_front_ptr().unwrap());
    drop(subscriber);
    assert_eq!(*message, 42);
    assert!(arena.try_allocate_uninit().is_none());
    drop(message);
    assert!(arena.try_allocate_uninit().is_some());
}

#[test]
fn overflow_and_scoped_worker_shutdown_return_slots() {
    let arena = Arena::new(2);
    let subscriber = DoubleBuffer::new(1);
    let publisher = Publisher {
        arena: &arena,
        output: subscriber.write_buffer(),
    };
    publisher.publish(10);
    publisher.publish(20);
    assert_eq!(subscriber.writer_drops(), 1);
    let reservation = arena.allocate_uninit();
    assert!(arena.try_allocate_uninit().is_none());
    drop(reservation);

    let (stop, stopped) = std::sync::mpsc::channel();
    let (ready, started) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            subscriber.drain_writer_to_reader();
            assert_eq!(subscriber.read_buffer().front(), Some(&20));
            ready.send(()).unwrap();
            stopped.recv().unwrap();
            // Normal destruction releases messages retained in the read queue.
        });
        started.recv().unwrap();
        publisher.publish(30);
        drop(publisher);
        assert!(arena.try_allocate_uninit().is_none());
        // The main thread models a signal handler requesting shutdown.
        stop.send(()).unwrap();
    });

    // Both the read queue and the pending write queue have been dropped.
    let first = arena.allocate_uninit();
    let second = arena.allocate_uninit();
    assert!(arena.try_allocate_uninit().is_none());
    drop((first, second));
}
