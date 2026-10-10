use clap::Parser;
use exact_replay_executor::{DivergencePolicy, ExactReplayConfig};
use fizz_buzz_application::{BuildError, with_exact_replay};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Parser, Debug)]
struct CliArgs {
    #[arg(short, long)]
    log_path: PathBuf,
    #[arg(long)]
    best_effort: bool,
    #[arg(long)]
    print: bool,
}

fn main() -> Result<(), BuildError> {
    let args = CliArgs::parse();
    if args.print {
        fizz_buzz_application::print_graph(false, false);
        return Ok(());
    }
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, term.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, term.clone())?;
    with_exact_replay(
        &args.log_path,
        ExactReplayConfig {
            divergence_policy: if args.best_effort {
                DivergencePolicy::BestEffort
            } else {
                DivergencePolicy::Strict
            },
            ..Default::default()
        },
        |mut executor, collected| {
            while !term.load(Ordering::Relaxed) && executor.step()?.is_some() {}
            println!("{:#?}", executor.replay_report());
            println!("Collected {} strings", collected.len());
            Ok(())
        },
    )
}
