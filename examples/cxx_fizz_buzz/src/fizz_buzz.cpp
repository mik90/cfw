#include "cxx_fizz_buzz/src/bridge.rs.h"

#include <cmath>
#include <cstdint>
#include <stdexcept>
#include <string>
#include <utility>

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
    std::uint64_t const* head = inputs.value_try_value();
    if (head == nullptr) {
        return;
    }
    std::size_t const len = inputs.value_len();
    std::uint64_t const first = inputs.value_at(0);
    std::int64_t const published_at = inputs.value_header().published_at;
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

void NormTask::run(NormCalculatorCxxInputs& inputs,
                   NormCalculatorCxxOutputs& outputs,
                   Context const& /*context*/) {
    cfw::messages::Vec3 const& value = inputs.value();
    double const length = std::sqrt(value.x * value.x + value.y * value.y +
                                    value.z * value.z);
    if (length == 0.0) {
        throw std::runtime_error("zero-length vector");
    }
    outputs.length_value() = length;
    outputs.length_send();
}

std::unique_ptr<NormTask> make_norm_task() {
    return std::make_unique<NormTask>();
}

} // namespace cfw::fizz_buzz
