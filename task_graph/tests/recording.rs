use task::{
    LoanError,
    recording::{ExecutionRecorder, RecordingMode, RecordingOptions},
    time::FrameworkTime,
};
use task_graph::GraphBuilder;

#[test]
fn unknown_override_leaves_recorder_available_and_successful_attachment_is_exclusive() {
    let recorder = ExecutionRecorder::new(1);
    let result = GraphBuilder::new().build().unwrap().with_recording_options(
        &recorder,
        RecordingOptions::default().with_callback("missing", RecordingMode::Off),
    );
    assert!(matches!(result, Err(error) if error == "unknown recording callback 'missing'"));
    assert!(recorder.intern_tables().is_none());
    assert!(recorder.descriptor().is_none());

    let graph = GraphBuilder::new()
        .build()
        .unwrap()
        .with_recording(&recorder)
        .unwrap();
    assert!(recorder.descriptor().unwrap().callbacks.is_empty());
    drop(graph);

    let result = GraphBuilder::new()
        .build()
        .unwrap()
        .with_recording(&recorder);
    assert!(matches!(result, Err(error) if error == "recorder already attached"));
}

#[test]
fn recording_preserves_borrowed_callbacks_and_ids_across_disabled_callbacks() {
    let recorder = ExecutionRecorder::new(1);
    let mut disabled_calls = 0;
    let mut recorded_calls = 0;
    {
        let mut builder = GraphBuilder::new();
        builder.add_callback("disabled", || {
            Ok(|_: FrameworkTime| {
                disabled_calls += 1;
                Ok::<(), LoanError>(())
            })
        });
        builder.add_callback("recorded", || {
            Ok(|_: FrameworkTime| {
                recorded_calls += 1;
                Ok::<(), LoanError>(())
            })
        });
        let graph = builder
            .build()
            .unwrap()
            .with_recording_options(
                &recorder,
                RecordingOptions::new(RecordingMode::DurationOnly)
                    .with_callback("disabled", RecordingMode::Off),
            )
            .unwrap();
        testing::BoundUnitTestExecutorBuilder::with_config(
            graph,
            testing::UnitTestExecutorConfig {
                virtual_pool_threads: vec![2],
                ..Default::default()
            },
        )
        .schedule_all(
            task_graph::CallbackSchedule::on_start()
                .with_execution_duration(std::time::Duration::from_nanos(1)),
        )
        .start_time(FrameworkTime::from_nanoseconds(42))
        .build()
        .run_until_idle(8)
        .unwrap();
    }
    assert_eq!((disabled_calls, recorded_calls), (1, 1));
    let descriptor = recorder.descriptor().unwrap();
    assert_eq!(descriptor.callbacks[0].name, "disabled");
    assert_eq!(descriptor.callbacks[1].name, "recorded");
    let records = recorder.drain();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].execution_time,
        FrameworkTime::from_nanoseconds(42)
    );
    assert_eq!(
        Some(records[0].callback_id),
        recorder
            .intern_tables()
            .unwrap()
            .callbacks
            .lookup_by_value("recorded"),
    );
}
