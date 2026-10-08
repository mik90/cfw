use simulation_executor::{SimulationConfig, SimulationState, StepError};
use std::sync::{
    Arc, Barrier, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use task::time::FrameworkTime;
use task::{
    CallbackSchedule, ChannelPlan, Context, GraphBuilder, GraphPlan, Input, LoanError, Publisher,
    RequiredInput,
};
use task_macros::task_callback;

type Trace<T> = Arc<Mutex<Vec<T>>>;

fn at(n: i64) -> FrameworkTime {
    FrameworkTime::from_nanoseconds(n)
}
fn ns(n: u64) -> Duration {
    Duration::from_nanos(n)
}

struct OrderedSource {
    value: u64,
    barrier: Arc<Barrier>,
    released: Arc<AtomicUsize>,
    order: Arc<Mutex<Vec<u64>>>,
}
#[task_callback]
impl OrderedSource {
    fn run(&self, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        self.barrier.wait();
        if self.value == 1 {
            while self.released.load(Ordering::Acquire) == 0 {
                std::thread::yield_now();
            }
        }
        self.order.lock().unwrap().push(self.value);
        output.publish(self.value)?;
        if self.value == 2 {
            self.released.store(1, Ordering::Release);
        }
        Ok(())
    }
}

#[test]
fn parallel_bodies_commit_in_scheduling_order_and_retained_messages_outlive_simulation() {
    let mut plan = ChannelPlan::new("numbers");
    let first = OrderedSource::declare(&mut plan).unwrap();
    let second = OrderedSource::declare(&mut plan).unwrap();
    let capture_key = plan.subscriber(2);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let capture = bindings.take_subscriber(&capture_key).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let released = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_scheduled_callback("first", CallbackSchedule::on_start(), || {
        Ok(OrderedSource {
            value: 1,
            barrier: barrier.clone(),
            released: released.clone(),
            order: order.clone(),
        }
        .bind(first, &bindings)?)
    });
    graph.add_scheduled_callback("second", CallbackSchedule::on_start(), || {
        Ok(OrderedSource {
            value: 2,
            barrier: barrier.clone(),
            released: released.clone(),
            order: order.clone(),
        }
        .bind(second, &bindings)?)
    });
    let config = SimulationConfig {
        virtual_pool_threads: vec![2],
        node_executor_thread_count: 2,
        start_time: at(100),
        ..Default::default()
    };
    let mut simulation = SimulationState::with_config(graph.build().unwrap(), config).unwrap();
    drop(bindings);
    let step = simulation.step().unwrap();
    assert_eq!(step.executed, [0, 1]);
    assert!(step.idle);
    assert_eq!(*order.lock().unwrap(), [2, 1]);
    capture.update();
    let retained: Vec<_> = capture.input().drain().collect();
    drop((simulation, capture));
    assert_eq!(
        retained.iter().map(|m| m.message).collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(retained.iter().all(|m| m.header.published_at == at(100)));
}

struct Source;
#[task_callback]
impl Source {
    fn run(&self, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        output.publish(42)
    }
}
struct Observe {
    observations: Trace<(FrameworkTime, Option<u64>)>,
}
#[task_callback]
impl Observe {
    fn run(&self, mut input: Input<u64>, context: &Context) {
        self.observations
            .lock()
            .unwrap()
            .push((context.now(), input.pop().map(|p| p.message)));
    }
}

#[test]
fn whole_batch_reads_precommit_inputs_then_new_work_runs_at_same_simulated_time() {
    let mut plan = ChannelPlan::new("value");
    let source = Source::declare(&mut plan).unwrap();
    let observe = Observe::declare(&mut plan).unwrap();
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let mut builder = GraphBuilder::new();
    builder.add_scheduled_callback("source", CallbackSchedule::on_start(), || {
        Ok(Source.bind(source, &bindings)?)
    });
    builder.add_scheduled_callback("observe", CallbackSchedule::on_start(), || {
        Ok(Observe {
            observations: observations.clone(),
        }
        .bind(observe, &bindings)?)
    });
    let mut simulation = SimulationState::with_config(
        builder.build().unwrap(),
        SimulationConfig {
            virtual_pool_threads: vec![2],
            node_executor_thread_count: 2,
            ..Default::default()
        },
    )
    .unwrap();
    let first = simulation.step().unwrap();
    assert_eq!(first.executed, [0, 1]);
    assert_eq!((first.before, first.after), (at(0), at(0)));
    assert!(!first.idle);
    assert_eq!(simulation.step().unwrap().executed, [1]);
    assert_eq!(
        *observations.lock().unwrap(),
        [(at(0), None), (at(0), Some(42))]
    );
}

#[test]
fn virtual_pool_contention_and_durations_advance_to_the_next_event() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut builder = GraphBuilder::new();
    for (name, pool, duration) in [("a", 0, 10), ("b", 0, 10), ("c", 1, 3)] {
        let trace = trace.clone();
        builder.add_scheduled_callback(
            name,
            CallbackSchedule::on_start()
                .in_pool(pool)
                .with_execution_duration(ns(duration)),
            move || {
                Ok(move |time| {
                    trace.lock().unwrap().push((name, time));
                    Ok(())
                })
            },
        );
    }
    let mut simulation = SimulationState::with_config(
        builder.build().unwrap(),
        SimulationConfig {
            virtual_pool_threads: vec![1, 1],
            ..Default::default()
        },
    )
    .unwrap();
    let first = simulation.step().unwrap();
    assert_eq!(first.executed, [0, 2]);
    assert_eq!(first.after, at(3));
    let second = simulation.step().unwrap();
    assert!(second.executed.is_empty());
    assert_eq!(second.after, at(10));
    let third = simulation.step().unwrap();
    assert_eq!(third.executed, [1]);
    assert_eq!(third.after, at(20));
    assert!(third.idle);
    assert_eq!(
        *trace.lock().unwrap(),
        [("a", at(0)), ("c", at(0)), ("b", at(10))]
    );
}

#[test]
fn startup_flag_and_custom_timing_use_completion_time() {
    for startup in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let times = Arc::new(Mutex::new(Vec::new()));
        let count = calls.clone();
        let mut schedule = CallbackSchedule::default()
            .with_execution_duration_callback(|| ns(2))
            .with_next_execution_time_callback(move |now| {
                (count.load(Ordering::Acquire) < 2)
                    .then(|| now.checked_add_duration(ns(5)).unwrap())
            });
        schedule.run_on_start = startup;
        let observed = times.clone();
        let mut builder = GraphBuilder::new();
        builder.add_scheduled_callback("timed", schedule, move || {
            Ok(move |time| {
                observed.lock().unwrap().push(time);
                calls.fetch_add(1, Ordering::Release);
                Ok(())
            })
        });
        let mut simulation = SimulationState::new(builder.build().unwrap()).unwrap();
        simulation.run_until_idle(10).unwrap();
        assert_eq!(
            *times.lock().unwrap(),
            if startup {
                vec![at(0), at(7)]
            } else {
                vec![at(5), at(12)]
            }
        );
        assert_eq!(
            simulation.current_time(),
            if startup { at(9) } else { at(14) }
        );
    }
}

