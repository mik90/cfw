use std::path::Path;

use exact_replay_executor::{
    ExactReplayConfig, ExactReplayExecutor, ReplayBindings, ReplayInputPlan, ReplayLog,
};
use logging::{
    CapturePlan, ExecutionRecorder, LogSession, PortCapture, ReplaySource, ReplaySourcePlan,
};
use task::{BuiltGraph, ChannelPlan, GraphBuilder, GraphPlan};
use test_tasks::{
    FIZZ_BUZZ_STRING_CHANNEL, FizzBuzzCalculator, INTEGER_CHANNEL, IncrementingIntegerPublisher,
    StringCollector,
};

pub type BuildError = Box<dyn std::error::Error + Send + Sync>;
const CAPACITY: usize = 64;
pub const PRODUCER: &str = "integer_publisher";
pub const CALCULATOR: &str = "fizz_buzz";
pub const COLLECTOR: &str = "string_collector";

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
    let mut integers = ChannelPlan::new(INTEGER_CHANNEL);
    let mut strings = ChannelPlan::new(FIZZ_BUZZ_STRING_CHANNEL);
    let producer = IncrementingIntegerPublisher::declare(&mut integers)?;
    let calculator = FizzBuzzCalculator::declare(&mut integers, &mut strings)?;
    let collector = StringCollector::declare(&mut strings)?;
    let integer_capture = log_integer.then(|| CapturePlan::declare(&mut integers, CAPACITY));
    let string_capture = CapturePlan::declare(&mut strings, CAPACITY);
    let storage = GraphPlan::new((integers, strings))
        .allocate()
        .map_err(|e| format!("{e:?}"))?;
    let integers = storage.channels().0.build();
    let strings = storage.channels().1.build();
    let collected = StringCollector::default();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_scheduled_callback(PRODUCER, IncrementingIntegerPublisher::schedule(), || {
        Ok(IncrementingIntegerPublisher::default().bind(producer, &integers)?)
    });
    graph.add_scheduled_callback(CALCULATOR, FizzBuzzCalculator::schedule(), || {
        Ok(FizzBuzzCalculator.bind(calculator, &integers, &strings)?)
    });
    graph.add_scheduled_callback(COLLECTOR, StringCollector::schedule(), || {
        Ok(collected.clone().bind(collector, &strings)?)
    });
    let recorder = ExecutionRecorder::new(CAPACITY * 3);
    let graph = recorder.attach(graph.build().map_err(|e| format!("{e:?}"))?)?;
    let mut captures = vec![string_capture.bind(&strings)?];
    if let Some(capture) = integer_capture {
        captures.push(capture.bind(&integers)?);
    }
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
    let mut integers = ChannelPlan::new(INTEGER_CHANNEL);
    let mut strings = ChannelPlan::new(FIZZ_BUZZ_STRING_CHANNEL);
    let producer = IncrementingIntegerPublisher::declare(&mut integers)?;
    let calculator = FizzBuzzCalculator::declare(&mut integers, &mut strings)?;
    let collector = StringCollector::declare(&mut strings)?;
    let integer_input = ReplayInputPlan::declare(&mut integers, calculator.integer_key())
        .map_err(|e| format!("{e:?}"))?;
    let string_input = ReplayInputPlan::declare(&mut strings, collector.string_key())
        .map_err(|e| format!("{e:?}"))?;
    let storage = GraphPlan::new((integers, strings))
        .allocate()
        .map_err(|e| format!("{e:?}"))?;
    let integers = storage.channels().0.build();
    let strings = storage.channels().1.build();
    let mut bindings = ReplayBindings::new();
    bindings.add_input(CALCULATOR, 0, integer_input.bind(&integers)?)?;
    bindings.add_input(COLLECTOR, 0, string_input.bind(&strings)?)?;
    bindings.add_output(
        PRODUCER,
        0,
        integers
            .configure_publisher(producer.output_key(), |p| PortCapture::native(p, CAPACITY))?,
    )?;
    bindings.add_output(
        CALCULATOR,
        0,
        strings.configure_publisher(calculator.fizz_buzz_string_key(), |p| {
            PortCapture::native(p, CAPACITY)
        })?,
    )?;
    let collected = StringCollector::default();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_scheduled_callback(PRODUCER, IncrementingIntegerPublisher::schedule(), || {
        Ok(IncrementingIntegerPublisher::default().bind(producer, &integers)?)
    });
    graph.add_scheduled_callback(CALCULATOR, FizzBuzzCalculator::schedule(), || {
        Ok(FizzBuzzCalculator.bind(calculator, &integers, &strings)?)
    });
    graph.add_scheduled_callback(COLLECTOR, StringCollector::schedule(), || {
        Ok(collected.clone().bind(collector, &strings)?)
    });
    run(
        ExactReplayExecutor::with_config(
            graph.build().map_err(|e| format!("{e:?}"))?,
            log,
            bindings,
            config,
        )?,
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
    let mut integers = ChannelPlan::new(INTEGER_CHANNEL);
    let mut strings = ChannelPlan::new(FIZZ_BUZZ_STRING_CHANNEL);
    let calculator = if strings_only {
        None
    } else {
        Some(FizzBuzzCalculator::declare(&mut integers, &mut strings)?)
    };
    let collector = StringCollector::declare(&mut strings)?;
    let integer_source =
        (!strings_only).then(|| ReplaySourcePlan::declare(&mut integers, CAPACITY));
    let string_source = strings_only.then(|| ReplaySourcePlan::declare(&mut strings, CAPACITY));
    let storage = GraphPlan::new((integers, strings))
        .allocate()
        .map_err(|e| format!("{e:?}"))?;
    let integers = storage.channels().0.build();
    let strings = storage.channels().1.build();
    let mut sources = Vec::new();
    if let Some(source) = integer_source {
        sources.push(source.bind(&integers)?);
    }
    if let Some(source) = string_source {
        sources.push(source.bind(&strings)?);
    }
    let collected = StringCollector::default();
    let mut graph = GraphBuilder::with_storage(&storage);
    if let Some(calculator) = calculator {
        graph.add_scheduled_callback(CALCULATOR, FizzBuzzCalculator::schedule(), || {
            Ok(FizzBuzzCalculator.bind(calculator, &integers, &strings)?)
        });
    }
    graph.add_scheduled_callback(COLLECTOR, StringCollector::schedule(), || {
        Ok(collected.clone().bind(collector, &strings)?)
    });
    run(
        graph.build().map_err(|e| format!("{e:?}"))?,
        sources,
        collected,
    )
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
