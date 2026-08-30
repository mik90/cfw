load("@crates//:defs.bzl", "lint_config")
load("@rules_rs//rs:rust_binary.bzl", _rust_binary = "rust_binary")
load("@rules_rs//rs:rust_library.bzl", _rust_library = "rust_library")
load("@rules_rs//rs:rust_proc_macro.bzl", _rust_proc_macro = "rust_proc_macro")
load("@rules_rs//rs:rust_test.bzl", _rust_test = "rust_test")
load("@rules_rs//rs/experimental/miri:miri_test.bzl", _miri_test = "miri_test")

def rust_binary(**kwargs):
    _rust_binary(lint_config = lint_config(), **kwargs)

def rust_library(**kwargs):
    _rust_library(lint_config = lint_config(), **kwargs)

def rust_proc_macro(**kwargs):
    _rust_proc_macro(lint_config = lint_config(), **kwargs)

def rust_test(**kwargs):
    _rust_test(lint_config = lint_config(), **kwargs)

def rust_benchmark(name, srcs, deps):
    rust_binary(
        name = name,
        srcs = srcs,
        tags = [
            "benchmark",
            "manual",
        ],
        deps = deps,
    )

def miri_test(**kwargs):
    _miri_test(
        edition = "2024",
        tags = [
            "manual",
            "miri",
        ],
        timeout = "long",
        **kwargs
    )
