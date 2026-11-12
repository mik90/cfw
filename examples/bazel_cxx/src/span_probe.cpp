#include "cxx_fizz_buzz/span_probe_gen/span_probe_cxx_bridge.rs.h"

#include <array>
#include <cstdint>
#include <string>
#include <utility>

#include "cxx_fizz_buzz/src/fizz_buzz.hpp"
#include "rust/cxx.h"

namespace cfw::fizz_buzz {

void SpanProbeTask::run(SpanProbeCxxInputs& inputs,
                        SpanProbeCxxOutputs& outputs,
                        const Context& /*context*/) {
    const std::uint64_t count = inputs.count();
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
