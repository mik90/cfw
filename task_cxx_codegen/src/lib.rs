//! Parsing and bridge generation for `define_cxx_task!`.
//!
//! The proc macro and `build.rs` share one declarative spec so the literal
//! `#[cxx::bridge]` module cannot drift from the generated views:
//!
//! - [`CxxTaskSpec`] parses a single task declaration.
//! - [`task_endpoints`] applies the same endpoint validation to both users.
//! - [`generate_bridge_file`] finds every `define_cxx_task!` in a source file
//!   and writes the matching `#[cxx::bridge]` module.
//!
//! cxx generates C++ from literal source files, so the bridge has to be
//! written to disk before `cxx_build::bridge` runs. That is why this lives in
//! a normal library rather than in the proc-macro crate.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use proc_macro2::Span;
use quote::ToTokens;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, Result, Token};

use task_macro_common::{
    Endpoint, EndpointKind, EndpointSpec, declared_endpoint, validate_endpoints,
};

/// A parsed `define_cxx_task!` invocation.
pub struct CxxTaskSpec {
    pub bridge: syn::Path,
    pub name: Ident,
    pub cxx_type: Ident,
    pub factory: Ident,
    pub fallible: bool,
    pub subscribers: Vec<EndpointSpec>,
    pub publishers: Vec<EndpointSpec>,
}

impl Parse for CxxTaskSpec {
    fn parse(input: ParseStream) -> Result<Self> {
        let bridge = parse_key::<syn::Path>(input, "bridge")?;
        input.parse::<Token![,]>()?;
        let name = parse_key::<Ident>(input, "name")?;
        input.parse::<Token![,]>()?;
        let cxx_type = parse_key::<Ident>(input, "cxx_type")?;
        input.parse::<Token![,]>()?;
        let factory = parse_key::<Ident>(input, "factory")?;
        input.parse::<Token![,]>()?;
        let fallible = if input
            .fork()
            .parse::<Ident>()
            .is_ok_and(|ident| ident == "fallible")
        {
            let _: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            let value: syn::LitBool = input.parse()?;
            input.parse::<Token![,]>()?;
            value.value
        } else {
            false
        };
        let subscribers = task_macro_common::parse_endpoint_block(input, "subscribers")?;
        input.parse::<Token![,]>()?;
        let publishers = task_macro_common::parse_endpoint_block(input, "publishers")?;
        let _ = input.parse::<Token![,]>();
        if !input.is_empty() {
            return Err(input.error("unexpected tokens after task definition"));
        }
        Ok(Self {
            bridge,
            name,
            cxx_type,
            factory,
            fallible,
            subscribers,
            publishers,
        })
    }
}

fn parse_key<T: Parse>(input: ParseStream, key: &str) -> Result<T> {
    let ident: Ident = input.parse()?;
    if ident != key {
        return Err(syn::Error::new_spanned(ident, format!("expected '{key}'")));
    }
    input.parse::<Token![:]>()?;
    input.parse()
}

/// Resolves and validates the endpoints of a C++ task.
///
/// Shared by the proc macro and the bridge generator so both reject the same
/// endpoint kinds and forwarding configurations.
pub fn task_endpoints(
    subscribers: &[EndpointSpec],
    publishers: &[EndpointSpec],
) -> Result<Vec<Endpoint>> {
    let mut endpoints = Vec::new();
    for subscriber in subscribers {
        endpoints.push(declared_endpoint(subscriber.clone(), true)?);
    }
    for publisher in publishers {
        endpoints.push(declared_endpoint(publisher.clone(), false)?);
    }
    validate_endpoints(&endpoints)?;
    for endpoint in &endpoints {
        match endpoint.kind {
            EndpointKind::Required | EndpointKind::Input => {}
            EndpointKind::Output
            | EndpointKind::OutputDefault
            | EndpointKind::Uninit
            | EndpointKind::OutputSpan => {}
            _ => {
                return Err(syn::Error::new_spanned(
                    &endpoint.name,
                    "C++ tasks support RequiredInput, OptionalInput, InputSpan, Output, \
                     OutputUninit, and OutputSpan endpoints only",
                ));
            }
        }
        if endpoint.forwardable || !endpoint.forwards.is_empty() || endpoint.forwarded.is_some() {
            return Err(syn::Error::new_spanned(
                &endpoint.name,
                "forwarding is not supported on C++ tasks yet",
            ));
        }
    }
    Ok(endpoints)
}

