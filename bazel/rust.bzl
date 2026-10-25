load("@crates//:defs.bzl", "lint_config")
load("@rules_rs//rs:rust_binary.bzl", _rust_binary = "rust_binary")
load("@rules_rs//rs:rust_library.bzl", _rust_library = "rust_library")
load("@rules_rs//rs:rust_proc_macro.bzl", _rust_proc_macro = "rust_proc_macro")
load("@rules_rs//rs:rust_test.bzl", _rust_test = "rust_test")
load("@rules_rs//rs/experimental/miri:miri_test.bzl", _miri_test = "miri_test")
load("//bazel:settings.bzl", "FormatGeneratedRustInfo")

_RUST_TOOLCHAIN = Label("@rules_rust//rust:toolchain_type")
_RUSTFMT_TOOLCHAIN = Label("@rules_rust//rust/rustfmt:toolchain_type")
_FORMAT_GENERATED_RUST = Label("//bazel:format_generated_rust")

RUST_SOURCE_TOOLCHAINS = [
    _RUST_TOOLCHAIN,
    _RUSTFMT_TOOLCHAIN,
]

def rust_source_attrs(attrs):
    """Adds the generated-source formatting setting to a rule's attrs."""
    attrs = dict(attrs)
    attrs["_format_generated_rust"] = attr.label(default = _FORMAT_GENERATED_RUST)
    return attrs

def render_rust_source(ctx, content, edition = "2024"):
    """Writes generated Rust source, formatting it with the Bazel toolchain."""
    output = ctx.actions.declare_file(ctx.label.name + ".rs")
    if not ctx.attr._format_generated_rust[FormatGeneratedRustInfo].value:
        ctx.actions.write(output, content)
        return output

    unformatted = ctx.actions.declare_file(ctx.label.name + ".unformatted.rs")
    ctx.actions.write(unformatted, content)

    rust = ctx.toolchains[_RUST_TOOLCHAIN]
    rustfmt = ctx.toolchains[_RUSTFMT_TOOLCHAIN]

    args = ctx.actions.args()
    args.add("--stdout-file", output)
    args.add("--")
    args.add(rustfmt.rustfmt)
    args.add("--quiet")
    args.add("--edition", edition)
    args.add("--emit", "stdout")
    args.add(unformatted)

    ctx.actions.run(
        executable = rust.process_wrapper,
        inputs = [unformatted],
        outputs = [output],
        tools = [rustfmt.all_files],
        arguments = [args],
        mnemonic = "RustfmtGenerated",
        progress_message = "Formatting generated Rust for %{label}",
        toolchain = _RUSTFMT_TOOLCHAIN,
    )
    return output

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
