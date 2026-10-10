use std::{
    sync::{Arc, Mutex},
    time::Instant,
};
use task::{executor::TimeSource, time::FrameworkTime};

pub struct MonotonicClock(Instant);
impl Default for MonotonicClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl TimeSource for MonotonicClock {
    fn now(&self) -> FrameworkTime {
        FrameworkTime::from_nanoseconds(self.0.elapsed().as_nanos().min(i64::MAX as u128) as i64)
    }
}
struct State {
    started: bool,
    paused: bool,
    speed: f64,
    anchor_base: FrameworkTime,
    anchor_log: FrameworkTime,
    last: FrameworkTime,
}
pub struct ReplayTimeSource<C: TimeSource = MonotonicClock> {
    base: Arc<C>,
    state: Arc<Mutex<State>>,
}
impl<C: TimeSource> Clone for ReplayTimeSource<C> {
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            state: self.state.clone(),
        }
    }
}
fn valid_speed(speed: f64) -> Result<(), String> {
    if speed.is_finite() && speed > 0.0 {
        Ok(())
    } else {
        Err("replay speed must be finite and positive".into())
    }
}
impl<C: TimeSource> ReplayTimeSource<C> {
    pub fn new(base: C, first: FrameworkTime, speed: f64, paused: bool) -> Result<Self, String> {
        valid_speed(speed)?;
        if first == FrameworkTime::INVALID {
            return Err("invalid replay start time".into());
        }
        let anchor_base = base.now();
        if anchor_base == FrameworkTime::INVALID {
            return Err("invalid underlying clock".into());
        }
        Ok(Self {
            base: Arc::new(base),
            state: Arc::new(Mutex::new(State {
                started: false,
                paused,
                speed,
                anchor_base,
                anchor_log: first,
                last: first,
            })),
        })
    }
    fn sample(state: &mut State, base: FrameworkTime) -> FrameworkTime {
        if !state.started || state.paused {
            return state.anchor_log;
        }
        let delta = (i128::from(base.to_nanoseconds())
            - i128::from(state.anchor_base.to_nanoseconds()))
        .max(0);
        let scaled = (delta as f64 * state.speed) as i128;
        let time = i128::from(state.anchor_log.to_nanoseconds())
            .saturating_add(scaled)
            .clamp(i128::from(i64::MIN) + 1, i128::from(i64::MAX));
        state.last = state.last.max(FrameworkTime::from_nanoseconds(time as i64));
        state.last
    }
    /// Start at worker/controller readiness, excluding graph startup wall time.
    pub fn start(&self) {
        let base = self.base.now();
        let mut state = self.state.lock().unwrap();
        if !state.started {
            state.anchor_base = base;
            state.started = true;
        }
    }
    pub fn pause(&self) {
        let base = self.base.now();
        let mut state = self.state.lock().unwrap();
        state.anchor_log = Self::sample(&mut state, base);
        state.anchor_base = base;
        state.paused = true;
    }
    pub fn resume(&self) {
        let base = self.base.now();
        let mut state = self.state.lock().unwrap();
        if state.paused {
            state.anchor_base = base;
            state.paused = false;
        }
    }
    pub fn set_speed(&self, speed: f64) -> Result<(), String> {
        valid_speed(speed)?;
        let base = self.base.now();
        let mut state = self.state.lock().unwrap();
        state.anchor_log = Self::sample(&mut state, base);
        state.anchor_base = base;
        state.speed = speed;
        Ok(())
    }
    pub fn is_paused(&self) -> bool {
        self.state.lock().unwrap().paused
    }
}
impl<C: TimeSource> TimeSource for ReplayTimeSource<C> {
    fn now(&self) -> FrameworkTime {
        let base = self.base.now();
        Self::sample(&mut self.state.lock().unwrap(), base)
    }
}
