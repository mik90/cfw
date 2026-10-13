use std::{
    any::TypeId,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use task::{
    ArenaReaderPtr, Forwardable, Forwarded, ForwardedSources as _, OutputUninit, RequiredInput,
    message::Message, time::FrameworkTime,
};
use task_graph::{CallbackSchedule, NamedPlan, TaskRegistration};
use task_macros::{ForwardedSources, task_callback};

struct Tracked {
    value: u64,
    drops: Arc<AtomicUsize>,
}
impl Drop for Tracked {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(ForwardedSources)]
struct Sources<'s> {
    left: ArenaReaderPtr<'s, Message<Tracked>>,
    right: ArenaReaderPtr<'s, Message<Tracked>>,
    scale: ArenaReaderPtr<'s, Message<u32>>,
}
struct Merge;
#[task_callback]
impl Merge {
    fn run<'s>(
        &self,
        mut a: RequiredInput<'_, 's, Tracked, Forwardable>,
        mut b: RequiredInput<'_, 's, Tracked, Forwardable>,
        mut c: RequiredInput<'_, 's, u32, Forwardable>,
        #[forwards(left = a, right = b, scale = c)] output: OutputUninit<
            '_,
            's,
            Forwarded<u64, Sources<'s>>,
        >,
    ) {
        let sources = Sources {
            left: a.forward().unwrap(),
            right: b.forward().unwrap(),
            scale: c.forward().unwrap(),
        };
        let value = (sources.left.message.value + sources.right.message.value)
            * u64::from(sources.scale.message);
        output.write(Forwarded::new(value, sources)).send();
    }
}

