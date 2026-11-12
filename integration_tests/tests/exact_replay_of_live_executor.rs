use exact_replay_executor::{ExactReplayExecutor, ExactReplayPlan, ReplayLog};
use live_executor::{LiveExecutor, clock::WallClock};
use logging::{
    CaptureOptions, ExecutionLogOptions, ExecutionLogPlan, GraphCapturePlan, LogSession,
    testing::InMemoryWriter,
};
use std::{
    sync::{
        Arc, Mutex,
        mpsc::{self, TryRecvError},
    },
    time::{Duration, Instant},
};
use task::{OutputDefault, RequiredInput};
use task_graph::{
    ApplicationBuilder, CallbackSchedule, GraphPlan, PoolIndex, ThreadPoolConfigList,
};
use task_macros::task_callback;

mod common;

const SOURCE_COUNT: usize = 4;
const MESSAGES_PER_SOURCE: u64 = 16;
const RESULT_COUNT: usize = MESSAGES_PER_SOURCE as usize;
const SOURCE_PUBLISH_PERIOD: Duration = Duration::from_micros(250);
const SOURCE_POOL_THREADS: usize = 4;
const PAYLOAD_CAPTURE_CAPACITY: usize = 256;
const EXECUTION_LOG_CAPACITY: usize = 512;

type CalculationResults = Arc<Mutex<Vec<u64>>>;

#[derive(Clone, Copy)]
struct SourceTaskConfig {
    base: u64,
    step: u64,
}
const SOURCE_CONFIGS: [SourceTaskConfig; SOURCE_COUNT] = [
    SourceTaskConfig { base: 1, step: 1 },
    SourceTaskConfig { base: 100, step: 2 },
    SourceTaskConfig {
        base: 1000,
        step: 3,
    },
    SourceTaskConfig { base: 1, step: 1 },
];

struct SourceTask {
    value: u64,
    step: u64,
    remaining: u64,
}
#[task_callback]
impl SourceTask {
    fn run(&mut self, mut output: OutputDefault<u64>) {
        if self.remaining == 0 {
            return;
        }
        *output = self.value;
        output.send();
        self.value += self.step;
        self.remaining -= 1;
    }
}

struct AddTask;
#[task_callback]
impl AddTask {
    fn run(
        &self,
        #[trigger(true)]
        #[capacity(64)]
        a: RequiredInput<u64>,
        #[trigger(true)]
        #[capacity(64)]
        b: RequiredInput<u64>,
        mut output: OutputDefault<u64>,
    ) {
        *output = *a + *b;
        output.send();
    }
}

struct SubtractTask;
#[task_callback]
impl SubtractTask {
    fn run(
        &self,
        #[trigger(true)]
        #[capacity(64)]
        a: RequiredInput<u64>,
        #[trigger(true)]
        #[capacity(64)]
        b: RequiredInput<u64>,
        mut output: OutputDefault<u64>,
    ) {
        *output = *a - *b;
        output.send();
    }
}

struct AddSubtractMergerTask {
    values: CalculationResults,
    stop: Option<mpsc::Sender<()>>,
}
#[task_callback]
impl AddSubtractMergerTask {
    fn run(
        &self,
        #[trigger(true)]
        #[capacity(64)]
        add: RequiredInput<u64>,
        #[trigger(true)]
        #[capacity(64)]
        sub: RequiredInput<u64>,
        mut output: OutputDefault<u64>,
    ) {
        let result = *add + *sub;
        *output = result;
        output.send();
        let mut values = self.values.lock().unwrap();
        values.push(result);
        if values.len() == RESULT_COUNT
            && let Some(stop) = &self.stop
        {
            let _ = stop.send(());
        }
    }
}

fn build_plan(stop: Option<mpsc::Sender<()>>, values: CalculationResults) -> GraphPlan {
    let mut app = ApplicationBuilder::new();
    for (index, spec) in SOURCE_CONFIGS.iter().enumerate() {
        let raw = format!("raw_{index}");
        app.add_task(
            format!("source_{index}"),
            SourceTask {
                value: spec.base,
                step: spec.step,
                remaining: MESSAGES_PER_SOURCE,
            },
            CallbackSchedule::periodic(SOURCE_PUBLISH_PERIOD)
                .in_pool(PoolIndex::new(0))
                .with_execution_duration(Duration::from_micros(1)),
        )
        .output_channel("output", raw);
    }
    app.add_task(
        "add",
        AddTask,
        CallbackSchedule::input_triggered()
            .in_pool(PoolIndex::new(0))
            .with_execution_duration(Duration::from_micros(10)),
    )
    .input_channel("a", "raw_0")
    .input_channel("b", "raw_1")
    .output_channel("output", "add");
    app.add_task(
        "sub",
        SubtractTask,
        CallbackSchedule::input_triggered()
            .in_pool(PoolIndex::new(0))
            .with_execution_duration(Duration::from_micros(12)),
    )
    .input_channel("a", "raw_2")
    .input_channel("b", "raw_3")
    .output_channel("output", "sub");
    app.add_task(
        "sink",
        AddSubtractMergerTask { values, stop },
        CallbackSchedule::input_triggered()
            .in_pool(PoolIndex::new(1))
            .with_execution_duration(Duration::from_micros(50)),
    )
    .input_channel("add", "add")
    .input_channel("sub", "sub")
    .output_channel("output", "result");
    app.plan().unwrap()
}

