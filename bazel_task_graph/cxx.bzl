"""Bazel helpers for cfw C++ task bridges."""

load("@bazel_skylib//rules:run_binary.bzl", "run_binary")
load("@rules_cc//cc:defs.bzl", "cc_library")
load(
    "//bazel:rust.bzl",
    "RUST_SOURCE_TOOLCHAINS",
    "render_rust_source",
    "rust_library",
    "rust_source_attrs",
)
load("//bazel_task_graph:labels.bzl", "field_name", "info_label")
load("//bazel_task_graph:publisher.bzl", "CfwPublisherInfo")
load("//bazel_task_graph:schedule.bzl", "CfwCallbackScheduleInfo")
load("//bazel_task_graph:subscriber.bzl", "CfwSubscriberInfo")
load("//bazel_task_graph:task.bzl", "cfw_schedule_bridge", "cfw_task_meta")

_CXX_SUBSCRIBER_KINDS = {
    "required": "RequiredInput",
    "optional": "Input",
    "span": "InputSpan",
}

_CXX_PUBLISHER_KINDS = {
    "output": "Output",
    "output_span": "OutputSpan",
    "uninit": "OutputUninit",
}

def _include_kwargs(include_prefix):
    if not include_prefix:
        return {}
    return {"include_prefix": include_prefix}

def _cxx_bridge_library(name, source, header, include_prefix, deps, visibility = None, **kwargs):
    cc_library(
        name = "%s/include" % name,
        hdrs = [header],
        deps = ["@cxx.rs//:core"],
        visibility = visibility,
        **_include_kwargs(include_prefix)
    )
    cc_library(
        name = name,
        srcs = [source],
        deps = deps + [
            ":%s/include" % name,
            "//bazel:cxx_runtime",
        ],
        visibility = visibility,
        **kwargs
    )

def _cfw_cxx_bridge(name, src, deps = [], include_prefix = "", output_prefix = "gen/", visibility = None, **kwargs):
    """Generates a task bridge from `define_cxx_task!` declarations and compiles it.

    Internal helper for `cfw_cxx_task`; not part of the public task-graph API.

    Args:
      name: Name of the generated `cc_library`.
      src: Rust source file containing only the task declarations.
      deps: Additional C++ dependencies for the generated bridge.
      include_prefix: Optional crate-name prefix for the generated header.
      output_prefix: Directory prefix for the generated Rust and C++ files.
      visibility: Visibility of the generated targets.
      **kwargs: Forwarded to the generated `cc_library`.
    """
    bridge = output_prefix + "%s.rs" % name
    combined = output_prefix + "%s_lib.rs" % name
    header = bridge + ".h"
    source = bridge + ".cc"

    run_binary(
        name = "%s/generate" % name,
        srcs = [src],
        outs = [
            bridge,
            combined,
        ],
        args = [
            "--tasks",
            "$(location %s)" % src,
            "--bridge-out",
            "$(location %s)" % bridge,
            "--combined-out",
            "$(location %s)" % combined,
        ],
        tool = "//task_cxx_codegen:task_cxx_bridge_gen",
    )

    run_binary(
        name = "%s/codegen" % name,
        srcs = [":%s" % bridge],
        outs = [
            header,
            source,
        ],
        args = [
            "$(location :%s)" % bridge,
            "-o",
            "$(location %s)" % header,
            "-o",
            "$(location %s)" % source,
        ],
        tool = "@cxx.rs//:codegen",
    )

    native.alias(
        name = "%s/rust" % name,
        actual = ":%s" % combined,
        visibility = visibility,
    )
    native.alias(
        name = "%s/header" % name,
        actual = ":%s" % header,
        visibility = visibility,
    )

    _cxx_bridge_library(
        name,
        source,
        header,
        include_prefix,
        deps + [
            "//task_cxx:bridge/include",
            "//task_cxx:output_headers",
        ],
        visibility = visibility,
        **kwargs
    )

