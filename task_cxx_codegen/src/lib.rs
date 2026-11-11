//! Parsing and bridge generation for `define_cxx_task!`.
//!
//! The proc macro and `build.rs` share one declarative spec so the literal
//! `#[cxx::bridge]` module cannot drift from the generated views:
//!
//! - [`CxxTaskSpec`] parses a single task declaration.
//! - [`CxxBridgeSpec`] parses the `define_cxx_bridge!` config block.
//! - [`task_endpoints`] applies the same endpoint validation to both users.
//! - [`generate_bridge_file`] reads both blocks from one source file and writes
//!   the matching `#[cxx::bridge]` module.
//!
//! cxx generates C++ from literal source files, so the bridge has to be
//! written to disk before `cxx_build::bridge` runs. That is why this lives in
//! a normal library rather than in the proc-macro crate.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Result, Token};

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

/// Bridge-level configuration parsed from `define_cxx_bridge!`.
pub struct CxxBridgeSpec {
    pub namespace: String,
    pub headers: Vec<String>,
    pub payloads: Vec<PayloadSpec>,
}

/// A payload type aliased into the bridge from another bridge.
pub struct PayloadSpec {
    pub rust_type: Ident,
    pub namespace: String,
    pub header: String,
    pub rust_path: syn::Path,
}

impl Parse for CxxBridgeSpec {
    fn parse(input: ParseStream) -> Result<Self> {
        let namespace: LitStr = parse_key(input, "namespace")?;
        input.parse::<Token![,]>()?;
        let headers = parse_headers(input)?;
        input.parse::<Token![,]>()?;
        let payloads = parse_payloads(input)?;
        let _ = input.parse::<Token![,]>();
        if !input.is_empty() {
            return Err(input.error("unexpected tokens after bridge config"));
        }
        Ok(Self {
            namespace: namespace.value(),
            headers,
            payloads,
        })
    }
}

impl CxxBridgeSpec {
    /// Generates the literal bridge for `tasks` using this config.
    pub fn generate(&self, tasks: &[CxxTaskSpec]) -> Result<String> {
        let headers: Vec<&str> = self.headers.iter().map(String::as_str).collect();
        let payload_strings: Vec<(String, String)> = self
            .payloads
            .iter()
            .map(|payload| {
                let path = &payload.rust_path;
                (payload.rust_type.to_string(), quote!(#path).to_string())
            })
            .collect();
        let payloads: Vec<PayloadAlias<'_>> = self
            .payloads
            .iter()
            .zip(&payload_strings)
            .map(|(payload, (rust_type, rust_path))| PayloadAlias {
                namespace: &payload.namespace,
                header: &payload.header,
                rust_type,
                rust_path,
            })
            .collect();
        generate_bridge(
            tasks,
            &BridgeConfig {
                namespace: &self.namespace,
                headers: &headers,
                payloads: &payloads,
            },
        )
    }
}

fn parse_headers(input: ParseStream) -> Result<Vec<String>> {
    let key: Ident = input.parse()?;
    if key != "headers" {
        return Err(syn::Error::new_spanned(key, "expected 'headers'"));
    }
    input.parse::<Token![:]>()?;
    let content;
    syn::bracketed!(content in input);
    let mut headers = Vec::new();
    while !content.is_empty() {
        let header: LitStr = content.parse()?;
        headers.push(header.value());
        if content.is_empty() {
            break;
        }
        content.parse::<Token![,]>()?;
    }
    Ok(headers)
}

fn parse_payloads(input: ParseStream) -> Result<Vec<PayloadSpec>> {
    let key: Ident = input.parse()?;
    if key != "payloads" {
        return Err(syn::Error::new_spanned(key, "expected 'payloads'"));
    }
    input.parse::<Token![:]>()?;
    let content;
    syn::braced!(content in input);
    let mut payloads = Vec::new();
    while !content.is_empty() {
        let rust_type: Ident = content.parse()?;
        content.parse::<Token![:]>()?;
        let body;
        syn::braced!(body in content);
        let namespace: LitStr = parse_key(&body, "namespace")?;
        body.parse::<Token![,]>()?;
        let header: LitStr = parse_key(&body, "header")?;
        body.parse::<Token![,]>()?;
        let rust_path: syn::Path = parse_key(&body, "rust_path")?;
        let _ = body.parse::<Token![,]>();
        if !body.is_empty() {
            return Err(body.error("unexpected tokens after payload alias"));
        }
        payloads.push(PayloadSpec {
            rust_type,
            namespace: namespace.value(),
            header: header.value(),
            rust_path,
        });
        if content.is_empty() {
            break;
        }
        content.parse::<Token![,]>()?;
    }
    Ok(payloads)
}

/// Parsed bridge config and task declarations from one source file.
pub struct ParsedSource {
    pub bridge: CxxBridgeSpec,
    pub tasks: Vec<CxxTaskSpec>,
}

/// Parses the `define_cxx_bridge!` config and every `define_cxx_task!` in `source`.
pub fn parse_source(source: &str) -> Result<ParsedSource> {
    let file = syn::parse_file(source)?;
    let mut bridge = None;
    let mut tasks = Vec::new();
    for item in file.items {
        let syn::Item::Macro(item_macro) = item else {
            continue;
        };
        let Some(last) = item_macro.mac.path.segments.last() else {
            continue;
        };
        match last.ident.to_string().as_str() {
            "define_cxx_bridge" => {
                if bridge.is_some() {
                    return Err(syn::Error::new(
                        item_macro.mac.path.segments.first().unwrap().ident.span(),
                        "only one define_cxx_bridge! is allowed per source file",
                    ));
                }
                bridge = Some(syn::parse2(item_macro.mac.tokens)?);
            }
            "define_cxx_task" => tasks.push(syn::parse2(item_macro.mac.tokens)?),
            _ => {}
        }
    }
    let bridge = bridge.ok_or_else(|| {
        syn::Error::new(Span::call_site(), "no define_cxx_bridge! invocation found")
    })?;
    if tasks.is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "no define_cxx_task! invocations found",
        ));
    }
    Ok(ParsedSource { bridge, tasks })
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

