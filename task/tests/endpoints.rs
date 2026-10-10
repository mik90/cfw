use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use base::arena::Arena;
use task::message::Message;
use task::time::FrameworkTime;
use task::{
    ForwardedMessage, LoanError, Publisher, PublisherOps, ReplayError, ReplayPublisher, Subscriber,
};

struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn timestamp() -> FrameworkTime {
    FrameworkTime::from_nanoseconds(123)
}

#[test]
fn span_inspection_retains_headers_and_payloads_until_consumed_or_evicted() {
    struct Value(u64, Counted);
    let drops = Arc::new(AtomicUsize::new(0));
    let arena = Arena::new(4);
    {
        let subscriber = Subscriber::new(2);
        let mut publisher = Publisher::new(arena.allocator(), 2);
        publisher.connect(&subscriber);
        for value in 0..2 {
            publisher
                .publish(Value(value, Counted(drops.clone())))
                .unwrap();
        }
        publisher.flush(timestamp());
        subscriber.update();
        for _ in 0..2 {
            let input = subscriber.input();
            assert_eq!(input.len(), 2);
            assert!(!input.is_empty());
            assert_eq!(
                input.inputs().map(|m| m.message.0).collect::<Vec<_>>(),
                [0, 1]
            );
            assert!(input.inputs().all(|m| m.header.published_at == timestamp()));
            assert_eq!(
                input
                    .inputs()
                    .rev()
                    .map(|m| m.message.0)
                    .collect::<Vec<_>>(),
                [1, 0]
            );
        }
        subscriber.finish_iteration();
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        publisher.publish(Value(2, Counted(drops.clone()))).unwrap();
        publisher.flush(FrameworkTime::from_nanoseconds(456));
        subscriber.update();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(subscriber.reader_drops(), 1);
        let mut input = subscriber.input();
        assert_eq!(
            input.inputs().map(|m| m.message.0).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(input.pop().unwrap().message.0, 1);
        assert_eq!(
            input
                .inputs()
                .next()
                .unwrap()
                .header
                .published_at
                .to_nanoseconds(),
            456
        );
        assert_eq!(input.drain().count(), 1);
        assert!(input.is_empty());
    }
    assert_eq!(drops.load(Ordering::Relaxed), 3);
}

#[test]
fn retained_fanout_messages_outlive_endpoints() {
    let drops = Arc::new(AtomicUsize::new(0));
    let arena = Arena::new(1);
    let first = Subscriber::new(1);
    let second = Subscriber::new(1);
    let mut publisher = Publisher::new(arena.allocator(), 1);
    publisher.connect(&first);
    publisher.connect(&second);
    publisher.publish(Counted(drops.clone())).unwrap();
    assert!(first.input().pop().is_none());
    publisher.flush(timestamp());
    drop(publisher);

    first.update();
    second.update();
    let message = first.input().pop().unwrap();
    assert_eq!(message.header.published_at, timestamp());
    drop(first);
    drop(second);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert!(arena.try_allocate_uninit().is_none());
    drop(message);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(arena.try_allocate_uninit().is_some());
}

#[test]
fn loans_are_exclusive_and_cancelled_outputs_release_capacity() {
    let drops = Arc::new(AtomicUsize::new(0));
    let arena = Arena::new(1);
    let mut publisher = Publisher::new(arena.allocator(), 1);
    drop(publisher.loan_uninit().unwrap());
    let output = publisher.loan(Counted(drops.clone())).unwrap();
    assert!(arena.try_allocate_uninit().is_none());
    drop(output);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    publisher.publish(Counted(drops.clone())).unwrap();
    assert!(matches!(
        publisher.loan_uninit(),
        Err(LoanError::LoanCapacityReached)
    ));
    drop(publisher);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    assert!(arena.try_allocate_uninit().is_some());
}

#[test]
fn batch_loans_share_quota_and_publish_in_send_order() {
    let arena = Arena::new(4);
    let subscriber = Subscriber::new(4);
    let mut publisher = Publisher::new(arena.allocator(), 4);
    publisher.connect(&subscriber);
    publisher.publish(10).unwrap();
    {
        let batch = publisher.batch();
        let first = batch.loan_uninit().unwrap();
        let second = batch.loan_uninit().unwrap();
        let cancelled = batch.loan(99).unwrap();
        assert!(matches!(
            batch.loan_uninit(),
            Err(LoanError::LoanCapacityReached)
        ));
        drop(cancelled);
        let mut third = batch.loan_uninit().unwrap();
        third.payload_uninit().write(30);
        // SAFETY: The entire u64 payload was initialized above.
        let third = unsafe { third.assume_init() };
        let mut first = first.write(1);
        let second = second.write(20);
        *first += *second;
        third.send();
        first.send();
        second.send();
        assert!(matches!(
            batch.loan_uninit(),
            Err(LoanError::LoanCapacityReached)
        ));
    }
    assert!(subscriber.input().is_empty());
    publisher.flush(timestamp());
    subscriber.update();
    assert_eq!(
        subscriber
            .input()
            .inputs()
            .map(|m| m.message)
            .collect::<Vec<_>>(),
        [10, 30, 21, 20]
    );
    assert!(
        subscriber
            .input()
            .inputs()
            .all(|m| m.header.published_at == timestamp())
    );
    subscriber.clear();
    publisher.batch().loan(40).unwrap().send();
    publisher.discard_pending();
    assert!(arena.try_allocate_uninit().is_some());
}

#[test]
fn batch_arena_exhaustion_and_unwind_release_unsent_and_cancel_sent_outputs() {
    let drops = Arc::new(AtomicUsize::new(0));
    let arena = Arena::new(2);
    let mut publisher = Publisher::new(arena.allocator(), 3);
    {
        let batch = publisher.batch();
        let first = batch.loan_uninit().unwrap();
        let second = batch.loan(Counted(drops.clone())).unwrap();
        assert!(matches!(
            batch.loan_uninit(),
            Err(LoanError::ArenaExhausted)
        ));
        drop(first);
        drop(batch.loan_uninit().unwrap());
        second.send();
    }
    publisher.discard_pending();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let batch = publisher.batch();
        let _unsent = batch.loan(Counted(drops.clone())).unwrap();
        batch.loan(Counted(drops.clone())).unwrap().send();
        panic!("callback initialization failed");
    }));
    assert!(failure.is_err());
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    publisher.discard_pending();
    assert_eq!(drops.load(Ordering::Relaxed), 3);
    let batch = publisher.batch();
    let _first = batch.loan_uninit().unwrap();
    let _second = batch.loan_uninit().unwrap();
}