#[test]
fn periodic_startup_is_explicit_instead_of_implicit() {
    for startup in [false, true] {
        let times = Arc::new(Mutex::new(Vec::new()));
        let observed = times.clone();
        let mut schedule = CallbackSchedule::periodic(ns(5));
        schedule.run_on_start = startup;
        let mut builder = GraphBuilder::new();
        builder.add_scheduled_callback("periodic", schedule, move || {
            Ok(move |time| {
                observed.lock().unwrap().push(time);
                Ok(())
            })
        });
        let mut simulation = SimulationState::new(builder.build().unwrap()).unwrap();
        for _ in 0..3 {
            simulation.step().unwrap();
            if times.lock().unwrap().len() == 2 {
                break;
            }
        }
        assert_eq!(
            *times.lock().unwrap(),
            if startup {
                vec![at(0), at(5)]
            } else {
                vec![at(5), at(10)]
            }
        );
    }
}

struct Gated {
    trace: Trace<(FrameworkTime, u64, u64)>,
}
#[task_callback]
impl Gated {
    fn run(
        &self,
        #[keep_across_runs(false)] trigger: RequiredInput<u64>,
        #[trigger(false)] cached: RequiredInput<u64>,
        context: &Context,
    ) {
        self.trace
            .lock()
            .unwrap()
            .push((context.now(), *trigger, *cached));
    }
}

