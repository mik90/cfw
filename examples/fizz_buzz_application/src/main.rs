use clap::Parser;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use logging::{self, ChannelRegistry};
use std::path::PathBuf;
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
    let registry = ChannelRegistry::new();
    let logging_build_step = Box::new(logging::log_build_step::LoggingBuildStep::new(
        config, registry,
    ));
    let graph = TaskGraphBuilder::new()
        .add_callback(test_tasks::IncrementingIntegerPublisher::build_callback_node())
        .add_callback(test_tasks::FizzBuzzCalculator::build_callback_node())
        .add_callback(test_tasks::StringCollector::build_callback_node_lite())
        .add_build_step(logging_build_step)
        .build()
        .expect("Could not build task graph");
    let mut executor = live_executor::LiveExecutor::new(thread_count, graph.nodes);
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
