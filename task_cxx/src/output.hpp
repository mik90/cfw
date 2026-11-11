#pragma once

#include <cstddef>
#include <memory>
#include <new>
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
/// Returned by value from a task's `*_uninit` bridge method. It is deliberately
/// trivially relocatable (see the Rust side in `task_cxx/src/output.rs`) so cxx
/// can pass it across the FFI. It must not outlive the C++ `run` call.
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
        return Output<T>(slot, context, send, discard, index,
                         std::forward<Args>(args)...);
    }
};

static_assert(std::is_trivially_move_constructible_v<OutputUninit>);
static_assert(std::is_trivially_destructible_v<OutputUninit>);

/// Owning handle for an initialized in-place output.
///
/// Dropping it without sending destroys the payload and releases the loan,
/// mirroring `task::OutputInit` in Rust. It must not outlive the C++ `run`
/// call.
template <typename T>
class Output {
  public:
    Output() noexcept = default;

    Output(Output&& other) noexcept
        : payload_(std::exchange(other.payload_, nullptr)),
          context_(other.context_), send_(other.send_),
          discard_(other.discard_), index_(other.index_) {}

    Output& operator=(Output&& other) noexcept {
        if (this != &other) {
            reset();
            payload_ = std::exchange(other.payload_, nullptr);
            context_ = other.context_;
            send_ = other.send_;
            discard_ = other.discard_;
            index_ = other.index_;
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
    /// `OutputInit::send`, this consumes the output.
    void send() && noexcept {
        if (payload_ != nullptr) {
            if (send_ != nullptr) {
                send_(context_, index_);
            }
            payload_ = nullptr;
        }
    }

  private:
    friend struct OutputUninit;

    template <typename... Args>
    Output(void* slot, void* context, detail::SlotThunk send,
           detail::SlotThunk discard, std::size_t index, Args&&... args)
        : payload_(slot == nullptr
                       ? nullptr
                       : ::new (slot) T(std::forward<Args>(args)...)),
          context_(context), send_(send), discard_(discard), index_(index) {}

    void reset() noexcept {
        if (payload_ != nullptr) {
            std::destroy_at(payload_);
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
};

} // namespace cfw