def _rust_ident(name):
    parts = [part for part in name.replace("-", "_").split("_") if part]
    return "".join([part[:1].upper() + part[1:] for part in parts])

def _bool_literal(value):
    return "true" if value else "false"

def _string_literal(value):
    return '"%s"' % value.replace("\\", "\\\\").replace('"', '\\"')

def _string_list_literal(values):
    return ", ".join([_string_literal(value) for value in values])

def _normalize_path(path):
    path = path.replace(" ", "")
    if path.startswith("::"):
        return path[2:]
    return path

CfwCxxPayloadInfo = provider(
    doc = "A Rust payload type exposed through a C++ task bridge.",
    fields = {
        "cxx_type": "Alias name used in the cxx bridge.",
        "header": "C++ header that declares the payload type.",
        "namespace": "C++ namespace of the payload type.",
        "rust_path": "Fully qualified Rust path of the payload type.",
    },
)

def _cxx_payload_impl(ctx):
    return [
        CfwCxxPayloadInfo(
            cxx_type = ctx.attr.cxx_type,
            header = ctx.attr.header,
            namespace = ctx.attr.namespace,
            rust_path = ctx.attr.rust_path,
        ),
    ]

cfw_cxx_payload = rule(
    implementation = _cxx_payload_impl,
    attrs = {
        "cxx_type": attr.string(mandatory = True),
        "header": attr.string(mandatory = True),
        "namespace": attr.string(mandatory = True),
        "rust_path": attr.string(mandatory = True),
    },
)

def _endpoint_lines(endpoint, wrapper, type_name, input):
    lines = [
        "        %s: %s<%s> {" % (
            endpoint.field,
            wrapper,
            type_name,
        ),
        "            channel: %s," % _string_literal(endpoint.channel_name),
        "            capacity: %d," % endpoint.capacity,
    ]
    if input:
        lines.extend([
            "            trigger: %s," % _bool_literal(endpoint.trigger),
            "            keep_across_runs: %s," % _bool_literal(endpoint.keep_across_runs),
        ])
    lines.append("        },")
    return lines