#[test]
fn named_bundle_budgets_channels_and_repeated_fields_and_drops_sources() {
    for shared in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut plan = NamedPlan::default();
        let mut task = TaskRegistration::new("merge", Merge, CallbackSchedule::default());
        if shared {
            task.input_channel("b", "a");
        }
        let task = task.register(&mut plan).unwrap();
        let a = plan.native::<Tracked>("a").unwrap().publisher(1);
        let b = if shared {
            None
        } else {
            Some(plan.native::<Tracked>("b").unwrap().publisher(1))
        };
        let c = plan.native::<u32>("c").unwrap().publisher(1);
        let unrelated = plan.native::<Tracked>("unrelated").unwrap().publisher(1);
        let capture = plan
            .bundle::<u64, Sources<'static>>("output")
            .unwrap()
            .subscriber(32);
        plan.prepare().unwrap();
        assert_eq!(
            plan.native::<Tracked>("unrelated")
                .unwrap()
                .publisher_capacity(&unrelated)
                .unwrap(),
            1
        );
        assert_eq!(
            plan.native::<Tracked>("a")
                .unwrap()
                .publisher_capacity(&a)
                .unwrap(),
            if shared { 139 } else { 70 }
        );
        plan.prepare().unwrap(); // Replanning must not accumulate previous budgets.
        assert_eq!(
            plan.native::<Tracked>("a")
                .unwrap()
                .publisher_capacity(&a)
                .unwrap(),
            if shared { 139 } else { 70 }
        );
        assert_eq!(plan.topology().forwarding.len(), 3);
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        assert!(
            bindings
                .native::<Forwarded<u64, Sources<'static>>>("output")
                .is_err()
        );
        let mut a = bindings
            .native::<Tracked>("a")
            .unwrap()
            .take_publisher(&a)
            .unwrap();
        let mut b = b.map(|key| {
            bindings
                .native::<Tracked>("b")
                .unwrap()
                .take_publisher(&key)
                .unwrap()
        });
        let mut c = bindings
            .native::<u32>("c")
            .unwrap()
            .take_publisher(&c)
            .unwrap();
        let capture = bindings
            .bundle::<u64, Sources<'static>>("output")
            .unwrap()
            .take_subscriber(&task_graph::bundle::subscriber_key::<u64, Sources<'static>>(capture))
            .unwrap();
        let mut graph = storage.graph_builder();
        task.add_to_graph(&mut graph, &bindings);
        let mut graph = testing::SequentialExecutor::new(graph.build().unwrap());
        for value in 0..32 {
            let now = FrameworkTime::from_nanoseconds(value as i64);
            a.publish(Tracked {
                value,
                drops: drops.clone(),
            })
            .unwrap();
            a.flush(now);
            if let Some(b) = &mut b {
                b.publish(Tracked {
                    value,
                    drops: drops.clone(),
                })
                .unwrap();
                b.flush(now);
            }
            c.publish(2).unwrap();
            c.flush(now);
            graph.step(now).unwrap();
            capture.update();
        }
        let retained: Vec<_> = capture.input_forwardable().drain().collect();
        assert_eq!(retained.len(), 32);
        drop((graph, a, b, c, capture, bindings));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let rebound = storage.bind().unwrap();
        assert!(rebound.bundle::<u64, Sources<'static>>("output").is_ok());
        drop(rebound);
        for (index, value) in retained.iter().enumerate() {
            assert_eq!(value.message.message, index as u64 * 4);
        }
        drop(retained);
        assert_eq!(drops.load(Ordering::SeqCst), if shared { 32 } else { 64 });
    }
}

#[derive(ForwardedSources)]
struct Nine<'s> {
    a: ArenaReaderPtr<'s, Message<u64>>,
    b: ArenaReaderPtr<'s, Message<u32>>,
    c: ArenaReaderPtr<'s, Message<u64>>,
    d: ArenaReaderPtr<'s, Message<u32>>,
    e: ArenaReaderPtr<'s, Message<u64>>,
    f: ArenaReaderPtr<'s, Message<u32>>,
    g: ArenaReaderPtr<'s, Message<u64>>,
    h: ArenaReaderPtr<'s, Message<u32>>,
    i: ArenaReaderPtr<'s, Message<u64>>,
}
#[test]
fn derive_has_no_eight_field_limit_and_validates_all_mappings() {
    assert_eq!(Nine::fields().len(), 9);
    let mut plan = NamedPlan::default();
    let fields = Nine::fields();
    let mapping: Vec<_> = fields
        .iter()
        .map(|(name, ty)| (*name, *name, *name, *ty))
        .collect();
    plan.map_forwarding::<Nine<'static>>("out", "out", &mapping)
        .unwrap();
    assert!(
        plan.map_forwarding::<Nine<'static>>("out", "out", &mapping[..8])
            .is_err()
    );
    let mut bad = mapping.clone();
    bad[8] = bad[0];
    assert!(
        plan.map_forwarding::<Nine<'static>>("out", "out", &bad)
            .is_err()
    );
    let mut bad = mapping.clone();
    bad[0].3 = TypeId::of::<u32>();
    assert!(
        plan.map_forwarding::<Nine<'static>>("out", "out", &bad)
            .is_err()
    );
    use task::forwarded_message::SerializableForward;
    let mut a = task::ChannelPlan::<u64>::new("a");
    let pa = a.publisher(1);
    let sa = a.subscriber(1);
    let mut b = task::ChannelPlan::<u32>::new("b");
    let pb = b.publisher(1);
    let sb = b.subscriber(1);
    let storage = task::GraphPlan::new((a, b)).allocate().unwrap();
    let a = storage.channels().0.build();
    let b = storage.channels().1.build();
    let mut pa = a.take_publisher(&pa).unwrap();
    let sa = a.take_subscriber(&sa).unwrap();
    let mut pb = b.take_publisher(&pb).unwrap();
    let sb = b.take_subscriber(&sb).unwrap();
    pa.publish(42).unwrap();
    pa.flush(FrameworkTime::from_nanoseconds(0));
    sa.update();
    pb.publish(7).unwrap();
    pb.flush(FrameworkTime::from_nanoseconds(0));
    sb.update();
    let a = sa.input_forwardable().pop().unwrap();
    let b = sb.input_forwardable().pop().unwrap();
    let nine = Nine {
        a: a.clone(),
        b: b.clone(),
        c: a.clone(),
        d: b.clone(),
        e: a.clone(),
        f: b.clone(),
        g: a.clone(),
        h: b.clone(),
        i: a.clone(),
    };
    let headers: Vec<_> = nine
        .headers()
        .into_iter()
        .map(|(name, header)| (name.to_owned(), header))
        .collect();
    let a = task::loggable::ReplayMessageLog::new(vec![a]);
    let b = task::loggable::ReplayMessageLog::new(vec![b]);
    let context = (
        &a as &dyn task::loggable::MessageLog<'_, u64>,
        &b as &dyn task::loggable::MessageLog<'_, u32>,
        &a as _,
        &b as _,
        &a as _,
        &b as _,
        &a as _,
        &b as _,
        &a as _,
    );
    let restored = Nine::resolve(&headers, context).unwrap();
    assert_eq!(restored.i.message, 42);
    assert_eq!(restored.h.message, 7);
    assert!(Nine::resolve(&headers[..8], context).is_err());
    let mut invalid = headers.clone();
    invalid[0].0 = "unknown".into();
    assert!(Nine::resolve(&invalid, context).is_err());
    invalid = headers;
    invalid[8].1.batch_index = 99;
    assert!(Nine::resolve(&invalid, context).is_err());
}

