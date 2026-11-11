#include "task_cxx_tests/src/lib.rs.h"

#include <cstdint>
#include <string>
#include <utility>

#include "task_cxx_tests/src/input_span.hpp"

namespace cfw::task_cxx_tests {

void InputSpanProbeTask::run(InputSpanProbeCxxInputs& inputs,
                             InputSpanProbeCxxOutputs& outputs,
                             Context const& /*context*/) {
    std::string report = std::to_string(inputs.value_len());
    if (inputs.value_len() > 0) {
        report.push_back(':');
        report += std::to_string(inputs.value_at(0));
    }

    report.push_back(':');
    report += inputs.value_pop() ? "1" : "0";

    report.push_back(':');
    report += std::to_string(inputs.value_len());
    if (inputs.value_len() > 0) {
        report.push_back(':');
        report += std::to_string(inputs.value_at(0));
    }

    inputs.value_clear();
    report.push_back(':');
    report += std::to_string(inputs.value_len());
    report.push_back(':');
    report += inputs.value_pop() ? "1" : "0";

    outputs.report_value() = std::move(report);
    outputs.report_send();
}

std::unique_ptr<InputSpanProbeTask> make_input_span_probe_task() {
    return std::make_unique<InputSpanProbeTask>();
}

} // namespace cfw::task_cxx_tests