#[test]
fn initialization_panic_releases_reservation_and_publisher_borrow() {
    let arena = Arena::<Message<u64>>::new(1);
    let subscriber = Subscriber::new(1);
    let mut publisher = Publisher::new(arena.allocator(), 1);
    publisher.connect(&subscriber);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut loan = publisher.loan_uninit().unwrap();
        loan.payload_uninit().write(7);
        panic!("initialization failed");
    }));
    assert!(result.is_err());
    let mut output = publisher.loan(20).unwrap();
    *output += 22;
    output.send();
    publisher.flush(timestamp());
    subscriber.update();
    assert_eq!(subscriber.input().pop().unwrap().message, 42);
}

#[test]
fn forwarded_messages_retain_source_storage_through_destination() {
    let source_drops = Arc::new(AtomicUsize::new(0));
    let destination_drops = Arc::new(AtomicUsize::new(0));
    // Source storage outlives destination storage containing borrowed sources.
    let source_arena = Arena::new(1);
    let destination_arena = Arena::new(1);
    let source_subscriber = Subscriber::new(1);
    let destination_subscriber = Subscriber::new(1);
    let mut source = Publisher::new(source_arena.allocator(), 1);
    let mut destination = Publisher::new(destination_arena.allocator(), 1);
    source.connect(&source_subscriber);
    destination.connect(&destination_subscriber);

    source.publish(Counted(source_drops.clone())).unwrap();
    source.flush(timestamp());
    source_subscriber.update();
    let forwarded = source_subscriber.input().pop().unwrap();
    destination
        .publish(ForwardedMessage::new(
            Counted(destination_drops.clone()),
            forwarded,
        ))
        .unwrap();
    // Type erasure accepts a publisher whose payload itself contains a borrow.
    let erased: &mut dyn PublisherOps = &mut destination;
    erased.flush(timestamp());
    drop(source);
    drop(destination);
    drop(source_subscriber);
    destination_subscriber.update();
    let retained = destination_subscriber.input().pop().unwrap();
    drop(destination_subscriber);
    assert_eq!(retained.message.forwarded.header.published_at, timestamp());
    assert!(source_arena.try_allocate_uninit().is_none());
    assert!(destination_arena.try_allocate_uninit().is_none());
    drop(retained);
    assert_eq!(source_drops.load(Ordering::Relaxed), 1);
    assert_eq!(destination_drops.load(Ordering::Relaxed), 1);
    assert!(source_arena.try_allocate_uninit().is_some());
    assert!(destination_arena.try_allocate_uninit().is_some());
}

