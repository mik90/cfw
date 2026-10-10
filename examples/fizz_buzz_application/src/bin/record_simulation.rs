use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    #[arg(short, long)]
    log_path: PathBuf,
    #[arg(long, default_value_t = 16)]
    count: usize,
    #[arg(long)]
    no_log_integer: bool,
}

fn main() -> Result<(), fizz_buzz_application::BuildError> {
    let args = Args::parse();
    let values =
        fizz_buzz_application::record_simulation(&args.log_path, !args.no_log_integer, args.count)?;
    println!("Recorded {} strings", values.len());
    Ok(())
}
