#include "cxx_fizz_buzz/optional_probe_gen/optional_probe_cxx_bridge.rs.h"

#include <cstdint>
#include <string>
#include <utility>

#include "cxx_fizz_buzz/src/fizz_buzz.hpp"

namespace cfw::fizz_buzz {

void OptionalProbeTask::run(OptionalProbeCxxInputs& inputs,
                            OptionalProbeCxxOutputs& outputs,
                            const Context& /*context*/) {
    const std::uint64_t* head = inputs.value_try_value();
    if (head == nullptr) {
        return;
    }
    const std::size_t len = inputs.value_len();
    const std::uint64_t first = inputs.value_at(0);
    const std::int64_t published_at = inputs.value_header().published_at;
    inputs.value_pop();

    std::string report = std::to_string(*head);
    report.push_back(':');
    report += std::to_string(len);
    report.push_back(':');
    report += std::to_string(first);
    report.push_back(':');
    report += std::to_string(published_at);
    outputs.report_value() = std::move(report);
    outputs.report_send();
}

std::unique_ptr<OptionalProbeTask> make_optional_probe_task() {
    return std::make_unique<OptionalProbeTask>();
}

} // namespace cfw::fizz_buzz
