"""Channel targets for cfw task payloads.

Each channel creates a tiny Rust re-export crate. That lets payload crate
dependencies flow through the Bazel dependency graph instead of being repeated
on every task.
"""

load("//bazel:rust.bzl", "rust_library")
load("//bazel_task_graph:endpoint_common.bzl", "endpoint_reexport_src")

_CHANNEL_KINDS = ["data", "iox2_event"]
_TRANSPORT_KINDS = ["native", "iceoryx2"]

CfwChannelInfo = provider(
    doc = "A cfw channel payload type.",
    fields = {
        "channel_name": "Framework channel name.",
        "kind": "Type of this channel. Must be one of: %s" % _CHANNEL_KINDS,
        "payload_type": "Rust payload type path. Empty for iox2 event channels.",
        "transport": "Transport name. Must be one of: %s" % _TRANSPORT_KINDS,
    },
)

def _channel_info_impl(ctx):
    return [
        CfwChannelInfo(
            channel_name = ctx.attr.channel_name,
            kind = ctx.attr.kind,
            payload_type = ctx.attr.payload_type,
            transport = ctx.attr.transport,
        ),
    ]

_channel_info = rule(
    implementation = _channel_info_impl,
    attrs = {
        "channel_name": attr.string(mandatory = True),
        "kind": attr.string(mandatory = True, values = _CHANNEL_KINDS),
        "payload_type": attr.string(),
        "transport": attr.string(mandatory = True),
    },
)

def _channel_impl(
        name,
        visibility,
        payload,
        deps,
        channel_name,
        kind,
        transport,
        **kwargs):
    if kind == "iox2_event":
        if transport != "iceoryx2":
            fail("channel '%s': event channels require iceoryx2 transport" % name)
        if payload:
            fail("channel '%s': event channels do not take a payload" % name)
        content = ""
    elif kind == "data":
        if not payload:
            fail("channel '%s': data channels require a payload" % name)
        content = "pub type Payload = %s;" % payload
    else:
        fail("channel '%s': kind must be data or event" % name)

    if transport == "iceoryx2":
        deps = deps + ["@crates//:iceoryx2"]

    endpoint_reexport_src(
        name = name + "_src",
        content = content,
    )
    rust_library(
        name = name,
        srcs = [":" + name + "_src"],
        deps = deps,
        visibility = visibility,
        **kwargs
    )
    _channel_info(
        name = name + "_info",
        channel_name = channel_name or name,
        kind = kind,
        payload_type = payload,
        transport = transport,
        visibility = visibility,
    )

cfw_channel = macro(
    attrs = {
        "payload": attr.string(
            default = "",
            configurable = False,
            doc = "Rust payload type path, e.g. 'my_messages::Value'.",
        ),
        "deps": attr.label_list(
            default = [],
            configurable = False,
            doc = "Rust crates needed to name the payload type.",
        ),
        "channel_name": attr.string(
            default = "",
            configurable = False,
            doc = "Framework channel name. Defaults to the target name.",
        ),
        "kind": attr.string(
            default = "data",
            values = _CHANNEL_KINDS,
            configurable = False,
            doc = "Data channel or iox2 event channel.",
        ),
        "transport": attr.string(
            default = "native",
            values = _TRANSPORT_KINDS,
            configurable = False,
            doc = "Channel transport.",
        ),
    },
    implementation = _channel_impl,
    inherit_attrs = "common",
)
