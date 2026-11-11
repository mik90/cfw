#include "cxx_fizz_buzz/gen/bridge.rs.h"

#include <array>
#include <cmath>
#include <cstdint>
#include <new>
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

void SpanProbeTask::run(SpanProbeCxxInputs& inputs,
                        SpanProbeCxxOutputs& outputs,
                        Context const& /*context*/) {
    std::uint64_t const count = inputs.count();
    auto span = outputs.numbers_span();

    // The declared span capacity is four.
    std::array<cfw::Output<std::uint64_t>, 4> slots;
    std::size_t loans = 0;
    for (std::uint64_t index = 0; index < count; ++index) {
        cfw::Output<std::uint64_t> slot;
        if (index % 2 == 0) {
            slot = span.loan(index);
        } else {
            slot = span.loan_uninit().init<std::uint64_t>(index);
        }
        if (!slot || loans == slots.size()) {
            break;
        }
        slots[loans++] = std::move(slot);
    }

    // In-place string output: the handle construction uses placement new and
    // owns the value until sent (or destroys it on early return).
    auto label = outputs.label_uninit().init<rust::String>(
        "span:" + std::to_string(loans));
    if (count == 0) {
        return;
    }
    std::move(label).send();

    // Publish the second loan out of order, then the rest in loan order.
    if (loans > 1) {
        std::move(slots[1]).send();
    }
    for (std::size_t index = 0; index < loans; ++index) {
        if (index != 1) {
            std::move(slots[index]).send();
        }
    }
}

std::unique_ptr<SpanProbeTask> make_span_probe_task() {
    return std::make_unique<SpanProbeTask>();
}

} // namespace cfw::fizz_buzz
