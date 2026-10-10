use exact_replay_executor::ExactReplayConfig;
use fizz_buzz_application::{replay_denylist, with_exact_replay_log, with_replay_graph};
use std::path::{Path, PathBuf};

fn fixture(unlogged: bool) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join(if unlogged {
            "fizz-buzz-log-integer-unlogged.ndjson"
        } else {
            "fizz-buzz-log.ndjson"
        })
}
fn expected() -> Vec<String> {
    (0..16).map(test_tasks::fizz_buzz).collect()
}

#[test]
fn exact_replays_both_fixtures() {
    for unlogged in [false, true] {
        let bytes = if unlogged {
            include_bytes!("../resources/fizz-buzz-log-integer-unlogged.ndjson").as_slice()
        } else {
            include_bytes!("../resources/fizz-buzz-log.ndjson").as_slice()
        };
        let reader = logging::log_file_json::JsonLogFileReader::from_reader(bytes).unwrap();
        let log = exact_replay_executor::ReplayLog::from_reader(&reader).unwrap();
        with_exact_replay_log(
            log,
            ExactReplayConfig::default(),
            |mut replay, collected| {
                let report = replay.run()?;
                assert!(report.is_exact(), "{report:?}");
                assert_eq!(report.consumed_executions(), 48);
                assert_eq!(collected.stored_strings(), expected());
                let integers = &report.channel_stats()["integer"];
                assert_eq!(integers.logged == 0, unlogged);
                assert_eq!(integers.reproduced > 0, unlogged);
                Ok(())
            },
        )
        .unwrap();
    }
}

#[test]
#[cfg_attr(miri, ignore = "sorted reader uses temporary filesystem merge files")]
fn simulated_replay_drains_both_fixtures() {
    for strings_only in [false, true] {
        with_replay_graph(strings_only, |graph, sources, collected| {
            let reader = logging::SortedLogStreamReader::from_path(&fixture(strings_only), 16)?;
            let mut replay = simulation_executor::LogSimulation::with_options(
                graph,
                reader,
                sources,
                simulation_executor::LogSimulationOptions {
                    denylist: replay_denylist(strings_only),
                    ..Default::default()
                },
            )?;
            replay.run_until_idle(128)?;
            assert!(replay.input_exhausted());
            assert_eq!(collected.stored_strings(), expected());
            Ok(())
        })
        .unwrap();
    }
}

#[test]
#[cfg_attr(miri, ignore = "wall-clock playback")]
fn live_replay_drains_both_fixtures() {
    for strings_only in [false, true] {
        with_replay_graph(strings_only, |graph, sources, collected| {
            let reader = logging::SortedLogStreamReader::from_path(&fixture(strings_only), 1024)?;
            let replay = live_replay_executor::LiveReplayExecutor::new(
                graph,
                reader,
                sources,
                live_replay_executor::LiveReplayConfig {
                    speed: 10.0,
                    denylist: replay_denylist(strings_only),
                    ..Default::default()
                },
            )?;
            let completion = replay.run()?;
            assert!(completion.input_exhausted && completion.drained);
            assert_eq!(collected.stored_strings(), expected());
            Ok(())
        })
        .unwrap();
    }
}