/// Parses `tasks_path` and writes the generated bridge to `output_path`.
///
/// Runs `rustfmt` when available and writes only when the content changes.
pub fn generate_bridge_file(
    tasks_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
) -> Result<()> {
    let tasks_path = tasks_path.as_ref();
    let parsed = parse_source(&read_source(tasks_path)?)?;
    let generated = parsed.bridge.generate(&parsed.tasks)?;
    let generated = rustfmt_source(&generated).unwrap_or(generated);
    write_if_changed(output_path.as_ref(), &generated)?;
    println!("cargo:rerun-if-changed={}", tasks_path.display());
    Ok(())
}

/// Generates the literal bridge and a combined source file for Bazel.
///
/// The combined file is the bridge followed by the original task declarations,
/// so both expand in the same Rust module.
pub fn generate_bridge_files(
    tasks_path: impl AsRef<Path>,
    bridge_path: impl AsRef<Path>,
    combined_path: impl AsRef<Path>,
) -> Result<()> {
    let tasks_path = tasks_path.as_ref();
    let source = read_source(tasks_path)?;
    let parsed = parse_source(&source)?;
    let bridge = parsed.bridge.generate(&parsed.tasks)?;
    write_if_changed(bridge_path.as_ref(), &bridge)?;
    let combined = format!("{bridge}\n{source}");
    write_if_changed(combined_path.as_ref(), &combined)?;
    Ok(())
}

fn read_source(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .map_err(|error| io_error(format!("failed to read {}: {error}", path.display())))
}

