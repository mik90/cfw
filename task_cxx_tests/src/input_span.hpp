#pragma once

#include <cstdint>
#include <memory>

namespace cfw {
// Defined by the cxx-generated header for task_cxx.
struct Context;
} // namespace cfw

namespace cfw::task_cxx_tests {

// Defined by the cxx-generated header for src/lib.rs; forward declared here so
// this header can be included from that generated header.
struct InputSpanProbeCxxInputs;
struct InputSpanProbeCxxOutputs;

/// Observes the input-span view: capacity, front inspection, single pop,
/// shifting, clear-all, and pop-on-empty.
class InputSpanProbeTask {
  public:
    void run(InputSpanProbeCxxInputs& inputs, InputSpanProbeCxxOutputs& outputs,
             const Context& context);
};

std::unique_ptr<InputSpanProbeTask> make_input_span_probe_task();

} // namespace cfw::task_cxx_tests
