"""Compose cfw tasks into a scheduling pool."""

load(
    "//bazel:rust.bzl",
    "RUST_SOURCE_TOOLCHAINS",
    "render_rust_source",
    "rust_library",
    "rust_source_attrs",
    "rust_test",
)
load("//bazel_task_graph:labels.bzl", "related_label")
load("//bazel_task_graph:task.bzl", "CfwTaskInfo")

CfwPoolInfo = provider(
    doc = "A cfw scheduling pool.",
    fields = {
        "crate_name": "Rust library crate that exposes add_to and THREADS.",
        "tasks": "Tasks that will be executed in the pool.",
        "threads": "Number of executor workers for this pool.",
    },
)

def _absolute_type(rust_type):
    if rust_type.startswith("::"):
        return rust_type
    return "::" + rust_type

def _pool_src_impl(ctx):
    tasks = [task[CfwTaskInfo] for task in ctx.attr.tasks]
    names = [task.callback_name for task in tasks]
    seen = {}
    for name in names:
        if name in seen:
            fail("duplicate callback name '%s' in pool '%s'" % (name, ctx.label.name))
        seen[name] = True

    lines = [
        "pub const THREADS: usize = %d;" % ctx.attr.threads,
        "",
        "pub fn add_to(app: &mut ::task_graph::ApplicationBuilder, pool_index: usize) {",
    ]
    for task in tasks:
        lines.extend([
            "    app.add_task(",
            '        "%s",' % task.callback_name,
            "        <%s as ::core::default::Default>::default()," % _absolute_type(task.rust_type),
            "        (%s).in_pool(pool_index)," % task.schedule,
            "    )",
        ])
        overrides = []
        for field, channel in task.input_channels.items():
            overrides.append('        .input_channel("%s", "%s")' % (field, channel))
        for field, channel in task.output_channels.items():
            overrides.append('        .output_channel("%s", "%s")' % (field, channel))
        if overrides:
            lines.extend(overrides)
        lines[-1] += ";"
    lines.extend([
        "}",
        "",
        "#[cfg(test)]",
        "mod tests {",
        "    #[test]",
        "    fn wiring_builds() {",
        "        let mut app = ::task_graph::ApplicationBuilder::new();",
        "        super::add_to(&mut app, 0);",
        "        app.plan().unwrap();",
        "    }",
        "}",
        "",
    ])

    output = render_rust_source(ctx, "\n".join(lines))
    return [
        DefaultInfo(files = depset([output])),
        CfwPoolInfo(
            crate_name = ctx.attr.crate_name,
            tasks = tasks,
            threads = ctx.attr.threads,
        ),
    ]

_pool_src = rule(
    implementation = _pool_src_impl,
    attrs = rust_source_attrs({
        "crate_name": attr.string(mandatory = True),
        "tasks": attr.label_list(
            mandatory = True,
            providers = [CfwTaskInfo],
        ),
        "threads": attr.int(mandatory = True),
    }),
    toolchains = RUST_SOURCE_TOOLCHAINS,
)

def _pool_impl(name, visibility, tasks, threads, deps, **kwargs):
    if threads < 1:
        fail("pool '%s' must have at least one thread" % name)
    crate_name = name.replace("-", "_")
    pool_deps = deps + [related_label(task, "_schedule_bridge") for task in tasks] + ["//task_graph:task_graph"]
    _pool_src(
        name = name + "_info",
        crate_name = crate_name,
        tasks = tasks,
        threads = threads,
        visibility = visibility,
    )
    rust_library(
        name = name,
        srcs = [":" + name + "_info"],
        deps = pool_deps,
        visibility = visibility,
        **kwargs
    )
    rust_test(
        name = name + "_test",
        srcs = [":" + name + "_info"],
        deps = pool_deps,
        visibility = visibility,
        **kwargs
    )

cfw_pool = macro(
    attrs = {
        "tasks": attr.label_list(
            mandatory = True,
            providers = [CfwTaskInfo],
            configurable = False,
            doc = "Task targets produced by cfw_task().",
        ),
        "threads": attr.int(
            default = 1,
            configurable = False,
            doc = "Number of executor workers for this pool.",
        ),
        "deps": attr.label_list(
            default = [],
            doc = "Rust crates that define the task types.",
        ),
    },
    implementation = _pool_impl,
    inherit_attrs = "common",
)
