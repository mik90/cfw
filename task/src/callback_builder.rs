use crate::callback::{ConnectedCallback, GenericCallback};
use crate::generic_publisher::GenericPublisher;
use crate::generic_subscriber::GenericSubscriber;
use crate::pub_sub::CallbackName;
use crate::time::FrameworkTime;
use std::fmt;
use std::time::Duration;

pub struct CallbackBuilder {
    subscribers: Vec<Box<dyn GenericSubscriber>>,
    publishers: Vec<Box<dyn GenericPublisher>>,
    /// Type-erased callback
    generic_callback: Box<dyn GenericCallback>,

    next_execution_time_callback: Option<Box<dyn Fn(FrameworkTime) -> Option<FrameworkTime>>>,
    execution_duration_callback: Option<Box<dyn Fn() -> Duration>>,

    name: CallbackName,

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
    MismatchedSubscriberCount(ExpectedVsActual), // Number of configured subscribers doesn't match generic callback
    MismatchedPublisherCount(ExpectedVsActual), // Number of configured publisher doesn't match generic callback
    MissingExecutionDurationCallback,           // No execution duration callback was added.
}

impl fmt::Display for CallbackBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MismatchedPublisherCount(e) => write!(
                f,
                "Number of configured subscribers doesn't match generic callback {}",
                e
            ),
            Self::MismatchedSubscriberCount(e) => write!(
                f,
                "Number of configured publisher doesn't match generic callback {}",
                e
            ),
            Self::MissingExecutionDurationCallback => {
                write!(f, "No execution duration callback was added.")
            }
        }
    }
}

impl CallbackBuilder {
    pub fn new(name: CallbackName, callback: Box<dyn GenericCallback>) -> CallbackBuilder {
        // Build the default subscribers/publishers from the generic callback,
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
            self.first_error = Some(CallbackBuildError::MismatchedSubscriberCount(
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
            self.first_error = Some(CallbackBuildError::MismatchedPublisherCount(
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

    pub fn build(self) -> Result<ConnectedCallback, CallbackBuildError> {
        if let Some(error) = self.first_error {
            return Err(error);
        }

        let mut callback = ConnectedCallback::new_with(
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
mod test {}
