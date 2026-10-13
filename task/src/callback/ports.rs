//! Statically dispatched traversal of heterogeneous callback endpoints.
use crate::{
    LoanError, Publisher, Subscriber, message::MessageHeader, recording::Transport,
    time::FrameworkTime, wake::WakeHandle,
};

pub trait Endpoint {
    type Payload;
    fn channel_name(&self) -> &str;
    fn transport(&self) -> Transport;
    fn publisher_index(&self) -> Option<u32> {
        None
    }
}

pub trait InputEndpoint: Endpoint {
    fn update(&mut self);
    fn clear(&mut self);
    fn finish(&mut self) {}
    fn set_waker(&mut self, _wake: WakeHandle) {}
    fn requests_execution(&self) -> bool {
        false
    }
    /// Readiness of the prepared snapshot, excluding subsequent arrivals.
    fn required_ready(&self) -> bool {
        true
    }
    /// Non-consuming preflight check that may include queued arrivals.
    fn required_available(&self) -> bool {
        true
    }
    fn visit_headers(&self, _visit: impl FnMut(MessageHeader)) {}
    fn visit_events(&self, _visit: impl FnMut(usize, u64)) {}
    fn stage_event(&mut self, _event_id: usize, _count: u64) -> Result<(), LoanError> {
        Err(LoanError::Transport("unknown replay event port".into()))
    }
    #[cfg(feature = "iceoryx2")]
    fn take_registration(&mut self) -> Option<crate::iox2::Iox2EventRegistration> {
        None
    }
}

pub trait OutputEndpoint: Endpoint {
    /// True when committing this output cannot publish to an external transport.
    fn is_local(&self) -> bool;
    fn flush(&mut self, timestamp: FrameworkTime);
    fn discard(&mut self);
    fn visit_headers(&self, _visit: impl FnMut(MessageHeader)) {}
    fn visit_events(&self, _visit: impl FnMut(usize, u64)) {}
}

pub trait InputVisitor {
    fn visit(&mut self, port: &impl InputEndpoint);
}
pub trait InputVisitorMut {
    fn visit(&mut self, port: &mut impl InputEndpoint);
}
pub trait OutputVisitor {
    fn visit(&mut self, port: &impl OutputEndpoint);
}
pub trait OutputVisitorMut {
    fn visit(&mut self, port: &mut impl OutputEndpoint);
}

/// Visit each input exactly once, in the same declaration order on every traversal.
pub trait Inputs {
    fn visit(&self, visitor: &mut impl InputVisitor);
    fn visit_mut(&mut self, visitor: &mut impl InputVisitorMut);
}
/// Visit each output exactly once, in the same declaration order on every traversal.
pub trait Outputs {
    fn visit(&self, visitor: &mut impl OutputVisitor);
    fn visit_mut(&mut self, visitor: &mut impl OutputVisitorMut);
}
impl<P: InputEndpoint> Inputs for P {
    fn visit(&self, visitor: &mut impl InputVisitor) {
        visitor.visit(self);
    }
    fn visit_mut(&mut self, visitor: &mut impl InputVisitorMut) {
        visitor.visit(self);
    }
}
impl<P: OutputEndpoint> Outputs for P {
    fn visit(&self, visitor: &mut impl OutputVisitor) {
        visitor.visit(self);
    }
    fn visit_mut(&mut self, visitor: &mut impl OutputVisitorMut) {
        visitor.visit(self);
    }
}
impl Inputs for () {
    fn visit(&self, _: &mut impl InputVisitor) {}
    fn visit_mut(&mut self, _: &mut impl InputVisitorMut) {}
}
impl Outputs for () {
    fn visit(&self, _: &mut impl OutputVisitor) {}
    fn visit_mut(&mut self, _: &mut impl OutputVisitorMut) {}
}
impl<A: Inputs, B: Inputs> Inputs for (A, B) {
    fn visit(&self, visitor: &mut impl InputVisitor) {
        self.0.visit(visitor);
        self.1.visit(visitor);
    }
    fn visit_mut(&mut self, visitor: &mut impl InputVisitorMut) {
        self.0.visit_mut(visitor);
        self.1.visit_mut(visitor);
    }
}
impl<A: Outputs, B: Outputs> Outputs for (A, B) {
    fn visit(&self, visitor: &mut impl OutputVisitor) {
        self.0.visit(visitor);
        self.1.visit(visitor);
    }
    fn visit_mut(&mut self, visitor: &mut impl OutputVisitorMut) {
        self.0.visit_mut(visitor);
        self.1.visit_mut(visitor);
    }
}

impl<T> Endpoint for Subscriber<'_, T> {
    type Payload = T;
    fn channel_name(&self) -> &str {
        self.channel_name()
    }
    fn transport(&self) -> Transport {
        Transport::Native
    }
}
impl<T> InputEndpoint for Subscriber<'_, T> {
    fn update(&mut self) {
        Subscriber::update(self);
    }
    fn clear(&mut self) {
        Subscriber::clear(self);
    }
    fn finish(&mut self) {
        self.finish_iteration();
    }
    fn set_waker(&mut self, wake: WakeHandle) {
        Subscriber::set_waker(self, wake);
    }
    fn requests_execution(&self) -> bool {
        self.requests_execution()
    }
    fn visit_headers(&self, visit: impl FnMut(MessageHeader)) {
        self.visit_headers(visit);
    }
}

