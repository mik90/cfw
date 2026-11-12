//! Generates a C++ task bridge from `define_cxx_task!` declarations.
//!
//! Used by the Bazel `cfw_cxx_task` rule. Cargo builds call the library
//! directly from `build.rs`.

use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut tasks = None;
    let mut bridge_out = None;
    let mut combined_out = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("missing value for {arg}"))
        };
        match arg.as_str() {
            "--tasks" => tasks = Some(PathBuf::from(value()?)),
            "--bridge-out" => bridge_out = Some(PathBuf::from(value()?)),
            "--combined-out" => combined_out = Some(PathBuf::from(value()?)),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }

    let tasks = tasks.ok_or("missing --tasks")?;
    let bridge_out = bridge_out.ok_or("missing --bridge-out")?;
    let combined_out = combined_out.ok_or("missing --combined-out")?;

    task_cxx_codegen::generate_bridge_files(tasks, bridge_out, combined_out)
        .map_err(|error| error.to_string())
}