/// Generates the literal `#[cxx::bridge]` module for `tasks`.
pub fn generate_bridge(tasks: &[CxxTaskSpec], config: &BridgeConfig) -> Result<String> {
    let resolved: Vec<ResolvedTask> = tasks
        .iter()
        .map(|task| {
            let endpoints = task_endpoints(&task.subscribers, &task.publishers)?;
            Ok(ResolvedTask {
                spec: task,
                endpoints,
            })
        })
        .collect::<Result<_>>()?;

    let namespace = config.namespace;
    let mut payload_aliases = Vec::with_capacity(config.payloads.len());
    for payload in config.payloads {
        let alias_namespace = payload.namespace;
        let header = payload.header;
        let rust_type: Ident = syn::parse_str(payload.rust_type)?;
        let rust_path: syn::Path = syn::parse_str(payload.rust_path)?;
        payload_aliases.push(quote! {
            #[namespace = #alias_namespace]
            unsafe extern "C++" {
                include!(#header);
                type #rust_type = #rust_path;
            }
        });
    }

    let framework = quote! {
        #[namespace = "cfw"]
        unsafe extern "C++" {
            include!("task_cxx/src/lib.rs.h");
            include!("task_cxx/src/output.hpp");

            type Context = ::task_cxx::ffi::Context;
            type MessageHeader = ::task_cxx::ffi::MessageHeader;
            type OutputUninit = ::task_cxx::OutputUninit;
            type SpanHandle = ::task_cxx::SpanHandle;
        }
    };

    let mut out = BridgeWriter::new();
    out.line("// @generated by task_cxx_codegen. Do not edit.");
    out.blank();
    out.item(quote!(#[cxx::bridge(namespace = #namespace)]));
    out.open(quote!(mod ffi));

    out.blank();
    out.item(framework);

    for alias in payload_aliases {
        out.blank();
        out.item(alias);
    }

    out.blank();
    out.open(quote!(unsafe extern "C++"));
    for header in config.headers {
        out.item(quote!(include!(#header);));
    }
    if !config.headers.is_empty() {
        out.blank();
    }
    for task in &resolved {
        for item in task.cxx_items() {
            out.item(item);
        }
        out.blank();
    }
    out.close();

    out.blank();
    out.open(quote!(extern "Rust"));
    for task in &resolved {
        for item in task.rust_items() {
            out.item(item);
        }
        out.blank();
    }
    out.close();

    out.close();
    Ok(out.finish())
}

fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| io_error(format!("failed to create {}: {error}", parent.display())))?;
    }
    if fs::read_to_string(path).ok().as_deref() != Some(contents) {
        fs::write(path, contents)
            .map_err(|error| io_error(format!("failed to write {}: {error}", path.display())))?;
    }
    Ok(())
}

fn io_error(message: String) -> syn::Error {
    syn::Error::new(Span::call_site(), message)
}

fn rustfmt_source(source: &str) -> Option<String> {
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(source.as_bytes()).ok()?;
    drop(child.stdin.take());
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

struct ResolvedTask<'a> {
    spec: &'a CxxTaskSpec,
    endpoints: Vec<Endpoint>,
}

impl ResolvedTask<'_> {
    fn cxx_items(&self) -> Vec<TokenStream> {
        let cxx_type = &self.spec.cxx_type;
        let factory = &self.spec.factory;
        let inputs = format_ident!("{}CxxInputs", self.spec.name);
        let outputs = format_ident!("{}CxxOutputs", self.spec.name);
        let result = if self.spec.fallible {
            quote!(-> Result<()>)
        } else {
            quote!()
        };
        vec![
            quote!(type #cxx_type;),
            quote! {
                fn run(
                    self: Pin<&mut #cxx_type>,
                    inputs: &mut #inputs,
                    outputs: &mut #outputs,
                    context: &Context,
                ) #result;
            },
            quote!(fn #factory() -> UniquePtr<#cxx_type>;),
        ]
    }

    fn rust_items(&self) -> Vec<TokenStream> {
        let inputs = format_ident!("{}CxxInputs", self.spec.name);
        let outputs = format_ident!("{}CxxOutputs", self.spec.name);
        let mut items = vec![quote!(type #inputs;)];
        for endpoint in self.endpoints.iter().filter(|endpoint| endpoint.is_input()) {
            items.extend(input_items(&inputs, endpoint));
        }
        items.push(quote!(type #outputs;));
        for endpoint in self
            .endpoints
            .iter()
            .filter(|endpoint| !endpoint.is_input())
        {
            items.extend(output_items(&outputs, endpoint));
        }
        items
    }
}

fn input_items(view: &Ident, endpoint: &Endpoint) -> Vec<TokenStream> {
    let name = &endpoint.name;
    let ty = &endpoint.payload;
    let header = format_ident!("{name}_header");
    match endpoint.kind {
        EndpointKind::Required => vec![
            quote!(unsafe fn #name(self: &#view) -> &#ty;),
            quote!(fn #header(self: &#view) -> MessageHeader;),
        ],
        EndpointKind::Input => {
            let has_value = format_ident!("{name}_has_value");
            let len = format_ident!("{name}_len");
            let value = format_ident!("{name}_value");
            let try_value = format_ident!("{name}_try_value");
            let at = format_ident!("{name}_at");
            let pop = format_ident!("{name}_pop");
            let clear = format_ident!("{name}_clear");
            vec![
                quote!(fn #has_value(self: &#view) -> bool;),
                quote!(fn #len(self: &#view) -> usize;),
                quote!(unsafe fn #value(self: &#view) -> &#ty;),
                quote!(fn #try_value(self: &#view) -> *const #ty;),
                quote!(unsafe fn #at(self: &#view, index: usize) -> &#ty;),
                quote!(fn #pop(self: &mut #view) -> bool;),
                quote!(fn #clear(self: &mut #view);),
                quote!(fn #header(self: &#view) -> MessageHeader;),
            ]
        }
        _ => unreachable!("validated input endpoint"),
    }
}

fn output_items(view: &Ident, endpoint: &Endpoint) -> Vec<TokenStream> {
    let name = &endpoint.name;
    let ty = &endpoint.payload;
    match endpoint.kind {
        EndpointKind::Output | EndpointKind::OutputDefault => {
            let value = format_ident!("{name}_value");
            let send = format_ident!("{name}_send");
            vec![
                quote!(unsafe fn #value(self: &mut #view) -> &mut #ty;),
                quote!(fn #send(self: &mut #view);),
            ]
        }
        EndpointKind::Uninit => {
            let uninit = format_ident!("{name}_uninit");
            vec![quote!(fn #uninit(self: &mut #view) -> OutputUninit;)]
        }
        EndpointKind::OutputSpan => {
            let span = format_ident!("{name}_span");
            vec![quote!(fn #span(self: &mut #view) -> SpanHandle;)]
        }
        _ => unreachable!("validated output endpoint"),
    }
}

struct BridgeWriter {
    out: String,
    indent: usize,
}

impl BridgeWriter {
    fn new() -> Self {
        Self {
            out: String::new(),
            indent: 0,
        }
    }

    fn line(&mut self, text: &str) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn item(&mut self, tokens: TokenStream) {
        self.line(&tokens.to_string());
    }

    fn open(&mut self, tokens: TokenStream) {
        self.line(&format!("{tokens} {{"));
        self.indent += 1;
    }

    fn close(&mut self) {
        self.indent -= 1;
        self.line("}");
    }

    fn blank(&mut self) {
        self.out.push('\n');
    }

    fn finish(self) -> String {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compact(source: &str) -> String {
        source.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    fn sample_tasks() -> &'static str {
        r#"
        task_cxx_macros::define_cxx_bridge! {
            namespace: "cfw::probe",
            headers: ["probe/src/probe.hpp"],
            payloads: {
                Vec3: {
                    namespace: "cfw::messages",
                    header: "probe_messages/src/lib.rs.h",
                    rust_path: ::probe_messages::Vec3,
                },
            },
        }

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
    fn parses_config_and_tasks_and_ignores_other_items() {
        let parsed = parse_source(sample_tasks()).unwrap();
        assert_eq!(parsed.bridge.namespace, "cfw::probe");
        assert_eq!(parsed.bridge.headers, ["probe/src/probe.hpp"]);
        assert_eq!(parsed.bridge.payloads.len(), 1);
        assert_eq!(parsed.tasks.len(), 1);
        assert_eq!(parsed.tasks[0].name.to_string(), "Probe");
        assert!(parsed.tasks[0].fallible);
    }

    #[test]
    fn generates_all_endpoint_declarations() {
        let parsed = parse_source(sample_tasks()).unwrap();
        let generated = parsed.bridge.generate(&parsed.tasks).unwrap();

        let compacted = compact(&generated);
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
                compacted.contains(&compact(expected)),
                "missing `{expected}` in generated bridge:\n{generated}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_endpoints() {
        let source = r#"
        task_cxx_macros::define_cxx_bridge! {
            namespace: "cfw::probe",
            headers: [],
            payloads: {},
        }

        task_cxx_macros::define_cxx_task! {
            bridge: ffi,
            name: Probe,
            cxx_type: ProbeTask,
            factory: make_probe,
            subscribers: {},
            publishers: { value: Iox2Output<u64> { channel: "out" } },
        }
        "#;
        let parsed = parse_source(source).unwrap();
        assert!(parsed.bridge.generate(&parsed.tasks).is_err());
    }

    #[test]
    fn requires_config_and_tasks() {
        assert!(parse_source("fn main() {}").is_err());
        assert!(
            parse_source(
                r#"
                task_cxx_macros::define_cxx_bridge! {
                    namespace: "cfw::probe",
                    headers: [],
                    payloads: {},
                }
                "#,
            )
            .is_err()
        );
    }
}
