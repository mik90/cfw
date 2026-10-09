use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use task::time::FrameworkTime;
use task::{
    ChannelPlan, EndpointError, ForwardedMessage, GraphBuildError, GraphBuilder, GraphPlan,
    LoanError, StorageError,
};

fn timestamp() -> FrameworkTime {
    FrameworkTime::from_nanoseconds(100)
}

#[test]
fn runtime_declarations_wire_fanin_and_fanout_before_factories_run() {
    let mut declarations = [ChannelPlan::<u64>::new("a"), ChannelPlan::<u64>::new("b")];
    let first_input = declarations[0].subscriber(4);
    let source_channels = [0, 1, 0];
    let sources: Vec<_> = source_channels
        .iter()
        .map(|&index| declarations[index].publisher(1))
        .collect();
    // Declarations can be expanded after publishers have been described.
    let second_input = declarations[1].subscriber(2);
    let fanout = declarations[0].subscriber(1);
    assert_eq!(declarations[0].publisher_capacity(&sources[0]).unwrap(), 13);
    assert_eq!(declarations[1].publisher_capacity(&sources[1]).unwrap(), 6);
    let [a, b] = declarations;
    let storage = GraphPlan::new((a, b)).allocate().unwrap();
    assert_eq!(storage.channels().0.name(), "a");
    assert_eq!(storage.channels().1.name(), "b");

    let (mut graph, first, second, fanout) = {
        let bindings = [storage.channels().0.build(), storage.channels().1.build()];
        let mut builder = GraphBuilder::new();
        for (index, key) in sources.iter().enumerate() {
            let bindings = &bindings[source_channels[index]];
            builder.add_callback(format!("source-{index}"), move || {
                let mut publisher = bindings.take_publisher(key)?;
                Ok(move |timestamp| {
                    publisher.publish(index as u64 + 1)?;
                    publisher.flush(timestamp);
                    Ok(())
                })
            });
        }
        let first = bindings[0].take_subscriber(&first_input).unwrap();
        let second = bindings[1].take_subscriber(&second_input).unwrap();
        let fanout = bindings[0].take_subscriber(&fanout).unwrap();
        (builder.build().unwrap(), first, second, fanout)
    }; // Bindings and factory borrows end before graph execution.

    std::thread::scope(|scope| {
        scope
            .spawn(move || graph.step(timestamp()).unwrap())
            .join()
            .unwrap();
    }); // Callback graph and publishers have been destroyed on the worker.
    first.update();
    second.update();
    fanout.update();
    assert_eq!(
        first.input().drain().map(|p| p.message).collect::<Vec<_>>(),
        [1, 3]
    );
    assert_eq!(second.input().pop().unwrap().message, 2);
    assert_eq!(fanout.writer_drops(), 1);
    let retained = fanout.input().pop().unwrap();
    drop((first, second, fanout));
    assert_eq!(retained.message, 3);
    assert_eq!(retained.header.published_at, timestamp());
}

#[test]
fn factories_connect_borrowed_forwarding_across_named_channels() {
    let mut sources = ChannelPlan::<u64>::new("source");
    let source_key = sources.publisher(1);
    let forward_input = sources.subscriber(1);
    let mut forwards = ChannelPlan::new("forwarded");
    let forward_key = forwards.publisher(1);
    let fixture_key = forwards.subscriber(1);
    sources
        .reserve_retained(
            &source_key,
            forwards.publisher_capacity(&forward_key).unwrap(),
        )
        .unwrap();
    let storage = GraphPlan::new((sources, forwards)).allocate().unwrap();

    let (mut graph, fixture) = {
        let source_bindings = storage.channels().0.build();
        let forward_bindings = storage.channels().1.build();
        let mut builder = GraphBuilder::new();
        builder.add_callback("source", || {
            let mut publisher = source_bindings.take_publisher(&source_key)?;
            Ok(move |timestamp| {
                publisher.publish(42)?;
                publisher.flush(timestamp);
                Ok(())
            })
        });
        builder.add_callback("forward", || {
            let input = source_bindings.take_subscriber(&forward_input)?;
            let mut publisher = forward_bindings.take_publisher(&forward_key)?;
            Ok(move |timestamp| {
                input.update();
                for message in input.input().drain() {
                    publisher.publish(ForwardedMessage::new(String::from("forwarded"), message))?;
                }
                publisher.flush(timestamp);
                Ok(())
            })
        });
        let fixture = forward_bindings.take_subscriber(&fixture_key).unwrap();
        (builder.build().unwrap(), fixture)
    };
    graph.step(timestamp()).unwrap();
    fixture.update();
    let retained = fixture.input().pop().unwrap();
    drop((graph, fixture));
    assert_eq!(retained.message.message, "forwarded");
    assert_eq!(retained.message.forwarded.message, 42);
}

