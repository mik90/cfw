use crate::callback::{Callback, CallbackNode};
use crate::generic_publisher::GenericPublisher;
use crate::generic_subscriber::GenericSubscriber;
use crate::pub_sub::CallbackNodeName;
use crate::time::FrameworkTime;
use std::fmt;
use std::time::Duration;

pub struct CallbackBuilder {
    subscribers: Vec<Box<dyn GenericSubscriber>>,
    publishers: Vec<Box<dyn GenericPublisher>>,
    /// Type-erased callback
    generic_callback: Box<dyn Callback>,

    next_execution_time_callback: Option<Box<dyn Fn(FrameworkTime) -> Option<FrameworkTime>>>,
    execution_duration_callback: Option<Box<dyn Fn() -> Duration>>,

    name: CallbackNodeName,

    /// First error seen in build tree.
    first_error: Option<CallbackBuildError>,
}

#[derive(Debug, Copy, Clone)]
pub struct ExpectedVsActual {
    expected: usize,
    actual: usize,
}

impl fmt::Display for ExpectedVsActual {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "expected: {}, actual: {}", self.expected, self.actual)
    }
}

#[derive(Debug)]
pub enum CallbackBuildError {
    MismatchedSubscriberCount(ExpectedVsActual), // Number of configured subscribers doesn't match callback
    MismatchedPublisherCount(ExpectedVsActual), // Number of configured publisher doesn't match callback
    MissingExecutionDurationCallback,           // No execution duration callback was added.
}

impl fmt::Display for CallbackBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MismatchedPublisherCount(e) => write!(
                f,
                "Number of configured subscribers doesn't match callback {}",
                e
            ),
            Self::MismatchedSubscriberCount(e) => write!(
                f,
                "Number of configured publisher doesn't match callback {}",
                e
            ),
            Self::MissingExecutionDurationCallback => {
                write!(f, "No execution duration callback was added.")
            }
        }
    }
}

impl CallbackBuilder {
    pub fn new(name: CallbackNodeName, callback: Box<dyn Callback>) -> CallbackBuilder {
        // Build the default subscribers/publishers from the callback,
        // the type in the function's signature should allow for some reasonable defaults.
        let subscribers = callback.build_subscribers();
        let publishers = callback.build_publishers();

        CallbackBuilder {
            name,
            subscribers,
            publishers,
            generic_callback: callback,
            next_execution_time_callback: None,
            execution_duration_callback: None,
            first_error: None,
        }
    }

    pub fn with_subscriber_channels(mut self, subscriber_channels: &[&str]) -> CallbackBuilder {
        if subscriber_channels.len() != self.subscribers.len() {
            self.first_error
                .get_or_insert(CallbackBuildError::MismatchedSubscriberCount(
                    ExpectedVsActual {
                        expected: self.subscribers.len(),
                        actual: subscriber_channels.len(),
                    },
                ));
            return self;
        }

        for (channel, config) in subscriber_channels.iter().zip(self.subscribers.iter_mut()) {
            config.get_config_mut().channel_name = channel.to_string();
        }
        self
    }

    pub fn with_publisher_channels(mut self, publisher_channels: &[&str]) -> CallbackBuilder {
        if publisher_channels.len() != self.publishers.len() {
            self.first_error
                .get_or_insert(CallbackBuildError::MismatchedPublisherCount(
                    ExpectedVsActual {
                        expected: self.publishers.len(),
                        actual: publisher_channels.len(),
                    },
                ));
            return self;
        }

        for (channel, config) in publisher_channels.iter().zip(self.publishers.iter_mut()) {
            config.get_config_mut().channel_name = channel.to_string();
        }
        self
    }

    /// TODO: Why use impl intead of box here and not elsewhere?
    pub fn with_execution_duration_callback(
        mut self,
        callback: impl Fn() -> Duration + 'static,
    ) -> CallbackBuilder {
        self.execution_duration_callback = Some(Box::new(callback));
        self
    }

    pub fn with_next_execution_time_callback(
        mut self,
        callback: impl Fn(FrameworkTime) -> Option<FrameworkTime> + 'static,
    ) -> CallbackBuilder {
        self.next_execution_time_callback = Some(Box::new(callback));
        self
    }

    pub fn build(self) -> Result<CallbackNode, CallbackBuildError> {
        if let Some(error) = self.first_error {
            return Err(error);
        }

        let mut callback = CallbackNode::new_with(
            self.generic_callback,
            self.subscribers,
            self.publishers,
            self.name,
        );

        if let Some(execution_duration_callback) = self.execution_duration_callback {
            callback.set_execution_duration_callback(execution_duration_callback);
        } else {
            return Err(CallbackBuildError::MissingExecutionDurationCallback);
        }

        if let Some(next_execution_time_callback) = self.next_execution_time_callback {
            callback.set_execution_time_callback(next_execution_time_callback);
        }

        Ok(callback)
    }
}

#[cfg(test)]
mod test {
    use std::assert_matches;

