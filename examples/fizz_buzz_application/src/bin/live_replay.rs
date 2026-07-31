use clap::Parser;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fizz_buzz_application::build_replay_graph;
use live_replay_executor::LiveReplayExecutor;
use task::executor::Executor;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct CliArgs {
    /// Path to the JSON log file to replay.
    #[arg(short, long)]
    log_path: PathBuf,

    /// Playback speed multiplier (1.0 = real-time, 2.0 = double speed).
    #[arg(short, long, default_value = "1.0")]
    speed: f32,

    /// Comma-separated channel names to exclude from replay.
    #[arg(long, default_value = "fizz_buzz_string,execution_log")]
    denylist: String,

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
    let denylist: HashSet<String> = args
        .denylist
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    println!(
        "Building fizz buzz callback nodes for live replay (speed: {:.1}x)",
        args.speed
    );

    let stop_signal_cell = Arc::new(std::sync::OnceLock::new());
    let (built, replay_cfg) = build_replay_graph(
        &args.log_path,
        args.speed,
        denylist,
        stop_signal_cell.clone(),
    )
    .expect("Could not build replay task graph");

    if args.print {
        built.graph.print();
        return;
    }

    let mut executor = LiveReplayExecutor::new_with_execution_log(
        built.graph.pools,
        built.graph.execution_log_publishers,
        Duration::from_millis(500),
        replay_cfg,
    );
    built.stop_signal_cell.set(executor.stop_signal()).ok();
    executor.start();

    while !term.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("Received stop signal, stopping threads");

    let stop_result = executor.stop();
    println!("Done: {:?}", stop_result);
}
