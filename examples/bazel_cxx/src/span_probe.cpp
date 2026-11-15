#include "cxx_fizz_buzz/span_probe_gen/span_probe_cxx_bridge.rs.h"

#include <array>
#include <cstdint>
#include <optional>
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
    std::size_t entries = 0;
    for (std::uint64_t index = 0; index < count; ++index) {
        std::optional<cfw::Output<std::uint64_t>> entry;
        if (index % 2 == 0) {
            entry = span.take_entry(index);
        } else {
            auto raw = span.take_entry_uninit();
            if (!raw) {
                break;
            }
            entry = std::move(*raw).init<std::uint64_t>(index);
        }
        if (!entry || entries == slots.size()) {
            break;
        }
        slots[entries++] = std::move(*entry);
    }

    // In-place string output: the handle construction uses placement new and
    // owns the value until sent (or destroys it on early return).
    auto label = outputs.label_uninit().init<rust::String>(
        "span:" + std::to_string(entries));
    if (count == 0) {
        return;
    }
    std::move(label).send();

    // Publish the second entry out of order, then the rest in entry order.
    if (entries > 1) {
        std::move(slots[1]).send();
    }
    for (std::size_t index = 0; index < entries; ++index) {
        if (index != 1) {
            std::move(slots[index]).send();
        }
    }
}

std::unique_ptr<SpanProbeTask> make_span_probe_task() {
    return std::make_unique<SpanProbeTask>();
}

} // namespace cfw::fizz_buzz
