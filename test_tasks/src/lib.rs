use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use task::callback::{Callback, CallbackNode, PubOrSub, PubOrSubMut};
use task::callback_builder::CallbackBuilder;
use task::callback_storage::CallbackStorage;
use task::context::Context;
use task::executor::ExecutorStopSignal;
use task::input::RequiredInput;
use task::iox2::{Iox2Event, Iox2NotifyOutput, Iox2OptionalInput, Iox2Output, Iox2SpanInput};
use task::output::Output;
use task::task_graph_builder::TaskGraphBuilder;
use task_macros::task_callback;

pub struct FizzBuzzTaskInfo {
    string_store: Arc<Mutex<Vec<String>>>,
    pub stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    pub integer_publisher_index: usize,
    pub fizz_buzz_index: usize,
    pub string_store_index: usize,
}

impl FizzBuzzTaskInfo {
    const INTEGER_CHANNEL: &'static str = "integer";
    const FIZZ_BUZZ_STRING_CHANNEL: &'static str = "fizz_buzz_string";
    pub fn stored_strings(&self) -> Vec<String> {
        self.string_store.lock().unwrap().clone()
    }
}

pub fn build_fizz_buzz_callback_nodes() -> (CallbackStorage, FizzBuzzTaskInfo) {
    let string_store = StringCollector::make_string_store();
    let stop_signal = Arc::new(OnceLock::new());

    let build_result = TaskGraphBuilder::new()
        .add_pool(1, |p| {
            p.add_callback(IncrementingIntegerPublisher::build_callback_node())
                .add_callback(FizzBuzzCalculator::build_callback_node())
                .add_callback(StringCollector::build_callback_node(
                    string_store.clone(),
                    stop_signal.clone(),
                    1,
                ))
        })
        .build();

    let nodes = match build_result {
        Ok(mut result) => result.pools.remove(0).nodes,
        Err(err) => panic!("Build result was {}", err),
    };

    (
        nodes,
        FizzBuzzTaskInfo {
            string_store,
            stop_signal,
            integer_publisher_index: 0,
            fizz_buzz_index: 1,
            string_store_index: 2,
        },
    )
}

pub struct IncrementingIntegerPublisher {
    value: u64,
}
#[task_callback]
impl IncrementingIntegerPublisher {
    fn run(&mut self, #[channel(FizzBuzzTaskInfo::INTEGER_CHANNEL)] mut output: Output<u64>) {
        println!("IncrementingIntegerPublisher run");
        *output = self.value;
        self.value += 1;
        output.send();
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_periodic_execution(std::time::Duration::from_millis(500))
            .with_execution_duration_callback(|| std::time::Duration::from_millis(1))
    }

    pub fn build_callback_node() -> CallbackNode {
        IncrementingIntegerPublisher { value: 0 }
            .callback_builder()
            .build()
            .unwrap()
    }
}

pub struct FizzBuzzCalculator {}
#[task_callback]
impl FizzBuzzCalculator {
    fn run(
        &mut self,
        #[channel(FizzBuzzTaskInfo::INTEGER_CHANNEL)] integer: RequiredInput<u64>,
        #[channel(FizzBuzzTaskInfo::FIZZ_BUZZ_STRING_CHANNEL)] mut fizz_buzz_string: Output<String>,
    ) {
        println!("FizzBuzzCalculator run");
        let is_fizz = (*integer).is_multiple_of(3);
        let is_buzz = (*integer).is_multiple_of(5);
        let is_fizz_buzz = is_fizz && is_buzz;

        if is_fizz_buzz {
            *fizz_buzz_string = String::from("FizzBuzz");
        } else if is_fizz {
            *fizz_buzz_string = String::from("Fizz");
        } else if is_buzz {
            *fizz_buzz_string = String::from("Buzz");
        } else {
            *fizz_buzz_string = integer.to_string();
        }
        fizz_buzz_string.send();
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_execution_duration_callback(|| std::time::Duration::from_millis(5))
    }

    pub fn build_callback_node() -> CallbackNode {
        FizzBuzzCalculator {}.callback_builder().build().unwrap()
    }
}

pub struct StringCollector {
    string_store: Arc<Mutex<Vec<String>>>,
    stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    target_count: usize,
}
#[task_callback]
impl StringCollector {
    fn run(
        &self,
        #[channel(FizzBuzzTaskInfo::FIZZ_BUZZ_STRING_CHANNEL)] string: RequiredInput<String>,
    ) {
        println!("StringCollector run");
        let mut store = self.string_store.lock().unwrap();
        store.push(string.clone());
        if store.len() >= self.target_count
            && let Some(signal) = self.stop_signal.get()
        {
            signal.request_stop();
        }
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_execution_duration_callback(|| std::time::Duration::from_millis(2))
    }

