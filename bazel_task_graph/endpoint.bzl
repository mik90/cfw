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
        "event": "Whether this is an IPC event channel.",
        "payload_type": "Rust payload type path. Empty for event channels.",
        "transport": "Transport name: native or iceoryx2.",
    },
)

CfwSubscriberInfo = provider(
    doc = "A configured CFW subscriber endpoint.",
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

CfwPublisherInfo = provider(
    doc = "A configured CFW publisher endpoint.",
    fields = {
        "capacity": "Queue capacity.",
        "channel_name": "Framework channel name.",
        "crate_name": "Rust crate that re-exports Payload.",
        "field": "Task output field name.",
        "has_payload": "Whether the endpoint carries a payload type.",
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
    return 4 if kind in ["span", "output_span", "iox2_span"] else 1

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
            event = ctx.attr.event,
            payload_type = ctx.attr.payload_type,
            transport = ctx.attr.transport,
        ),
    ]

_channel_info = rule(
    implementation = _channel_info_impl,
    attrs = {
        "channel_name": attr.string(mandatory = True),
        "event": attr.bool(mandatory = True),
        "payload_type": attr.string(default = ""),
        "transport": attr.string(mandatory = True),
    },
)

def _validate_transport(label, kind, channel):
    ipc = kind.startswith("iox2_")
    event_endpoint = kind in ["iox2_event", "iox2_notifier"]
    if not ipc:
        if channel.transport != "native":
            fail(
                "%s kind '%s' requires a native channel, got '%s'" % (
                    label,
                    kind,
                    channel.transport,
                ),
            )
        if channel.event:
            fail("%s kind '%s' cannot use an event channel" % (label, kind))
        if not channel.payload_type:
            fail("%s kind '%s' requires a data channel with a payload" % (label, kind))
        return

    if channel.transport != "iceoryx2":
        fail(
            "%s kind '%s' requires an iceoryx2 channel, got '%s'" % (
                label,
                kind,
                channel.transport,
            ),
        )
    if not event_endpoint:
        if channel.event:
            fail("%s kind '%s' requires a data channel" % (label, kind))
        if not channel.payload_type:
            fail("%s kind '%s' requires a data channel with a payload" % (label, kind))

def _subscriber_info_impl(ctx):
    channel = ctx.attr.channel[CfwChannelInfo]
    _validate_transport(ctx.label.name, ctx.attr.kind, channel)
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

def _publisher_info_impl(ctx):
    channel = ctx.attr.channel[CfwChannelInfo]
    _validate_transport(ctx.label.name, ctx.attr.kind, channel)
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

def _channel_impl(
        name,
        visibility,
        payload,
        deps,
        channel_name,
        kind,
        transport,
        **kwargs):
    if kind == "event":
        if transport != "iceoryx2":
            fail("channel '%s': event channels require iceoryx2 transport" % name)
        if payload:
            fail("channel '%s': event channels do not take a payload" % name)
        content = ""
        event = True
    elif kind == "data":
        if not payload:
            fail("channel '%s': data channels require a payload" % name)
        content = "pub type Payload = %s;" % payload
        event = False
    else:
        fail("channel '%s': kind must be data or event" % name)

    if transport == "iceoryx2":
        deps = deps + ["@crates//:iceoryx2"]

    _reexport_src(
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
        event = event,
        payload_type = payload,
        transport = transport,
        visibility = visibility,
    )

channel = macro(
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
            values = ["data", "event"],
            configurable = False,
            doc = "Data channel or IPC event channel.",
        ),
        "transport": attr.string(
            default = "native",
            values = ["native", "iceoryx2"],
            configurable = False,
            doc = "Channel transport.",
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
    _reexport_src(
        name = name + "_src",
        content = "" if event else "pub type Payload = %s::Payload;" % _crate_name(channel),
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
            doc = "Queue capacity. Zero uses the kind default.",
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
    _reexport_src(
        name = name + "_src",
        content = "" if event else "pub type Payload = %s::Payload;" % _crate_name(channel),
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
            doc = "Queue capacity. Zero uses the kind default.",
        ),
    },
    implementation = _publisher_impl,
    inherit_attrs = "common",
)
