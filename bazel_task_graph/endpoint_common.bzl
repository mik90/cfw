"""Shared helpers for cfw endpoint macros."""

load(
    "//bazel:rust.bzl",
    "RUST_SOURCE_TOOLCHAINS",
    "render_rust_source",
    "rust_source_attrs",
)

_SPAN_KINDS = ["span", "output_span", "iox2_span"]

def endpoint_default_capacity(label, kind, queue_capacity):
    """Resolve an endpoint queue capacity, requiring explicit span capacities."""
    if queue_capacity < 0:
        fail("%s: queue_capacity must be positive or zero" % label)
    if kind in _SPAN_KINDS:
        if queue_capacity == 0:
            fail(
                "%s: span endpoint kind '%s' requires an explicit queue_capacity" % (
                    label,
                    kind,
                ),
            )
        return queue_capacity
    return queue_capacity if queue_capacity > 0 else 1

def _endpoint_reexport_src_impl(ctx):
    output = render_rust_source(ctx, ctx.attr.content)
    return [DefaultInfo(files = depset([output]))]

endpoint_reexport_src = rule(
    implementation = _endpoint_reexport_src_impl,
    attrs = rust_source_attrs({
        "content": attr.string(mandatory = True),
    }),
    toolchains = RUST_SOURCE_TOOLCHAINS,
)

def validate_endpoint_transport(label, kind, channel):
    """Check that an endpoint kind matches its channel transport and payload."""
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
        if channel.kind == "iox2_event":
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
        if channel.kind != "data":
            fail("%s kind '%s' requires a data channel" % (label, kind))
        if not channel.payload_type:
            fail("%s kind '%s' requires a data channel with a payload" % (label, kind))