    pub fn make_string_store() -> Arc<Mutex<Vec<String>>> {
        Arc::new(Mutex::new(vec![]))
    }

    pub fn build_callback_node(
        string_store: Arc<Mutex<Vec<String>>>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target_count: usize,
    ) -> CallbackNode {
        StringCollector {
            string_store,
            stop_signal,
            target_count,
        }
        .callback_builder()
        .build()
        .unwrap()
    }

    pub fn build_callback_node_lite() -> CallbackNode {
        let string_store = StringCollector::make_string_store();
        let stop_signal = Arc::new(OnceLock::new());
        StringCollector {
            string_store,
            stop_signal,
            target_count: usize::MAX,
        }
        .callback_builder()
        .build()
        .unwrap()
    }
}

pub struct NoOpCallback;

impl Callback for NoOpCallback {
    fn run(&mut self, _ctx: &Context) {}
    fn for_each_pub_or_sub<'a>(&'a self, _f: &mut dyn FnMut(PubOrSub<'a>)) {}
    fn for_each_pub_or_sub_mut<'a>(&'a mut self, _f: &mut dyn FnMut(PubOrSubMut<'a>)) {}
}

pub fn build_no_op_callback_node() -> CallbackNode {
    CallbackBuilder::new("no-op".into(), Box::new(NoOpCallback))
        .with_execution_duration_callback(|| Duration::from_millis(1))
        .with_next_execution_time_callback(Some)
        .build()
        .unwrap()
}

/// Example callback declaring all five macro-supported iox2 views.
pub struct Iox2MacroDemo;

#[task_callback]
impl Iox2MacroDemo {
    fn run(
        &mut self,
        sensor: Iox2OptionalInput<u64>,
        history: Iox2SpanInput<u64>,
        events: Iox2Event,
        mut output: Iox2Output<u64>,
        signal: Iox2NotifyOutput,
        required: RequiredInput<u64>,
    ) {
        if let Some(value) = sensor.value() {
            *output = *value + history.len() as u64 + events.count() + *required;
            output.send();
        }
        signal.send();
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_execution_duration_callback(|| Duration::from_millis(1))
    }

    /// Construct this demo's generated callback builder.
    pub fn build_callback_builder() -> CallbackBuilder {
        Iox2MacroDemo
            .callback_builder()
            .with_subscriber_channels(&[
                "macro_sensor",
                "macro_history",
                "macro_events",
                "macro_required",
            ])
            .with_publisher_channels(&["macro_output", "macro_signal"])
    }
}

#[cfg(all(test, not(miri)))]
mod iox2_macro_tests {
    use super::*;
    use task::callback::{PubOrSub, PubOrSubMut};
    use task::publisher::{Publisher, PublisherConfig};
    use task::subscriber::{Subscriber, SubscriberConfig};

    struct NativePublisherNode(Publisher<u64>);
    impl Callback for NativePublisherNode {
        fn run(&mut self, _ctx: &Context) {
            let mut output = Output::new_default(&mut self.0);
            *output = 1;
            output.send();
        }
        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Publisher(&self.0));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Publisher(&mut self.0));
        }
    }

    struct NativeSubscriberNode(Subscriber<u64>);
    impl Callback for NativeSubscriberNode {
        fn run(&mut self, _ctx: &Context) {}
        fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
            f(PubOrSub::Subscriber(&self.0));
        }
        fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
            f(PubOrSubMut::Subscriber(&mut self.0));
        }
    }

    /// The generated iox2 endpoints coexist with a native producer/consumer pair in one graph.
    #[test]
    fn macro_generated_iox2_fields_build_with_native_channels() {
        let result = TaskGraphBuilder::new()
            .add_pool(1, |pool| {
                pool.add_callback_builder(Iox2MacroDemo::build_callback_builder())
                    .add_callback_builder(
                        CallbackBuilder::new(
                            "native_producer".into(),
                            Box::new(NativePublisherNode(Publisher::new(PublisherConfig {
                                capacity: 1,
                                channel_name: "macro_required".into(),
                            }))),
                        )
                        .with_publisher_channels(&["macro_required"])
                        .with_execution_duration_callback(|| Duration::from_millis(1)),
                    )
                    .add_callback_builder(
                        CallbackBuilder::new(
                            "native_consumer".into(),
                            Box::new(NativeSubscriberNode(Subscriber::new(SubscriberConfig {
                                is_optional: true,
                                capacity: 1,
                                is_trigger: false,
                                keep_across_runs: true,
                                channel_name: "macro_required".into(),
                            }))),
                        )
                        .with_subscriber_channels(&["macro_required"])
                        .with_execution_duration_callback(|| Duration::from_millis(1)),
                    )
            })
            .build();
        assert!(
            result.is_ok(),
            "macro generated iox2 graph failed: {result:?}"
        );
    }
}
