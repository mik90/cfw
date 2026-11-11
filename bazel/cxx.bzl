"""Bazel helpers for cxx bridges."""

load("@bazel_skylib//rules:run_binary.bzl", "run_binary")
load("@rules_cc//cc:defs.bzl", "cc_library")

def _include_kwargs(include_prefix):
    if not include_prefix:
        return {}
    return {"include_prefix": include_prefix}

def _cxx_bridge_library(name, source, header, include_prefix, deps, **kwargs):
    cc_library(
        name = "%s/include" % name,
        hdrs = [header],
        deps = ["@cxx.rs//:core"],
        **_include_kwargs(include_prefix)
    )
    cc_library(
        name = name,
        srcs = [source],
        deps = deps + [
            ":%s/include" % name,
            "//bazel:cxx_runtime",
        ],
        **kwargs
    )

def rust_cxx_bridge(name, src, deps = [], include_prefix = "", **kwargs):
    """Generates and compiles the C++ side of a literal `#[cxx::bridge]`.

    Args:
      name: Name of the generated `cc_library`.
      src: Rust source file containing the bridge.
      deps: Additional C++ dependencies for the generated bridge.
      include_prefix: Optional crate-name prefix for the generated header.
      **kwargs: Forwarded to the generated `cc_library`.
    """
    header = src + ".h"
    source = src + ".cc"

    native.alias(
        name = "%s/header" % name,
        actual = header,
    )

    run_binary(
        name = "%s/generated" % name,
        srcs = [src],
        outs = [
            header,
            source,
        ],
        args = [
            "$(location %s)" % src,
            "-o",
            "$(location %s)" % header,
            "-o",
            "$(location %s)" % source,
        ],
        tool = "@cxx.rs//:codegen",
    )

    _cxx_bridge_library(name, source, header, include_prefix, deps, **kwargs)

def cfw_cxx_bridge(name, src, deps = [], include_prefix = "", **kwargs):
    """Generates a task bridge from `define_cxx_task!` declarations and compiles it.

    Args:
      name: Name of the generated `cc_library`.
      src: Rust source file containing only the task declarations.
      deps: Additional C++ dependencies for the generated bridge.
      include_prefix: Optional crate-name prefix for the generated header.
      **kwargs: Forwarded to the generated `cc_library`.
    """
    bridge = "gen/%s.rs" % name
    combined = "gen/%s_lib.rs" % name
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
    )
    native.alias(
        name = "%s/header" % name,
        actual = ":%s" % header,
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
        **kwargs
    )
