"""Declare callback scheduling settings in Bazel.

Each schedule target is a small Rust library exposing `__cfw_schedule()`. Tasks
forward that function through a bridge crate, so custom schedule crates flow
through the normal Bazel dependency graph.
"""

load("//bazel:rust.bzl", "rust_library")
load("//bazel_task_graph:labels.bzl", "crate_name")

CfwCallbackScheduleInfo = provider(
    doc = "A cfw callback schedule.",
    fields = {
        "crate": "Rust library exposing __cfw_schedule().",
        "kind": "Either 'fixed' or 'custom'.",
    },
)

_DECIMAL_SCALES = [1, 10, 100, 1000, 10000, 100000, 1000000, 10000000, 100000000, 1000000000]

_DURATION_UNITS = {
    "ns": 1,
    "nanos": 1,
    "nanosecond": 1,
    "nanoseconds": 1,
    "us": 1000,
    "micros": 1000,
    "microsecond": 1000,
    "microseconds": 1000,
    "ms": 1000000,
    "millis": 1000000,
    "millisecond": 1000000,
    "milliseconds": 1000000,
    "s": 1000000000,
    "sec": 1000000000,
    "secs": 1000000000,
    "second": 1000000000,
    "seconds": 1000000000,
    "m": 60000000000,
    "min": 60000000000,
    "mins": 60000000000,
    "minute": 60000000000,
    "minutes": 60000000000,
    "h": 3600000000000,
    "hr": 3600000000000,
    "hrs": 3600000000000,
    "hour": 3600000000000,
    "hours": 3600000000000,
}

def _duration_nanos(attr_name, value):
    text = value.replace(" ", "").lower()
    if not text:
        fail("%s must be a duration such as '100ms'" % attr_name)

    split = len(text)
    for index in range(len(text)):
        char = text[index]
        if char not in "0123456789.":
            split = index
            break
    if split == 0 or split == len(text):
        fail("%s must be a number followed by a unit, got '%s'" % (attr_name, value))

    number = text[:split]
    unit = text[split:]
    if unit not in _DURATION_UNITS:
        fail(
            "%s has unknown unit '%s' in '%s'; expected one of: %s" % (
                attr_name,
                unit,
                value,
                ", ".join(sorted(_DURATION_UNITS.keys())),
            ),
        )

    parts = number.split(".")
    if len(parts) > 2:
        fail("%s has an invalid number in '%s'" % (attr_name, value))
    whole = parts[0]
    fraction = parts[1] if len(parts) == 2 else ""
    if (whole and not whole.isdigit()) or (fraction and not fraction.isdigit()):
        fail("%s has an invalid number in '%s'" % (attr_name, value))
    if len(fraction) > 9:
        fail("%s is more precise than one nanosecond in '%s'" % (attr_name, value))

    unit_nanos = _DURATION_UNITS[unit]
    whole_nanos = int(whole or "0") * unit_nanos
    fraction_nanos = 0
    if fraction:
        scale = _DECIMAL_SCALES[len(fraction)]
        scaled = int(fraction) * unit_nanos
        if scaled % scale:
            fail("%s is more precise than one nanosecond in '%s'" % (attr_name, value))
        fraction_nanos = scaled // scale
    return whole_nanos + fraction_nanos

def _duration_expr(nanos):
    return "::core::time::Duration::from_nanos(%d)" % nanos

def _schedule_src_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".rs")
    ctx.actions.write(output, ctx.attr.content)
    return [DefaultInfo(files = depset([output]))]

_schedule_src = rule(
    implementation = _schedule_src_impl,
    attrs = {
        "content": attr.string(mandatory = True),
    },
)

def _schedule_info_impl(ctx):
    return [
        CfwCallbackScheduleInfo(
            crate = ctx.attr.crate.label,
            kind = ctx.attr.kind,
        ),
    ]

_schedule_info = rule(
    implementation = _schedule_info_impl,
    attrs = {
        "crate": attr.label(mandatory = True),
        "kind": attr.string(
            mandatory = True,
            values = ["fixed", "custom"],
        ),
    },
)

def _schedule_library(name, visibility, kind, content, deps, **kwargs):
    _schedule_src(
        name = name + "_src",
        content = content,
    )
    rust_library(
        name = name,
        srcs = [":" + name + "_src"],
        deps = deps + ["//task_graph:task_graph"],
        visibility = visibility,
        **kwargs
    )
    _schedule_info(
        name = name + "_info",
        crate = ":" + name,
        kind = kind,
        visibility = visibility,
    )

def _fixed_schedule_impl(name, visibility, execution_duration, periodic, **kwargs):
    runtime_nanos = _duration_nanos("execution_duration", execution_duration)
    base = "::task_graph::CallbackSchedule::input_triggered()"
    if periodic:
        period_nanos = _duration_nanos("periodic", periodic)
        if period_nanos == 0:
            fail("fixed_schedule '%s' periodic must be greater than zero" % name)
        base = "::task_graph::CallbackSchedule::periodic(%s)" % _duration_expr(period_nanos)

    content = """/// Schedule selected by a Bazel fixed_schedule declaration.
pub fn __cfw_schedule() -> ::task_graph::CallbackSchedule {
    %s.with_execution_duration(%s)
}
""" % (base, _duration_expr(runtime_nanos))

    _schedule_library(
        name = name,
        visibility = visibility,
        kind = "fixed",
        content = content,
        deps = [],
        **kwargs
    )

fixed_schedule = macro(
    attrs = {
        "execution_duration": attr.string(
            mandatory = True,
            configurable = False,
            doc = "Fixed simulated execution duration, such as '1ms'.",
        ),
        "periodic": attr.string(
            default = "",
            configurable = False,
            doc = "Optional periodic rate, such as '100ms'. Implies run_on_start.",
        ),
    },
    implementation = _fixed_schedule_impl,
    inherit_attrs = "common",
)

def _custom_schedule_impl(name, visibility, crate, function, **kwargs):
    function = function or "schedule"
    if function.startswith("::"):
        fail("custom_schedule '%s' function must be relative to its crate" % name)

    content = """/// Schedule selected by a Bazel custom_schedule declaration.
pub fn __cfw_schedule() -> ::task_graph::CallbackSchedule {
    ::%s::%s()
}
""" % (crate_name(crate), function)

    _schedule_library(
        name = name,
        visibility = visibility,
        kind = "custom",
        content = content,
        deps = [crate],
        **kwargs
    )

custom_schedule = macro(
    attrs = {
        "crate": attr.label(
            mandatory = True,
            configurable = False,
            doc = "Rust crate that implements the schedule function.",
        ),
        "function": attr.string(
            default = "schedule",
            configurable = False,
            doc = "Function path within the crate, such as 'Doubler::schedule'.",
        ),
    },
    implementation = _custom_schedule_impl,
    inherit_attrs = "common",
)
