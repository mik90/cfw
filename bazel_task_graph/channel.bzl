"""Channel targets for cfw task payloads.

Each channel creates a tiny Rust re-export crate. That lets payload crate
dependencies flow through the Bazel dependency graph instead of being repeated
on every task.
"""

load(
    "//bazel:rust.bzl",
    "RUST_SOURCE_TOOLCHAINS",
    "render_rust_source",
    "rust_library",
    "rust_source_attrs",
)
load("//bazel_task_graph:endpoint_common.bzl", "endpoint_reexport_src")
load("//bazel_task_graph:labels.bzl", "field_name")

_CHANNEL_KINDS = ["data", "forwarded", "iox2_event"]
_TRANSPORT_KINDS = ["native", "iceoryx2"]

CfwChannelInfo = provider(
    doc = "A cfw channel payload type.",
    fields = {
        "channel_name": "Framework channel name.",
        "crate_name": "Rust crate that re-exports Payload.",
        "kind": "Type of this channel. Must be one of: %s" % _CHANNEL_KINDS,
        "payload_type": "Rust payload type path. Empty for iox2 event channels.",
        "transport": "Transport name. Must be one of: %s" % _TRANSPORT_KINDS,
    },
)

def _channel_info_impl(ctx):
    return [
        CfwChannelInfo(
            channel_name = ctx.attr.channel_name,
            crate_name = ctx.attr.crate_name,
            kind = ctx.attr.kind,
            payload_type = ctx.attr.payload_type,
            transport = ctx.attr.transport,
        ),
    ]

_channel_info = rule(
    implementation = _channel_info_impl,
    attrs = {
        "channel_name": attr.string(mandatory = True),
        "crate_name": attr.string(mandatory = True),
        "kind": attr.string(mandatory = True, values = _CHANNEL_KINDS),
        "payload_type": attr.string(),
        "transport": attr.string(mandatory = True),
    },
)

def _channel_impl(
        name,
        visibility,
        payload,
        sources,
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
        if sources:
            fail("channel '%s': event channels do not take sources" % name)
        content = ""
    elif kind == "forwarded":
        if not payload:
            fail("channel '%s': forwarded channels require a payload" % name)
        if not sources:
            fail("channel '%s': forwarded channels require sources" % name)
        if transport != "native":
            fail("channel '%s': forwarded channels require native transport" % name)
        deps = deps + ["//task:task"]
        content = """pub type Message = %s;
pub type Sources = %s;
pub type Payload<'a> =
    ::task::Forwarded<Message, <Sources as ::task::ForwardedSources>::Bound<'a>>;
""" % (payload, sources)
    elif kind == "data":
        if not payload:
            fail("channel '%s': data channels require a payload" % name)
        if sources:
            fail("channel '%s': sources applies to forwarded channels only" % name)
        content = "pub type Payload = %s;" % payload
    else:
        fail("channel '%s': kind must be data, forwarded, or iox2_event" % name)

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
        crate_name = field_name(name),
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
        "sources": attr.string(
            default = "",
            configurable = False,
            doc = "ForwardedSources type for forwarded channels, e.g. 'my_types::Sources<'static>'.",
        ),
        "deps": attr.label_list(
            default = [],
            configurable = False,
            doc = "Rust crates needed to name the payload and sources types.",
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
            doc = "Data channel, forwarded channel, or iox2 event channel.",
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

def _payload_alias_src_impl(ctx):
    channel = ctx.attr.channel[CfwChannelInfo]
    crate = channel.crate_name
    if channel.kind == "iox2_event":
        content = ""
    elif channel.kind == "forwarded":
        content = """pub use %s::{Message, Sources};
pub type Payload<'a> = %s::Payload<'a>;
""" % (crate, crate)
    else:
        content = "pub type Payload = %s::Payload;" % crate
    output = render_rust_source(ctx, content)
    return [DefaultInfo(files = depset([output]))]

payload_alias_src = rule(
    implementation = _payload_alias_src_impl,
    attrs = rust_source_attrs({
        "channel": attr.label(
            mandatory = True,
            providers = [CfwChannelInfo],
        ),
    }),
    toolchains = RUST_SOURCE_TOOLCHAINS,
)
