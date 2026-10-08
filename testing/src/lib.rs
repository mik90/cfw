#![doc = include_str!("../README.md")]

pub mod builder;
pub mod unit_test_executor;
#[cfg(feature = "iceoryx2")]
pub use builder::TestNotifier;
pub use builder::{
    ExecutionDuration, TestInput, TestOutput, UnitTestExecutorBuilder, UnitTestSetup,
};
pub use task::automatic::BuildError as TestBuildError;
pub use unit_test_executor::{
    BoundUnitTestExecutorBuilder, DEFAULT_TEST_SUBSCRIBER_CAPACITY, DroppedMessages, StepResult,
    TestPublisher, TestSubscriber, UnitTestExecutor, UnitTestExecutorConfig,
};
#[cfg(feature = "iceoryx2")]
pub use unit_test_executor::{Iox2TestNotifier, Iox2TestPublisher, Iox2TestSubscriber};
