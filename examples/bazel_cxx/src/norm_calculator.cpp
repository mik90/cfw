#include "cxx_fizz_buzz/norm_calculator_gen/norm_calculator_cxx_bridge.rs.h"

#include <cmath>
#include <stdexcept>

#include "cxx_fizz_buzz/src/fizz_buzz.hpp"

namespace cfw::fizz_buzz {

void NormTask::run(NormCalculatorCxxInputs& inputs,
                   NormCalculatorCxxOutputs& outputs,
                   const Context& /*context*/) {
    const cfw::messages::Vec3& value = inputs.value();
    const double length = std::sqrt(value.x * value.x + value.y * value.y +
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
