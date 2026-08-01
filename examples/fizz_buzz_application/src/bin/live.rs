use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fizz_buzz_application::build_live_graph_with;
use live_executor::LiveExecutor;
use task::executor::Executor;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct CliArgs {
    /// File to log to including file extension.
    #[arg(short, long, default_value = "./tmp/log.ndjson")]
    log_path: PathBuf,

    /// Do not log the intermediate `integer` channel, so exact replay must
    /// reproduce the integers by re-running the producer.
    #[arg(long)]
    no_log_integer: bool,

    /// Print the task graph and exit without running.
    #[arg(long)]
    print: bool,
}

fn main() {
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))
        .expect("Could not register signal hook");
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))
        .expect("Could not register signal hook");

    let args = CliArgs::parse();

    println!("Building fizz buzz callback nodes for live execution");

    let built = build_live_graph_with(&args.log_path, !args.no_log_integer)
        .expect("Could not build task graph");

    if args.print {
        built.graph.print();
        return;
    }

    let mut executor = LiveExecutor::new_multi_pool_with_execution_log(
        built.graph.pools,
        built.graph.execution_log_publishers,
        Duration::from_millis(500),
    );
    built.stop_signal_cell.set(executor.stop_signal()).ok();
    executor.start_threads();

    while !term.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("Received stop signal, stopping threads");

    executor.stop_threads().expect("Could not stop threads");
    println!("Done");
}