def _cxx_task_src_impl(ctx):
    subscribers = [subscriber[CfwSubscriberInfo] for subscriber in ctx.attr.subscribers]
    publishers = [publisher[CfwPublisherInfo] for publisher in ctx.attr.publishers]

    payloads_by_path = {}
    for payload in [payload[CfwCxxPayloadInfo] for payload in ctx.attr.payloads]:
        path = _normalize_path(payload.rust_path)
        if path in payloads_by_path:
            fail(
                "C++ task '%s': payload '%s' is declared more than once" % (
                    ctx.attr.task_name,
                    payload.rust_path,
                ),
            )
        payloads_by_path[path] = payload

    used_payloads = {}

    def endpoint_type(endpoint):
        payload = payloads_by_path.get(_normalize_path(endpoint.payload_type))
        if payload == None:
            return endpoint.payload_type
        existing = used_payloads.get(payload.cxx_type)
        if existing != None and (
            existing.namespace != payload.namespace or
            existing.header != payload.header or
            existing.rust_path != payload.rust_path
        ):
            fail(
                "C++ task '%s': payload alias '%s' has conflicting declarations" % (
                    ctx.attr.task_name,
                    payload.cxx_type,
                ),
            )
        used_payloads[payload.cxx_type] = payload
        return payload.cxx_type

    subscriber_lines = []
    for endpoint in subscribers:
        wrapper = _CXX_SUBSCRIBER_KINDS.get(endpoint.kind)
        if wrapper == None:
            fail(
                "C++ task '%s': subscriber kind '%s' is not supported; use required, optional, or span" % (
                    ctx.attr.task_name,
                    endpoint.kind,
                ),
            )
        if endpoint.forwarded or endpoint.forwardable:
            fail(
                "C++ task '%s': forwarding is not supported on subscribers" % ctx.attr.task_name,
            )
        if not endpoint.has_payload or not endpoint.payload_type:
            fail(
                "C++ task '%s': subscriber '%s' requires a data payload" % (
                    ctx.attr.task_name,
                    endpoint.field,
                ),
            )
        subscriber_lines.extend(_endpoint_lines(endpoint, wrapper, endpoint_type(endpoint), True))

    publisher_lines = []
    for endpoint in publishers:
        wrapper = _CXX_PUBLISHER_KINDS.get(endpoint.kind)
        if wrapper == None:
            fail(
                "C++ task '%s': publisher kind '%s' is not supported; use output, output_span, or uninit" % (
                    ctx.attr.task_name,
                    endpoint.kind,
                ),
            )
        if endpoint.forwarded or endpoint.forwards:
            fail(
                "C++ task '%s': forwarding is not supported on publishers" % ctx.attr.task_name,
            )
        if not endpoint.has_payload or not endpoint.payload_type:
            fail(
                "C++ task '%s': publisher '%s' requires a data payload" % (
                    ctx.attr.task_name,
                    endpoint.field,
                ),
            )
        publisher_lines.extend(_endpoint_lines(endpoint, wrapper, endpoint_type(endpoint), False))

    use_lines = []
    payload_lines = []
    for cxx_type in sorted(used_payloads.keys()):
        payload = used_payloads[cxx_type]
        rust_path = _normalize_path(payload.rust_path)
        if rust_path.split("::")[-1] == cxx_type:
            use_lines.append("use %s;" % payload.rust_path)
        else:
            use_lines.append("use %s as %s;" % (payload.rust_path, payload.cxx_type))
        payload_lines.extend([
            "        %s: {" % payload.cxx_type,
            "            namespace: %s," % _string_literal(payload.namespace),
            "            header: %s," % _string_literal(payload.header),
            "            rust_path: %s," % payload.rust_path,
            "        },",
        ])
    payload_block = "{\n%s\n    }" % "\n".join(payload_lines) if payload_lines else "{}"

    fallible = "\n    fallible: true," if ctx.attr.fallible else ""
    content = """%stask_cxx_macros::define_cxx_bridge! {
    namespace: %s,
    headers: [%s],
    payloads: %s,
}

task_cxx_macros::define_cxx_task! {
    bridge: ffi,
    name: %s,
    cxx_type: %s,
    factory: %s,%s
    subscribers: {
%s
    },
    publishers: {
%s
    },
}
""" % (
        "\n".join(use_lines) + "\n" if use_lines else "",
        _string_literal(ctx.attr.namespace),
        _string_list_literal(ctx.attr.headers),
        payload_block,
        ctx.attr.task_name,
        ctx.attr.cxx_type,
        ctx.attr.factory,
        fallible,
        "\n".join(subscriber_lines),
        "\n".join(publisher_lines),
    )
    output = render_rust_source(ctx, content)
    return [DefaultInfo(files = depset([output]))]

