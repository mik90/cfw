"""Generate a live-executor binary from one or more cfw pools."""

load("//bazel:rust.bzl", "rust_binary")
load("//bazel_task_graph:pools.bzl", "CfwPoolInfo")

def _info_label(label):
    label = str(label)
    package, target = label.rsplit(":", 1)
    return package + ":" + target + "_info"

def _application_src_impl(ctx):
    pools = [pool[CfwPoolInfo] for pool in ctx.attr.pools]
    if not pools:
        fail("live application '%s' requires at least one pool" % ctx.label.name)

    lines = [
        "fn main() -> ::core::result::Result<(), ::std::boxed::Box<dyn ::std::error::Error>> {",
        "    let mut app = ::task_graph::ApplicationBuilder::new();",
    ]
    for index, pool in enumerate(pools):
        lines.append("    ::%s::add_to(&mut app, %d);" % (pool.crate_name, index))
    lines.extend([
        "    let storage = app.allocate()?;",
        "    let graph = storage.build()?;",
        "    ::live_executor::LiveExecutor::new_multi_pool(",
        "        vec![%s]," % ", ".join(["::%s::THREADS" % pool.crate_name for pool in pools]),
        "        graph,",
        "    )?.run()?;",
        "    Ok(())",
        "}",
        "",
    ])

    output = ctx.actions.declare_file(ctx.label.name + ".rs")
    ctx.actions.write(output, "\n".join(lines))
    return [DefaultInfo(files = depset([output]))]

_application_src = rule(
    implementation = _application_src_impl,
    attrs = {
        "pools": attr.label_list(
            mandatory = True,
            providers = [CfwPoolInfo],
        ),
    },
)

def _live_application_impl(name, visibility, pools, **kwargs):
    if not pools:
        fail("live application '%s' requires at least one pool" % name)
    _application_src(
        name = name + "_src",
        pools = [_info_label(pool) for pool in pools],
    )
    rust_binary(
        name = name,
        srcs = [":" + name + "_src"],
        deps = pools + [
            "//live_executor:live_executor",
            "//task_graph:task_graph",
        ],
        visibility = visibility,
        **kwargs
    )

live_application = macro(
    attrs = {
        "pools": attr.label_list(
            mandatory = True,
            configurable = False,
            doc = "Pool targets produced by pool().",
        ),
    },
    implementation = _live_application_impl,
    inherit_attrs = "common",
)
