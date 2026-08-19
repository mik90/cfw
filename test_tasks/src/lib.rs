use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use task::callback::{Callback, CallbackNode, PortMut, Run};
use task::callback_builder::CallbackBuilder;
use task::callback_storage::CallbackStorage;
use task::context::Context;
use task::executor::ExecutorStopSignal;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::input::RequiredInput;
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
    fn run(&mut self, _ctx: &Context) -> Run {
        Run::new(1)
    }
    fn for_each_subscriber<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {}
    fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
    fn for_each_subscriber_mut<'a>(
        &'a mut self,
        _f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
    ) {
    }
    fn for_each_publisher_mut<'a>(&'a mut self, _f: &mut dyn FnMut(&'a mut dyn GenericPublisher)) {}
    fn for_each_port_mut<'a>(&'a mut self, _f: &mut dyn FnMut(PortMut<'a>)) {}
}

pub fn build_no_op_callback_node() -> CallbackNode {
    CallbackBuilder::new("no-op".into(), Box::new(NoOpCallback))
        .with_execution_duration_callback(|| Duration::from_millis(1))
        .with_next_execution_time_callback(Some)
        .build()
        .unwrap()
}