/// Parses every top-level `define_cxx_task!` invocation in `source`.
pub fn parse_tasks(source: &str) -> Result<Vec<CxxTaskSpec>> {
    let file = syn::parse_file(source)?;
    let mut tasks = Vec::new();
    for item in file.items {
        let syn::Item::Macro(item_macro) = item else {
            continue;
        };
        let last = item_macro.mac.path.segments.last();
        if last.is_some_and(|segment| segment.ident == "define_cxx_task") {
            tasks.push(syn::parse2(item_macro.mac.tokens)?);
        }
    }
    if tasks.is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "no define_cxx_task! invocations found",
        ));
    }
    Ok(tasks)
}

/// A payload type that must be aliased into the generated bridge.
pub struct PayloadAlias<'a> {
    pub namespace: &'a str,
    pub header: &'a str,
    pub rust_type: &'a str,
    pub rust_path: &'a str,
}

/// Per-crate bridge configuration that is not part of a task declaration.
pub struct BridgeConfig<'a> {
    /// C++ namespace of the user's task classes.
    pub namespace: &'a str,
    /// C++ headers that declare the task classes and factories.
    pub headers: &'a [&'a str],
    /// Payload types declared in other bridges.
    pub payloads: &'a [PayloadAlias<'a>],
}

/// Generates the literal `#[cxx::bridge]` module for `tasks`.
pub fn generate_bridge(tasks: &[CxxTaskSpec], config: &BridgeConfig) -> Result<String> {
    let resolved: Vec<(&CxxTaskSpec, Vec<Endpoint>)> = tasks
        .iter()
        .map(|task| {
            let endpoints = task_endpoints(&task.subscribers, &task.publishers)?;
            Ok((task, endpoints))
        })
        .collect::<Result<_>>()?;

    let mut out = String::new();
    writeln!(out, "// @generated by task_cxx_codegen. Do not edit.").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "#[cxx::bridge(namespace = {:?})]", config.namespace).unwrap();
    writeln!(out, "mod ffi {{").unwrap();

    writeln!(out, "    #[namespace = \"cfw\"]").unwrap();
    writeln!(out, "    unsafe extern \"C++\" {{").unwrap();
    writeln!(out, "        include!(\"task_cxx/src/lib.rs.h\");").unwrap();
    writeln!(out, "        include!(\"task_cxx/src/output.hpp\");").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "        type Context = ::task_cxx::ffi::Context;").unwrap();
    writeln!(
        out,
        "        type MessageHeader = ::task_cxx::ffi::MessageHeader;"
    )
    .unwrap();
    writeln!(out, "        type OutputUninit = ::task_cxx::OutputUninit;").unwrap();
    writeln!(out, "        type SpanHandle = ::task_cxx::SpanHandle;").unwrap();
    writeln!(out, "    }}").unwrap();

    for payload in config.payloads {
        writeln!(out).unwrap();
        writeln!(out, "    #[namespace = {:?}]", payload.namespace).unwrap();
        writeln!(out, "    unsafe extern \"C++\" {{").unwrap();
        writeln!(out, "        include!({:?});", payload.header).unwrap();
        writeln!(
            out,
            "        type {} = {};",
            payload.rust_type, payload.rust_path
        )
        .unwrap();
        writeln!(out, "    }}").unwrap();
    }

    writeln!(out).unwrap();
    writeln!(out, "    unsafe extern \"C++\" {{").unwrap();
    for header in config.headers {
        writeln!(out, "        include!({:?});", header).unwrap();
    }
    if !config.headers.is_empty() {
        writeln!(out).unwrap();
    }
    for (task, _) in &resolved {
        let cxx_type = &task.cxx_type;
        writeln!(out, "        type {cxx_type};").unwrap();
        writeln!(out).unwrap();
        writeln!(out, "        fn run(").unwrap();
        writeln!(out, "            self: Pin<&mut {cxx_type}>,").unwrap();
        writeln!(out, "            inputs: &mut {}CxxInputs,", task.name).unwrap();
        writeln!(out, "            outputs: &mut {}CxxOutputs,", task.name).unwrap();
        writeln!(out, "            context: &Context,").unwrap();
        if task.fallible {
            writeln!(out, "        ) -> Result<()>;").unwrap();
        } else {
            writeln!(out, "        );").unwrap();
        }
        writeln!(
            out,
            "        fn {}() -> UniquePtr<{cxx_type}>;",
            task.factory
        )
        .unwrap();
        writeln!(out).unwrap();
    }
    writeln!(out, "    }}").unwrap();

    writeln!(out).unwrap();
    writeln!(out, "    extern \"Rust\" {{").unwrap();
    for (task, endpoints) in &resolved {
        writeln!(out, "        type {}CxxInputs;", task.name).unwrap();
        for endpoint in endpoints.iter().filter(|endpoint| endpoint.is_input()) {
            write_input_methods(&mut out, task, endpoint);
        }
        writeln!(out).unwrap();
        writeln!(out, "        type {}CxxOutputs;", task.name).unwrap();
        for endpoint in endpoints.iter().filter(|endpoint| !endpoint.is_input()) {
            write_output_methods(&mut out, task, endpoint);
        }
        writeln!(out).unwrap();
    }
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();

    Ok(out)
}