#[derive(ForwardedSources)]
struct Nested<'s> {
    previous: ArenaReaderPtr<'s, Message<Forwarded<u64, Sources<'s>>>>,
}
struct Relay;
#[task_callback]
impl Relay {
    fn run<'s>(
        &self,
        mut output: RequiredInput<'_, 's, Forwarded<u64, Sources<'s>>, Forwardable>,
        #[forwards(previous = output)] last: OutputUninit<'_, 's, Forwarded<bool, Nested<'s>>>,
    ) {
        last.write(Forwarded::new(
            true,
            Nested {
                previous: output.forward().unwrap(),
            },
        ))
        .send();
    }
}
#[test]
fn nested_named_bundle_retention_propagates_to_original_sources() {
    let mut plan = NamedPlan::default();
    let merge = TaskRegistration::new("merge", Merge, CallbackSchedule::default())
        .register(&mut plan)
        .unwrap();
    let relay = TaskRegistration::new("relay", Relay, CallbackSchedule::default())
        .register(&mut plan)
        .unwrap();
    let a = plan.native::<Tracked>("a").unwrap().publisher(1);
    let b = plan.native::<Tracked>("b").unwrap().publisher(1);
    let c = plan.native::<u32>("c").unwrap().publisher(1);
    let last = plan
        .bundle::<bool, Nested<'static>>("last")
        .unwrap()
        .subscriber(32);
    plan.prepare().unwrap();
    // Last: 66 slots. Middle: one loan + a 3-slot input footprint + 66 retained.
    // Source: one loan + a 3-slot input footprint + 70 retained.
    assert_eq!(
        plan.native::<Tracked>("a")
            .unwrap()
            .publisher_capacity(&a)
            .unwrap(),
        74
    );
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let capture = bindings
        .bundle::<bool, Nested<'static>>("last")
        .unwrap()
        .take_subscriber(&task_graph::bundle::subscriber_key::<bool, Nested<'static>>(last))
        .unwrap();
    let mut a = bindings
        .native::<Tracked>("a")
        .unwrap()
        .take_publisher(&a)
        .unwrap();
    let mut b = bindings
        .native::<Tracked>("b")
        .unwrap()
        .take_publisher(&b)
        .unwrap();
    let mut c = bindings
        .native::<u32>("c")
        .unwrap()
        .take_publisher(&c)
        .unwrap();
    let mut graph = storage.graph_builder();
    merge.add_to_graph(&mut graph, &bindings);
    relay.add_to_graph(&mut graph, &bindings);
    let mut executor = testing::SequentialExecutor::new(graph.build().unwrap());
    let drops = Arc::new(AtomicUsize::new(0));
    for index in 0..80 {
        let now = FrameworkTime::from_nanoseconds(index);
        a.publish(Tracked {
            value: 2,
            drops: drops.clone(),
        })
        .unwrap();
        b.publish(Tracked {
            value: 3,
            drops: drops.clone(),
        })
        .unwrap();
        c.publish(4).unwrap();
        a.flush(now);
        b.flush(now);
        c.flush(now);
        executor.step(now).unwrap();
        capture.update();
        let value = capture.input_forwardable().pop().unwrap();
        assert!(value.message.message);
        assert_eq!(
            value.message.forwarded_messages.previous.message.message,
            20
        );
    }
    drop((executor, a, b, c, capture, bindings));
    assert_eq!(drops.load(Ordering::SeqCst), 160);
}

#[derive(ForwardedSources)]
struct One<'s> {
    source: ArenaReaderPtr<'s, Message<u64>>,
}

