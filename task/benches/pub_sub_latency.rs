//! End-to-end typed loan -> send -> flush -> drain -> read latency.
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use task::time::FrameworkTime;
use task::{ChannelPlan, GraphPlan};

fn bench_publish_to_receive(c: &mut Criterion) {
    let mut plan = ChannelPlan::<u64>::new("bench_channel");
    let publisher = plan.publisher(1);
    let subscriber = plan.subscriber(1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut publisher = bindings.take_publisher(&publisher).unwrap();
    let subscriber = bindings.take_subscriber(&subscriber).unwrap();
    let mut value = 0_u64;
    c.bench_function("pub_to_recv/typed", |b| {
        b.iter(|| {
            value = value.wrapping_add(1);
            publisher.loan(value).unwrap().send();
            publisher.flush(FrameworkTime::from_nanoseconds(value as i64));
            subscriber.update();
            black_box(subscriber.input().pop().unwrap().message)
        })
    });
}

criterion_group!(benches, bench_publish_to_receive);
criterion_main!(benches);