/// Parses `tasks_path` and writes the generated bridge to `output_path`.
///
/// Writes only when the content changes and emits `rerun-if-changed` for the
/// task source, so it is safe to call from `build.rs`.
pub fn generate_bridge_file(
    tasks_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    config: &BridgeConfig,
) -> Result<()> {
    let tasks_path = tasks_path.as_ref();
    let output_path = output_path.as_ref();
    let source = fs::read_to_string(tasks_path)
        .map_err(|error| io_error(format!("failed to read {}: {error}", tasks_path.display())))?;
    let tasks = parse_tasks(&source)?;
    let generated = generate_bridge(&tasks, config)?;

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| io_error(format!("failed to create {}: {error}", parent.display())))?;
    }
    if fs::read_to_string(output_path).ok().as_deref() != Some(generated.as_str()) {
        fs::write(output_path, generated).map_err(|error| {
            io_error(format!(
                "failed to write {}: {error}",
                output_path.display()
            ))
        })?;
    }

    println!("cargo:rerun-if-changed={}", tasks_path.display());
    Ok(())
}

fn io_error(message: String) -> syn::Error {
    syn::Error::new(Span::call_site(), message)
}

fn write_input_methods(out: &mut String, task: &CxxTaskSpec, endpoint: &Endpoint) {
    let name = &endpoint.name;
    let ty = endpoint.payload.to_token_stream().to_string();
    let view = format!("{}CxxInputs", task.name);
    match endpoint.kind {
        EndpointKind::Required => {
            writeln!(out, "        unsafe fn {name}(self: &{view}) -> &{ty};").unwrap();
            writeln!(
                out,
                "        fn {name}_header(self: &{view}) -> MessageHeader;"
            )
            .unwrap();
        }
        EndpointKind::Input => {
            writeln!(out, "        fn {name}_has_value(self: &{view}) -> bool;").unwrap();
            writeln!(out, "        fn {name}_len(self: &{view}) -> usize;").unwrap();
            writeln!(
                out,
                "        unsafe fn {name}_value(self: &{view}) -> &{ty};"
            )
            .unwrap();
            writeln!(
                out,
                "        fn {name}_try_value(self: &{view}) -> *const {ty};"
            )
            .unwrap();
            writeln!(
                out,
                "        unsafe fn {name}_at(self: &{view}, index: usize) -> &{ty};"
            )
            .unwrap();
            writeln!(out, "        fn {name}_pop(self: &mut {view}) -> bool;").unwrap();
            writeln!(out, "        fn {name}_clear(self: &mut {view});").unwrap();
            writeln!(
                out,
                "        fn {name}_header(self: &{view}) -> MessageHeader;"
            )
            .unwrap();
        }
        _ => unreachable!("validated input endpoint"),
    }
}

