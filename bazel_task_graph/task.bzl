"""Declare a cfw task linked to a user implementation crate.

The generated API crate contains the user-facing `Inputs`/`Outputs` types, the
`Run` trait, and an exported `impl_task!` macro. The implementation crate
implements `Run` and invokes `impl_task!` for each task type. A bridge crate
forwards the schedule selected by `cfw_fixed_schedule()` or `cfw_custom_schedule()`.

One Rust type may back one task definition. Reuse the same type in multiple
pools with per-instance channel overrides; use a newtype if the endpoint
structure differs.
"""

load("//bazel:rust.bzl", "RUST_SOURCE_TOOLCHAINS", "render_rust_source", "rust_library", "rust_source_attrs")
load("//bazel_task_graph:labels.bzl", "crate_name", "field_name", "info_label")
load("//bazel_task_graph:publisher.bzl", "CfwPublisherInfo")
load("//bazel_task_graph:schedule.bzl", "CfwCallbackScheduleInfo")
load("//bazel_task_graph:subscriber.bzl", "CfwSubscriberInfo")

CfwTaskInfo = provider(
    doc = "Composition metadata for a cfw task.",
    fields = {
        "callback_name": "Name used in ApplicationBuilder.",
        "implementation": "Rust crate that defines the task type.",
        "input_channels": "Endpoint-name to channel overrides for inputs.",
        "output_channels": "Endpoint-name to channel overrides for outputs.",
        "rust_type": "Fully qualified Rust type of the task.",
        "schedule": "Rust expression producing a CallbackSchedule.",
    },
)

def _rust_ident(name):
    parts = [part for part in name.replace("-", "_").split("_") if part]
    return "".join([part[:1].upper() + part[1:] for part in parts])

def _wrapper_type(kind, input):
    if input:
        wrappers = {
            "required": "RequiredInput",
            "optional": "Input",
            "span": "InputSpan",
            "iox2_input": "Iox2OptionalInput",
            "iox2_span": "Iox2SpanInput",
            "iox2_event": "Iox2Event",
        }
    else:
        wrappers = {
            "output": "Output",
            "output_span": "OutputSpan",
            "uninit": "OutputUninit",
            "publisher": "Publisher",
            "iox2_output": "Iox2Output",
            "iox2_notifier": "Iox2NotifyOutput",
        }
    if kind not in wrappers:
        fail("unknown %s endpoint kind '%s'" % ("subscriber" if input else "publisher", kind))
    return wrappers[kind]

def _bool_literal(value):
    return "true" if value else "false"

