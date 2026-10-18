use std::path::Path;

use exact_replay_executor::{ExactReplayConfig, ExactReplayExecutor, ExactReplayPlan, ReplayLog};
use logging::{
    CaptureOptions, ExecutionRecorder, GraphCapturePlan, GraphReplayPlan, LogSession,
    ReplayOptions, ReplaySource,
};
use task_graph::{ApplicationBuilder, BuiltGraph};
use test_tasks::{
    FIZZ_BUZZ_STRING_CHANNEL, FizzBuzzCalculator, INTEGER_CHANNEL, IncrementingIntegerPublisher,
    StringCollector,
};

pub type BuildError = Box<dyn std::error::Error + Send + Sync>;
const CAPACITY: usize = 64;
pub const PRODUCER: &str = "integer_publisher";
pub const CALCULATOR: &str = "fizz_buzz";
pub const COLLECTOR: &str = "string_collector";

fn application(
    producer: bool,
    calculator: bool,
    collected: &StringCollector,
) -> ApplicationBuilder {
    let mut app = ApplicationBuilder::new();
    if producer {
        app.add_task(
            PRODUCER,
            IncrementingIntegerPublisher::default(),
            IncrementingIntegerPublisher::schedule(),
        );
    }
    if calculator {
        app.add_task(
            CALCULATOR,
            FizzBuzzCalculator,
            FizzBuzzCalculator::schedule(),
        );
    }
    app.add_task(COLLECTOR, collected.clone(), StringCollector::schedule());
    app
}

pub fn replay_denylist(strings_only: bool) -> std::collections::HashSet<String> {
    [if strings_only {
        INTEGER_CHANNEL
    } else {
        FIZZ_BUZZ_STRING_CHANNEL
    }
    .into()]
    .into()
}

/// Storage belongs to this application scope; callbacks and log captures borrow it.
/// The caller flushes periodically and finishes the session after workers join.
pub fn with_recording<R>(
    path: &Path,
    log_integer: bool,
    run: impl for<'s> FnOnce(BuiltGraph<'s>, LogSession<'s>, StringCollector) -> Result<R, BuildError>,
) -> Result<R, BuildError> {
    let collected = StringCollector::default();
    let mut plan = application(true, true, &collected).plan()?;
    let mut options = CaptureOptions::new(CAPACITY);
    if !log_integer {
        options = options.exclude(INTEGER_CHANNEL);
    }
    let captures = GraphCapturePlan::declare(&mut plan, &options)?;
    let storage = plan.allocate()?;
    let (graph, bindings) = storage.bind_and_build()?;
    let recorder = ExecutionRecorder::new(CAPACITY * 3);
    let graph = graph.with_recording(&recorder)?;
    let captures = captures.bind(&bindings)?;
    let writer = logging::log_file_json::JsonLogFileWriter::new(std::io::BufWriter::new(
        std::fs::File::create(path)?,
    ));
    let session = LogSession::new(writer, captures).with_recording(recorder)?;
    run(graph, session, collected)
}

/// Record a bounded simulation, including explicit modeled callback durations.
pub fn record_simulation(
    path: &Path,
    log_integer: bool,
    count: usize,
) -> Result<Vec<String>, BuildError> {
    with_recording(path, log_integer, |graph, mut session, collected| {
        let mut simulation = simulation_executor::SimulationState::new(graph)?;
        while collected.len() < count {
            simulation.step()?;
            session.flush()?;
        }
        session.finish()?;
        Ok(collected.stored_strings())
    })
}

pub fn with_exact_replay<R>(
    path: &Path,
    config: ExactReplayConfig,
    run: impl for<'s> FnOnce(ExactReplayExecutor<'s>, StringCollector) -> Result<R, BuildError>,
) -> Result<R, BuildError> {
    let log = ReplayLog::from_sorted(logging::SortedLogStreamReader::from_path(path, 1024)?)?;
    with_exact_replay_log(log, config, run)
}

pub fn with_exact_replay_log<R>(
    log: ReplayLog,
    config: ExactReplayConfig,
    run: impl for<'s> FnOnce(ExactReplayExecutor<'s>, StringCollector) -> Result<R, BuildError>,
) -> Result<R, BuildError> {
    let collected = StringCollector::default();
    let mut plan = application(true, true, &collected).plan()?;
    let replay = ExactReplayPlan::declare(&mut plan, &log)?;
    let storage = plan.allocate()?;
    let endpoints = storage.bind()?;
    let bindings = replay.bind(&endpoints)?;
    let graph = storage.build_with(&endpoints)?;
    run(
        ExactReplayExecutor::with_config(graph, log, bindings, config)?,
        collected,
    )
}

/// Replay integers through the calculator, or recorded strings directly to the collector.
pub fn with_replay_graph<R>(
    strings_only: bool,
    run: impl for<'s> FnOnce(
        BuiltGraph<'s>,
        Vec<ReplaySource<'s>>,
        StringCollector,
    ) -> Result<R, BuildError>,
) -> Result<R, BuildError> {
    let collected = StringCollector::default();
    let mut plan = application(false, !strings_only, &collected).plan()?;
    let channel = if strings_only {
        FIZZ_BUZZ_STRING_CHANNEL
    } else {
        INTEGER_CHANNEL
    };
    let replay = GraphReplayPlan::declare(&mut plan, [channel], &ReplayOptions::new(CAPACITY))?;
    let storage = plan.allocate()?;
    let (graph, bindings) = storage.bind_and_build()?;
    run(graph, replay.bind(&bindings)?, collected)
}

pub fn print_graph(strings_only: bool, replay: bool) {
    if !replay {
        println!("{PRODUCER}: -> {INTEGER_CHANNEL}");
    }
    if !strings_only {
        println!("{CALCULATOR}: {INTEGER_CHANNEL} -> {FIZZ_BUZZ_STRING_CHANNEL}");
    }
    println!("{COLLECTOR}: {FIZZ_BUZZ_STRING_CHANNEL} -> collected strings");
}
