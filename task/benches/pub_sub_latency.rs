//! Benchmarks the round trip of the pub/sub primitive: how long it takes,
//! end to end, for a value loaned and sent on a `Publisher` to become visible
//! to a connected `Subscriber` — i.e. loan -> send -> flush -> drain -> read.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use task::generic_publisher::GenericPublisher;
use task::input::OptionalInput;
use task::output::Output;
use task::publisher::{Publisher, PublisherConfig};
use task::subscriber::{Subscriber, SubscriberConfig};
use task::time::FrameworkTime;

fn bench_publish_to_receive(c: &mut Criterion) {
    let mut publisher: Publisher<u64> = Publisher::new(PublisherConfig {
        capacity: 1,
        channel_name: "bench_channel".into(),
    });
    let mut subscriber: Subscriber<u64> = Subscriber::new(SubscriberConfig {
        is_optional: true,
        capacity: 1,
        is_trigger: true,
        keep_across_runs: true,
        channel_name: "bench_channel".into(),
    });

    publisher.add_typed_subscriber(&mut subscriber);
    publisher.increase_arena_size(subscriber.get_config().capacity);
    publisher.allocate_arena();

    let mut value: u64 = 0;

    let mut group = c.benchmark_group("pub_to_recv");

    group.bench_function("no_downcast", |b| {
        b.iter(|| {
            value = value.wrapping_add(1);

            {
                let mut output = Output::new_default(&mut publisher);
                *output = value;
                output.send();
            }
            publisher.flush_loaned_values(FrameworkTime::from_nanoseconds(value as i64));
            subscriber.drain_writer_to_reader();

            let received = {
                let input = OptionalInput::new(&mut subscriber);
                *input.value().expect("sent value should be readable")
            };
            subscriber.get_read_buffer().pop_front();
            black_box(received)
        });
    });
    group.bench_function("with_downcast", |b| {
        b.iter(|| {
            value = value.wrapping_add(1);

            {
                let mut output = Output::new_downcasted(&mut publisher);
                *output = value;
                output.send();
            }
            publisher.flush_loaned_values(FrameworkTime::from_nanoseconds(value as i64));
            subscriber.drain_writer_to_reader();

            let received: u64 = {
                let input = OptionalInput::new_downcasted(&mut subscriber);
                *input.value().expect("sent value should be readable")
            };
            subscriber.get_read_buffer().pop_front();
            black_box(received)
        });
    });
}

criterion_group!(benches, bench_publish_to_receive);
criterion_main!(benches);