#[test]
fn nontrigger_input_resumes_pending_request_and_input_retention_is_respected() {
    let mut trigger = ChannelPlan::new("trigger");
    let mut cached = ChannelPlan::new("cached");
    let declaration = Gated::declare(&mut trigger, &mut cached).unwrap();
    let trigger_key = trigger.publisher(1);
    let cached_key = cached.publisher(1);
    let storage = GraphPlan::new((trigger, cached)).allocate().unwrap();
    let trigger = storage.channels().0.build();
    let cached = storage.channels().1.build();
    let trigger_source = Arc::new(Mutex::new(trigger.take_publisher(&trigger_key).unwrap()));
    let cached_source = Arc::new(Mutex::new(cached.take_publisher(&cached_key).unwrap()));
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut builder = GraphBuilder::new();
    builder.add_callback("gated", || {
        Ok(Gated {
            trace: trace.clone(),
        }
        .bind(declaration, &trigger, &cached)?)
    });
    let mut simulation = SimulationState::new(builder.build().unwrap()).unwrap();
    for (time, publisher, value) in [
        (0, trigger_source.clone(), 1),
        (5, cached_source.clone(), 10),
        (10, cached_source.clone(), 20),
        (15, trigger_source.clone(), 2),
    ] {
        simulation
            .schedule_at(at(time), move |stamp| {
                let mut publisher = publisher.lock().unwrap();
                publisher.publish(value).map_err(|e| format!("{e:?}"))?;
                publisher.flush(stamp);
                Ok(())
            })
            .unwrap();
    }
    simulation.run_until_idle(10).unwrap();
    assert_eq!(*trace.lock().unwrap(), [(at(5), 1, 10), (at(15), 2, 20)]);
}

#[test]
fn invalid_configs_past_actions_and_infinite_schedules_report_errors() {
    let graph = GraphBuilder::new().build().unwrap();
    assert!(matches!(
        SimulationState::with_config(
            graph,
            SimulationConfig {
                node_executor_thread_count: 0,
                ..Default::default()
            }
        ),
        Err(StepError::InvalidConfig(_))
    ));
    let mut builder = GraphBuilder::new();
    builder.add_scheduled_callback("forever", CallbackSchedule::periodic(ns(1)), || {
        Ok(|_| Ok(()))
    });
    let mut simulation = SimulationState::new(builder.build().unwrap()).unwrap();
    assert!(matches!(
        simulation.schedule_at(at(-1), |_| Ok(())),
        Err(StepError::PastAction)
    ));
    assert!(matches!(
        simulation.run_until_idle(3),
        Err(StepError::StepLimitExceeded)
    ));
}

#[test]
fn oldest_ready_callback_wins_when_a_virtual_pool_becomes_free() {
    let mut b = ChannelPlan::new("b");
    let mut c = ChannelPlan::new("c");
    let b_decl = Observe::declare(&mut b).unwrap();
    let c_decl = Observe::declare(&mut c).unwrap();
    let b_source = b.publisher(1);
    let c_source = c.publisher(1);
    let storage = GraphPlan::new((b, c)).allocate().unwrap();
    let b = storage.channels().0.build();
    let c = storage.channels().1.build();
    let b_source = b.take_publisher(&b_source).unwrap();
    let c_source = c.take_publisher(&c_source).unwrap();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut builder = GraphBuilder::new();
    builder.add_scheduled_callback(
        "busy",
        CallbackSchedule::on_start().with_execution_duration(ns(10)),
        || Ok(|_| Ok(())),
    );
    builder.add_scheduled_callback(
        "b",
        CallbackSchedule::default().with_execution_duration(ns(5)),
        || {
            Ok(Observe {
                observations: trace.clone(),
            }
            .bind(b_decl, &b)?)
        },
    );
    builder.add_scheduled_callback(
        "c",
        CallbackSchedule::default().with_execution_duration(ns(5)),
        || {
            Ok(Observe {
                observations: trace.clone(),
            }
            .bind(c_decl, &c)?)
        },
    );
    let mut simulation = SimulationState::new(builder.build().unwrap()).unwrap();
    for (time, mut source, value) in [(1, c_source, 3), (2, b_source, 2)] {
        simulation
            .schedule_at(at(time), move |stamp| {
                source.publish(value).map_err(|e| format!("{e:?}"))?;
                source.flush(stamp);
                Ok(())
            })
            .unwrap();
    }
    simulation.run_until_idle(10).unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        [(at(10), Some(3)), (at(15), Some(2))]
    );
}