fn source_sequence(index: usize) -> Vec<u64> {
    let spec = SOURCE_CONFIGS[index];
    (0..MESSAGES_PER_SOURCE)
        .map(|offset| spec.base + offset * spec.step)
        .collect()
}

fn expected_combined_results() -> Vec<u64> {
    let first = source_sequence(0);
    let second = source_sequence(1);
    let third = source_sequence(2);
    let fourth = source_sequence(3);
    (0..RESULT_COUNT)
        .map(|index| (first[index] + second[index]) + (third[index] - fourth[index]))
        .collect()
}

fn sorted(mut values: Vec<u64>) -> Vec<u64> {
    values.sort_unstable();
    values
}

fn run_live() -> (ReplayLog, Vec<u64>) {
    let values: CalculationResults = Default::default();
    let (stop_tx, stop_rx) = mpsc::channel();
    let mut plan = build_plan(Some(stop_tx), values.clone());
    let captures =
        GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(PAYLOAD_CAPTURE_CAPACITY))
            .unwrap();
    let storage = plan.allocate().unwrap();
    let (graph, bindings) = storage.bind_and_build().unwrap();
    let execution_log =
        ExecutionLogPlan::for_graph(&graph, ExecutionLogOptions::new(EXECUTION_LOG_CAPACITY, 8))
            .unwrap();
    let log_storage = execution_log.allocate().unwrap();
    let (graph, metadata, execution_captures) = log_storage.attach(graph).unwrap();
    let mut captures = captures.bind(&bindings).unwrap();
    captures.extend(execution_captures);

    let writer = InMemoryWriter::new();
    let logged_data = writer.logged_data();
    let mut session = LogSession::new(writer, captures)
        .with_execution_log(metadata)
        .unwrap();

    let executor = LiveExecutor::new_multi_pool_with_time(
        ThreadPoolConfigList::try_from([SOURCE_POOL_THREADS, 1]).unwrap(),
        graph,
        WallClock,
    )
    .unwrap();
    let mut stopped_by_sink = false;
    executor
        .run_with(|stop| {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                session.flush().unwrap();
                match stop_rx.try_recv() {
                    Ok(()) => {
                        stopped_by_sink = true;
                        stop.request_stop();
                        break;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        panic!("sink dropped without requesting a stop")
                    }
                }
                if stop.is_stopped() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for the sink to request a stop"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        })
        .unwrap();
    session.finish().unwrap();
    assert!(
        stopped_by_sink,
        "live executor stopped before the sink finished"
    );

    let live_values = values.lock().unwrap().clone();
    assert_eq!(live_values.len(), RESULT_COUNT);
    assert_eq!(
        sorted(live_values.clone()),
        sorted(expected_combined_results())
    );
    assert_eq!(
        logged_data
            .lock()
            .unwrap()
            .messages()
            .iter()
            .filter(|message| message.channel() == "result")
            .count(),
        RESULT_COUNT
    );

    let log = common::replay_log_from_recorded_data(&logged_data);
    (log, live_values)
}

fn run_replay(log: ReplayLog) -> Vec<u64> {
    let values: CalculationResults = Default::default();
    let mut plan = build_plan(None, values.clone());
    let replay = ExactReplayPlan::declare(&mut plan, &log).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let replay_bindings = replay.bind(&bindings).unwrap();
    let graph = storage.build_with(&bindings).unwrap();
    let mut executor = ExactReplayExecutor::new(graph, log, replay_bindings).unwrap();
    let report = executor.run().unwrap();
    assert!(report.is_exact(), "{report:?}");
    assert_eq!(report.consumed_executions(), report.total_executions());
    for channel in ["raw_0", "raw_1", "raw_2", "raw_3", "add", "sub", "result"] {
        let stats = &report.channel_stats()[channel];
        assert!(stats.logged > 0, "expected logged messages on '{channel}'");
        assert_eq!(stats.gaps, 0, "unexpected gaps on '{channel}'");
        assert_eq!(stats.mismatches, 0, "unexpected mismatches on '{channel}'");
    }
    values.lock().unwrap().clone()
}

#[test]
#[cfg_attr(miri, ignore = "miri would be very slow")]
fn exact_replay_reproduces_live_multi_threaded_execution() {
    let (log, live_values) = run_live();
    let replayed_values = run_replay(log);
    assert_eq!(replayed_values, live_values);
    assert_eq!(sorted(replayed_values), sorted(expected_combined_results()));
}
