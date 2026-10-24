"""Channel, subscriber, and publisher targets for CFW tasks.

Each target also creates a tiny Rust re-export crate. That lets payload crate
dependencies flow through the Bazel dependency graph instead of being repeated
on every task.
"""

load("//bazel:rust.bzl", "rust_library")

CfwChannelInfo = provider(
    doc = "A CFW channel payload type.",
    fields = {
        "channel_name": "Framework channel name.",
        "payload_type": "Rust payload type path.",
    },
)

CfwSubscriberInfo = provider(
    doc = "A configured CFW subscriber endpoint.",
    fields = {
        "capacity": "Queue capacity.",
        "channel_name": "Framework channel name.",
        "crate_name": "Rust crate that re-exports Payload.",
        "field": "Task input field name.",
        "keep_across_runs": "Whether the input is retained across runs.",
        "kind": "Endpoint kind.",
        "trigger": "Whether the input triggers execution.",
    },
)

CfwPublisherInfo = provider(
    doc = "A configured CFW publisher endpoint.",
    fields = {
        "capacity": "Queue capacity.",
        "channel_name": "Framework channel name.",
        "crate_name": "Rust crate that re-exports Payload.",
        "field": "Task output field name.",
        "kind": "Endpoint kind.",
    },
)

def _crate_name(label):
    return label.name.replace("-", "_")

def _field_name(name):
    return name.replace("-", "_")

def _info_label(label):
    label = str(label)
    package, target = label.rsplit(":", 1)
    return package + ":" + target + "_info"

def _default_capacity(kind, queue_capacity):
    if queue_capacity < 0:
        fail("queue_capacity must be positive or zero")
    if queue_capacity > 0:
        return queue_capacity
    return 4 if kind in ["span", "output_span"] else 1

def _reexport_src_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".rs")
    ctx.actions.write(output, ctx.attr.content)
    return [DefaultInfo(files = depset([output]))]

_reexport_src = rule(
    implementation = _reexport_src_impl,
    attrs = {
        "content": attr.string(mandatory = True),
    },
)

def _channel_info_impl(ctx):
    return [
        CfwChannelInfo(
            channel_name = ctx.attr.channel_name,
            payload_type = ctx.attr.payload_type,
        ),
    ]

_channel_info = rule(
    implementation = _channel_info_impl,
    attrs = {
        "channel_name": attr.string(mandatory = True),
        "payload_type": attr.string(mandatory = True),
    },
)

def _subscriber_info_impl(ctx):
    channel = ctx.attr.channel[CfwChannelInfo]
    return [
        CfwSubscriberInfo(
            capacity = ctx.attr.capacity,
            channel_name = channel.channel_name,
            crate_name = ctx.attr.crate_name,
            field = ctx.attr.field,
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

def _publisher_info_impl(ctx):
    channel = ctx.attr.channel[CfwChannelInfo]
    return [
        CfwPublisherInfo(
            capacity = ctx.attr.capacity,
            channel_name = channel.channel_name,
            crate_name = ctx.attr.crate_name,
            field = ctx.attr.field,
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

def _channel_impl(name, visibility, payload, deps, channel_name, **kwargs):
    _reexport_src(
        name = name + "_src",
        content = "pub type Payload = %s;" % payload,
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
        payload_type = payload,
        visibility = visibility,
    )

channel = macro(
    attrs = {
        "payload": attr.string(
            mandatory = True,
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
    },
    implementation = _channel_impl,
    inherit_attrs = "common",
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
    if kind not in ["required", "optional", "span"]:
        fail("subscriber kind must be required, optional, or span")
    _reexport_src(
        name = name + "_src",
        content = "pub type Payload = %s::Payload;" % _crate_name(channel),
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
        capacity = _default_capacity(kind, queue_capacity),
        channel = _info_label(channel),
        crate_name = _field_name(name),
        field = field or _field_name(name),
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
            values = ["required", "optional", "span"],
            configurable = False,
            doc = "Subscriber endpoint kind.",
        ),
        "queue_capacity": attr.int(
            default = 0,
            configurable = False,
            doc = "Queue capacity. Zero uses the kind default.",
        ),
        "trigger": attr.bool(
            default = True,
            configurable = False,
            doc = "Whether input arrival triggers execution.",
        ),
        "keep_across_runs": attr.bool(
            default = True,
            configurable = False,
            doc = "Whether the input is retained across runs.",
        ),
    },
    implementation = _subscriber_impl,
    inherit_attrs = "common",
)

def _publisher_impl(name, visibility, channel, field, kind, queue_capacity, **kwargs):
    if kind not in ["output", "output_span", "uninit", "publisher"]:
        fail("publisher kind must be output, output_span, uninit, or publisher")
    _reexport_src(
        name = name + "_src",
        content = "pub type Payload = %s::Payload;" % _crate_name(channel),
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
        capacity = _default_capacity(kind, queue_capacity),
        channel = _info_label(channel),
        crate_name = _field_name(name),
        field = field or _field_name(name),
        kind = kind,
        visibility = visibility,
    )

publisher = macro(
    attrs = {
        "channel": attr.label(
            mandatory = True,
            configurable = False,
            doc = "Channel target produced by channel().",
        ),
        "field": attr.string(
            default = "",
            configurable = False,
            doc = "Task output field name. Defaults to the target name.",
        ),
        "kind": attr.string(
            default = "output",
            values = ["output", "output_span", "uninit", "publisher"],
            configurable = False,
            doc = "Publisher endpoint kind.",
        ),
        "queue_capacity": attr.int(
            default = 0,
            configurable = False,
            doc = "Queue capacity. Zero uses the kind default.",
        ),
    },
    implementation = _publisher_impl,
    inherit_attrs = "common",
)
