use std::sync::{Arc, Mutex};
use std::time::Duration;
use task::{CallbackSchedule, Output, RequiredInput};
use task_macros::task_callback;

pub const INTEGER_CHANNEL: &str = "integer";
pub const FIZZ_BUZZ_STRING_CHANNEL: &str = "fizz_buzz_string";

#[derive(Default)]
pub struct IncrementingIntegerPublisher {
    value: u64,
}

#[task_callback]
impl IncrementingIntegerPublisher {
    fn run(&mut self, #[channel(INTEGER_CHANNEL)] mut output: Output<u64>) {
        *output = self.value;
        self.value += 1;
        output.send();
    }

    pub fn schedule() -> CallbackSchedule {
        CallbackSchedule::periodic(Duration::from_millis(500))
            .with_execution_duration(Duration::from_millis(1))
    }
}

pub fn fizz_buzz(value: u64) -> String {
    match (value.is_multiple_of(3), value.is_multiple_of(5)) {
        (true, true) => "FizzBuzz".into(),
        (true, false) => "Fizz".into(),
        (false, true) => "Buzz".into(),
        (false, false) => value.to_string(),
    }
}

pub struct FizzBuzzCalculator;

#[task_callback]
impl FizzBuzzCalculator {
    fn run(
        &self,
        #[channel(INTEGER_CHANNEL)] integer: RequiredInput<u64>,
        #[channel(FIZZ_BUZZ_STRING_CHANNEL)] mut fizz_buzz_string: Output<String>,
    ) {
        *fizz_buzz_string = fizz_buzz(*integer);
        fizz_buzz_string.send();
    }

    pub fn schedule() -> CallbackSchedule {
        CallbackSchedule::default().with_execution_duration(Duration::from_millis(5))
    }
}

#[derive(Default, Clone)]
pub struct StringCollector {
    strings: Arc<Mutex<Vec<String>>>,
}

#[task_callback]
impl StringCollector {
    fn run(&self, #[channel(FIZZ_BUZZ_STRING_CHANNEL)] string: RequiredInput<String>) {
        self.strings.lock().unwrap().push(string.clone());
    }

    pub fn stored_strings(&self) -> Vec<String> {
        self.strings.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.strings.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn schedule() -> CallbackSchedule {
        CallbackSchedule::default().with_execution_duration(Duration::from_millis(2))
    }
}

pub struct NoOpCallback;

#[task_callback]
impl NoOpCallback {
    fn run(&self) {}
}

/// A mixed native/IPC task declaring all five iceoryx2 views.
pub struct Iox2MacroDemo;

#[task_callback]
impl Iox2MacroDemo {
    fn run(
        &self,
        #[channel("macro_sensor")] sensor: task::iox2::Iox2OptionalInput<u64>,
        #[channel("macro_history")] history: task::iox2::Iox2SpanInput<u64>,
        #[channel("macro_events")] events: task::iox2::Iox2Event,
        #[channel("macro_output")] mut output: task::iox2::Iox2Output<u64>,
        #[channel("macro_signal")] signal: task::iox2::Iox2NotifyOutput,
        #[channel("macro_required")] required: RequiredInput<u64>,
    ) {
        if let Some(value) = sensor.value() {
            *output = *value + history.len() as u64 + events.count() + *required;
            output.send();
        }
        signal.send();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testing::UnitTestExecutorBuilder;

    #[test]
    fn fizz_buzz_pipeline() {
        let collector = StringCollector::default();
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("calculator", FizzBuzzCalculator, Duration::from_millis(5));
        builder.add_task("collector", collector.clone(), Duration::from_millis(2));
        let mut input = builder.add_test_publisher::<u64>(INTEGER_CHANNEL);
        builder.run(|mut executor| {
            for value in 0..16 {
                input.send(value);
                executor.step();
                executor.step();
            }
        });
        assert_eq!(
            collector.stored_strings(),
            (0..16).map(fizz_buzz).collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg(not(miri))]
    fn mixed_native_ipc_declarations_build() {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task("demo", Iox2MacroDemo, Duration::from_millis(1));
        let _input = builder.add_test_publisher::<u64>("macro_required");
        builder.run(|_| {});
    }
}