#[test]
fn keys_check_channel_names_and_single_endpoint_ownership() {
    let mut first = ChannelPlan::<u64>::new("first");
    let publisher = first.publisher(1);
    let subscriber = first.subscriber(1);
    let mut second = ChannelPlan::<u64>::new("second");
    let foreign_pub = second.publisher(1);
    let foreign_sub = second.subscriber(1);
    assert_eq!(
        first.reserve_retained(&foreign_pub, 1),
        Err(StorageError::ForeignPublisherKey)
    );
    let storage = GraphPlan::new((first, second)).allocate().unwrap();
    let bindings = storage.channels().0.build();
    assert!(matches!(
        bindings.take_publisher(&foreign_pub),
        Err(EndpointError::WrongChannel)
    ));
    assert!(matches!(
        bindings.take_subscriber(&foreign_sub),
        Err(EndpointError::WrongChannel)
    ));
    let _publisher = bindings.take_publisher(&publisher).unwrap();
    assert!(matches!(
        bindings.take_publisher(&publisher.clone()),
        Err(EndpointError::AlreadyTaken)
    ));
    let _subscriber = bindings.take_subscriber(&subscriber).unwrap();
    assert!(matches!(
        bindings.take_subscriber(&subscriber.clone()),
        Err(EndpointError::AlreadyTaken)
    ));
}

#[test]
fn duplicate_channel_names_are_rejected_before_endpoint_construction() {
    let mut numbers = ChannelPlan::<u64>::new("same-name");
    numbers.publisher(1);
    let mut strings = ChannelPlan::<String>::new("same-name");
    strings.subscriber(1);
    assert!(matches!(GraphPlan::new((numbers, strings)).allocate(),
        Err(StorageError::DuplicateChannel(channel)) if channel == "same-name"));

    // Names are unique even for same-type or empty plans.
    let first = ChannelPlan::<u64>::new("duplicate");
    let second = ChannelPlan::<u64>::new("duplicate");
    assert!(matches!(GraphPlan::new((first, second)).allocate(),
        Err(StorageError::DuplicateChannel(channel)) if channel == "duplicate"));

    let mut disconnected = ChannelPlan::<u64>::new("no-publisher");
    disconnected.subscriber(0);
    assert!(matches!(
        GraphPlan::new(disconnected).allocate(),
        Err(StorageError::ZeroSubscriberCapacity)
    ));
}

struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn factory_failure_drops_already_built_callbacks_and_their_pending_loans() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut declarations = ChannelPlan::<Counted>::new("pending");
    let key = declarations.publisher(1);
    let storage = GraphPlan::new(declarations).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut builder = GraphBuilder::new();
    builder.add_callback("first", || {
        let mut publisher = bindings.take_publisher(&key)?;
        publisher.publish(Counted(drops.clone())).unwrap();
        Ok(move |timestamp| {
            publisher.flush(timestamp);
            Ok(())
        })
    });
    builder.add_callback("second", || {
        let _duplicate = bindings.take_publisher(&key)?;
        Ok(|_| Ok(()))
    });
    match builder.build() {
        Err(GraphBuildError::Factory { callback, source }) => {
            assert_eq!(callback, "second");
            assert_eq!(
                source.downcast_ref::<EndpointError>(),
                Some(&EndpointError::AlreadyTaken)
            );
        }
        _ => panic!("expected a factory failure"),
    }
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    let rebuilt = storage.channels().build();
    rebuilt
        .take_publisher(&key)
        .unwrap()
        .publish(Counted(drops.clone()))
        .unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

#[test]
fn duplicate_callback_names_do_not_run_factories() {
    let invoked = AtomicUsize::new(0);
    let mut builder = GraphBuilder::new();
    for _ in 0..2 {
        builder.add_callback("duplicate", || {
            invoked.fetch_add(1, Ordering::Relaxed);
            Ok(|_| Ok(()))
        });
    }
    assert!(
        matches!(builder.build(), Err(GraphBuildError::DuplicateCallback(name)) if name == "duplicate")
    );
    assert_eq!(invoked.load(Ordering::Relaxed), 0);
}

#[test]
fn step_error_preserves_published_messages_and_skips_later_callbacks() {
    let mut declarations = ChannelPlan::<u64>::new("output");
    let publisher = declarations.publisher(1);
    let subscriber = declarations.subscriber(1);
    let storage = GraphPlan::new(declarations).allocate().unwrap();
    let bindings = storage.channels().build();
    let fixture = bindings.take_subscriber(&subscriber).unwrap();
    let later_calls = Arc::new(AtomicUsize::new(0));
    let mut builder = GraphBuilder::new();
    builder.add_callback("publish", || {
        let mut publisher = bindings.take_publisher(&publisher)?;
        Ok(move |timestamp| {
            publisher.publish(7)?;
            publisher.flush(timestamp);
            Ok(())
        })
    });
    builder.add_callback("fail", || Ok(|_| Err(LoanError::ArenaExhausted)));
    let calls = later_calls.clone();
    builder.add_callback("later", move || {
        Ok(move |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    });
    let mut graph = builder.build().unwrap();
    let error = graph.step(timestamp()).unwrap_err();
    assert_eq!(error.callback, "fail");
    assert_eq!(error.source, LoanError::ArenaExhausted);
    assert_eq!(later_calls.load(Ordering::Relaxed), 0);
    drop((graph, bindings));
    fixture.update();
    assert_eq!(fixture.input().pop().unwrap().message, 7);
}
