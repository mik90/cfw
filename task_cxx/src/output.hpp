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
/// Returned by value from a task's `*_uninit` bridge method or by
/// `SpanHandle::loan_uninit()`. It is deliberately trivially relocatable (see
/// the Rust side in `task_cxx/src/output.rs`) so cxx can pass it across the
/// FFI. It must not outlive the C++ `run` call.
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

/// Trivial proxy for a span publisher, mirroring Rust's `OutputSpan`.
///
/// Each loan returns an owning `Output<T>` that sends or releases its own slot.
/// It must not outlive the C++ `run` call.
struct SpanHandle {
    void* context = nullptr;
    OutputUninit (*loan_uninit_fn)(void*) = nullptr;
    OutputUninit (*loan_fn)(void*, void*) = nullptr;

    /// Reserves an uninitialized slot.
    OutputUninit loan_uninit() const noexcept {
        return loan_uninit_fn(context);
    }

    /// Reserves a slot initialized with `value`.
    template <typename T>
    Output<T> loan(T value) const {
        alignas(T) unsigned char storage[sizeof(T)];
        T* staged = ::new (static_cast<void*>(storage)) T(std::move(value));
        OutputUninit raw = loan_fn(context, staged);
        // Rust always takes ownership of the staged value, whether or not the
        // reservation succeeded, so C++ must not destroy it afterwards.
        return std::move(raw).adopt<T>();
    }
};

static_assert(std::is_trivially_move_constructible_v<SpanHandle>);
static_assert(std::is_trivially_destructible_v<SpanHandle>);

} // namespace cfw