_cxx_task_src = rule(
    implementation = _cxx_task_src_impl,
    attrs = rust_source_attrs({
        "cxx_type": attr.string(mandatory = True),
        "factory": attr.string(mandatory = True),
        "fallible": attr.bool(),
        "headers": attr.string_list(),
        "namespace": attr.string(mandatory = True),
        "payloads": attr.label_list(
            default = [],
            providers = [CfwCxxPayloadInfo],
        ),
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

def _cxx_task_impl(
        name,
        visibility,
        implementation,
        cxx_type,
        factory,
        namespace,
        headers,
        fallible,
        payloads,
        subscribers,
        publishers,
        schedule,
        callback_name,
        include_prefix,
        deps,
        rust_deps,
        input_channels,
        output_channels,
        **kwargs):
    if not headers:
        fail("C++ task '%s' requires at least one C++ header" % name)
    if not cxx_type:
        fail("C++ task '%s' requires cxx_type" % name)
    if not factory:
        fail("C++ task '%s' requires factory" % name)
    if not namespace:
        fail("C++ task '%s' requires a C++ namespace" % name)

    bridge = name + "_cxx_bridge"
    rust_crate = name + "_rust"
    task_name = _rust_ident(name)
    crate_name = field_name(name)

    _cxx_task_src(
        name = name + "_cxx_decl",
        cxx_type = cxx_type,
        factory = factory,
        fallible = fallible,
        headers = headers,
        namespace = namespace,
        payloads = payloads,
        publishers = [info_label(publisher) for publisher in publishers],
        subscribers = [info_label(subscriber) for subscriber in subscribers],
        task_name = task_name,
        visibility = visibility,
    )
    _cfw_cxx_bridge(
        name = bridge,
        src = ":" + name + "_cxx_decl",
        deps = [implementation] + deps,
        include_prefix = include_prefix or native.package_name(),
        output_prefix = name + "_gen/",
        visibility = visibility,
    )
    native.alias(
        name = name + "_include",
        actual = ":" + bridge + "/include",
        visibility = visibility,
    )
    rust_library(
        name = rust_crate,
        srcs = [":" + bridge + "/rust"],
        crate_root = ":" + bridge + "/rust",
        crate_name = crate_name,
        crate_features = ["cxx"],
        include_default_features = False,
        deps = subscribers + publishers + [
            ":" + bridge,
            implementation,
            "//task:task",
            "//task_graph:task_graph",
            "//task_cxx:task_cxx",
            "@crates//:cxx-1.0.205",
        ] + rust_deps,
        proc_macro_deps = ["//task_cxx_macros:task_cxx_macros"],
        visibility = visibility,
        **kwargs
    )
    cfw_schedule_bridge(
        name = name,
        schedule = schedule,
        visibility = visibility,
        **kwargs
    )
    cfw_task_meta(
        name = name,
        callback_name = callback_name or name,
        implementation = ":" + rust_crate,
        input_channels = input_channels,
        output_channels = output_channels,
        rust_type = "::%s::%s" % (crate_name, task_name),
        schedule = "::%s::__cfw_schedule()" % field_name(name + "_schedule_bridge"),
        visibility = visibility,
    )

cfw_cxx_task = macro(
    attrs = {
        "implementation": attr.label(
            mandatory = True,
            configurable = False,
            doc = "C++ cc_library that defines the task class and factory.",
        ),
        "cxx_type": attr.string(
            mandatory = True,
            configurable = False,
            doc = "C++ task class name, e.g. 'FizzBuzzTask'.",
        ),
        "factory": attr.string(
            mandatory = True,
            configurable = False,
            doc = "C++ factory function name, e.g. 'make_fizz_buzz_task'.",
        ),
        "namespace": attr.string(
            mandatory = True,
            configurable = False,
            doc = "C++ namespace of the task class and factory.",
        ),
        "headers": attr.string_list(
            mandatory = True,
            configurable = False,
            doc = "C++ headers that declare the task class and factory.",
        ),
        "payloads": attr.label_list(
            default = [],
            configurable = False,
            providers = [CfwCxxPayloadInfo],
            doc = "Payload aliases produced by cfw_cxx_payload().",
        ),
        "fallible": attr.bool(
            default = False,
            configurable = False,
            doc = "Declare the C++ run method as returning Result<()>.",
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
        "callback_name": attr.string(
            default = "",
            configurable = False,
            doc = "Override the callback name used by ApplicationBuilder.",
        ),
        "include_prefix": attr.string(
            default = "",
            configurable = False,
            doc = "Include prefix for the generated bridge header. Defaults to the package path.",
        ),
        "deps": attr.label_list(
            default = [],
            configurable = False,
            doc = "Additional C++ dependencies for the generated bridge.",
        ),
        "rust_deps": attr.label_list(
            default = [],
            configurable = False,
            doc = "Additional Rust dependencies for the generated task crate.",
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
    },
    implementation = _cxx_task_impl,
    inherit_attrs = "common",
)
