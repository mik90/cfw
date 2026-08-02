use crate::callback::{Callback, CallbackNode, CallbackViews};
use crate::pub_sub::CallbackNodeName;
use crate::time::FrameworkTime;
use std::fmt;
use std::time::Duration;

pub struct CallbackBuilder {
    generic_callback: Box<dyn Callback>,

    next_execution_time_callback: Option<Box<dyn Fn(FrameworkTime) -> Option<FrameworkTime>>>,
    execution_duration_callback: Option<Box<dyn Fn() -> Duration>>,

    log_executions: bool,

    name: CallbackNodeName,

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
    MismatchedSubscriberCount(ExpectedVsActual),
    MismatchedPublisherCount(ExpectedVsActual),
    MissingExecutionDurationCallback,
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
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn new(name: CallbackNodeName, callback: Box<dyn Callback>) -> CallbackBuilder {
        CallbackBuilder {
            name,
            generic_callback: callback,
            next_execution_time_callback: None,
            execution_duration_callback: None,
            log_executions: false,
            first_error: None,
        }
    }

    /// Override the callback node's name (defaults to the task type name for
    /// `#[task_callback]`-generated builders).
    pub fn with_name(mut self, name: impl Into<CallbackNodeName>) -> CallbackBuilder {
        self.name = name.into();
        self
    }

    /// Run this callback once every `period`, starting `period` after the
    /// previous execution. Shorthand for
    /// `with_next_execution_time_callback(move |t| Some(t + period))`.
    pub fn with_periodic_execution(mut self, period: Duration) -> CallbackBuilder {
        self.next_execution_time_callback = Some(Box::new(move |t| Some(t + period)));
        self
    }

    pub fn with_subscriber_channels(mut self, subscriber_channels: &[&str]) -> CallbackBuilder {
        let mut subs = self.generic_callback.collect_subscribers_mut();
        if subscriber_channels.len() != subs.len() {
            self.first_error
                .get_or_insert(CallbackBuildError::MismatchedSubscriberCount(
                    ExpectedVsActual {
                        expected: subs.len(),
                        actual: subscriber_channels.len(),
                    },
                ));
            return self;
        }

        for (channel, s) in subscriber_channels.iter().zip(subs.iter_mut()) {
            s.config_mut().channel_name = channel.to_string();
        }
        self
    }

    pub fn with_publisher_channels(mut self, publisher_channels: &[&str]) -> CallbackBuilder {
        let mut pubs = self.generic_callback.collect_publishers_mut();
        if publisher_channels.len() != pubs.len() {
            self.first_error
                .get_or_insert(CallbackBuildError::MismatchedPublisherCount(
                    ExpectedVsActual {
                        expected: pubs.len(),
                        actual: publisher_channels.len(),
                    },
                ));
            return self;
        }

        for (channel, p) in publisher_channels.iter().zip(pubs.iter_mut()) {
            p.config_mut().channel_name = channel.to_string();
        }
        self
    }

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

    pub fn with_execution_logging(mut self, enabled: bool) -> CallbackBuilder {
        self.log_executions = enabled;
        self
    }

    pub fn build(self) -> Result<CallbackNode, CallbackBuildError> {
        if let Some(error) = self.first_error {
            return Err(error);
        }

        let mut callback = CallbackNode::new_named(self.generic_callback, self.name);

        if self.log_executions {
            callback.set_log_executions(true);
        }

        if let Some(cb) = self.execution_duration_callback {
            callback.set_execution_duration_callback(cb);
        } else {
            return Err(CallbackBuildError::MissingExecutionDurationCallback);
        }

        if let Some(cb) = self.next_execution_time_callback {
            callback.set_execution_time_callback(cb);
        }

        Ok(callback)
    }
}

#[cfg(test)]
mod test {
    use std::assert_matches;

    use super::*;
    use crate::callback::{Callback, PortMut, Run};
    use crate::context::Context;
    use crate::generic_publisher::GenericPublisher;
    use crate::generic_subscriber::GenericSubscriber;
    use crate::publisher::{Publisher, PublisherConfig};
    use crate::subscriber::{Subscriber, SubscriberConfig};
    use crate::time::FrameworkTime;

    struct DummyCallback {
        subs: Vec<Box<dyn GenericSubscriber>>,
        pubs: Vec<Box<dyn GenericPublisher>>,
    }

    impl Callback for DummyCallback {
        fn run(&mut self, _ctx: &Context) -> Run {
            Run::new(1)
        }

        fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
            for s in &self.subs {
                f(s.as_ref());
            }
        }
        fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
            for p in &self.pubs {
                f(p.as_ref());
            }
        }
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
            for s in self.subs.iter_mut() {
                f(s.as_mut());
            }
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
            for p in self.pubs.iter_mut() {
                f(p.as_mut());
            }
        }
        fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
            for s in self.subs.iter_mut() {
                f(PortMut::Subscriber(s.as_mut()));
            }
            for p in self.pubs.iter_mut() {
                f(PortMut::Publisher(p.as_mut()));
            }
        }
    }

    fn make_callback(num_subscribers: usize, num_publishers: usize) -> Box<dyn Callback> {
        Box::new(DummyCallback {
            subs: (0..num_subscribers)
                .map(|_| {
                    Box::new(Subscriber::<u64>::new(SubscriberConfig {
                        is_optional: false,
                        capacity: 1,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: String::new(),
                    })) as Box<dyn GenericSubscriber>
                })
                .collect(),
            pubs: (0..num_publishers)
                .map(|_| {
                    Box::new(Publisher::<u64>::new(PublisherConfig {
                        capacity: 1,
                        channel_name: String::new(),
                    })) as Box<dyn GenericPublisher>
                })
                .collect(),
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
        assert_eq!(callback.name(), "MyCallback");

        let subs = callback.callback().collect_subscribers();
        assert_eq!(subs[0].config().channel_name, "in");
        let pubs = callback.callback().collect_publishers();
        assert_eq!(pubs[0].config().channel_name, "out");

        assert_eq!(callback.execution_duration(), Duration::from_millis(1));
        assert_eq!(
            callback.next_requested_execution_time(FrameworkTime::from_nanoseconds(0)),
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
            callback.next_requested_execution_time(now),
            Some(FrameworkTime::from_nanoseconds(110))
        );
    }

    #[test]
    fn first_error_wins() {
        let result = CallbackBuilder::new("MultiError".into(), make_callback(2, 2))
            .with_subscriber_channels(&["x"])
            .with_publisher_channels(&["y"])
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
