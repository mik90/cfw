load("@crates//:defs.bzl", "lint_config")
load("@rules_rs//rs:rust_binary.bzl", _rust_binary = "rust_binary")
load("@rules_rs//rs:rust_library.bzl", _rust_library = "rust_library")
load("@rules_rs//rs:rust_proc_macro.bzl", _rust_proc_macro = "rust_proc_macro")
load("@rules_rs//rs:rust_test.bzl", _rust_test = "rust_test")
load("@rules_rs//rs/experimental/miri:miri_test.bzl", _miri_test = "miri_test")

# Bazel does not inherit Cargo's default features. Match the Cargo defaults for
# first-party targets so IPC-capable crates do not silently lose iceoryx2 code.
# Pass include_default_features = False to opt out for a native-only target.
DEFAULT_CRATE_FEATURES = ["iceoryx2"]

def _crate_features(kwargs):
    include_defaults = kwargs.pop("include_default_features", True)
    features = list(kwargs.get("crate_features", []))
    if include_defaults:
        for feature in DEFAULT_CRATE_FEATURES:
            if feature not in features:
                features.append(feature)
    return features

def rust_binary(**kwargs):
    kwargs["crate_features"] = _crate_features(kwargs)
    _rust_binary(lint_config = lint_config(), **kwargs)

def rust_library(**kwargs):
    kwargs["crate_features"] = _crate_features(kwargs)
    _rust_library(lint_config = lint_config(), **kwargs)

def rust_proc_macro(**kwargs):
    _rust_proc_macro(lint_config = lint_config(), **kwargs)

def rust_test(**kwargs):
    kwargs["crate_features"] = _crate_features(kwargs)
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
