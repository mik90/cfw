use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use exact_replay_executor::{DivergencePolicy, ExactReplayConfig, ExactReplayExecutor};
use fizz_buzz_application::build_exact_replay_graph;
use task::executor::Executor;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct CliArgs {
    /// Path to the NDJSON log file to replay.
    #[arg(short, long)]
    log_path: PathBuf,

    /// Continue past output mismatches instead of stopping on the first one.
    #[arg(long)]
    best_effort: bool,

    /// Print the callback graph and exit without running.
    #[arg(long)]
    print: bool,
}

fn print_nodes(pools: &[task::executor::ThreadPoolConfig]) {
    let mut index = 0;
    for pool in pools {
        for node in pool.nodes.iter_shared() {
            // Build time: the graph is not running yet.
            node.access(|node| {
                println!("node[{index}] '{}'", node.name());
                node.callback().for_each_subscriber(&mut |s| {
                    println!("\t subscriber -> {}", s.config().channel_name);
                });
                node.callback().for_each_publisher(&mut |p| {
                    println!("\t publisher  -> {}", p.config().channel_name);
                });
            });
            index += 1;
        }
    }
}

fn main() {
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))
        .expect("Could not register signal hook");
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))
        .expect("Could not register signal hook");

    let args = CliArgs::parse();

    println!("Building fizz buzz callback nodes for exact replay");
    let graph =
        build_exact_replay_graph(&args.log_path).expect("Could not build exact replay graph");

    if args.print {
        print_nodes(graph.executor_params.pools());
        return;
    }

    let config = ExactReplayConfig::new(graph.executor_params, graph.registry, graph.log_reader);
    let config = if args.best_effort {
        config.with_divergence_policy(DivergencePolicy::BestEffort)
    } else {
        config
    };

    let mut executor =
        ExactReplayExecutor::new(config).expect("Could not construct exact replay executor");
    executor.start();

    // Exact replay completes naturally when every logged execution has been
    // replayed; the signal loop just lets an operator stop it early.
    while executor.is_running() && !term.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
    if term.load(Ordering::Relaxed) {
        println!("Received stop signal, stopping replay");
    }

    let stop_result = executor.stop();
    println!("Done: {:?}", stop_result);

    let replay_errors = executor.replay_errors();
    println!(
        "Replayed {} of {} executions with {} replay error(s)",
        executor.consumed_count(),
        executor.execution_count(),
        replay_errors.len()
    );
    for error in replay_errors {
        println!("  - {error}");
    }

    let report = executor.replay_report();
    println!(
        "Reproduction: exact={} ratio={:.3} logged={} reproduced={} mismatches={} gaps={}",
        report.is_exact(),
        report.exact_reproduction_ratio(),
        report.logged_count(),
        report.reproduced_count(),
        report.mismatch_count(),
        report.gap_count()
    );
    for (channel, stats) in report.channel_stats() {
        println!(
            "  channel '{channel}': logged={} reproduced={} mismatches={} gaps={}",
            stats.logged, stats.reproduced, stats.mismatches, stats.gaps
        );
    }
    for detail in report.mismatch_details() {
        println!("  mismatch: {detail}");
    }
}