fn write_output_methods(out: &mut String, task: &CxxTaskSpec, endpoint: &Endpoint) {
    let name = &endpoint.name;
    let ty = endpoint.payload.to_token_stream().to_string();
    let view = format!("{}CxxOutputs", task.name);
    match endpoint.kind {
        EndpointKind::Output | EndpointKind::OutputDefault => {
            writeln!(
                out,
                "        unsafe fn {name}_value(self: &mut {view}) -> &mut {ty};"
            )
            .unwrap();
            writeln!(out, "        fn {name}_send(self: &mut {view});").unwrap();
        }
        EndpointKind::Uninit => {
            writeln!(
                out,
                "        fn {name}_uninit(self: &mut {view}) -> OutputUninit;"
            )
            .unwrap();
        }
        EndpointKind::OutputSpan => {
            writeln!(
                out,
                "        fn {name}_span(self: &mut {view}) -> SpanHandle;"
            )
            .unwrap();
        }
        _ => unreachable!("validated output endpoint"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tasks() -> &'static str {
        r#"
        task_cxx_macros::define_cxx_task! {
            bridge: ffi,
            name: Probe,
            cxx_type: ProbeTask,
            factory: make_probe,
            fallible: true,
            subscribers: {
                required: RequiredInput<u64> { channel: "required", trigger: true },
                optional: OptionalInput<f64> { channel: "optional", capacity: 4 },
                span_in: InputSpan<u32> { channel: "span_in", capacity: 4 },
            },
            publishers: {
                value: Output<String> { channel: "value" },
                default: OutputDefault<u64> { channel: "default" },
                raw: OutputUninit<Vec3> { channel: "raw" },
                numbers: OutputSpan<i16> { channel: "numbers", capacity: 4 },
            },
        }

        fn unrelated() {}
        "#
    }

    #[test]
    fn parses_tasks_and_ignores_other_items() {
        let tasks = parse_tasks(sample_tasks()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name.to_string(), "Probe");
        assert!(tasks[0].fallible);
    }

    #[test]
    fn generates_all_endpoint_declarations() {
        let tasks = parse_tasks(sample_tasks()).unwrap();
        let generated = generate_bridge(
            &tasks,
            &BridgeConfig {
                namespace: "cfw::probe",
                headers: &["probe/src/probe.hpp"],
                payloads: &[PayloadAlias {
                    namespace: "cfw::messages",
                    header: "probe_messages/src/lib.rs.h",
                    rust_type: "Vec3",
                    rust_path: "::probe_messages::Vec3",
                }],
            },
        )
        .unwrap();

        for expected in [
            "#[cxx::bridge(namespace = \"cfw::probe\")]",
            "include!(\"probe/src/probe.hpp\");",
            "type Vec3 = ::probe_messages::Vec3;",
            "type ProbeTask;",
            "inputs: &mut ProbeCxxInputs,",
            "outputs: &mut ProbeCxxOutputs,",
            ") -> Result<()>;",
            "fn make_probe() -> UniquePtr<ProbeTask>;",
            "unsafe fn required(self: &ProbeCxxInputs) -> &u64;",
            "fn required_header(self: &ProbeCxxInputs) -> MessageHeader;",
            "fn optional_has_value(self: &ProbeCxxInputs) -> bool;",
            "fn optional_len(self: &ProbeCxxInputs) -> usize;",
            "unsafe fn optional_value(self: &ProbeCxxInputs) -> &f64;",
            "fn optional_try_value(self: &ProbeCxxInputs) -> *const f64;",
            "unsafe fn optional_at(self: &ProbeCxxInputs, index: usize) -> &f64;",
            "fn optional_pop(self: &mut ProbeCxxInputs) -> bool;",
            "fn optional_clear(self: &mut ProbeCxxInputs);",
            "fn span_in_header(self: &ProbeCxxInputs) -> MessageHeader;",
            "unsafe fn value_value(self: &mut ProbeCxxOutputs) -> &mut String;",
            "fn value_send(self: &mut ProbeCxxOutputs);",
            "unsafe fn default_value(self: &mut ProbeCxxOutputs) -> &mut u64;",
            "fn default_send(self: &mut ProbeCxxOutputs);",
            "fn raw_uninit(self: &mut ProbeCxxOutputs) -> OutputUninit;",
            "fn numbers_span(self: &mut ProbeCxxOutputs) -> SpanHandle;",
        ] {
            assert!(
                generated.contains(expected),
                "missing `{expected}` in generated bridge:\n{generated}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_endpoints() {
        let source = r#"
        task_cxx_macros::define_cxx_task! {
            bridge: ffi,
            name: Probe,
            cxx_type: ProbeTask,
            factory: make_probe,
            subscribers: {},
            publishers: { value: Iox2Output<u64> { channel: "out" } },
        }
        "#;
        let tasks = parse_tasks(source).unwrap();
        assert!(
            generate_bridge(
                &tasks,
                &BridgeConfig {
                    namespace: "cfw::probe",
                    headers: &[],
                    payloads: &[],
                },
            )
            .is_err()
        );
    }

    #[test]
    fn requires_at_least_one_task() {
        assert!(parse_tasks("fn main() {}").is_err());
    }
}
