#![cfg(feature = "iceoryx2")]
use simulation_executor::SimulationState;
use std::sync::{Arc, Mutex};
use task::iox2::{Iox2ChannelPlan, Iox2Event, Iox2OptionalInput, Iox2Runtime};
use task::time::FrameworkTime;
use task::{Context, GraphBuilder, GraphPlan};
use task_macros::task_callback;

type Observation = (i64, u64, i64, u64);
struct Observe {
    samples: Arc<Mutex<Vec<Observation>>>,
}
#[task_callback]
impl Observe {
    fn run(&self, input: Iox2OptionalInput<u64>, event: Iox2Event, context: &Context) {
        self.samples.lock().unwrap().push((
            context.now().to_nanoseconds(),
            *input.value().unwrap(),
            input.header().unwrap().published_at.to_nanoseconds(),
            event.count(),
        ));
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn timestamped_data_and_counted_events_are_injected_without_duplicate_notifications() {
    let runtime = Iox2Runtime::new().unwrap();
    let channel = format!("cfw_sim_injection_{}", std::process::id());
    let mut plan = Iox2ChannelPlan::new(&channel, &runtime);
    let declaration = ObserveDeclaration::from_keys(plan.subscriber(1), plan.events(1));
    let publisher = plan.publisher(1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build().unwrap();
    let publisher = Arc::new(Mutex::new(bindings.take_publisher(&publisher).unwrap()));
    let samples = Arc::new(Mutex::new(Vec::new()));
    let mut builder = GraphBuilder::with_storage(&storage);
    builder.add_callback("observe", || {
        Ok(Observe {
            samples: samples.clone(),
        }
        .bind(declaration, &bindings, &bindings)?)
    });
    let mut simulation = SimulationState::new(builder.build().unwrap()).unwrap();
    for (time, value, count) in [(10, 42, 3), (20, 84, 2)] {
        let time = FrameworkTime::from_nanoseconds(time);
        simulation
            .schedule_iox2_input(time, publisher.clone(), value)
            .unwrap();
        simulation
            .schedule_event(time, &channel, iceoryx2::prelude::EventId::new(0), count)
            .unwrap();
    }
    simulation.run_until_idle(10).unwrap();
    assert_eq!(*samples.lock().unwrap(), [(10, 42, 10, 3), (20, 84, 20, 2)]);
    // Real listeners are also polled: silent data injection must not leave a
    // duplicate kernel notification that schedules a later invocation.
    assert!(simulation.step().unwrap().executed.is_empty());
}
