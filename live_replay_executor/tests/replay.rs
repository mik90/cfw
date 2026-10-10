use live_replay_executor::{
    LiveReplayConfig, LiveReplayError, LiveReplayExecutor, ReplayTimeSource,
};
use logging::{CapturePlan, OwnedLogEntry, ReplaySourcePlan, SortedLogStreamReader};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use task::{
    ChannelPlan, GraphBuilder, GraphPlan, LoanError, Publisher, RequiredInput,
    executor::TimeSource, time::FrameworkTime,
};
use task_macros::task_callback;
fn at(n: i64) -> FrameworkTime {
    FrameworkTime::from_nanoseconds(n)
}
#[derive(Clone)]
struct Manual(Arc<AtomicI64>);
impl TimeSource for Manual {
    fn now(&self) -> FrameworkTime {
        at(self.0.load(Ordering::SeqCst))
    }
}
#[test]
fn clock_excludes_startup_and_pause_time_and_changes_speed_continuously() {
    let base = Manual(Arc::new(AtomicI64::new(0)));
    let clock = ReplayTimeSource::new(base.clone(), at(100), 2.0, false).unwrap();
    base.0.store(50, Ordering::SeqCst);
    assert_eq!(clock.now(), at(100));
    clock.start();
    base.0.store(60, Ordering::SeqCst);
    clock.pause();
    assert_eq!(clock.now(), at(120));
    base.0.store(1000, Ordering::SeqCst);
    assert_eq!(clock.now(), at(120));
    clock.resume();
    base.0.store(1010, Ordering::SeqCst);
    assert_eq!(clock.now(), at(140));
    clock.set_speed(0.5).unwrap();
    base.0.store(1030, Ordering::SeqCst);
    assert_eq!(clock.now(), at(150));
    base.0.store(1020, Ordering::SeqCst);
    assert_eq!(clock.now(), at(150));
    for speed in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(clock.set_speed(speed).is_err());
        assert!(ReplayTimeSource::new(base.clone(), at(0), speed, false).is_err());
    }
    let precise =
        ReplayTimeSource::new(base.clone(), at(1_700_000_000_000_000_000), 1.0, false).unwrap();
    precise.start();
    base.0.store(1021, Ordering::SeqCst);
    assert_eq!(precise.now(), at(1_700_000_000_000_000_001));
}
struct Double {
    slow: bool,
    calls: Arc<AtomicUsize>,
}
#[task_callback]
impl Double {
    fn run(
        &self,
        #[keep_across_runs(false)] input: RequiredInput<u64>,
        output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        if self.slow {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        output.publish(*input * 2)
    }
}
fn row(time: i64, bytes: &[u8]) -> OwnedLogEntry {
    OwnedLogEntry {
        header: task::message::MessageHeader::new(at(time)),
        channel_name: "input".into(),
        serialized_body: bytes.into(),
    }
}

#[test]
fn pause_resume_and_eof_drain_preserve_the_entire_native_pipeline() {
    let mut input = ChannelPlan::new("input");
    let mut middle = ChannelPlan::new("middle");
    let mut output = ChannelPlan::new("output");
    let a = Double::declare(&mut input, &mut middle).unwrap();
    let b = Double::declare(&mut middle, &mut output).unwrap();
    let source = ReplaySourcePlan::declare(&mut input, 1);
    let capture = CapturePlan::declare(&mut output, 1);
    let storage = GraphPlan::new((input, (middle, output)))
        .allocate()
        .unwrap();
    let input = storage.channels().0.build();
    let middle = storage.channels().1.0.build();
    let output = storage.channels().1.1.build();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut graph = GraphBuilder::new();
    graph.add_callback("a", || {
        Ok(Double {
            slow: true,
            calls: calls.clone(),
        }
        .bind(a, &input, &middle)?)
    });
    graph.add_callback("b", || {
        Ok(Double {
            slow: false,
            calls: calls.clone(),
        }
        .bind(b, &middle, &output)?)
    });
    let log = SortedLogStreamReader::from_entries(vec![row(100, b"42")], HashMap::new()).unwrap();
    let replay = LiveReplayExecutor::new(
        graph.build().unwrap(),
        log,
        [source.bind(&input).unwrap()],
        LiveReplayConfig {
            start_paused: true,
            ..Default::default()
        },
    )
    .unwrap();
    let (value, completion) = replay
        .run_with(|control| {
            std::thread::sleep(Duration::from_millis(5));
            assert_eq!(control.now(), at(100));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(!control.input_exhausted());
            control.resume();
            control.wait();
            7
        })
        .unwrap();
    assert_eq!(value, 7);
    assert!(completion.input_exhausted && completion.drained);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let messages = capture.bind(&output).unwrap().drain_to_vec().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].1, b"168");
}

