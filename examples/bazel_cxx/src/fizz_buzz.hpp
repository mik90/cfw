#pragma once

#include <cstdint>
#include <memory>

namespace cfw {
// Defined by the cxx-generated header for task_cxx.
struct Context;
struct MessageHeader;
} // namespace cfw

namespace cfw::messages {
// Defined by the cxx-generated header for cxx_fizz_buzz_messages.
struct Vec3;
} // namespace cfw::messages

namespace cfw::fizz_buzz {

// Defined by the cxx-generated header for src/bridge.rs; forward declared here
// so this header can be included from that generated header.
struct FizzBuzzCalculatorCxxInputs;
struct FizzBuzzCalculatorCxxOutputs;
struct OptionalProbeCxxInputs;
struct OptionalProbeCxxOutputs;
struct NormCalculatorCxxInputs;
struct NormCalculatorCxxOutputs;
struct SpanProbeCxxInputs;
struct SpanProbeCxxOutputs;

class FizzBuzzTask {
  public:
    void run(FizzBuzzCalculatorCxxInputs& inputs,
             FizzBuzzCalculatorCxxOutputs& outputs, const Context& context);
};

std::unique_ptr<FizzBuzzTask> make_fizz_buzz_task();

/// Reports the optional input head, queue length, first element, and its
/// publication time, then pops.
class OptionalProbeTask {
  public:
    void run(OptionalProbeCxxInputs& inputs, OptionalProbeCxxOutputs& outputs,
             const Context& context);
};

std::unique_ptr<OptionalProbeTask> make_optional_probe_task();

/// Computes the norm of a shared-struct payload; throws on a zero-length
/// vector to exercise the fallible `Result` bridge.
class NormTask {
  public:
    void run(NormCalculatorCxxInputs& inputs, NormCalculatorCxxOutputs& outputs,
             const Context& context);
};

std::unique_ptr<NormTask> make_norm_task();

/// Writes a span of outputs with mixed `write` and placement-new loans,
/// exercising capacity exhaustion, per-index sends, and an in-place String.
class SpanProbeTask {
  public:
    void run(SpanProbeCxxInputs& inputs, SpanProbeCxxOutputs& outputs,
             const Context& context);
};

std::unique_ptr<SpanProbeTask> make_span_probe_task();

} // namespace cfw::fizz_buzz
