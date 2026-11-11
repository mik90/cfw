task_cxx_macros::define_cxx_bridge! {
    namespace: "cfw::task_cxx_tests",
    headers: ["task_cxx_tests/src/input_span.hpp"],
    payloads: {},
}

pub const INPUT_SPAN_IN_CHANNEL: &str = "input_span_in";
pub const INPUT_SPAN_OUT_CHANNEL: &str = "input_span_out";

task_cxx_macros::define_cxx_task! {
    bridge: ffi,
    name: InputSpanProbe,
    cxx_type: InputSpanProbeTask,
    factory: make_input_span_probe_task,
    subscribers: {
        value: InputSpan<u64> { channel: "input_span_in", capacity: 4, trigger: true },
    },
    publishers: {
        report: Output<String> { channel: "input_span_out" },
    },
}
