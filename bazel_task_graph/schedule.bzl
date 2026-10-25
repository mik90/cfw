"""Declare callback scheduling settings in Bazel.

Each schedule target is a small Rust library exposing `__cfw_schedule()`. Tasks
forward that function through a bridge crate, so custom schedule crates flow
through the normal Bazel dependency graph.
"""

load("//bazel:rust.bzl", "rust_library")
load("//bazel_task_graph:labels.bzl", "crate_name")
load("//bazel_task_graph:time.bzl", "duration_expression", "duration_nanos")

CfwCallbackScheduleInfo = provider(
    doc = "A cfw callback schedule.",
    fields = {
        "crate": "Rust library exposing __cfw_schedule().",
        "kind": "Either 'fixed' or 'custom'.",
    },
)

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
    runtime_nanos = duration_nanos("execution_duration", execution_duration)
    base = "::task_graph::CallbackSchedule::input_triggered()"
    if periodic:
        period_nanos = duration_nanos("periodic", periodic)
        if period_nanos == 0:
            fail("fixed_schedule '%s' periodic must be greater than zero" % name)
        base = "::task_graph::CallbackSchedule::periodic(%s)" % duration_expression(period_nanos)

    content = """/// Schedule selected by a Bazel fixed_schedule declaration.
pub fn __cfw_schedule() -> ::task_graph::CallbackSchedule {
    %s.with_execution_duration(%s)
}
""" % (base, duration_expression(runtime_nanos))

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
