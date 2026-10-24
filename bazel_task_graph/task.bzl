"""Declare a CFW task linked to a user implementation crate.

The generated API crate contains the user-facing `Inputs`/`Outputs` types, the
`Run` trait, and an exported `impl_task!` macro. The implementation crate
implements `Run` and invokes `impl_task!` for each task type.

One Rust type may back one task definition. Reuse the same type in multiple
pools with per-instance channel overrides; use a newtype if the endpoint
structure differs.
"""

load("//bazel:rust.bzl", "rust_library")

CfwTaskInfo = provider(
    doc = "Composition metadata for a CFW task.",
    fields = {
        "callback_name": "Name used in ApplicationBuilder.",
        "implementation": "Rust crate that defines the task type.",
        "input_channels": "Endpoint-name to channel overrides for inputs.",
        "output_channels": "Endpoint-name to channel overrides for outputs.",
        "rust_type": "Fully qualified Rust type of the task.",
        "schedule": "Rust expression producing a CallbackSchedule.",
    },
)

def endpoint(rust_type, kind = None, channel = None):
    """Describe one task endpoint.

    Symbolic macro attributes cannot hold Starlark structs, so this returns an
    encoded descriptor string for task().

    Args:
        rust_type: Rust payload type, e.g. "u64" or "String".
        kind: One of required, optional, input, span, output, output_span,
            uninit, publisher. Defaults to required for inputs and output for
            outputs.
        channel: Default channel name. Defaults to the endpoint field name.

    Returns:
        An encoded endpoint descriptor.
    """
    return "%s|%s|%s" % (kind or "", rust_type, channel or "")

def _rust_ident(name):
    parts = [part for part in name.replace("-", "_").split("_") if part]
    return "".join([part[:1].upper() + part[1:] for part in parts])

def _parse_endpoint(value, field):
    parts = value.split("|")
    if len(parts) != 3 or not parts[1]:
        fail(
            "endpoint '%s' has an invalid descriptor; use endpoint()" % field,
        )
    return struct(
        channel = parts[2] or field,
        kind = parts[0] or None,
        rust_type = parts[1],
    )

def _endpoint_type(endpoint, input):
    kind = endpoint.kind
    if kind == None:
        kind = "required" if input else "output"
    if kind == "input":
        kind = "optional"

    wrappers = {
        "required": "RequiredInput",
        "optional": "Input",
        "span": "InputSpan" if input else "OutputSpan",
        "output": "Output",
        "output_span": "OutputSpan",
        "uninit": "OutputUninit",
        "publisher": "Publisher",
    }
    if kind not in wrappers:
        fail("unknown endpoint kind '%s'" % kind)
    if not input and kind in ["required", "optional", "span"]:
        fail("input endpoint kind '%s' cannot be used for an output" % kind)
    if input and kind in ["output", "output_span", "uninit", "publisher"]:
        fail("output endpoint kind '%s' cannot be used for an input" % kind)
    return "%s<%s>" % (wrappers[kind], endpoint.rust_type)

def _render_endpoints(endpoints, input):
    lines = []
    for field, descriptor in endpoints.items():
        endpoint = _parse_endpoint(descriptor, field)
        lines.append(
            '        %s: %s = "%s",' % (
                field,
                _endpoint_type(endpoint, input),
                endpoint.channel,
            ),
        )
    return "\n".join(lines)

def _task_api_src_impl(ctx):
    content = """::task_macros::define_task! {
    api: %s,
    name: %s,
    inputs: {
%s
    },
    outputs: {
%s
    },
}
""" % (
        ctx.attr.api_crate,
        ctx.attr.task_name,
        ctx.attr.inputs,
        ctx.attr.outputs,
    )
    output = ctx.actions.declare_file(ctx.label.name + ".rs")
    ctx.actions.write(output, content)
    return [DefaultInfo(files = depset([output]))]

_task_api_src = rule(
    implementation = _task_api_src_impl,
    attrs = {
        "api_crate": attr.string(mandatory = True),
        "inputs": attr.string(default = ""),
        "outputs": attr.string(default = ""),
        "task_name": attr.string(mandatory = True),
    },
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
        "schedule": attr.string(default = "::task_graph::CallbackSchedule::default()"),
    },
)

def _task_impl(
        name,
        visibility,
        implementation,
        rust_type,
        inputs,
        outputs,
        schedule,
        input_channels,
        output_channels,
        callback_name,
        **kwargs):
    api_crate = name.replace("-", "_") + "_api"
    seen_endpoints = {}
    for endpoint_name in list(inputs.keys()) + list(outputs.keys()):
        if endpoint_name in seen_endpoints:
            fail(
                "task '%s' declares endpoint '%s' more than once" % (
                    name,
                    endpoint_name,
                ),
            )
        seen_endpoints[endpoint_name] = True

    _task_api_src(
        name = name + "_api_src",
        api_crate = api_crate,
        inputs = _render_endpoints(inputs, True),
        outputs = _render_endpoints(outputs, False),
        task_name = _rust_ident(name),
    )
    rust_library(
        name = name + "_api",
        srcs = [":" + name + "_api_src"],
        deps = ["//task:task"],
        proc_macro_deps = ["//task_macros:task_macros"],
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
        schedule = schedule or "::task_graph::CallbackSchedule::default()",
        visibility = visibility,
    )

task = macro(
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
        "inputs": attr.string_dict(
            default = {},
            configurable = False,
            doc = "Dict of endpoint name to endpoint().",
        ),
        "outputs": attr.string_dict(
            default = {},
            configurable = False,
            doc = "Dict of endpoint name to endpoint().",
        ),
        "schedule": attr.string(
            default = "",
            configurable = False,
            doc = "Rust expression producing a CallbackSchedule.",
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
