use std::sync::{Arc, OnceLock};

use task::channel_registry::ChannelRegistry;
use task::execution_log::ExecutionLogDescriptor;
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
    registry.register_channel::<ExecutionLogDescriptor>("execution_log_descriptor".into());

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
