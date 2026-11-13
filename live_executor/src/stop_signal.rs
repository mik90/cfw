use crate::pool_state::Scheduler;
use crossbeam::channel::Receiver;
use std::alloc::{Allocator, Global};
#[cfg(feature = "iceoryx2")]
use std::collections::BTreeMap;
use std::sync::Arc;
#[cfg(feature = "iceoryx2")]
use std::sync::OnceLock;

/// Event targets borrow callback publishers; the router lives only while the
/// executor graph storage is alive.
#[cfg(feature = "iceoryx2")]
#[derive(Debug)]
pub(crate) struct EventRouter<'graph_storage> {
    pub targets: BTreeMap<(String, usize), task::iox2::Iox2EventTarget<'graph_storage>>,
}

/// Stop and drain controls. `'graph_storage` is the graph lifetime used by event
/// target observers.
#[derive(Clone)]
pub struct StopSignal<'graph_storage, A: Allocator + Clone + Send + Sync + 'static = Global> {
    scheduler: Arc<Scheduler<A>>,
    #[cfg(feature = "iceoryx2")]
    events: Arc<OnceLock<EventRouter<'graph_storage>>>,
    // Keeps the graph lifetime in the type when event routing is disabled.
    #[cfg(not(feature = "iceoryx2"))]
    _graph_storage: std::marker::PhantomData<&'graph_storage ()>,
}

impl<'graph_storage, A: Allocator + Clone + Send + Sync + 'static> StopSignal<'graph_storage, A> {
    pub(crate) fn new(
        scheduler: Arc<Scheduler<A>>,
        #[cfg(feature = "iceoryx2")] events: Arc<OnceLock<EventRouter<'graph_storage>>>,
    ) -> Self {
        Self {
            scheduler,
            #[cfg(feature = "iceoryx2")]
            events,
            #[cfg(not(feature = "iceoryx2"))]
            _graph_storage: std::marker::PhantomData,
        }
    }

    /// Stop generating timer work after dispatching currently due timers, then
    /// wait for queued/running work and pending IPC readiness to drain. Gated
    /// callbacks awaiting absent inputs are idle. Future external input is outside
    /// this boundary; the caller should stop its producers first.
    pub fn drain(&self, now: task::time::FrameworkTime, timeout: std::time::Duration) -> bool {
        self.scheduler.quiesce_timers(now);
        let started = std::time::Instant::now();
        loop {
            if self.is_stopped() || started.elapsed() >= timeout {
                return false;
            }
            let generation = self.scheduler.completion_epoch();
            if self.scheduler.is_idle() {
                #[cfg(feature = "iceoryx2")]
                {
                    let ticket = self.scheduler.request_poll();
                    while !self.scheduler.polled(ticket) {
                        if self.is_stopped() || started.elapsed() >= timeout {
                            return false;
                        }
                        let _ = self
                            .scheduler
                            .stop_rx
                            .recv_timeout(std::time::Duration::from_millis(1));
                    }
                }
                if self.scheduler.is_idle() && generation == self.scheduler.completion_epoch() {
                    return true;
                }
            }
            let _ = self
                .scheduler
                .stop_rx
                .recv_timeout(std::time::Duration::from_millis(1));
        }
    }
    #[cfg(feature = "iceoryx2")]
    pub fn inject_event(
        &self,
        callback: &str,
        ordinal: usize,
        channel: &str,
        at: task::time::FrameworkTime,
        id: usize,
        count: u64,
    ) -> Result<(), String> {
        if self.is_stopped() {
            return Err("executor is stopped".into());
        }
        let target = self
            .events
            .get()
            .and_then(|router| router.targets.get(&(callback.into(), ordinal)))
            .ok_or_else(|| format!("unknown event recipient '{callback}' subscriber {ordinal}"))?;
        if target.channel != channel {
            return Err(format!(
                "event recipient '{callback}' expects '{}', got '{channel}'",
                target.channel
            ));
        }
        target.inject(at, iceoryx2::prelude::EventId::new(id), count);
        Ok(())
    }
    pub fn request_stop(&self) {
        self.scheduler.request_stop();
    }
    pub fn is_stopped(&self) -> bool {
        self.scheduler.is_stopped()
    }
    /// Block until shutdown is requested, including a worker failure.
    pub fn wait(&self) {
        let _ = self.scheduler.stop_rx.recv();
    }
    /// Disconnection signals shutdown to every receiver. A main-thread signal
    /// loop can select on this alongside its OS-signal/event receiver.
    pub fn receiver(&self) -> Receiver<()> {
        self.scheduler.stop_rx.clone()
    }
}

impl<A: Allocator + Clone + Send + Sync + 'static> task::executor::ExecutorStopSignal
    for StopSignal<'_, A>
{
    fn request_stop(&self) {
        StopSignal::request_stop(self);
    }
}
