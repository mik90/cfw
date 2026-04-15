use crate::callback;
use crate::callback::{ConnectedCallback, connect_callbacks};
use crate::executor::ExecutorStopSignal;
use crate::generic_publisher::GenericPublisher;
use crate::generic_subscriber::GenericSubscriber;
use crate::publisher;
use crate::subscriber;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub struct FizzBuzzTaskInfo {
    pub string_store: Arc<Mutex<Vec<String>>>,
    pub stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    pub integer_publisher_index: usize,
    pub fizz_buzz_index: usize,
    pub string_store_index: usize,
}

impl FizzBuzzTaskInfo {
    pub fn get_stored_strings(&self) -> Vec<String> {
        self.string_store.lock().unwrap().clone()
    }
}

pub fn build_fizz_buzz_tasks() -> (Vec<ConnectedCallback>, FizzBuzzTaskInfo) {
    let string_store = StringCollector::make_string_store();
    let stop_signal = Arc::new(OnceLock::new());

    let mut callbacks = vec![
        IncrementingIntegerPublisher::build_connected_callback(),
        FizzBuzzCalculator::build_connected_callback(),
        StringCollector::build_connected_callback(string_store.clone(), stop_signal.clone(), 1),
    ];
    let connect_result = connect_callbacks(&mut callbacks);
    assert!(
        connect_result.is_ok(),
        "Result was {}",
        connect_result.unwrap_err()
    );

    (
        callbacks,
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
    pub fn run(&mut self, mut output: publisher::Output<u64>) {
        println!("IncrementingIntegerPublisher run");
        *output = self.value;
        self.value += 1;
        output.send();
    }

    pub fn build_connected_callback() -> callback::ConnectedCallback {
        let callback: Box<dyn callback::GenericCallback> =
            Box::new(IncrementingIntegerPublisher { value: 0 });
        let subscribers = callback.build_subscribers();
        let mut publishers = callback.build_publishers();
        publishers[0].get_config_mut().channel_name = "integer".into();

        let mut callback = callback::ConnectedCallback::new_with(
            callback,
            subscribers,
            publishers,
            "IncrementingIntegerPublisher".into(),
        );
        callback.set_execution_time_callback(Box::new(|t| {
            Some(t + std::time::Duration::from_millis(500))
        }));
        callback.set_execution_duration_callback(Box::new(|| std::time::Duration::from_millis(1)));
        callback
    }
}

impl callback::GenericCallback for IncrementingIntegerPublisher {
    fn run_generic(
        &mut self,
        _subscribers: &mut [Box<dyn crate::subscriber::GenericSubscriber>],
        publishers: &mut [Box<dyn crate::publisher::GenericPublisher>],
        _ctx: &crate::context::Context,
    ) -> crate::callback::Run {
        self.run(publisher::Output::<u64>::new_downcasted(
            &mut *publishers[0],
        ));
        crate::callback::Run::new(1)
    }

    fn build_subscribers(&self) -> Vec<Box<dyn crate::subscriber::GenericSubscriber>> {
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
        integer: subscriber::RequiredInput<u64>,
        mut fizz_buzz_string: publisher::Output<String>,
    ) {
        println!("FizzBuzzCalculator run");
        let is_fizz = (*integer % 3) == 0;
        let is_buzz = (*integer % 5) == 0;
        let is_fizz_buzz = is_fizz && is_buzz;

        if is_fizz_buzz {
            *fizz_buzz_string = String::from("FizzBuzz");
        } else if is_fizz {
            *fizz_buzz_string = String::from("Fizz");
        } else if is_buzz {
            *fizz_buzz_string = String::from("Buzz");
        } else {
            *fizz_buzz_string = String::from(integer.to_string());
        }
        fizz_buzz_string.send();
    }
    pub fn build_connected_callback() -> callback::ConnectedCallback {
        let callback: Box<dyn callback::GenericCallback> = Box::new(FizzBuzzCalculator {});
        let mut subscribers = callback.build_subscribers();
        subscribers[0].get_config_mut().channel_name = "integer".into();

        let mut publishers = callback.build_publishers();
        publishers[0].get_config_mut().channel_name = "fizz_buzz_string".into();

        let mut callback = callback::ConnectedCallback::new_with(
            callback,
            subscribers,
            publishers,
            "FizzBuzzCalculator".into(),
        );
        callback.set_execution_duration_callback(Box::new(|| std::time::Duration::from_millis(5)));
        callback
    }
}

impl callback::GenericCallback for FizzBuzzCalculator {
    fn run_generic(
        &mut self,
        subscribers: &mut [Box<dyn crate::subscriber::GenericSubscriber>],
        publishers: &mut [Box<dyn crate::publisher::GenericPublisher>],
        _ctx: &crate::context::Context,
    ) -> crate::callback::Run {
        self.run(
            subscriber::RequiredInput::<u64>::new_downcasted(&mut *subscribers[0]),
            publisher::Output::<String>::new_downcasted(&mut *publishers[0]),
        );
        crate::callback::Run::new(1)
    }

    fn build_subscribers(&self) -> Vec<Box<dyn crate::subscriber::GenericSubscriber>> {
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
    pub fn run(&self, string: subscriber::RequiredInput<String>) {
        println!("StringCollector run");
        let mut store = self.string_store.lock().unwrap();
        store.push(string.clone());
        if store.len() >= self.target_count {
            if let Some(signal) = self.stop_signal.get() {
                signal.request_stop();
            }
        }
    }

    pub fn make_string_store() -> Arc<Mutex<Vec<String>>> {
        Arc::new(Mutex::new(vec![]))
    }

    pub fn build_connected_callback(
        string_store: Arc<Mutex<Vec<String>>>,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
        target_count: usize,
    ) -> callback::ConnectedCallback {
        let callback: Box<dyn callback::GenericCallback> = Box::new(StringCollector {
            string_store,
            stop_signal,
            target_count,
        });
        let mut subscribers = callback.build_subscribers();
        subscribers[0].get_config_mut().channel_name = "fizz_buzz_string".into();
        let publishers = callback.build_publishers();

        let mut callback = callback::ConnectedCallback::new_with(
            callback,
            subscribers,
            publishers,
            "StringCollector".into(),
        );
        callback.set_execution_duration_callback(Box::new(|| std::time::Duration::from_millis(2)));
        callback
    }
}

impl callback::GenericCallback for StringCollector {
    fn run_generic(
        &mut self,
        subscribers: &mut [Box<dyn crate::subscriber::GenericSubscriber>],
        _publishers: &mut [Box<dyn crate::publisher::GenericPublisher>],
        _ctx: &crate::context::Context,
    ) -> crate::callback::Run {
        self.run(subscriber::RequiredInput::<String>::new_downcasted(
            &mut *subscribers[0],
        ));
        crate::callback::Run::new(1)
    }

    fn build_subscribers(&self) -> Vec<Box<dyn crate::subscriber::GenericSubscriber>> {
        vec![Box::new(subscriber::Subscriber::<String>::new(
            callback::InputKind::Required.into(),
        ))]
    }
    fn build_publishers(&self) -> Vec<Box<dyn publisher::GenericPublisher>> {
        vec![]
    }
}

/// A minimal no-op task with no subscribers or publishers.
pub struct NoOpTask;

impl callback::GenericCallback for NoOpTask {
    fn run_generic(
        &mut self,
        _subscribers: &mut [Box<dyn GenericSubscriber>],
        _publishers: &mut [Box<dyn GenericPublisher>],
        _ctx: &crate::context::Context,
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

/// Build a [`ConnectedCallback`] wrapping a [`NoOpTask`] that reschedules itself
/// for the instant it finishes (period = 0), so it is always immediately re-ready.
pub fn build_no_op_callback() -> ConnectedCallback {
    let cb: Box<dyn callback::GenericCallback> = Box::new(NoOpTask);
    let subs = cb.build_subscribers();
    let pubs = cb.build_publishers();
    let mut connected = ConnectedCallback::new_with(cb, subs, pubs, "no-op".into());
    connected.set_execution_time_callback(Box::new(|t| Some(t)));
    connected.set_execution_duration_callback(Box::new(|| Duration::from_millis(1)));
    connected
}