#[test]
fn stopping_a_paused_replay_and_controller_panic_join_all_threads() {
    for panic in [false, true] {
        let mut plan = ChannelPlan::<u64>::new("input");
        let source = ReplaySourcePlan::declare(&mut plan, 1);
        let storage = GraphPlan::new(plan).allocate().unwrap();
        let bindings = storage.channels().build();
        let reader =
            SortedLogStreamReader::from_entries(vec![row(0, b"1")], HashMap::new()).unwrap();
        let replay = LiveReplayExecutor::new(
            GraphBuilder::new().build().unwrap(),
            reader,
            [source.bind(&bindings).unwrap()],
            LiveReplayConfig {
                start_paused: true,
                ..Default::default()
            },
        )
        .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            replay.run_with(|control| {
                assert!(control.is_paused());
                if panic {
                    panic!("controller failed");
                }
                control.request_stop();
            })
        }));
        if panic {
            assert!(result.is_err());
        } else {
            let (_, completion) = result.unwrap().unwrap();
            assert!(!completion.input_exhausted);
            assert!(!completion.drained);
        }
    }
}
#[test]
fn malformed_payload_reports_input_failure_and_empty_logs_complete() {
    let mut plan = ChannelPlan::<u64>::new("input");
    let source = ReplaySourcePlan::declare(&mut plan, 1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let reader = SortedLogStreamReader::from_entries(vec![row(0, b"bad")], HashMap::new()).unwrap();
    let replay = LiveReplayExecutor::new(
        GraphBuilder::new().build().unwrap(),
        reader,
        [source.bind(&bindings).unwrap()],
        LiveReplayConfig::default(),
    )
    .unwrap();
    assert!(matches!(replay.run(), Err(LiveReplayError::Input(_))));
    let empty = SortedLogStreamReader::from_entries(vec![], HashMap::new()).unwrap();
    let completion = LiveReplayExecutor::new(
        GraphBuilder::new().build().unwrap(),
        empty,
        [],
        LiveReplayConfig::default(),
    )
    .unwrap()
    .run()
    .unwrap();
    assert!(completion.input_exhausted && completion.drained);
}

struct Poll;
#[task_callback]
impl Poll {
    fn run(
        &self,
        #[trigger(false)]
        #[keep_across_runs(false)]
        input: RequiredInput<u64>,
        output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        output.publish(*input)
    }
}
#[test]
fn configured_tail_allows_periodic_consumers_before_quiescing_timers() {
    let mut input = ChannelPlan::new("input");
    let mut output = ChannelPlan::new("output");
    let declaration = Poll::declare(&mut input, &mut output).unwrap();
    let source = ReplaySourcePlan::declare(&mut input, 1);
    let capture = CapturePlan::declare(&mut output, 2);
    let storage = GraphPlan::new((input, output)).allocate().unwrap();
    let input = storage.channels().0.build();
    let output = storage.channels().1.build();
    let mut graph = GraphBuilder::new();
    graph.add_scheduled_callback(
        "poll",
        task::CallbackSchedule::periodic(Duration::from_millis(1)),
        || Ok(Poll.bind(declaration, &input, &output)?),
    );
    let reader = SortedLogStreamReader::from_entries(vec![row(0, b"7")], HashMap::new()).unwrap();
    let completion = LiveReplayExecutor::new(
        graph.build().unwrap(),
        reader,
        [source.bind(&input).unwrap()],
        LiveReplayConfig {
            tail_duration: Duration::from_millis(5),
            ..Default::default()
        },
    )
    .unwrap()
    .run()
    .unwrap();
    assert!(completion.drained);
    assert!(completion.final_time >= at(5_000_000));
    let messages = capture.bind(&output).unwrap().drain_to_vec().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].1, b"7");
}
struct Feedback;
#[task_callback]
impl Feedback {
    fn run(
        &self,
        #[keep_across_runs(false)] input: task::OptionalInput<u64>,
        output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        let _ = input;
        output.publish(1)
    }
}
struct Panics;
#[task_callback]
impl Panics {
    fn run(&self, input: RequiredInput<u64>) {
        let _ = input;
        panic!("worker failed");
    }
}
#[test]
fn drain_timeout_and_worker_failure_are_not_successful_completion() {
    let mut plan = ChannelPlan::new("feedback");
    let declaration = FeedbackDeclaration::from_keys(plan.subscriber(1), plan.publisher(1));
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut graph = GraphBuilder::new();
    graph.add_scheduled_callback("feedback", task::CallbackSchedule::on_start(), || {
        Ok(Feedback.bind(declaration, &bindings, &bindings)?)
    });
    let reader = SortedLogStreamReader::from_entries(vec![], HashMap::new()).unwrap();
    let replay = LiveReplayExecutor::new(
        graph.build().unwrap(),
        reader,
        [],
        LiveReplayConfig {
            drain_timeout: Duration::from_millis(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(replay.run(), Err(LiveReplayError::DrainTimeout)));

    let mut input = ChannelPlan::new("input");
    let declaration = Panics::declare(&mut input).unwrap();
    let source = ReplaySourcePlan::declare(&mut input, 1);
    let storage = GraphPlan::new(input).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut graph = GraphBuilder::new();
    graph.add_callback("panic", || Ok(Panics.bind(declaration, &bindings)?));
    let reader = SortedLogStreamReader::from_entries(vec![row(0, b"1")], HashMap::new()).unwrap();
    let replay = LiveReplayExecutor::new(
        graph.build().unwrap(),
        reader,
        [source.bind(&bindings).unwrap()],
        LiveReplayConfig::default(),
    )
    .unwrap();
    assert!(matches!(replay.run(), Err(LiveReplayError::Live(_))));
}