/// A native input whose prepared snapshot must contain a message before invocation.
pub struct Required<P>(pub P);
impl<T> Endpoint for Required<Subscriber<'_, T>> {
    type Payload = T;
    fn channel_name(&self) -> &str {
        self.0.channel_name()
    }
    fn transport(&self) -> Transport {
        Transport::Native
    }
}
impl<T> InputEndpoint for Required<Subscriber<'_, T>> {
    fn update(&mut self) {
        self.0.update();
    }
    fn clear(&mut self) {
        self.0.clear();
    }
    fn finish(&mut self) {
        self.0.finish_iteration();
    }
    fn set_waker(&mut self, wake: WakeHandle) {
        self.0.set_waker(wake);
    }
    fn requests_execution(&self) -> bool {
        self.0.requests_execution()
    }
    fn required_ready(&self) -> bool {
        !self.0.is_empty()
    }
    fn required_available(&self) -> bool {
        self.0.has_pending()
    }
    fn visit_headers(&self, visit: impl FnMut(MessageHeader)) {
        self.0.visit_headers(visit);
    }
}
impl<T> Endpoint for Publisher<'_, T> {
    type Payload = T;
    fn channel_name(&self) -> &str {
        self.channel_name()
    }
    fn transport(&self) -> Transport {
        Transport::Native
    }
    fn publisher_index(&self) -> Option<u32> {
        Some(self.publisher_index())
    }
}
impl<T> OutputEndpoint for Publisher<'_, T> {
    fn is_local(&self) -> bool {
        true
    }
    fn flush(&mut self, timestamp: FrameworkTime) {
        self.flush(timestamp);
    }
    fn discard(&mut self) {
        self.discard_pending();
    }
    fn visit_headers(&self, visit: impl FnMut(MessageHeader)) {
        self.visit_pending_headers(visit);
    }
}

#[cfg(feature = "iceoryx2")]
mod ipc {
    use super::*;
    use crate::iox2::{
        Iox2EventRegistration, Iox2EventSubscriber, Iox2Notifier, Iox2Publisher, Iox2Subscriber,
    };
    use iceoryx2::prelude::ZeroCopySend;
    use std::fmt::Debug;

    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Endpoint for Iox2Subscriber<T> {
        type Payload = T;
        fn channel_name(&self) -> &str {
            self.channel_name()
        }
        fn transport(&self) -> Transport {
            Transport::Ipc
        }
    }
    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> InputEndpoint for Iox2Subscriber<T> {
        fn update(&mut self) {
            Iox2Subscriber::update(self);
        }
        fn clear(&mut self) {
            Iox2Subscriber::clear(self);
        }
        fn visit_headers(&self, visit: impl FnMut(MessageHeader)) {
            self.visit_headers(visit);
        }
    }
    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Endpoint for Iox2Publisher<T> {
        type Payload = T;
        fn channel_name(&self) -> &str {
            self.channel_name()
        }
        fn transport(&self) -> Transport {
            Transport::Ipc
        }
        fn publisher_index(&self) -> Option<u32> {
            Some(self.publisher_index())
        }
    }
    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> OutputEndpoint for Iox2Publisher<T> {
        fn is_local(&self) -> bool {
            self.delivery() == crate::iox2::OutputDelivery::Local
        }
        fn flush(&mut self, timestamp: FrameworkTime) {
            self.flush(timestamp);
        }
        fn discard(&mut self) {
            self.discard_pending();
        }
        fn visit_headers(&self, visit: impl FnMut(MessageHeader)) {
            self.visit_pending_headers(visit);
        }
        fn visit_events(&self, mut visit: impl FnMut(usize, u64)) {
            self.visit_pending_headers(|_| visit(0, 1));
        }
    }
    impl Endpoint for Iox2EventSubscriber {
        type Payload = ();
        fn channel_name(&self) -> &str {
            self.channel_name()
        }
        fn transport(&self) -> Transport {
            Transport::Event
        }
    }
    impl InputEndpoint for Iox2EventSubscriber {
        fn update(&mut self) {
            Iox2EventSubscriber::update(self);
        }
        fn clear(&mut self) {
            Iox2EventSubscriber::clear(self);
        }
        fn set_waker(&mut self, wake: WakeHandle) {
            self.set_waker(wake);
        }
        fn requests_execution(&self) -> bool {
            self.has_pending()
        }
        fn visit_events(&self, mut visit: impl FnMut(usize, u64)) {
            self.visit_records(|event| visit(event.event_id.as_value(), event.count));
        }
        fn stage_event(&mut self, id: usize, count: u64) -> Result<(), LoanError> {
            self.stage_replay(id, count)
        }
        fn take_registration(&mut self) -> Option<Iox2EventRegistration> {
            self.take_registration()
        }
    }
    impl Endpoint for Iox2Notifier {
        type Payload = ();
        fn channel_name(&self) -> &str {
            self.channel_name()
        }
        fn transport(&self) -> Transport {
            Transport::Event
        }
    }
    impl OutputEndpoint for Iox2Notifier {
        fn is_local(&self) -> bool {
            self.delivery() == crate::iox2::OutputDelivery::Local
        }
        fn flush(&mut self, timestamp: FrameworkTime) {
            self.flush(timestamp);
        }
        fn discard(&mut self) {
            self.discard_pending();
        }
        fn visit_events(&self, mut visit: impl FnMut(usize, u64)) {
            if let Some(event) = self.pending_event() {
                visit(event.event_id.as_value(), event.count);
            }
        }
    }
}
