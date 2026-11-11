#pragma once

#include <cstdint>
#include <memory>

namespace cfw {
// Defined by the cxx-generated header for task_cxx.
struct Context;
} // namespace cfw

namespace cfw::fizz_buzz {

// Defined by the cxx-generated header for src/bridge.rs; forward declared here
// so this header can be included from that generated header.
struct FizzBuzzCalculatorCxxInputs;
struct FizzBuzzCalculatorCxxOutputs;
struct OptionalProbeCxxInputs;
struct OptionalProbeCxxOutputs;

class FizzBuzzTask {
  public:
    void run(FizzBuzzCalculatorCxxInputs& inputs,
             FizzBuzzCalculatorCxxOutputs& outputs, Context const& context);
};

std::unique_ptr<FizzBuzzTask> make_fizz_buzz_task();

/// Reports the optional input head, queue length, and first element, then pops.
class OptionalProbeTask {
  public:
    void run(OptionalProbeCxxInputs& inputs, OptionalProbeCxxOutputs& outputs,
             Context const& context);
};

std::unique_ptr<OptionalProbeTask> make_optional_probe_task();

} // namespace cfw::fizz_buzz
