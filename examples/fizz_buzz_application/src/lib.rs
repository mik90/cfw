use std::sync::{Arc, OnceLock};

use task::channel_registry::ChannelRegistry;
use task::executor::ExecutorStopSignal;
use task::task_graph_builder::{BuiltTaskGraph, TaskGraphBuilder};

pub struct BuiltGraph {
    pub graph: BuiltTaskGraph,
    pub stop_signal_cell: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
}

/// Constructs the callback graph in live mode (original publishers + logging).
pub type BuildError = Box<dyn std::error::Error + Send + Sync>;

pub fn build_live_graph(log_path: &std::path::Path) -> Result<BuiltGraph, BuildError> {
    let config = logging::LogTaskConfiguration {
        output_path: log_path.to_path_buf(),
        period: std::time::Duration::from_millis(1000),
        num_tasks: 1,
    };

    let mut registry = ChannelRegistry::new();
    registry.register_loggable::<task::execution_log::ExecutionLogMessage>();
    registry.register_loggable::<String>();
    registry.register_loggable::<u64>();

    let logging_build_step = Box::new(logging::log_build_step::LoggingBuildStep::new(
        config, registry,
    ));

    let mut node_registry = ChannelRegistry::new();
    let stop_signal_cell = Arc::new(OnceLock::new());
    let graph = TaskGraphBuilder::new()
        .add_pool(2, |p| {
            p.add_callback(
                test_tasks::IncrementingIntegerPublisher::build_callback_node(&mut node_registry),
            )
            .add_callback(test_tasks::FizzBuzzCalculator::build_callback_node(
                &mut node_registry,
            ))
            .add_callback(test_tasks::StringCollector::build_callback_node_lite())
        })
        .add_build_step(logging_build_step)
        .with_log_executions(true)
        .build()
        .map_err(|e| -> BuildError { e.to_string().into() })?;

    Ok(BuiltGraph {
        graph,
        stop_signal_cell,
    })
}

pub fn build_replay_graph(
    log_path: &std::path::Path,
    speed: f32,
    denylist: std::collections::HashSet<String>,
    stop_signal_cell: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
) -> Result<(BuiltGraph, live_replay_executor::LiveReplayConfig), BuildError> {
    let mut registry = ChannelRegistry::new();
    registry.register_channel::<u64>("integer".into());

    let registry = Arc::new(registry);
    let (replay_cfg, build_step) = live_replay_executor::build_replay(
        log_path.to_path_buf(),
        speed,
        registry,
        denylist,
        stop_signal_cell.clone(),
    )?;

    let mut node_registry = ChannelRegistry::new();
    let graph = TaskGraphBuilder::new()
        .add_pool(2, |p| {
            p.add_callback(test_tasks::FizzBuzzCalculator::build_callback_node(
                &mut node_registry,
            ))
            .add_callback(test_tasks::StringCollector::build_callback_node_lite())
        })
        .add_build_step(Box::new(build_step))
        .build()
        .map_err(|e| -> BuildError { e.to_string().into() })?;

    Ok((
        BuiltGraph {
            graph,
            stop_signal_cell,
        },
        replay_cfg,
    ))
}

/// Everything the exact replay executor needs to replay a recorded log: the
/// application callback nodes (rebuilt in the same global order as the
/// original run), a channel registry for deserialization/output capture, and
/// the parsed log file.
pub struct ExactReplayGraph {
    pub nodes: Vec<task::callback::CallbackNode>,
    pub registry: ChannelRegistry,
    pub log_reader: Box<dyn logging::log_file::LogFileReader>,
}

/// Builds the application callback graph for exact replay. Unlike the live
/// builder, this does **not** add logging tasks (they would truncate the log
/// being replayed) and does not enable execution logging.
pub fn build_exact_replay_graph(
    log_path: &std::path::Path,
) -> Result<ExactReplayGraph, BuildError> {
    // Register the channels that appear in the replay records. `register_channel`
    // provides the serializer (output capture), deserializer and publisher
    // factory (input hydration) the exact replay executor needs.
    let mut registry = ChannelRegistry::new();
    registry.register_channel::<u64>("integer".into());
    registry.register_channel::<String>("fizz_buzz_string".into());

    let mut node_registry = ChannelRegistry::new();
    let graph = TaskGraphBuilder::new()
        .add_pool(2, |p| {
            p.add_callback(
                test_tasks::IncrementingIntegerPublisher::build_callback_node(&mut node_registry),
            )
            .add_callback(test_tasks::FizzBuzzCalculator::build_callback_node(
                &mut node_registry,
            ))
            .add_callback(test_tasks::StringCollector::build_callback_node_lite())
        })
        .build()
        .map_err(|e| -> BuildError { e.to_string().into() })?;

    // Flatten pools into the same global node order used by the execution log
    // descriptor. The example uses a single pool, so this preserves the
    // application node indices 0..3 from the live run.
    let nodes = graph
        .pools
        .into_iter()
        .flat_map(|pool| pool.nodes)
        .collect::<Vec<_>>();

    let file = std::fs::File::open(log_path)?;
    let reader = std::io::BufReader::new(file);
    let log_reader = Box::new(logging::log_file_json::JsonLogFileReader::from_reader(
        reader,
    )?);

    Ok(ExactReplayGraph {
        nodes,
        registry,
        log_reader,
    })
}
