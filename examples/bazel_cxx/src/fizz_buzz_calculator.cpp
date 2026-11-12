#include "cxx_fizz_buzz/fizz_buzz_calculator_gen/fizz_buzz_calculator_cxx_bridge.rs.h"

#include <cstdint>
#include <string>
#include <utility>

#include "cxx_fizz_buzz/src/fizz_buzz.hpp"

namespace cfw::fizz_buzz {

void FizzBuzzTask::run(FizzBuzzCalculatorCxxInputs& inputs,
                       FizzBuzzCalculatorCxxOutputs& outputs,
                       const Context& /*context*/) {
    const std::uint64_t value = inputs.integer();
    std::string result;
    const bool is_fizz = value % 3 == 0;
    const bool is_buzz = value % 5 == 0;
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

} // namespace cfw::fizz_buzz
