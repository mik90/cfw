use std::process::Command;

#[test]
#[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
fn two_processes_print_every_value_and_final_metrics() {
    let output = Command::new(env!("CARGO_BIN_EXE_iceoryx2_application"))
        .args(["--count", "5"])
        .output()
        .expect("start launcher");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("UTF-8 output");
    let results: Vec<_> = stdout
        .lines()
        .filter_map(|line| {
            let result = line
                .split_once(" (fizz=")
                .map_or(line, |(result, _)| result);
            (matches!(result, "Fizz" | "Buzz" | "FizzBuzz")
                || result
                    .parse::<u64>()
                    .is_ok_and(|value| (1..=5).contains(&value)))
            .then_some(result)
        })
        .collect();
    assert_eq!(results, ["1", "2", "Fizz", "4", "Buzz"]);
    assert!(stdout.contains("Total: 5 (fizz=1, buzz=1, fizzbuzz=0, numbers=3)"));
}
