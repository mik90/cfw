"""Publisher endpoint targets for cfw tasks.

Each publisher creates a tiny Rust re-export crate so payload dependencies flow
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

CfwPublisherInfo = provider(
    doc = "A configured cfw publisher endpoint.",
    fields = {
        "capacity": "Queue capacity.",
        "channel_name": "Framework channel name.",
        "crate_name": "Rust crate that re-exports Payload.",
        "field": "Task output field name.",
        "has_payload": "Whether the endpoint carries a payload type.",
        "kind": "Endpoint kind.",
    },
)

def _publisher_info_impl(ctx):
    channel = ctx.attr.channel[CfwChannelInfo]
    validate_endpoint_transport(ctx.label.name, ctx.attr.kind, channel)
    return [
        CfwPublisherInfo(
            capacity = ctx.attr.capacity,
            channel_name = channel.channel_name,
            crate_name = ctx.attr.crate_name,
            field = ctx.attr.field,
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
        "kind": attr.string(mandatory = True),
    },
)

def _publisher_impl(name, visibility, channel, field, kind, queue_capacity, **kwargs):
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
    event = kind == "iox2_notifier"
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
    _publisher_info(
        name = name + "_info",
        capacity = endpoint_default_capacity(name, kind, queue_capacity),
        channel = info_label(channel),
        crate_name = field_name(name),
        field = field or field_name(name),
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
    },
    implementation = _publisher_impl,
    inherit_attrs = "common",
)
