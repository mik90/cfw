"""Subscriber endpoint targets for cfw tasks.

Each subscriber creates a tiny Rust re-export crate so payload dependencies flow
through the Bazel dependency graph.
"""

load("//bazel:rust.bzl", "rust_library")
load("//bazel_task_graph:channel.bzl", "CfwChannelInfo")
load(
    "//bazel_task_graph:endpoint_common.bzl",
    "endpoint_default_capacity",
    "endpoint_reexport_src",
    "validate_endpoint_transport",
)
load("//bazel_task_graph:labels.bzl", "crate_name", "field_name", "info_label")

CfwSubscriberInfo = provider(
    doc = "A configured cfw subscriber endpoint.",
    fields = {
        "capacity": "Queue capacity.",
        "channel_name": "Framework channel name.",
        "crate_name": "Rust crate that re-exports Payload.",
        "field": "Task input field name.",
        "has_payload": "Whether the endpoint carries a payload type.",
        "keep_across_runs": "Whether the input is retained across runs.",
        "kind": "Endpoint kind.",
        "trigger": "Whether the input triggers execution.",
    },
)

def _subscriber_info_impl(ctx):
    channel = ctx.attr.channel[CfwChannelInfo]
    validate_endpoint_transport(ctx.label.name, ctx.attr.kind, channel)
    return [
        CfwSubscriberInfo(
            capacity = ctx.attr.capacity,
            channel_name = channel.channel_name,
            crate_name = ctx.attr.crate_name,
            field = ctx.attr.field,
            has_payload = ctx.attr.kind != "iox2_event",
            keep_across_runs = ctx.attr.keep_across_runs,
            kind = ctx.attr.kind,
            trigger = ctx.attr.trigger,
        ),
    ]

_subscriber_info = rule(
    implementation = _subscriber_info_impl,
    attrs = {
        "capacity": attr.int(mandatory = True),
        "channel": attr.label(
            mandatory = True,
            providers = [CfwChannelInfo],
        ),
        "crate_name": attr.string(mandatory = True),
        "field": attr.string(mandatory = True),
        "keep_across_runs": attr.bool(mandatory = True),
        "kind": attr.string(mandatory = True),
        "trigger": attr.bool(mandatory = True),
    },
)

def _subscriber_impl(
        name,
        visibility,
        channel,
        field,
        kind,
        queue_capacity,
        trigger,
        keep_across_runs,
        **kwargs):
    if kind not in [
        "required",
        "optional",
        "span",
        "iox2_input",
        "iox2_span",
        "iox2_event",
    ]:
        fail(
            "subscriber '%s': kind must be required, optional, span, iox2_input, iox2_span, or iox2_event" % name,
        )
    if kind.startswith("iox2_") and (not trigger or not keep_across_runs):
        fail(
            "subscriber '%s': trigger and keep_across_runs apply to native subscribers only" % name,
        )
    event = kind == "iox2_event"
    endpoint_reexport_src(
        name = name + "_src",
        content = "" if event else "pub type Payload = %s::Payload;" % crate_name(channel),
    )
    rust_library(
        name = name,
        srcs = [":" + name + "_src"],
        deps = [channel],
        visibility = visibility,
        **kwargs
    )
    _subscriber_info(
        name = name + "_info",
        capacity = endpoint_default_capacity(name, kind, queue_capacity),
        channel = info_label(channel),
        crate_name = field_name(name),
        field = field or field_name(name),
        keep_across_runs = keep_across_runs,
        kind = kind,
        trigger = trigger,
        visibility = visibility,
    )

subscriber = macro(
    attrs = {
        "channel": attr.label(
            mandatory = True,
            configurable = False,
            doc = "Channel target produced by channel().",
        ),
        "field": attr.string(
            default = "",
            configurable = False,
            doc = "Task input field name. Defaults to the target name.",
        ),
        "kind": attr.string(
            default = "required",
            values = [
                "required",
                "optional",
                "span",
                "iox2_input",
                "iox2_span",
                "iox2_event",
            ],
            configurable = False,
            doc = "Subscriber endpoint kind.",
        ),
        "queue_capacity": attr.int(
            default = 0,
            configurable = False,
            doc = "Queue capacity. Required for span kinds; zero uses the default for other kinds.",
        ),
        "trigger": attr.bool(
            default = True,
            configurable = False,
            doc = "Whether input arrival triggers execution. Native only.",
        ),
        "keep_across_runs": attr.bool(
            default = True,
            configurable = False,
            doc = "Whether the input is retained across runs. Native only.",
        ),
    },
    implementation = _subscriber_impl,
    inherit_attrs = "common",
)
