#![doc = include_str!("../README.md")]

pub mod unit_test_executor;
pub use unit_test_executor::{
    DEFAULT_TEST_SUBSCRIBER_CAPACITY, DroppedMessages, StepResult, TestPublisher, TestSubscriber,
    UnitTestExecutor, UnitTestExecutorBuilder, UnitTestExecutorConfig,
};
#[cfg(feature = "iceoryx2")]
pub use unit_test_executor::{Iox2TestNotifier, Iox2TestPublisher, Iox2TestSubscriber};
