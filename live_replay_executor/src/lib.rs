//! Wall-clock-paced log input replay with scoped workers and explicit EOF draining.
mod clock;
pub use clock::{MonotonicClock, ReplayTimeSource};
use live_executor::{LiveExecutor, StopSignal};
use logging::{ReplayFeed, ReplaySource, SortedLogStreamReader};
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use task::{BuiltGraph, executor::TimeSource, time::FrameworkTime};

pub struct LiveReplayConfig {
    pub speed: f64,
    pub start_paused: bool,
    pub pool_threads: Vec<usize>,
    pub poll_interval: Duration,
    pub drain_timeout: Duration,
    /// Continue the replay clock beyond the final recorded timestamp, allowing
    /// delayed periodic consumers to run before timer generation is stopped.
    pub tail_duration: Duration,
    pub denylist: HashSet<String>,
}
impl Default for LiveReplayConfig {
    fn default() -> Self {
        Self {
            speed: 1.0,
            start_paused: false,
            pool_threads: vec![1],
            poll_interval: Duration::from_millis(1),
            drain_timeout: Duration::from_secs(10),
            tail_duration: Duration::ZERO,
            denylist: HashSet::new(),
        }
    }
}
#[derive(Debug)]
pub enum LiveReplayError {
    Setup(String),
    Input(String),
    DrainTimeout,
    ProducerPanicked,
    Live(live_executor::LiveExecutorError),
}
impl std::fmt::Display for LiveReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "live replay: {self:?}")
    }
}
impl std::error::Error for LiveReplayError {}
pub struct ReplayControl<C: TimeSource = MonotonicClock> {
    clock: ReplayTimeSource<C>,
    stop: StopSignal,
    exhausted: Arc<AtomicBool>,
}
impl<C: TimeSource> Clone for ReplayControl<C> {
    fn clone(&self) -> Self {
        Self {
            clock: self.clock.clone(),
            stop: self.stop.clone(),
            exhausted: self.exhausted.clone(),
        }
    }
}
impl<C: TimeSource> ReplayControl<C> {
    pub fn pause(&self) {
        self.clock.pause();
    }
    pub fn resume(&self) {
        self.clock.resume();
    }
    pub fn is_paused(&self) -> bool {
        self.clock.is_paused()
    }
    pub fn set_speed(&self, speed: f64) -> Result<(), String> {
        self.clock.set_speed(speed)
    }
    pub fn now(&self) -> FrameworkTime {
        self.clock.now()
    }
    pub fn request_stop(&self) {
        self.stop.request_stop();
    }
    pub fn is_stopped(&self) -> bool {
        self.stop.is_stopped()
    }
    pub fn wait(&self) {
        self.stop.wait();
    }
    pub fn input_exhausted(&self) -> bool {
        self.exhausted.load(Ordering::Acquire)
    }
}
#[derive(Debug)]
pub struct ReplayCompletion {
    pub input_exhausted: bool,
    pub drained: bool,
    pub final_time: FrameworkTime,
}
pub struct LiveReplayExecutor<'storage, C: TimeSource = MonotonicClock> {
    inner: LiveExecutor<'storage, ReplayTimeSource<C>>,
    feed: ReplayFeed<'storage>,
    control: ReplayControl<C>,
    config: LiveReplayConfig,
}
impl<'storage> LiveReplayExecutor<'storage> {
    pub fn new(
        graph: BuiltGraph<'storage>,
        reader: SortedLogStreamReader,
        sources: impl IntoIterator<Item = ReplaySource<'storage>>,
        config: LiveReplayConfig,
    ) -> Result<Self, LiveReplayError> {
        Self::with_clock(graph, reader, sources, config, MonotonicClock::default())
    }
}
struct StopOnDrop(StopSignal);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.request_stop();
    }
}
impl<'storage, C: TimeSource> LiveReplayExecutor<'storage, C> {
    pub fn with_clock(
        graph: BuiltGraph<'storage>,
        reader: SortedLogStreamReader,
        sources: impl IntoIterator<Item = ReplaySource<'storage>>,
        mut config: LiveReplayConfig,
        base: C,
    ) -> Result<Self, LiveReplayError> {
        if config.poll_interval.is_zero() || config.drain_timeout.is_zero() {
            return Err(LiveReplayError::Setup(
                "poll interval and drain timeout must be positive".into(),
            ));
        }
        let first = reader
            .first_log_time()
            .unwrap_or_else(|| FrameworkTime::from_nanoseconds(0));
        let clock = ReplayTimeSource::new(base, first, config.speed, config.start_paused)
            .map_err(LiveReplayError::Setup)?;
        let feed = ReplayFeed::new(reader, sources, std::mem::take(&mut config.denylist))
            .map_err(|e| LiveReplayError::Setup(e.to_string()))?;
        let inner = LiveExecutor::new_multi_pool_with_time(
            config.pool_threads.clone(),
            graph,
            clock.clone(),
        )
        .map_err(|e| LiveReplayError::Setup(e.to_string()))?;
        let control = ReplayControl {
            clock,
            stop: inner.stop_signal(),
            exhausted: Arc::new(AtomicBool::new(feed.exhausted())),
        };
        Ok(Self {
            inner,
            feed,
            control,
            config,
        })
    }
    pub fn control(&self) -> ReplayControl<C> {
        self.control.clone()
    }
    pub fn run(self) -> Result<ReplayCompletion, LiveReplayError> {
        self.run_with(|control| control.wait())
            .map(|(_, completion)| completion)
    }
    /// The controller runs on the calling thread; source playback and all workers
    /// are scoped and joined, including when the controller panics or returns early.
    pub fn run_with<R>(
        self,
        controller: impl FnOnce(&ReplayControl<C>) -> R,
    ) -> Result<(R, ReplayCompletion), LiveReplayError> {
        let Self {
            inner,
            mut feed,
            control,
            config,
        } = self;
        inner
            .run_with(|stop| {
                std::thread::scope(|scope| {
                    let _controller_stop = StopOnDrop(stop.clone());
                    control.clock.start();
                    let producer_control = control.clone();
                    let handle = scope.spawn(move || {
                        let _producer_stop = StopOnDrop(producer_control.stop.clone());
                        play(&mut feed, &producer_control, &config)
                    });
                    let result = controller(&control);
                    stop.request_stop();
                    let completion = handle
                        .join()
                        .map_err(|_| LiveReplayError::ProducerPanicked)?;
                    Ok::<_, LiveReplayError>((result, completion?))
                })
            })
            .map_err(LiveReplayError::Live)?
    }
}
fn play<C: TimeSource>(
    feed: &mut ReplayFeed<'_>,
    control: &ReplayControl<C>,
    config: &LiveReplayConfig,
) -> Result<ReplayCompletion, LiveReplayError> {
    let stopped = || ReplayCompletion {
        input_exhausted: feed.exhausted(),
        drained: false,
        final_time: control.now(),
    };
    if control.is_stopped() {
        return Ok(stopped());
    }
    while !feed.exhausted() {
        if control.is_stopped() {
            return Ok(ReplayCompletion {
                input_exhausted: feed.exhausted(),
                drained: false,
                final_time: control.now(),
            });
        }
        if !control.is_paused() {
            feed.inject_due_while(
                control.now(),
                |callback, channel, event| {
                    #[cfg(feature = "iceoryx2")]
                    {
                        control
                            .stop
                            .inject_event(
                                callback,
                                event.event.ordinal,
                                channel,
                                control.now(),
                                event.event.event_id,
                                event.event.count,
                            )
                            .map_err(Into::into)
                    }
                    #[cfg(not(feature = "iceoryx2"))]
                    {
                        let _ = (callback, channel, event);
                        Err("replaying event records requires iceoryx2".into())
                    }
                },
                || !control.is_stopped() && !control.is_paused(),
            )
            .map_err(|e| LiveReplayError::Input(e.to_string()))?;
            control.exhausted.store(feed.exhausted(), Ordering::Release);
        }
        if !feed.exhausted() {
            let _ = control.stop.receiver().recv_timeout(config.poll_interval);
        }
    }
    let end = feed
        .last_time()
        .unwrap_or_else(|| control.now())
        .checked_add_duration(config.tail_duration)
        .ok_or_else(|| LiveReplayError::Input("replay tail time overflow".into()))?;
    while !control.is_stopped() && control.now() < end {
        let _ = control.stop.receiver().recv_timeout(config.poll_interval);
    }
    if control.is_stopped() {
        return Ok(ReplayCompletion {
            input_exhausted: true,
            drained: false,
            final_time: control.now(),
        });
    }
    let drained = control.stop.drain(control.now(), config.drain_timeout);
    if !drained && !control.is_stopped() {
        return Err(LiveReplayError::DrainTimeout);
    }
    Ok(ReplayCompletion {
        input_exhausted: true,
        drained,
        final_time: control.now(),
    })
}
