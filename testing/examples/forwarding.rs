//! Run with: cargo run -p testing --example forwarding
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use task::{
    ArenaReaderPtr, Forwardable, Forwarded, InputSpan, OutputUninit, RequiredInput,
    message::Message,
};
use task_macros::{ForwardedSources, task_callback};
use testing::UnitTestExecutorBuilder;

struct Image {
    pixels: Vec<u8>,
}
struct Calibration {
    scale: u32,
}
struct Detection {
    score: u32,
}

#[derive(ForwardedSources)]
struct DetectionSources<'storage> {
    image: ArenaReaderPtr<'storage, Message<Image>>,
    calibration: ArenaReaderPtr<'storage, Message<Calibration>>,
}

struct Detect;
#[task_callback]
impl Detect {
    fn run<'storage>(
        &self,
        mut images: RequiredInput<'_, 'storage, Image, Forwardable>,
        mut calibration: RequiredInput<'_, 'storage, Calibration, Forwardable>,
        #[forwards(image = images, calibration = calibration)] detections: OutputUninit<
            '_,
            'storage,
            Forwarded<Detection, DetectionSources<'storage>>,
        >,
    ) {
        let image = images.forward().unwrap();
        let calibration = calibration.forward().unwrap();
        let score = u32::from(image.message.pixels[0]) * calibration.message.scale;
        // The executor supplies a loan from the detection publisher's arena.
        detections
            .write(Forwarded::new(
                Detection { score },
                DetectionSources { image, calibration },
            ))
            .send();
    }
}

struct Retain(Arc<AtomicUsize>);
#[task_callback]
impl Retain {
    fn run<'storage>(
        &self,
        #[capacity(32)]
        #[keep_across_runs(true)]
        detections: InputSpan<
            '_,
            'storage,
            Forwarded<Detection, DetectionSources<'storage>>,
        >,
    ) {
        for detection in detections.inputs() {
            let sources = &detection.message.forwarded_messages;
            assert_eq!(
                detection.message.message.score,
                u32::from(sources.image.message.pixels[0]) * sources.calibration.message.scale
            );
        }
        self.0.store(detections.len(), Ordering::SeqCst);
    }
}

fn main() {
    let retained = Arc::new(AtomicUsize::new(0));
    let mut builder = UnitTestExecutorBuilder::new();
    builder.add_task("detect", Detect, Duration::from_nanos(1));
    builder.add_task("retain", Retain(retained.clone()), Duration::from_nanos(1));
    let mut images = builder.add_test_publisher::<Image>("images");
    let mut calibration = builder.add_test_publisher::<Calibration>("calibration");
    builder.run(|mut executor| {
        for value in 0..32 {
            images.send(Image {
                pixels: vec![value],
            });
            calibration.send(Calibration { scale: 2 });
            for _ in 0..4 {
                executor.step();
            }
        }
        assert_eq!(retained.load(Ordering::SeqCst), 32);
    });
    println!("Retained 32 detections with image and calibration sources");
}

#[test]
fn forwarding_example() {
    main();
}