    use super::*;
    use crate::callback::{Callback, Run};
    use crate::context::Context;
    use crate::generic_publisher::GenericPublisher;
    use crate::generic_subscriber::GenericSubscriber;
    use crate::publisher::{Publisher, PublisherConfig};
    use crate::subscriber::{Subscriber, SubscriberConfig};
    use crate::time::FrameworkTime;

    /// A callback with a configurable number of default subscribers/publishers.
    struct DummyCallback {
        num_subscribers: usize,
        num_publishers: usize,
    }

    impl Callback for DummyCallback {
        fn run_generic(
            &mut self,
            _subscribers: &mut [Box<dyn GenericSubscriber>],
            _publishers: &mut [Box<dyn GenericPublisher>],
            _ctx: &Context,
        ) -> Run {
            Run::new(1)
        }

        fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
            (0..self.num_subscribers)
                .map(|_| {
                    Box::new(Subscriber::<u64>::new(SubscriberConfig {
                        is_optional: false,
                        capacity: 1,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: String::new(),
                    })) as Box<dyn GenericSubscriber>
                })
                .collect()
        }

        fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
            (0..self.num_publishers)
                .map(|_| {
                    Box::new(Publisher::<u64>::new(PublisherConfig {
                        capacity: 1,
                        channel_name: String::new(),
                    })) as Box<dyn GenericPublisher>
                })
                .collect()
        }
    }

    fn make_callback(num_subscribers: usize, num_publishers: usize) -> Box<dyn Callback> {
        Box::new(DummyCallback {
            num_subscribers,
            num_publishers,
        })
    }

    #[test]
    fn build_succeeds_with_required_components() {
        let callback = CallbackBuilder::new("MyCallback".into(), make_callback(1, 1))
            .with_subscriber_channels(&["in"])
            .with_publisher_channels(&["out"])
            .with_execution_duration_callback(|| Duration::from_millis(1))
            .build();

        assert!(callback.is_ok());
        let callback = callback.unwrap();
        assert_eq!(callback.get_name(), "MyCallback");
        assert_eq!(
            callback.get_subscribers()[0].get_config().channel_name,
            "in"
        );
        assert_eq!(
            callback.get_publishers()[0].get_config().channel_name,
            "out"
        );
        assert_eq!(callback.get_execution_duration(), Duration::from_millis(1));
        assert_eq!(
            callback.get_next_requested_execution_time(FrameworkTime::from_nanoseconds(0)),
            None
        );
    }

    #[test]
    fn missing_execution_duration_is_an_error() {
        let result = CallbackBuilder::new("NoDuration".into(), make_callback(0, 0)).build();

        assert_matches!(
            result,
            Err(CallbackBuildError::MissingExecutionDurationCallback)
        );
    }

    #[test]
    fn mismatched_subscriber_count_is_an_error() {
        let result = CallbackBuilder::new("BadSubs".into(), make_callback(2, 0))
            .with_subscriber_channels(&["only_one"])
            .with_execution_duration_callback(|| Duration::from_millis(1))
            .build();

        assert_matches!(
            result,
            Err(CallbackBuildError::MismatchedSubscriberCount(
                ExpectedVsActual {
                    expected: 2,
                    actual: 1
                }
            ))
        );
    }

    #[test]
    fn mismatched_publisher_count_is_an_error() {
        let result = CallbackBuilder::new("BadPubs".into(), make_callback(0, 2))
            .with_publisher_channels(&["only_one"])
            .with_execution_duration_callback(|| Duration::from_millis(1))
            .build();

        assert_matches!(
            result,
            Err(CallbackBuildError::MismatchedPublisherCount(
                ExpectedVsActual {
                    expected: 2,
                    actual: 1
                }
            ))
        );
    }

    #[test]
    fn next_execution_time_callback_is_optional() {
        let callback = CallbackBuilder::new("Timed".into(), make_callback(0, 0))
            .with_execution_duration_callback(|| Duration::from_millis(5))
            .with_next_execution_time_callback(|now| Some(now + Duration::from_nanos(10)))
            .build()
            .expect("build should succeed");

        let now = FrameworkTime::from_nanoseconds(100);
        assert_eq!(
            callback.get_next_requested_execution_time(now),
            Some(FrameworkTime::from_nanoseconds(110))
        );
    }

    #[test]
    fn first_error_wins() {
        // Both subscriber and publisher counts are wrong; the first one configured wins.
        let result = CallbackBuilder::new("MultiError".into(), make_callback(2, 2))
            .with_subscriber_channels(&["x"]) // wrong count first
            .with_publisher_channels(&["y"]) // also wrong, but first error wins
            .with_execution_duration_callback(|| Duration::from_millis(1))
            .build();

        assert_matches!(
            result,
            Err(CallbackBuildError::MismatchedSubscriberCount(
                ExpectedVsActual {
                    expected: 2,
                    actual: 1
                }
            ))
        );
    }
}
