use crate::{LoanError, Publisher, time::FrameworkTime};
use std::any::Any;

/// Executor operations erase the message type without requiring Any on endpoints.
/// Store these as `Box<dyn PublisherOps + 'storage>` for heterogeneous graphs.
pub trait PublisherOps: Send {
    fn flush(&mut self, timestamp: FrameworkTime);
}

impl<T: Send + Sync> PublisherOps for Publisher<'_, T> {
    fn flush(&mut self, timestamp: FrameworkTime) {
        Publisher::flush(self, timestamp);
    }
}

/// Replay errors return ownership of the input so callers can retry capacity errors.
pub enum ReplayError {
    TypeMismatch(Box<dyn Any + Send>),
    Loan {
        reason: LoanError,
        value: Box<dyn Any + Send>,
    },
}

/// Replay erases owned payloads, not the borrowed publisher containing them.
/// Borrowed forwarded payloads use typed publishing instead of this interface.
pub trait ReplayPublisher: PublisherOps {
    fn publish_boxed(&mut self, value: Box<dyn Any + Send>) -> Result<(), ReplayError>;
}

impl<T: Send + Sync + 'static> ReplayPublisher for Publisher<'_, T> {
    fn publish_boxed(&mut self, value: Box<dyn Any + Send>) -> Result<(), ReplayError> {
        let value = value.downcast::<T>().map_err(ReplayError::TypeMismatch)?;
        match self.loan_uninit() {
            Ok(loan) => {
                loan.write(*value).send();
                Ok(())
            }
            Err(reason) => Err(ReplayError::Loan { reason, value }),
        }
    }
}