def _task_api_src_impl(ctx):
    subscribers = [subscriber[CfwSubscriberInfo] for subscriber in ctx.attr.subscribers]
    publishers = [publisher[CfwPublisherInfo] for publisher in ctx.attr.publishers]

    seen = {}
    for endpoint in subscribers + publishers:
        if endpoint.field in seen:
            fail(
                "task '%s' declares endpoint '%s' more than once" % (
                    ctx.attr.task_name,
                    endpoint.field,
                ),
            )
        seen[endpoint.field] = True

    subscriber_lines = []
    for endpoint in subscribers:
        wrapper = _wrapper_type(endpoint.kind, True)
        if endpoint.has_payload:
            alias = "%s::%s::Payload" % (ctx.attr.payloads_module, endpoint.field)
            if endpoint.forwarded:
                alias += "<'storage>"
            type_expr = "%s<%s>" % (wrapper, alias)
        else:
            type_expr = wrapper
        lines = [
            "        %s: %s {" % (endpoint.field, type_expr),
            '            channel: "%s",' % endpoint.channel_name,
            "            capacity: %d," % endpoint.capacity,
        ]
        if not endpoint.kind.startswith("iox2_"):
            lines.append("            trigger: %s," % _bool_literal(endpoint.trigger))
        if endpoint.kind != "iox2_event":
            lines.append(
                "            keep_across_runs: %s," % _bool_literal(endpoint.keep_across_runs),
            )
        if endpoint.forwarded:
            lines.extend([
                "            forwarded_message: %s::%s::Message," % (
                    ctx.attr.payloads_module,
                    endpoint.field,
                ),
                "            forwarded_sources: %s::%s::Sources," % (
                    ctx.attr.payloads_module,
                    endpoint.field,
                ),
            ])
        if endpoint.forwardable:
            lines.append("            forwardable: true,")
        lines.append("        },")
        subscriber_lines.extend(lines)

    publisher_lines = []
    for endpoint in publishers:
        wrapper = _wrapper_type(endpoint.kind, False)
        if endpoint.has_payload:
            alias = "%s::%s::Payload" % (ctx.attr.payloads_module, endpoint.field)
            if endpoint.forwarded:
                alias += "<'storage>"
            type_expr = "%s<%s>" % (wrapper, alias)
        else:
            type_expr = wrapper
        lines = [
            "        %s: %s {" % (endpoint.field, type_expr),
            '            channel: "%s",' % endpoint.channel_name,
        ]
        if endpoint.kind != "iox2_notifier":
            lines.append("            capacity: %d," % endpoint.capacity)
        if endpoint.forwarded:
            lines.extend([
                "            forwarded_message: %s::%s::Message," % (
                    ctx.attr.payloads_module,
                    endpoint.field,
                ),
                "            forwarded_sources: %s::%s::Sources," % (
                    ctx.attr.payloads_module,
                    endpoint.field,
                ),
            ])
        if endpoint.forwards:
            mappings = ", ".join([
                "%s: %s" % (source, input)
                for source, input in sorted(endpoint.forwards.items())
            ])
            lines.append("            forwards: { %s }," % mappings)
        lines.append("        },")
        publisher_lines.extend(lines)

    payload_lines = [
        "        %s: %s::Payload," % (endpoint.field, endpoint.crate_name)
        for endpoint in subscribers + publishers
        if endpoint.has_payload
    ]

    content = """::task_macros::define_task! {
    api: %s,
    name: %s,
    payloads_module: %s,
    subscribers: {
%s
    },
    publishers: {
%s
    },
    payloads: {
%s
    },
}
""" % (
        ctx.attr.api_crate,
        ctx.attr.task_name,
        ctx.attr.payloads_module,
        "\n".join(subscriber_lines),
        "\n".join(publisher_lines),
        "\n".join(payload_lines),
    )
    output = render_rust_source(ctx, content)
    return [DefaultInfo(files = depset([output]))]

_task_api_src = rule(
    implementation = _task_api_src_impl,
    attrs = rust_source_attrs({
        "api_crate": attr.string(mandatory = True),
        "payloads_module": attr.string(mandatory = True),
        "publishers": attr.label_list(
            default = [],
            providers = [CfwPublisherInfo],
        ),
        "subscribers": attr.label_list(
            default = [],
            providers = [CfwSubscriberInfo],
        ),
        "task_name": attr.string(mandatory = True),
    }),
    toolchains = RUST_SOURCE_TOOLCHAINS,
)

def _task_schedule_src_impl(ctx):
    if ctx.attr.schedule:
        schedule = ctx.attr.schedule[CfwCallbackScheduleInfo]
        expression = "::%s::__cfw_schedule()" % crate_name(schedule.crate)
    else:
        expression = "::task_graph::CallbackSchedule::input_triggered()"

    content = """/// Schedule selected by the Bazel task declaration.
pub fn __cfw_schedule() -> ::task_graph::CallbackSchedule {
    %s
}
""" % expression
    output = render_rust_source(ctx, content)
    return [DefaultInfo(files = depset([output]))]

_task_schedule_src = rule(
    implementation = _task_schedule_src_impl,
    attrs = rust_source_attrs({
        "schedule": attr.label(
            default = None,
            providers = [CfwCallbackScheduleInfo],
        ),
    }),
    toolchains = RUST_SOURCE_TOOLCHAINS,
)

