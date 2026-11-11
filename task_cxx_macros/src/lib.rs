//! C++ task adapter generation.
//!
//! `define_cxx_task!` consumes a declarative endpoint list, reuses the shared
//! registration/collection generators from [`task_macro_common`], and emits:
//!
//! - the task struct holding a C++ object behind [`task_cxx::SendTask`]
//! - lifetime-erased input/output view types (declared by the user bridge as
//!   `extern "Rust"` opaque types)
//! - the `task_graph::Task` adapter that loans inputs/outputs and calls the
//!   C++ `run` method through the cxx bridge
//!
//! The literal `#[cxx::bridge]` module stays hand-written (cxx codegen scans
//! source files), but the method names it declares follow a fixed convention
//! derived from the endpoint names; mismatches are compile errors.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, Path, Result, Token};

use task_macro_common::{
    Endpoint, EndpointKind, EndpointSpec, TaskDefinition, declared_endpoint, generate,
    generate_registration, parse_endpoint_block, validate_endpoints,
};

/// Declare a C++-backed task.
///
/// ```ignore
/// define_cxx_task! {
///     bridge: ffi,
///     name: FizzBuzzCalculator,
///     cxx_type: FizzBuzzTask,
///     factory: make_fizz_buzz_task,
///     subscribers: {
///         integer: RequiredInput<u64> { channel: "integer", trigger: true },
///     },
///     publishers: {
///         fizz_buzz_string: Output<String> { channel: "fizz_buzz_string" },
///     },
/// }
/// ```
///
/// Bridge method convention for an endpoint named `foo` with payload `T`
/// (`run` receives `Pin<&mut CxxTask>`, `&mut Inputs`, `&mut Outputs`, `&Context`;
/// inputs are mutable so queue operations can drain prepared messages):
///
/// - required input: `unsafe fn foo(self: &FooCxxInputs) -> &T;`
/// - optional input: `foo_has_value(&self) -> bool`, `foo_len(&self) -> usize`,
///   `unsafe fn foo_value(&self) -> &T`, `unsafe fn foo_at(&self, usize) -> &T`,
///   `foo_try_value(&self) -> *const T`, `foo_pop(&mut self)`
/// - input span (`InputSpan`, requires `capacity`): the same methods as an
///   optional input
/// - default-initialized output (`Output`/`OutputDefault`):
///   `unsafe fn foo_value(&mut self) -> &mut T`, `foo_send(&mut self)`
/// - in-place output (`OutputUninit`): `fn foo_uninit(&mut self) -> OutputUninit`
///   (C++ calls `cfw::OutputUninit::init<T>(...)` to construct, send, or drop
///   the payload through the owning `cfw::Output<T>` handle)
/// - span output (`OutputSpan`, requires `capacity`):
///   `fn foo_loan(&mut self) -> *mut T` (null when exhausted),
///   `fn foo_write(&mut self, T) -> bool`, `foo_len(&self) -> usize`,
///   `foo_send(&mut self, index: usize)`, `foo_send_all(&mut self)`,
///   `foo_discard(&mut self)`
///
/// Both input kinds also expose `foo_header(&self) -> MessageHeader`, where the
/// bridge aliases `type MessageHeader = ::task_cxx::ffi::MessageHeader`.
///
/// Set `fallible: true` to declare `run` as returning `Result<()>` in the bridge.
/// C++ exceptions are then caught by cxx and surfaced as callback panics
/// (`BatchFailure::Panic` to executors, like any panicking task) instead of
/// terminating the process.
#[proc_macro]
pub fn define_cxx_task(input: TokenStream) -> TokenStream {
    let spec = syn::parse_macro_input!(input as CxxTaskSpec);
    expand(spec)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

struct CxxTaskSpec {
    bridge: Path,
    name: Ident,
    cxx_type: Ident,
    factory: Ident,
    fallible: bool,
    subscribers: Vec<EndpointSpec>,
    publishers: Vec<EndpointSpec>,
}

impl Parse for CxxTaskSpec {
    fn parse(input: ParseStream) -> Result<Self> {
        let bridge = parse_key::<Path>(input, "bridge")?;
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
        let subscribers = parse_endpoint_block(input, "subscribers")?;
        input.parse::<Token![,]>()?;
        let publishers = parse_endpoint_block(input, "publishers")?;
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

fn expand(spec: CxxTaskSpec) -> Result<TokenStream2> {
    let CxxTaskSpec {
        bridge,
        name,
        cxx_type,
        factory,
        fallible,
        subscribers,
        publishers,
    } = spec;

    let mut endpoints = Vec::new();
    for subscriber in subscribers {
        endpoints.push(declared_endpoint(subscriber, true)?);
    }
    for publisher in publishers {
        endpoints.push(declared_endpoint(publisher, false)?);
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

    let declaration = format_ident!("{name}Declaration");
    let inputs = format_ident!("{name}Inputs");
    let outputs = format_ident!("{name}Outputs");
    let view_inputs = format_ident!("{name}CxxInputs");
    let view_outputs = format_ident!("{name}CxxOutputs");

    let task = TaskDefinition {
        item: None,
        name: name.clone(),
        self_ty: quote!(#name),
        endpoints,
        arguments: Vec::new(),
        fallible,
    };
    let endpoints = &task.endpoints;

    let declaration_tokens = generate::declaration(&task, &declaration);
    let registration = generate_registration(&quote!(#name), &declaration, endpoints);
    let input_collection = generate::collection(endpoints, &inputs, true);
    let output_collection = generate::collection(endpoints, &outputs, false);
    let views = view_definitions(&name, &view_inputs, &view_outputs, endpoints);
    let invocation = invocation(&name, &view_inputs, &view_outputs, endpoints, fallible);
    let binding = generate::binding(&task, &declaration, &inputs, &outputs, &invocation);

    let task_struct = quote! {
        /// C++-backed task generated by `define_cxx_task!`.
        pub struct #name {
            task: ::task_cxx::SendTask<#bridge::#cxx_type>,
        }

        impl #name {
            pub fn new() -> Self {
                Self {
                    task: ::task_cxx::SendTask::new(#bridge::#factory()),
                }
            }
        }

        impl ::core::default::Default for #name {
            fn default() -> Self {
                Self::new()
            }
        }
    };

    Ok(quote! {
        #task_struct
        #declaration_tokens
        #registration
        #input_collection
        #output_collection
        #views
        #binding
    })
}

fn view_definitions(
    task_name: &Ident,
    view_inputs: &Ident,
    view_outputs: &Ident,
    endpoints: &[Endpoint],
) -> TokenStream2 {
    let input_endpoints: Vec<_> = endpoints.iter().filter(|e| e.is_input()).collect();
    let output_endpoints: Vec<_> = endpoints.iter().filter(|e| !e.is_input()).collect();

    let input_fields = input_endpoints.iter().map(|endpoint| {
        let name = &endpoint.name;
        let payload = &endpoint.payload;
        match endpoint.kind {
            EndpointKind::Required => quote!(
                #name: *const ::task::RequiredInput<'static, 'static, #payload>
            ),
            _ => quote!(#name: *mut ::task::Input<'static, 'static, #payload>),
        }
    });
    let input_params = input_endpoints.iter().map(|endpoint| {
        let name = &endpoint.name;
        let payload = &endpoint.payload;
        match endpoint.kind {
            EndpointKind::Required => quote!(#name: &::task::RequiredInput<'_, '_, #payload>),
            _ => quote!(#name: &mut ::task::Input<'_, '_, #payload>),
        }
    });
    let input_inits = input_endpoints.iter().map(|endpoint| {
        let name = &endpoint.name;
        let payload = &endpoint.payload;
        match endpoint.kind {
            EndpointKind::Required => quote!(
                #name: #name as *const ::task::RequiredInput<'_, '_, #payload>
                    as *const ::task::RequiredInput<'static, 'static, #payload>
            ),
            _ => quote!(
                #name: #name as *mut ::task::Input<'_, '_, #payload>
                    as *mut ::task::Input<'static, 'static, #payload>
            ),
        }
    });
    let input_methods = input_endpoints.iter().map(|endpoint| {
        let name = &endpoint.name;
        let payload = &endpoint.payload;
        let header = format_ident!("{name}_header");
        match endpoint.kind {
            EndpointKind::Required => quote! {
                /// # Safety
                /// The returned reference is only valid for the duration of the C++ `run` call.
                pub unsafe fn #name(&self) -> &#payload {
                    // SAFETY: view construction guarantees the endpoint outlives the view,
                    // which never escapes the C++ `run` call.
                    unsafe { &*self.#name }.value()
                }

                pub fn #header(&self) -> ::task_cxx::ffi::MessageHeader {
                    // SAFETY: view construction guarantees the endpoint outlives the view.
                    ::task_cxx::message_header(unsafe { &*self.#name }.header())
                }
            },
            _ => {
                let has_value = format_ident!("{name}_has_value");
                let len = format_ident!("{name}_len");
                let value = format_ident!("{name}_value");
                let try_value = format_ident!("{name}_try_value");
                let at = format_ident!("{name}_at");
                let pop = format_ident!("{name}_pop");
                quote! {
                    pub fn #has_value(&self) -> bool {
                        // SAFETY: view construction guarantees the endpoint outlives the view.
                        unsafe { &*self.#name }.value().is_some()
                    }

                    pub fn #len(&self) -> usize {
                        // SAFETY: view construction guarantees the endpoint outlives the view.
                        unsafe { &*self.#name }.len()
                    }

                    /// # Safety
                    /// The returned reference is only valid for the duration of the C++ `run` call.
                    pub unsafe fn #value(&self) -> &#payload {
                        // SAFETY: view construction guarantees the endpoint outlives the view.
                        unsafe { &*self.#name }
                            .value()
                            .expect("input value accessed while empty")
                    }

                    /// Returns null when the input queue is empty. The pointer is only
                    /// valid for the duration of the C++ `run` call.
                    pub fn #try_value(&self) -> *const #payload {
                        // SAFETY: view construction guarantees the endpoint outlives the view.
                        unsafe { &*self.#name }
                            .value()
                            .map_or(::core::ptr::null(), |value| value as *const #payload)
                    }

                    /// # Safety
                    /// The returned reference is only valid for the duration of the C++ `run` call.
                    pub unsafe fn #at(&self, index: usize) -> &#payload {
                        // SAFETY: view construction guarantees the endpoint outlives the view.
                        unsafe { &*self.#name }
                            .inputs()
                            .nth(index)
                            .map(|message| &message.message)
                            .expect("input index out of range")
                    }

                    pub fn #pop(&mut self) {
                        // SAFETY: view construction guarantees the endpoint outlives the view.
                        unsafe { &mut *self.#name }.clear();
                    }

                    pub fn #header(&self) -> ::task_cxx::ffi::MessageHeader {
                        // SAFETY: view construction guarantees the endpoint outlives the view.
                        let header = unsafe { &*self.#name }
                            .header()
                            .expect("input header accessed while empty");
                        ::task_cxx::message_header(header)
                    }
                }
            }
        }
    });

    let output_fields = output_endpoints.iter().map(|endpoint| {
        let name = &endpoint.name;
        let ty = &endpoint.payload;
        match endpoint.kind {
            EndpointKind::Output | EndpointKind::OutputDefault => quote!(
                #name: *mut ::core::option::Option<
                    ::task::OutputDefault<'static, 'static, #ty>
                >
            ),
            EndpointKind::Uninit => quote!(
                #name: *mut ::core::option::Option<
                    ::task::Output<'static, 'static, #ty>
                >
            ),
            EndpointKind::OutputSpan => {
                let slot = span_slot_ident(task_name, endpoint);
                let entries = span_entries_ident(endpoint);
                quote!(
                    #name: *mut ::task::OutputSpan<'static, 'static, #ty>,
                    #entries: ::std::vec::Vec<#slot>
                )
            }
            _ => quote!(),
        }
    });
    let output_params = output_endpoints.iter().map(|endpoint| {
        let name = &endpoint.name;
        let ty = &endpoint.payload;
        match endpoint.kind {
            EndpointKind::Output | EndpointKind::OutputDefault => quote!(
                #name: &mut ::core::option::Option<::task::OutputDefault<'_, '_, #ty>>
            ),
            EndpointKind::Uninit => quote!(
                #name: &mut ::core::option::Option<::task::Output<'_, '_, #ty>>
            ),
            EndpointKind::OutputSpan => quote!(
                #name: &mut ::task::OutputSpan<'_, '_, #ty>
            ),
            _ => quote!(),
        }
    });
    let output_inits = output_endpoints.iter().map(|endpoint| {
        let name = &endpoint.name;
        let ty = &endpoint.payload;
        match endpoint.kind {
            EndpointKind::Output | EndpointKind::OutputDefault => quote!(
                #name: #name as *mut ::core::option::Option<
                    ::task::OutputDefault<'_, '_, #ty>
                > as *mut ::core::option::Option<
                    ::task::OutputDefault<'static, 'static, #ty>
                >
            ),
            EndpointKind::Uninit => quote!(
                #name: #name as *mut ::core::option::Option<
                    ::task::Output<'_, '_, #ty>
                > as *mut ::core::option::Option<
                    ::task::Output<'static, 'static, #ty>
                >
            ),
            EndpointKind::OutputSpan => {
                let entries = span_entries_ident(endpoint);
                quote!(
                    #name: #name as *mut ::task::OutputSpan<'_, '_, #ty>
                        as *mut ::task::OutputSpan<'static, 'static, #ty>,
                    #entries: ::std::vec::Vec::new()
                )
            }
            _ => quote!(),
        }
    });
    let output_methods = output_endpoints.iter().map(|endpoint| {
        let name = &endpoint.name;
        let ty = &endpoint.payload;
        match endpoint.kind {
            EndpointKind::Output | EndpointKind::OutputDefault => {
                let value = format_ident!("{name}_value");
                let send = format_ident!("{name}_send");
                quote! {
                    /// # Safety
                    /// The returned reference is only valid until the next `#send` call and
                    /// must not escape the C++ `run` call.
                    pub unsafe fn #value(&mut self) -> &mut #ty {
                        // SAFETY: view construction guarantees the slot outlives the view;
                        // the `&mut self` borrow makes access exclusive for its duration.
                        let slot = unsafe { &mut *self.#name };
                        slot.as_mut()
                            .expect("output value accessed after send")
                            .value_mut()
                    }

                    pub fn #send(&mut self) {
                        // SAFETY: view construction guarantees the slot outlives the view;
                        // the `&mut self` borrow makes access exclusive for its duration.
                        if let ::core::option::Option::Some(output) =
                            unsafe { &mut *self.#name }.take()
                        {
                            output.send();
                        }
                    }
                }
            }
            EndpointKind::Uninit => {
                let uninit = format_ident!("{name}_uninit");
                let (send_thunk, discard_thunk) = uninit_thunk_idents(task_name, endpoint);
                quote! {
                    /// Returns a descriptor for in-place construction. Feed it to
                    /// `cfw::OutputUninit::init<T>(...)` in C++ to get an owning
                    /// output handle. Null payload means no slot was available.
                    pub fn #uninit(&mut self) -> ::task_cxx::OutputUninit {
                        // SAFETY: view construction guarantees the slot outlives the view;
                        // the `&mut self` borrow makes access exclusive for its duration.
                        let slot = unsafe { &mut *self.#name };
                        let payload = slot.as_mut().map_or(
                            ::core::ptr::null_mut(),
                            |output| {
                                output.payload_uninit().as_mut_ptr()
                                    as *mut ::core::ffi::c_void
                            },
                        );
                        ::task_cxx::OutputUninit {
                            payload,
                            context: self as *mut Self as *mut ::core::ffi::c_void,
                            send: #send_thunk,
                            discard: #discard_thunk,
                            index: 0,
                        }
                    }
                }
            }
            EndpointKind::OutputSpan => {
                let slot = span_slot_ident(task_name, endpoint);
                let entries = span_entries_ident(endpoint);
                let loan = format_ident!("{name}_loan");
                let write = format_ident!("{name}_write");
                let len = format_ident!("{name}_len");
                let send = format_ident!("{name}_send");
                let send_all = format_ident!("{name}_send_all");
                let discard = format_ident!("{name}_discard");
                quote! {
                    /// Reserves an output slot for placement-new initialization and
                    /// returns a pointer to its payload, or null when the publisher's
                    /// quota or arena is exhausted. The pointer is valid until the slot
                    /// is sent or discarded; C++ must placement-new the payload before
                    /// calling `send`/`send_all`. Dropping an unsent placement-new slot
                    /// does not run the payload destructor.
                    pub fn #loan(&mut self) -> *mut #ty {
                        // SAFETY: view construction guarantees the span outlives the view.
                        let span = unsafe { &*self.#name };
                        let mut entry = match span.loan_uninit() {
                            ::core::result::Result::Ok(entry) => entry,
                            ::core::result::Result::Err(_) => return ::core::ptr::null_mut(),
                        };
                        let payload = entry.payload_uninit().as_mut_ptr();
                        self.#entries.push(#slot::Uninit(entry));
                        payload
                    }

                    /// Reserves an output slot initialized with `value`. Returns false
                    /// when the publisher's quota or arena is exhausted.
                    pub fn #write(&mut self, value: #ty) -> bool {
                        // SAFETY: view construction guarantees the span outlives the view.
                        let span = unsafe { &*self.#name };
                        match span.loan(value) {
                            ::core::result::Result::Ok(entry) => {
                                self.#entries.push(#slot::Written(entry));
                                true
                            }
                            ::core::result::Result::Err(_) => false,
                        }
                    }

                    /// Number of outstanding (unsent) loans.
                    pub fn #len(&self) -> usize {
                        self.#entries
                            .iter()
                            .filter(|slot| !::core::matches!(slot, #slot::Sent))
                            .count()
                    }

                    /// Publishes loan `index` in loan order. Does nothing when the
                    /// index is out of range or already sent.
                    pub fn #send(&mut self, index: usize) {
                        let ::core::option::Option::Some(slot) = self.#entries.get_mut(index) else {
                            return;
                        };
                        let slot = ::core::mem::replace(slot, #slot::Sent);
                        match slot {
                            // SAFETY: C++ placement-new initialized the payload in place.
                            #slot::Uninit(entry) => unsafe { entry.assume_init() }.send(),
                            #slot::Written(entry) => entry.send(),
                            #slot::Sent => {}
                        }
                    }

                    /// Publishes outstanding loans in loan order and clears the span.
                    pub fn #send_all(&mut self) {
                        for slot in self.#entries.drain(..) {
                            match slot {
                                // SAFETY: C++ placement-new initialized the payload in place.
                                #slot::Uninit(entry) => unsafe { entry.assume_init() }.send(),
                                #slot::Written(entry) => entry.send(),
                                #slot::Sent => {}
                            }
                        }
                    }

                    /// Drops all outstanding loans without publishing them. Slots
                    /// initialized with placement new must be destroyed by C++ first.
                    pub fn #discard(&mut self) {
                        self.#entries.clear();
                    }
                }
            }
            _ => quote!(),
        }
    });

    let uninit_thunks = output_endpoints
        .iter()
        .filter(|endpoint| matches!(endpoint.kind, EndpointKind::Uninit))
        .map(|endpoint| {
            let name = &endpoint.name;
            let (send_thunk, discard_thunk) = uninit_thunk_idents(task_name, endpoint);
            quote! {
                #[allow(non_snake_case)]
                unsafe extern "C" fn #send_thunk(
                    context: *mut ::core::ffi::c_void,
                    _index: usize,
                ) {
                    // SAFETY: context is the live view for the duration of `run`.
                    let view = unsafe { &mut *(context as *mut #view_outputs) };
                    if let ::core::option::Option::Some(output) =
                        unsafe { &mut *view.#name }.take()
                    {
                        // SAFETY: C++ initialized the payload before sending.
                        unsafe { output.assume_init() }.send();
                    }
                }

                #[allow(non_snake_case)]
                unsafe extern "C" fn #discard_thunk(
                    context: *mut ::core::ffi::c_void,
                    _index: usize,
                ) {
                    // SAFETY: context is the live view for the duration of `run`.
                    let view = unsafe { &mut *(context as *mut #view_outputs) };
                    let _ = unsafe { &mut *view.#name }.take();
                }
            }
        });

    let span_slots = output_endpoints
        .iter()
        .filter(|endpoint| matches!(endpoint.kind, EndpointKind::OutputSpan))
        .map(|endpoint| {
            let slot = span_slot_ident(task_name, endpoint);
            let ty = &endpoint.payload;
            quote! {
                #[doc(hidden)]
                enum #slot {
                    Uninit(::task::OutputSpanEntryUninit<'static, 'static, 'static, #ty>),
                    Written(::task::OutputSpanEntry<'static, 'static, 'static, #ty>),
                    Sent,
                }
            }
        });

    quote! {
        #(#uninit_thunks)*
        #(#span_slots)*

        #[doc(hidden)]
        pub struct #view_inputs {
            #(#input_fields,)*
        }

        impl #view_inputs {
            #[doc(hidden)]
            pub fn new(#(#input_params),*) -> Self {
                Self { #(#input_inits,)* }
            }

            #(#input_methods)*
        }

        #[doc(hidden)]
        pub struct #view_outputs {
            #(#output_fields,)*
        }

        impl #view_outputs {
            #[doc(hidden)]
            pub fn new(#(#output_params),*) -> Self {
                Self { #(#output_inits,)* }
            }

            #(#output_methods)*
        }
    }
}

fn uninit_thunk_idents(task_name: &Ident, endpoint: &Endpoint) -> (Ident, Ident) {
    let name = &endpoint.name;
    (
        format_ident!("__cfw_{task_name}_{name}_uninit_send"),
        format_ident!("__cfw_{task_name}_{name}_uninit_discard"),
    )
}

fn span_slot_ident(task_name: &Ident, endpoint: &Endpoint) -> Ident {
    let name = &endpoint.name;
    format_ident!("{task_name}{name}SpanSlot")
}

fn span_entries_ident(endpoint: &Endpoint) -> Ident {
    let name = &endpoint.name;
    format_ident!("{name}_entries")
}

fn invocation(
    name: &Ident,
    view_inputs: &Ident,
    view_outputs: &Ident,
    endpoints: &[Endpoint],
    fallible: bool,
) -> TokenStream2 {
    let mut prelude = Vec::new();
    let mut input_args = Vec::new();
    let mut output_args = Vec::new();
    for endpoint in endpoints {
        let endpoint_name = &endpoint.name;
        if endpoint.is_input() {
            match endpoint.kind {
                EndpointKind::Required => {
                    prelude.push(quote!(
                        let #endpoint_name = ::task::RequiredInput::new(&__cfw_inputs.#endpoint_name.0);
                    ));
                    input_args.push(quote!(&#endpoint_name));
                }
                _ => {
                    prelude.push(quote!(
                        let mut #endpoint_name = __cfw_inputs.#endpoint_name.input_readonly();
                    ));
                    input_args.push(quote!(&mut #endpoint_name));
                }
            }
        } else {
            match endpoint.kind {
                EndpointKind::Output | EndpointKind::OutputDefault => {
                    let slot = format_ident!("{endpoint_name}_slot");
                    prelude.push(quote! {
                        let mut #slot = ::core::option::Option::Some(
                            __cfw_outputs.#endpoint_name.loan(::core::default::Default::default())?
                        );
                    });
                    output_args.push(quote!(&mut #slot));
                }
                EndpointKind::Uninit => {
                    let slot = format_ident!("{endpoint_name}_slot");
                    prelude.push(quote! {
                        let mut #slot = ::core::option::Option::Some(
                            __cfw_outputs.#endpoint_name.loan_uninit()?
                        );
                    });
                    output_args.push(quote!(&mut #slot));
                }
                EndpointKind::OutputSpan => {
                    // The span is declared before the view, so the view's entries
                    // (which borrow it) drop first on both normal and unwinding exits.
                    prelude.push(quote! {
                        let mut #endpoint_name = __cfw_outputs.#endpoint_name.span();
                    });
                    output_args.push(quote!(&mut #endpoint_name));
                }
                _ => {}
            }
        }
    }
    let call = if fallible {
        quote! {
            if let ::core::result::Result::Err(error) = __cfw_user
                .task
                .pin_mut()
                .run(&mut __cfw_cxx_inputs, &mut __cfw_cxx_outputs, &__cfw_cxx_context)
            {
                ::std::panic!("{} failed: {error}", ::core::stringify!(#name));
            }
        }
    } else {
        quote! {
            __cfw_user
                .task
                .pin_mut()
                .run(&mut __cfw_cxx_inputs, &mut __cfw_cxx_outputs, &__cfw_cxx_context);
        }
    };
    quote! {
        #(#prelude)*
        let mut __cfw_cxx_inputs = #view_inputs::new(#(#input_args),*);
        let mut __cfw_cxx_outputs = #view_outputs::new(#(#output_args),*);
        let __cfw_cxx_context = ::task_cxx::context(__cfw_context);
        #call
        ::core::result::Result::Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand_source(source: &str) -> Result<TokenStream2> {
        expand(syn::parse_str::<CxxTaskSpec>(source)?)
    }

    #[test]
    fn generates_documented_view_conventions() {
        let tokens = expand_source(
            r#"
            bridge: ffi,
            name: Probe,
            cxx_type: ProbeTask,
            factory: make_probe_task,
            subscribers: {
                integer: RequiredInput<u64> { channel: "integer", trigger: true },
                gain: OptionalInput<f64> { channel: "gain", capacity: 4 },
                span_in: InputSpan<u64> { channel: "span_in", capacity: 4 },
            },
            publishers: {
                report: Output<String> { channel: "report" },
                raw: OutputUninit<String> { channel: "raw" },
                numbers: OutputSpan<u64> { channel: "numbers", capacity: 4 },
            },
            "#,
        )
        .unwrap()
        .to_string();
        for expected in [
            "pub unsafe fn integer",
            "pub fn integer_header",
            "pub fn gain_has_value",
            "pub fn gain_len",
            "pub unsafe fn gain_value",
            "pub fn gain_try_value",
            "pub unsafe fn gain_at",
            "pub fn gain_pop",
            "pub fn gain_header",
            "pub unsafe fn report_value",
            "pub fn report_send",
            "pub fn raw_uninit",
            "pub fn numbers_loan",
            "pub fn numbers_write",
            "pub fn numbers_len",
            "pub fn numbers_send",
            "pub fn numbers_send_all",
            "pub fn numbers_discard",
            "ProbenumbersSpanSlot",
        ] {
            assert!(
                tokens.contains(expected),
                "missing `{expected}` in generated tokens:\n{tokens}"
            );
        }
    }

    #[test]
    fn fallible_run_maps_errors_to_panics() {
        let tokens = expand_source(
            r#"
            bridge: ffi,
            name: Probe,
            cxx_type: ProbeTask,
            factory: make_probe_task,
            fallible: true,
            subscribers: {
                integer: RequiredInput<u64> { channel: "integer" },
            },
            publishers: {},
            "#,
        )
        .unwrap()
        .to_string();
        assert!(tokens.contains("panic !"), "{tokens}");
        assert!(tokens.contains("stringify ! (Probe)"), "{tokens}");
    }

    #[test]
    fn rejects_unsupported_endpoints() {
        for source in [
            r#"
            bridge: ffi, name: Probe, cxx_type: T, factory: make,
            subscribers: {},
            publishers: { value: OutputSpan<u64> { channel: "out" } },
            "#,
            r#"
            bridge: ffi, name: Probe, cxx_type: T, factory: make,
            subscribers: {},
            publishers: { value: Iox2Output<u64> { channel: "out" } },
            "#,
            r#"
            bridge: ffi, name: Probe, cxx_type: T, factory: make,
            subscribers: { value: RequiredInput<u64, Forwardable> { channel: "in" } },
            publishers: {},
            "#,
        ] {
            assert!(expand_source(source).is_err(), "{source}");
        }
    }
}