#[test]
fn erased_publishers_keep_storage_borrows_and_replay_checks_only_payloads() {
    let numbers = Arena::new(1);
    let strings = Arena::new(1);
    let number_sub = Subscriber::new(1);
    let string_sub = Subscriber::new(1);
    let mut number_pub = Publisher::new(numbers.allocator(), 1);
    let mut string_pub = Publisher::new(strings.allocator(), 1);
    number_pub.connect(&number_sub);
    string_pub.connect(&string_sub);
    number_pub.publish(42_u64).unwrap();
    string_pub.publish(String::from("hello")).unwrap();
    let mut publishers: Vec<Box<dyn PublisherOps + '_>> =
        vec![Box::new(number_pub), Box::new(string_pub)];
    for publisher in &mut publishers {
        publisher.flush(timestamp());
    }
    drop(publishers);
    number_sub.update();
    string_sub.update();
    assert_eq!(number_sub.input().pop().unwrap().message, 42);
    assert_eq!(string_sub.input().pop().unwrap().message, "hello");

    let mut publisher = Publisher::<u64>::new(numbers.allocator(), 1);
    publisher.connect(&number_sub);
    let replay: &mut dyn ReplayPublisher = &mut publisher;
    match replay.publish_boxed(Box::new(String::from("wrong type"))) {
        Err(ReplayError::TypeMismatch(value)) => {
            assert_eq!(*value.downcast::<String>().unwrap(), "wrong type")
        }
        _ => panic!("expected a recoverable type mismatch"),
    }
    assert!(replay.publish_boxed(Box::new(10_u64)).is_ok());
    let retry = match replay.publish_boxed(Box::new(20_u64)) {
        Err(ReplayError::Loan {
            reason: LoanError::LoanCapacityReached,
            value,
        }) => value,
        _ => panic!("expected a recoverable loan-capacity error"),
    };
    replay.flush(timestamp());
    // Capacity is still retained by the subscriber, rather than by pending loans.
    let retry = match replay.publish_boxed(retry) {
        Err(ReplayError::Loan {
            reason: LoanError::ArenaExhausted,
            value,
        }) => value,
        _ => panic!("expected subscriber-retained storage to remain occupied"),
    };
    number_sub.update();
    assert_eq!(number_sub.input().pop().unwrap().message, 10);
    assert!(replay.publish_boxed(retry).is_ok());
    replay.flush(timestamp());
    number_sub.update();
    assert_eq!(number_sub.input().pop().unwrap().message, 20);
}

#[test]
fn worker_allocates_while_main_thread_consumes_and_requests_stop() {
    let arena = Arena::new(2);
    let subscriber = Subscriber::new(1);
    let mut publisher = Publisher::new(arena.allocator(), 1);
    publisher.connect(&subscriber);
    let (published, publications) = std::sync::mpsc::channel();
    let (control, controls) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut next = 0_u64;
            while controls.recv().unwrap() {
                publisher.publish(next).unwrap();
                publisher.flush(timestamp());
                published.send(()).unwrap();
                next += 1;
            }
        });
        for expected in 0..32 {
            control.send(true).unwrap();
            publications.recv().unwrap();
            subscriber.update();
            let message = subscriber.input().pop().unwrap();
            assert_eq!(message.message, expected);
            // Allocation on the main thread can coexist with worker allocation.
            let spare = arena.allocate_uninit();
            assert!(arena.try_allocate_uninit().is_none());
            drop((spare, message));
        }
        // Models the control loop receiving an application shutdown signal.
        control.send(false).unwrap();
    });
    drop(subscriber);
    let first = arena.allocate_uninit();
    let second = arena.allocate_uninit();
    assert!(arena.try_allocate_uninit().is_none());
    drop((first, second));
}

#[test]
fn worker_panic_leaves_published_messages_valid_and_releases_pending_loans() {
    let drops = Arc::new(AtomicUsize::new(0));
    let arena = Arena::new(2);
    let subscriber = Subscriber::new(1);
    let mut publisher = Publisher::new(arena.allocator(), 1);
    publisher.connect(&subscriber);
    std::thread::scope(|scope| {
        let drops = drops.clone();
        let worker = scope.spawn(move || {
            publisher.publish(Counted(drops.clone())).unwrap();
            publisher.flush(timestamp());
            publisher.publish(Counted(drops)).unwrap();
            panic!("worker failed with a pending loan");
        });
        assert!(worker.join().is_err());
    });
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    subscriber.update();
    let retained = subscriber.input().pop().unwrap();
    assert_eq!(retained.header.published_at, timestamp());
    drop(subscriber);
    drop(retained);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}