def _task_meta_impl(ctx):
    return [
        CfwTaskInfo(
            callback_name = ctx.attr.callback_name,
            implementation = ctx.attr.implementation,
            input_channels = ctx.attr.input_channels,
            output_channels = ctx.attr.output_channels,
            rust_type = ctx.attr.rust_type,
            schedule = ctx.attr.schedule,
        ),
    ]

_task_meta = rule(
    implementation = _task_meta_impl,
    attrs = {
        "callback_name": attr.string(mandatory = True),
        "implementation": attr.label(mandatory = True),
        "input_channels": attr.string_dict(default = {}),
        "output_channels": attr.string_dict(default = {}),
        "rust_type": attr.string(mandatory = True),
        "schedule": attr.string(),
    },
)

def _task_impl(
        name,
        visibility,
        implementation,
        rust_type,
        subscribers,
        publishers,
        schedule,
        input_channels,
        output_channels,
        callback_name,
        **kwargs):
    api_crate = name.replace("-", "_") + "_api"
    payloads_module = "__cfw_payloads_" + field_name(name)
    subscriber_info = [info_label(subscriber) for subscriber in subscribers]
    publisher_info = [info_label(publisher) for publisher in publishers]
    schedule_info = info_label(schedule) if schedule else None
    schedule_expression = "::%s::__cfw_schedule()" % field_name(name + "_schedule_bridge")

    _task_api_src(
        name = name + "_api_src",
        api_crate = api_crate,
        payloads_module = payloads_module,
        publishers = publisher_info,
        subscribers = subscriber_info,
        task_name = _rust_ident(name),
    )
    rust_library(
        name = name + "_api",
        srcs = [":" + name + "_api_src"],
        deps = subscribers + publishers + ["//task:task"],
        proc_macro_deps = ["//task_macros:task_macros"],
        visibility = visibility,
        **kwargs
    )
    _task_schedule_src(
        name = name + "_schedule_src",
        schedule = schedule_info,
    )
    rust_library(
        name = name + "_schedule_bridge",
        srcs = [":" + name + "_schedule_src"],
        deps = ([schedule] if schedule else []) + ["//task_graph:task_graph"],
        visibility = visibility,
        **kwargs
    )
    _task_meta(
        name = name,
        callback_name = callback_name or name,
        implementation = implementation,
        input_channels = input_channels,
        output_channels = output_channels,
        rust_type = rust_type,
        schedule = schedule_expression,
        visibility = visibility,
    )

cfw_task = macro(
    attrs = {
        "implementation": attr.label(
            mandatory = True,
            configurable = False,
            doc = "Rust crate that defines the task type.",
        ),
        "rust_type": attr.string(
            mandatory = True,
            configurable = False,
            doc = "Fully qualified Rust type, e.g. 'task_impls::Counter'.",
        ),
        "subscribers": attr.label_list(
            default = [],
            configurable = False,
            doc = "Subscriber targets produced by cfw_subscriber().",
        ),
        "publishers": attr.label_list(
            default = [],
            configurable = False,
            doc = "Publisher targets produced by cfw_publisher().",
        ),
        "schedule": attr.label(
            default = None,
            providers = [CfwCallbackScheduleInfo],
            configurable = False,
            doc = "Schedule target produced by cfw_fixed_schedule() or cfw_custom_schedule().",
        ),
        "input_channels": attr.string_dict(
            default = {},
            configurable = False,
            doc = "Per-instance input channel overrides.",
        ),
        "output_channels": attr.string_dict(
            default = {},
            configurable = False,
            doc = "Per-instance output channel overrides.",
        ),
        "callback_name": attr.string(
            default = "",
            configurable = False,
            doc = "Override the callback name used by ApplicationBuilder.",
        ),
    },
    implementation = _task_impl,
    inherit_attrs = "common",
)
