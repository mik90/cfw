"""Publisher endpoint targets for cfw tasks.

Each publisher creates a tiny Rust re-export crate so payload dependencies flow
through the Bazel dependency graph.
"""

load("//bazel:rust.bzl", "rust_library")
load("//bazel_task_graph:channel.bzl", "CfwChannelInfo", "payload_alias_src")
load(
    "//bazel_task_graph:endpoint_common.bzl",
    "endpoint_default_capacity",
    "validate_endpoint_transport",
)
load("//bazel_task_graph:labels.bzl", "field_name", "info_label")

CfwPublisherInfo = provider(
    doc = "A configured cfw publisher endpoint.",
    fields = {
        "capacity": "Queue capacity.",
        "channel_name": "Framework channel name.",
        "crate_name": "Rust crate that re-exports Payload.",
        "field": "Task output field name.",
        "forwarded": "Whether the endpoint carries a forwarded payload.",
        "forwards": "Forwarded source field to task input field mapping.",
        "has_payload": "Whether the endpoint carries a payload type.",
        "kind": "Endpoint kind.",
    },
)

def _publisher_info_impl(ctx):
    channel = ctx.attr.channel[CfwChannelInfo]
    validate_endpoint_transport(ctx.label.name, ctx.attr.kind, channel)
    if ctx.attr.forwards and channel.kind != "forwarded":
        fail(
            "%s: forwards requires a forwarded channel" % ctx.label.name,
        )
    return [
        CfwPublisherInfo(
            capacity = ctx.attr.capacity,
            channel_name = channel.channel_name,
            crate_name = ctx.attr.crate_name,
            field = ctx.attr.field,
            forwarded = channel.kind == "forwarded",
            forwards = ctx.attr.forwards,
            has_payload = ctx.attr.kind != "iox2_notifier",
            kind = ctx.attr.kind,
        ),
    ]

_publisher_info = rule(
    implementation = _publisher_info_impl,
    attrs = {
        "capacity": attr.int(mandatory = True),
        "channel": attr.label(
            mandatory = True,
            providers = [CfwChannelInfo],
        ),
        "crate_name": attr.string(mandatory = True),
        "field": attr.string(mandatory = True),
        "forwards": attr.string_dict(),
        "kind": attr.string(mandatory = True),
    },
)

def _publisher_impl(name, visibility, channel, field, kind, queue_capacity, forwards, **kwargs):
    if kind not in [
        "output",
        "output_span",
        "uninit",
        "publisher",
        "iox2_output",
        "iox2_notifier",
    ]:
        fail(
            "publisher '%s': kind must be output, output_span, uninit, publisher, iox2_output, or iox2_notifier" % name,
        )
    if forwards and kind not in [
        "output",
        "output_span",
        "uninit",
        "publisher",
    ]:
        fail(
            "publisher '%s': forwards applies to native outputs only" % name,
        )
    payload_alias_src(
        name = name + "_src",
        channel = info_label(channel),
    )
    rust_library(
        name = name,
        srcs = [":" + name + "_src"],
        deps = [channel],
        visibility = visibility,
        **kwargs
    )
    _publisher_info(
        name = name + "_info",
        capacity = endpoint_default_capacity(name, kind, queue_capacity),
        channel = info_label(channel),
        crate_name = field_name(name),
        field = field or field_name(name),
        forwards = forwards,
        kind = kind,
        visibility = visibility,
    )

cfw_publisher = macro(
    attrs = {
        "channel": attr.label(
            mandatory = True,
            configurable = False,
            doc = "Channel target produced by cfw_channel().",
        ),
        "field": attr.string(
            default = "",
            configurable = False,
            doc = "Task output field name. Defaults to the target name.",
        ),
        "kind": attr.string(
            default = "output",
            values = [
                "output",
                "output_span",
                "uninit",
                "publisher",
                "iox2_output",
                "iox2_notifier",
            ],
            configurable = False,
            doc = "Publisher endpoint kind.",
        ),
        "queue_capacity": attr.int(
            default = 0,
            configurable = False,
            doc = "Queue capacity. Required for span kinds; zero uses the default for other kinds.",
        ),
        "forwards": attr.string_dict(
            default = {},
            configurable = False,
            doc = "Forwarded source field to task input field mapping.",
        ),
    },
    implementation = _publisher_impl,
    inherit_attrs = "common",
)
