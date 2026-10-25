"""Build settings for cfw Bazel helpers."""

FormatGeneratedRustInfo = provider(
    doc = "Whether generated Rust sources should be formatted.",
    fields = {
        "value": "Boolean value of the build setting.",
    },
)

def _format_generated_rust_impl(ctx):
    return [FormatGeneratedRustInfo(value = ctx.build_setting_value)]

format_generated_rust = rule(
    implementation = _format_generated_rust_impl,
    build_setting = config.bool(flag = True),
)
