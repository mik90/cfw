use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use task::callback;
use task::callback::CallbackNode;
use task::callback_builder::CallbackBuilder;
use task::executor::ExecutorStopSignal;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::input;
use task::output;
use task::publisher;
use task::subscriber;
use task::task_graph_builder::TaskGraphBuilder;

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

pub fn build_fizz_buzz_callback_nodes() -> (Vec<CallbackNode>, FizzBuzzTaskInfo) {
    let string_store = StringCollector::make_string_store();
    let stop_signal = Arc::new(OnceLock::new());

    let build_result = TaskGraphBuilder::new()
        .add_callback(IncrementingIntegerPublisher::build_callback_node())
        .add_callback(FizzBuzzCalculator::build_callback_node())
        .add_callback(StringCollector::build_callback_node(
            string_store.clone(),
            stop_signal.clone(),
            1,
        ))
        .build();

    let nodes = match build_result {
        Ok(result) => result.nodes,
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
impl IncrementingIntegerPublisher {
    pub fn run(&mut self, mut output: output::Output<u64>) {
        println!("IncrementingIntegerPublisher run");
        *output = self.value;
        self.value += 1;
        output.send();
    }

    pub fn build_callback_node() -> callback::CallbackNode {
        CallbackBuilder::new(
            "IncrementingIntegerPublisher".into(),
            Box::new(IncrementingIntegerPublisher { value: 0 }),
        )
        .with_publisher_channels(&[FizzBuzzTaskInfo::INTEGER_CHANNEL])
        .with_next_execution_time_callback(|t| Some(t + std::time::Duration::from_millis(500)))
        .with_execution_duration_callback(|| std::time::Duration::from_millis(1))
        .build()
        .unwrap()
    }
}

impl callback::Callback for IncrementingIntegerPublisher {
    fn run_generic(
        &mut self,
        _subscribers: &mut [Box<dyn task::subscriber::GenericSubscriber>],
        publishers: &mut [Box<dyn task::publisher::GenericPublisher>],
        _ctx: &task::context::Context,
    ) -> task::callback::Run {
        self.run(output::Output::<u64>::new_downcasted(&mut *publishers[0]));
        task::callback::Run::new(1)
    }

    fn build_subscribers(&self) -> Vec<Box<dyn task::subscriber::GenericSubscriber>> {
        vec![]
    }
    fn build_publishers(&self) -> Vec<Box<dyn publisher::GenericPublisher>> {
        vec![Box::new(publisher::Publisher::<u64>::new(
            callback::OutputKind::Default.into(),
        ))]
    }
}

pub struct FizzBuzzCalculator {}
impl FizzBuzzCalculator {
    pub fn run(
        &mut self,
        integer: input::RequiredInput<u64>,
        mut fizz_buzz_string: output::Output<String>,
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
    pub fn build_callback_node() -> callback::CallbackNode {
        CallbackBuilder::new("FizzBuzzCalculator".into(), Box::new(FizzBuzzCalculator {}))
            .with_subscriber_channels(&[FizzBuzzTaskInfo::INTEGER_CHANNEL])
            .with_publisher_channels(&[FizzBuzzTaskInfo::FIZZ_BUZZ_STRING_CHANNEL])
            .with_execution_duration_callback(|| std::time::Duration::from_millis(5))
            .build()
            .unwrap()
    }
}

impl callback::Callback for FizzBuzzCalculator {
    fn run_generic(
        &mut self,
        subscribers: &mut [Box<dyn task::subscriber::GenericSubscriber>],
        publishers: &mut [Box<dyn task::publisher::GenericPublisher>],
        _ctx: &task::context::Context,
    ) -> task::callback::Run {
        self.run(
            input::RequiredInput::<u64>::new_downcasted(&mut *subscribers[0]),
            output::Output::<String>::new_downcasted(&mut *publishers[0]),
        );
        task::callback::Run::new(1)
    }

    fn build_subscribers(&self) -> Vec<Box<dyn task::subscriber::GenericSubscriber>> {
        vec![Box::new(subscriber::Subscriber::<u64>::new(
            callback::InputKind::Required.into(),
        ))]
    }
    fn build_publishers(&self) -> Vec<Box<dyn publisher::GenericPublisher>> {
        vec![Box::new(publisher::Publisher::<String>::new(
            callback::OutputKind::Default.into(),
        ))]
    }
}
pub struct StringCollector {
    string_store: Arc<Mutex<Vec<String>>>,
    stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    target_count: usize,
}
impl StringCollector {
    pub fn run(&self, string: input::RequiredInput<String>) {
        println!("StringCollector run");
        let mut store = self.string_store.lock().unwrap();
        store.push(string.clone());
        if store.len() >= self.target_count
            && let Some(signal) = self.stop_signal.get()
        {
            signal.request_stop();
        }
    }

    pub fn make_string_store() -> Arc<Mutex<Vec<String>>> {
        Arc::new(Mutex::new(vec![]))
    }

    pub fn build_callback_node(
        string_store: Arc<Mutex<Vec<String>>>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target_count: usize,
    ) -> callback::CallbackNode {
        CallbackBuilder::new(
            "StringCollector".into(),
            Box::new(StringCollector {
                string_store,
                stop_signal,
                target_count,
            }),
        )
        .with_subscriber_channels(&[FizzBuzzTaskInfo::FIZZ_BUZZ_STRING_CHANNEL])
        .with_execution_duration_callback(|| std::time::Duration::from_millis(2))
        .build()
        .unwrap()
    }
}

impl callback::Callback for StringCollector {
    fn run_generic(
        &mut self,
        subscribers: &mut [Box<dyn task::subscriber::GenericSubscriber>],
        _publishers: &mut [Box<dyn task::publisher::GenericPublisher>],
        _ctx: &task::context::Context,
    ) -> task::callback::Run {
        self.run(input::RequiredInput::<String>::new_downcasted(
            &mut *subscribers[0],
        ));
        task::callback::Run::new(1)
    }

    fn build_subscribers(&self) -> Vec<Box<dyn task::subscriber::GenericSubscriber>> {
        vec![Box::new(subscriber::Subscriber::<String>::new(
            callback::InputKind::Required.into(),
        ))]
    }
    fn build_publishers(&self) -> Vec<Box<dyn publisher::GenericPublisher>> {
        vec![]
    }
}

/// A minimal no-op callback with no subscribers or publishers.
pub struct NoOpCallback;

impl callback::Callback for NoOpCallback {
    fn run_generic(
        &mut self,
        _subscribers: &mut [Box<dyn GenericSubscriber>],
        _publishers: &mut [Box<dyn GenericPublisher>],
        _ctx: &task::context::Context,
    ) -> callback::Run {
        callback::Run::new(1)
    }
    fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
        vec![]
    }
    fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
        vec![]
    }
}

/// Build a [`CallbackNode`] wrapping a [`NoOpCallback`] that reschedules itself
/// for the instant it finishes (period = 0), so it is always immediately re-ready.
pub fn build_no_op_callback_node() -> CallbackNode {
    CallbackBuilder::new("no-op".into(), Box::new(NoOpCallback))
        .with_execution_duration_callback(|| Duration::from_millis(1))
        .with_next_execution_time_callback(Some) // forward the time we're given to execute immediately
        .build()
        .unwrap()
}
