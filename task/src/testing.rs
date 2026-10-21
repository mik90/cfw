//! Test doubles for callbacks.

use crate::recording::{EndpointDescriptor, LoggedEvent, LoggedMessage};
use crate::time::FrameworkTime;
use crate::wake::WakeHandle;
use crate::{Callback, Context, LoanError};

/// Body invoked by [`Callback::run`] on a [`FakeCallback`].
pub type FakeBody = Box<dyn FnMut(&Context) -> Result<(), LoanError> + Send>;

/// Configurable callback for tests that need no real endpoints.
///
/// Every [`Callback`] method is implemented explicitly; the public fields
/// express the behaviors that tests commonly override.
pub struct FakeCallback {
    /// Body invoked by [`Callback::run`]. Defaults to returning `Ok(())`.
    pub run_hook: FakeBody,
    /// Reported by [`Callback::outputs_are_local`].
    pub outputs_are_local: bool,
    /// Reported by [`Callback::required_inputs_ready`].
    pub required_inputs_ready: bool,
    /// Reported by [`Callback::required_inputs_available`]. `None` mirrors
    /// [`Self::required_inputs_ready`].
    pub required_inputs_available: Option<bool>,
    /// Reported by [`Callback::has_pending_inputs`].
    pub has_pending_inputs: bool,
    /// Names yielded by [`Callback::visit_channel_names`], in order.
    pub channel_names: Vec<String>,
    /// Timestamps recorded by [`Callback::flush_outputs`], in commit order.
    pub output_flush_timestamps: Vec<FrameworkTime>,
    /// Panic from [`Callback::visit_prepared_messages`].
    pub panic_on_prepared_visit: bool,
    /// Panic from [`Callback::flush_outputs`] after recording the timestamp.
    pub panic_on_flush: bool,
}

impl Default for FakeCallback {
    fn default() -> Self {
        Self {
            run_hook: Box::new(|_| Ok(())),
            outputs_are_local: true,
            required_inputs_ready: true,
            required_inputs_available: None,
            has_pending_inputs: false,
            channel_names: Vec::new(),
            output_flush_timestamps: Vec::new(),
            panic_on_prepared_visit: false,
            panic_on_flush: false,
        }
    }
}

impl Callback for FakeCallback {
    fn outputs_are_local(&self) -> bool {
        self.outputs_are_local
    }
    fn clear_replay_inputs(&mut self) -> Result<(), LoanError> {
        Err(LoanError::Transport(
            "fake callback does not support replay input reset".into(),
        ))
    }
    fn stage_replay_event(&mut self, _: usize, _: usize, _: u64) -> Result<(), LoanError> {
        Err(LoanError::Transport("unknown replay event endpoint".into()))
    }
    fn recording_endpoints(&self) -> Option<Vec<EndpointDescriptor>> {
        None
    }
    fn visit_prepared_messages(&self, _visit: &mut dyn FnMut(LoggedMessage)) {
        assert!(
            !self.panic_on_prepared_visit,
            "prepared payload visitation disabled"
        );
    }
    fn visit_pending_messages(&self, _visit: &mut dyn FnMut(LoggedMessage)) {}
    fn visit_prepared_events(&self, _visit: &mut dyn FnMut(LoggedEvent)) {}
    fn visit_pending_events(&self, _visit: &mut dyn FnMut(LoggedEvent)) {}
    #[cfg(feature = "iceoryx2")]
    fn take_iox2_events(&mut self) -> Vec<crate::iox2::Iox2EventRegistration> {
        Vec::new()
    }
    fn set_waker(&mut self, _wake: WakeHandle) {}
    fn has_pending_inputs(&self) -> bool {
        self.has_pending_inputs
    }
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        for name in &self.channel_names {
            visit(name);
        }
    }
    fn update_inputs(&mut self) {}
    fn required_inputs_available(&self) -> bool {
        self.required_inputs_available
            .unwrap_or(self.required_inputs_ready)
    }
    fn required_inputs_ready(&self) -> bool {
        self.required_inputs_ready
    }
    fn run(&mut self, context: &Context) -> Result<(), LoanError> {
        (self.run_hook)(context)
    }
    fn finish_inputs(&mut self) {}
    fn flush_outputs(&mut self, timestamp: FrameworkTime) {
        self.output_flush_timestamps.push(timestamp);
        assert!(!self.panic_on_flush, "flush failed");
    }
    fn discard_outputs(&mut self) {}
}