#[test]
fn named_bundle_rebinds_after_separately_owned_sources_are_destroyed() {
    let mut plan = NamedPlan::default();
    let channel = plan.bundle::<(), One<'static>>("forwarded").unwrap();
    let output = channel.publisher(1);
    let input = channel.subscriber(1);
    let storage = plan.allocate().unwrap();
    for value in 0..8 {
        let source =
            task::GraphPlan::new(task::PublisherStoragePlan::<u64>::new(1).with_subscriber(1))
                .allocate()
                .unwrap();
        let mut source_endpoints = source.channels().build();
        let bindings = storage.bind().unwrap();
        let channel = bindings.bundle::<(), One<'static>>("forwarded").unwrap();
        let mut publisher = channel
            .take_publisher(&task_graph::bundle::publisher_key::<(), One<'static>>(
                output.clone(),
            ))
            .unwrap();
        let subscriber = channel
            .take_subscriber(&task_graph::bundle::subscriber_key::<(), One<'static>>(
                input.clone(),
            ))
            .unwrap();
        let now = FrameworkTime::from_nanoseconds(value as i64);
        source_endpoints.publisher.publish(value).unwrap();
        source_endpoints.publisher.flush(now);
        source_endpoints.subscribers[0].update();
        let source_message = source_endpoints.subscribers[0]
            .input_forwardable()
            .pop()
            .unwrap();
        publisher
            .publish(Forwarded::new(
                (),
                One {
                    source: source_message,
                },
            ))
            .unwrap();
        publisher.flush(now);
        subscriber.update();
        let retained = subscriber.input_forwardable().pop().unwrap();
        drop((publisher, subscriber, bindings, source_endpoints));
        assert_eq!(retained.message.forwarded_messages.source.message, value);
        drop(retained);
        drop(source);
    }
}
#[test]
fn retention_dependencies_propagate_and_reject_cycles() {
    let mut plan = NamedPlan::default();
    let source = plan.native::<u64>("source").unwrap().publisher(1);
    let middle = plan.native::<u64>("middle").unwrap().publisher(1);
    plan.native::<u64>("last").unwrap().publisher(1);
    plan.native::<u64>("last").unwrap().subscriber(8);
    plan.map_forwarding::<One<'static>>(
        "middle",
        "middle",
        &[("source", "input", "source", TypeId::of::<u64>())],
    )
    .unwrap();
    plan.map_forwarding::<One<'static>>(
        "last",
        "last",
        &[("source", "input", "middle", TypeId::of::<u64>())],
    )
    .unwrap();
    plan.prepare().unwrap();
    assert_eq!(
        plan.native::<u64>("middle")
            .unwrap()
            .publisher_capacity(&middle)
            .unwrap(),
        19
    );
    assert_eq!(
        plan.native::<u64>("source")
            .unwrap()
            .publisher_capacity(&source)
            .unwrap(),
        20
    );
    plan.map_forwarding::<One<'static>>(
        "source",
        "source",
        &[("source", "input", "last", TypeId::of::<u64>())],
    )
    .unwrap();
    assert!(
        plan.prepare()
            .unwrap_err()
            .to_string()
            .contains("cyclic forwarding")
    );
}
