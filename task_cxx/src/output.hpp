#pragma once

#include <cstddef>
#include <cstdint>
#include <memory>
#include <new>
#include <optional>
#include <stdexcept>
#include <type_traits>
#include <utility>

namespace cfw {

namespace detail {
using SlotThunk = void (*)(void*, std::size_t);
} // namespace detail

template <typename T>
class Output;

/// Reserved but uninitialized output slot.
///
/// Returned by value from a task's `*_uninit` bridge method or by
/// `SpanHandle::take_entry_uninit()`. It is deliberately trivially relocatable
/// (see the Rust side in `task_cxx/src/output.rs`) so cxx can pass it across
/// the FFI. It must not outlive the C++ `run` call.
struct OutputUninit {
    void* payload = nullptr;
    void* context = nullptr;
    detail::SlotThunk send = nullptr;
    detail::SlotThunk discard = nullptr;
    std::size_t index = 0;

    explicit operator bool() const noexcept { return payload != nullptr; }

    /// Constructs `T` in place with the forwarded constructor arguments and
    /// returns an owning handle. Consumes this descriptor.
    ///
    /// When no slot is available (`payload` is null) the result is an empty
    /// handle; check it with `operator bool` before using it.
    template <typename T, typename... Args>
    Output<T> init(Args&&... args) && {
        void* slot = std::exchange(payload, nullptr);
        return Output<T>(typename Output<T>::InitInPlace{}, slot, context,
                         send, discard, index,
                         std::forward<Args>(args)...);
    }

  private:
    /// Adopts a payload already constructed by the framework (span `loan`).
    template <typename T>
    Output<T> adopt() && {
        void* slot = std::exchange(payload, nullptr);
        return Output<T>(typename Output<T>::RustOwned{}, slot, context, send,
                         discard, index);
    }

    friend struct SpanHandle;
};

static_assert(std::is_trivially_move_constructible_v<OutputUninit>);
static_assert(std::is_trivially_destructible_v<OutputUninit>);

/// Owning handle for an initialized output.
///
/// Dropping it without sending destroys the payload when C++ constructed it
/// in place and releases the loan, mirroring `task::OutputInit` in Rust. It
/// must not outlive the C++ `run` call.
template <typename T>
class Output {
  public:
    Output() noexcept = default;

    Output(Output&& other) noexcept
        : payload_(std::exchange(other.payload_, nullptr)),
          context_(other.context_), send_(other.send_),
          discard_(other.discard_), index_(other.index_),
          owns_payload_(other.owns_payload_) {}

    Output& operator=(Output&& other) noexcept {
        if (this != &other) {
            reset();
            payload_ = std::exchange(other.payload_, nullptr);
            context_ = other.context_;
            send_ = other.send_;
            discard_ = other.discard_;
            index_ = other.index_;
            owns_payload_ = other.owns_payload_;
        }
        return *this;
    }

    Output(const Output&) = delete;
    Output& operator=(const Output&) = delete;

    ~Output() { reset(); }

    explicit operator bool() const noexcept { return payload_ != nullptr; }
    T* operator->() const noexcept { return payload_; }
    T& operator*() const noexcept { return *payload_; }
    T& value() const noexcept { return *payload_; }
    T& value_mut() const noexcept { return *payload_; }

    /// Publishes the payload and disarms the handle. Like Rust's
    /// `OutputSpanEntry::send`, this consumes the output.
    void send() && {
        if (payload_ != nullptr) {
            if (send_ != nullptr) {
                send_(context_, index_);
            }
            payload_ = nullptr;
        }
    }

  private:
    friend struct OutputUninit;

    struct InitInPlace {};
    struct RustOwned {};

    template <typename... Args>
    Output(InitInPlace, void* slot, void* context, detail::SlotThunk send,
           detail::SlotThunk discard, std::size_t index, Args&&... args)
        : payload_(slot == nullptr
                       ? nullptr
                       : ::new (slot) T(std::forward<Args>(args)...)),
          context_(context), send_(send), discard_(discard), index_(index),
          owns_payload_(true) {}

    Output(RustOwned, void* slot, void* context, detail::SlotThunk send,
           detail::SlotThunk discard, std::size_t index)
        : payload_(slot == nullptr ? nullptr : static_cast<T*>(slot)),
          context_(context), send_(send), discard_(discard), index_(index),
          owns_payload_(false) {}

    void reset() noexcept {
        if (payload_ != nullptr) {
            if (owns_payload_) {
                std::destroy_at(payload_);
            }
            if (discard_ != nullptr) {
                discard_(context_, index_);
            }
            payload_ = nullptr;
        }
    }

    T* payload_ = nullptr;
    void* context_ = nullptr;
    detail::SlotThunk send_ = nullptr;
    detail::SlotThunk discard_ = nullptr;
    std::size_t index_ = 0;
    bool owns_payload_ = false;
};

/// Status returned by a span entry thunk.
enum class SpanEntryStatus : std::uint8_t {
    Taken = 0,
    Full = 1,
    ArenaExhausted = 2,
};

/// Result of taking an entry through a span thunk.
struct SpanEntryOutcome {
    OutputUninit output;
    SpanEntryStatus status;
};

/// Trivial proxy for a span publisher, mirroring Rust's `OutputSpan`.
///
/// Each take returns an owning `Output<T>` that sends or releases its own slot.
/// It must not outlive the C++ `run` call.
struct SpanHandle {
    void* context = nullptr;
    SpanEntryOutcome (*take_entry_uninit_fn)(void*) = nullptr;
    SpanEntryOutcome (*take_entry_fn)(void*, void*) = nullptr;

    /// Takes an uninitialized entry.
    ///
    /// Returns `std::nullopt` when every entry is already reserved or sent.
    /// Throws `std::runtime_error` if the publisher arena is smaller than the
    /// span's declared capacity.
    std::optional<OutputUninit> take_entry_uninit() const {
        SpanEntryOutcome outcome = take_entry_uninit_fn(context);
        switch (outcome.status) {
        case SpanEntryStatus::Taken:
            return outcome.output;
        case SpanEntryStatus::Full:
            return std::nullopt;
        case SpanEntryStatus::ArenaExhausted:
            throw std::runtime_error(
                "cfw span entry exceeded planned publisher arena capacity");
        }
        throw std::runtime_error("cfw span entry returned an unknown status");
    }

    /// Takes an entry initialized with `value`.
    ///
    /// Returns `std::nullopt` when every entry is already reserved or sent.
    /// Throws `std::runtime_error` if the publisher arena is smaller than the
    /// span's declared capacity. Rust always consumes the staged value, whether
    /// or not an entry was available.
    template <typename T>
    std::optional<Output<T>> take_entry(T value) const {
        alignas(T) unsigned char storage[sizeof(T)];
        T* staged = ::new (static_cast<void*>(storage)) T(std::move(value));
        SpanEntryOutcome outcome = take_entry_fn(context, staged);
        switch (outcome.status) {
        case SpanEntryStatus::Taken:
            return std::optional<Output<T>>(
                std::move(outcome.output).adopt<T>());
        case SpanEntryStatus::Full:
            return std::nullopt;
        case SpanEntryStatus::ArenaExhausted:
            throw std::runtime_error(
                "cfw span entry exceeded planned publisher arena capacity");
        }
        throw std::runtime_error("cfw span entry returned an unknown status");
    }
};

static_assert(std::is_trivially_move_constructible_v<SpanHandle>);
static_assert(std::is_trivially_destructible_v<SpanHandle>);

} // namespace cfw
