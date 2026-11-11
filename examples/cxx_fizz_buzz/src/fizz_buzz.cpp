#include "cxx_fizz_buzz/src/bridge.rs.h"

#include <cstdint>
#include <string>

#include "cxx_fizz_buzz/src/fizz_buzz.hpp"

namespace cfw::fizz_buzz {

void FizzBuzzTask::run(FizzBuzzCalculatorCxxInputs& inputs,
                       FizzBuzzCalculatorCxxOutputs& outputs,
                       Context const& /*context*/) {
    std::uint64_t const value = inputs.integer();
    std::string result;
    bool const is_fizz = value % 3 == 0;
    bool const is_buzz = value % 5 == 0;
    if (is_fizz && is_buzz) {
        result = "FizzBuzz";
    } else if (is_fizz) {
        result = "Fizz";
    } else if (is_buzz) {
        result = "Buzz";
    } else {
        result = std::to_string(value);
    }
    outputs.fizz_buzz_string_value() = std::move(result);
    outputs.fizz_buzz_string_send();
}

std::unique_ptr<FizzBuzzTask> make_fizz_buzz_task() {
    return std::make_unique<FizzBuzzTask>();
}

void OptionalProbeTask::run(OptionalProbeCxxInputs& inputs,
                            OptionalProbeCxxOutputs& outputs,
                            Context const& /*context*/) {
    if (!inputs.value_has_value()) {
        return;
    }
    std::uint64_t const head = inputs.value_value();
    std::size_t const len = inputs.value_len();
    std::uint64_t const first = inputs.value_at(0);
    inputs.value_pop();

    std::string report = std::to_string(head);
    report.push_back(':');
    report += std::to_string(len);
    report.push_back(':');
    report += std::to_string(first);
    outputs.report_value() = std::move(report);
    outputs.report_send();
}

std::unique_ptr<OptionalProbeTask> make_optional_probe_task() {
    return std::make_unique<OptionalProbeTask>();
}

} // namespace cfw::fizz_buzz
