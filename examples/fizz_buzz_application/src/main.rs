use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use logging::{self, ChannelRegistry};
use task::task_graph_builder::TaskGraphBuilder;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct CliArgs {
    /// File to log to including file extension.
    #[arg(short, long)]
    log_path: PathBuf,
}

fn main() {
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))
        .expect("Could not register signal hook");
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))
        .expect("Could not register signal hook");

    let thread_count = 2;
    println!(
        "Building fizz buzz callback nodes with {} threads",
        thread_count
    );

    let args = CliArgs::parse();
    let config = logging::LogTaskConfiguration {
        output_path: args.log_path,
        period: std::time::Duration::from_millis(1000),
        num_tasks: 1,
    };
    let mut registry = ChannelRegistry::new();
    registry.register_loggable::<task::execution_log::ExecutionLogMessage>();
    let logging_build_step = Box::new(logging::log_build_step::LoggingBuildStep::new(
        config, registry,
    ));
    let mut node_registry = task::channel_registry::ChannelRegistry::new();
    let graph = TaskGraphBuilder::new()
        .add_pool(thread_count, |p| {
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
        .expect("Could not build task graph");

    // No node here subscribes to the execution-log channel, so the
    // publishers have no subscriber — they loan log messages and
    // harmlessly discard them on flush. Subscribe a node to `execution_log`
    // (by registering `ExecutionLogMessage` in the `ChannelRegistry`) to
    // actually drain them.
    let mut executor = live_executor::LiveExecutor::new_multi_pool_with_execution_log(
        graph.pools,
        graph.execution_log_publishers,
        Duration::from_millis(500),
    );
    executor.start_threads();

    while !term.load(Ordering::Relaxed) {
        // Do some time-limited stuff here
        // (if this could block forever, then there's no guarantee the signal will have any
        // effect).
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("Recieved stop signal, stopping threads");

    executor.stop_threads().expect("Could not stop threads");
    println!("Done");
}
