use proc_macro::TokenStream;
use quote::quote;
use syn::visit_mut::VisitMut;
use syn::{
    FnArg, GenericArgument, ItemImpl, Pat, PathArguments, ReturnType, Type, parse_macro_input,
    parse_quote,
};

/// Derive a lifetime-indexed bundle of named ArenaReaderPtr<Message<T>> fields.
/// There is no field-count limit. Every field uses the struct's single lifetime;
/// nested forwarded payloads may use that lifetime as well.
///
/// Bound endpoints cannot promote source handles to static:
/// ```compile_fail
/// use task::{ArenaReaderPtr, Forwarded, Subscriber, SubscriberKey, message::Message};
/// use task_graph::GraphBindings;
/// use task_macros::ForwardedSources;
/// #[derive(ForwardedSources)]
/// struct Sources<'s> { source: ArenaReaderPtr<'s,Message<u64>> }
/// fn extend<'a>(bindings: &GraphBindings<'a>, key: SubscriberKey<Forwarded<bool,Sources<'static>>>)
///     -> Subscriber<'a,Forwarded<bool,Sources<'static>>> {
///     bindings.bundle::<bool,Sources<'static>>("output").unwrap()
///         .take_subscriber(&task_graph::bundle::subscriber_key::<bool,Sources<'static>>(key)).unwrap()
/// }
/// ```
#[proc_macro_derive(ForwardedSources)]
pub fn forwarded_sources(item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as syn::DeriveInput);
    derive_sources(item)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn derive_sources(item: syn::DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &item.ident;
    if item.generics.params.len() != 1
        || item.generics.lifetimes().count() != 1
        || item.generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(
            &item,
            "source bundles require exactly one lifetime parameter",
        ));
    }
    let lifetime = &item.generics.lifetimes().next().unwrap().lifetime;
    let syn::Data::Struct(data) = &item.data else {
        return Err(syn::Error::new_spanned(
            &item,
            "expected a named source struct",
        ));
    };
    let syn::Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &item,
            "expected named source fields",
        ));
    };
    if fields.named.is_empty() {
        return Err(syn::Error::new_spanned(
            &item,
            "source bundles cannot be empty",
        ));
    }
    let mut names = Vec::new();
    let mut types = Vec::new();
    for field in &fields.named {
        let Type::Path(path) = &field.ty else {
            return Err(syn::Error::new_spanned(
                field,
                "expected ArenaReaderPtr<'storage, Message<T>>",
            ));
        };
        let segment = path.path.segments.last().unwrap();
        let PathArguments::AngleBracketed(args) = &segment.arguments else {
            return Err(syn::Error::new_spanned(
                field,
                "expected ArenaReaderPtr<'storage, Message<T>>",
            ));
        };
        if segment.ident != "ArenaReaderPtr"
            || args.args.len() != 2
            || !matches!(args.args.first(), Some(GenericArgument::Lifetime(l)) if l == lifetime)
        {
            return Err(syn::Error::new_spanned(
                field,
                "expected ArenaReaderPtr using the bundle lifetime",
            ));
        }
        let Some(GenericArgument::Type(Type::Path(message))) = args.args.last() else {
            return Err(syn::Error::new_spanned(field, "expected Message<T>"));
        };
        let message = message.path.segments.last().unwrap();
        let PathArguments::AngleBracketed(args) = &message.arguments else {
            return Err(syn::Error::new_spanned(field, "expected Message<T>"));
        };
        let Some(GenericArgument::Type(payload)) = args.args.first() else {
            return Err(syn::Error::new_spanned(field, "expected Message<T>"));
        };
        if message.ident != "Message" || args.args.len() != 1 {
            return Err(syn::Error::new_spanned(field, "expected Message<T>"));
        }
        names.push(field.ident.as_ref().unwrap());
        types.push(payload);
    }
    let indices: Vec<_> = (0..names.len()).map(syn::Index::from).collect();
    let static_types: Vec<_> = types.iter().map(|ty| canonical_type(ty)).collect();
    let count = names.len();
    Ok(quote! {
        // SAFETY: This bundle consists exclusively of arena handles whose only
        // rebound parameter is their common source-storage lifetime.
        unsafe impl ::task::ForwardedSources for #name<'static> {
            type Bound<'a> = #name<'a>;
            fn fields() -> Vec<(&'static str, ::std::any::TypeId)> {
                vec![#((stringify!(#names), ::std::any::TypeId::of::<#static_types>())),*]
            }
        }
        impl<#lifetime> ::task::forwarded_message::SerializableForward for #name<#lifetime> {
            type Context<'a> = (#(&'a dyn ::task::loggable::MessageLog<#lifetime, #types>,)*) where Self: 'a;
            fn headers(&self) -> Vec<(&'static str, ::task::message::MessageHeader)> {
                vec![#((stringify!(#names), self.#names.header)),*]
            }
            fn resolve<'a>(headers: &[(String, ::task::message::MessageHeader)], context: Self::Context<'a>) -> Result<Self, ::task::loggable::DeserializeError> where Self: 'a {
                if headers.len() != #count { return Err("forwarded source field count mismatch".into()); }
                Ok(Self { #(#names: {
                    if headers[#indices].0 != stringify!(#names) { return Err("forwarded source field name mismatch".into()); }
                    context.#indices.lookup(&headers[#indices].1).ok_or("forwarded source publication missing")?
                }),* })
            }
        }
    })
}

fn canonical_type(ty: &Type) -> Type {
    struct Static;
    impl VisitMut for Static {
        fn visit_lifetime_mut(&mut self, lifetime: &mut syn::Lifetime) {
            *lifetime = parse_quote!('static);
        }
    }
    let mut ty = ty.clone();
    Static.visit_type_mut(&mut ty);
    ty
}

/// Generate declaration-driven registration and a storage-borrowing callback.
/// Register tasks through `ApplicationBuilder` or `TaskRegistration`; channel
/// annotations supply default wiring and registrations support per-instance overrides.
/// Typed binding details stay within the declaring crate. Tests additionally get
/// `declare` and key accessors for checking low-level endpoint behavior.
/// Generated registration requires direct dependencies on `task` and `task_graph`.
#[proc_macro_attribute]
pub fn task_callback(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemImpl);
    expand(item)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[derive(Clone, Copy)]
enum EndpointKind {
    Input,
    Required,
    Output,
    OutputSpan,
    Uninit,
    Publisher,
    IoxInput,
    IoxSpan,
    IoxOutput,
    IoxEvent,
    IoxNotifier,
}

struct Endpoint {
    forwardable: bool,
    forwards: Vec<(syn::Ident, syn::Ident)>,
    name: syn::Ident,
    payload: Type,
    kind: EndpointKind,
    channel: Option<syn::Expr>,
    capacity: syn::Expr,
    trigger: syn::Expr,
    keep: syn::Expr,
}

struct StorageLifetimes;
impl VisitMut for StorageLifetimes {
    fn visit_lifetime_mut(&mut self, lifetime: &mut syn::Lifetime) {
        if lifetime.ident != "static" {
            *lifetime = parse_quote!('storage);
        }
    }
}

mod generate;

struct TaskDefinition {
    item: ItemImpl,
    name: syn::Ident,
    endpoints: Vec<Endpoint>,
    arguments: Vec<Option<usize>>,
    fallible: bool,
}

fn expand(item: ItemImpl) -> syn::Result<proc_macro2::TokenStream> {
    let task = parse_task(item)?;
    validate_endpoints(&task.endpoints)?;
    Ok(generate::task(&task))
}

fn parse_task(mut item: ItemImpl) -> syn::Result<TaskDefinition> {
    if item.trait_.is_some() || !item.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item,
            "task_callback requires a non-generic inherent impl",
        ));
    }
    let Type::Path(self_ty) = item.self_ty.as_ref() else {
        return Err(syn::Error::new_spanned(
            &item.self_ty,
            "expected a task type",
        ));
    };
    let Some(task_name) = self_ty.path.get_ident().cloned() else {
        return Err(syn::Error::new_spanned(
            self_ty,
            "expected a simple task type",
        ));
    };
    let run = item
        .items
        .iter_mut()
        .find_map(|item| match item {
            syn::ImplItem::Fn(method) if method.sig.ident == "run" => Some(method),
            _ => None,
        })
        .ok_or_else(|| syn::Error::new_spanned(&task_name, "expected a run method"))?;
    if run.sig.asyncness.is_some()
        || run.sig.unsafety.is_some()
        || run.sig.generics.type_params().next().is_some()
        || run.sig.generics.const_params().next().is_some()
    {
        return Err(syn::Error::new_spanned(
            &run.sig,
            "run must be synchronous, safe, and have no type or const parameters",
        ));
    }
    if !matches!(run.sig.inputs.first(), Some(FnArg::Receiver(r)) if r.reference.is_some()) {
        return Err(syn::Error::new_spanned(
            &run.sig,
            "run must take &self or &mut self",
        ));
    }

    let mut endpoints = Vec::new();
    let mut arguments = Vec::new();
    for arg in run.sig.inputs.iter_mut().skip(1) {
        let FnArg::Typed(arg) = arg else {
            unreachable!()
        };
        let Pat::Ident(pattern) = arg.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &arg.pat,
                "endpoint argument must have a name",
            ));
        };
        let name = pattern.ident.clone();
        if name.to_string().starts_with("__cfw_") {
            return Err(syn::Error::new_spanned(
                name,
                "endpoint names beginning with __cfw_ are reserved",
            ));
        }
        let (ty, reference) = match arg.ty.as_ref() {
            Type::Reference(r) => (r.elem.as_ref(), Some(r)),
            ty => (ty, None),
        };
        let Type::Path(path) = ty else {
            return Err(syn::Error::new_spanned(
                ty,
                "expected a typed input or output endpoint",
            ));
        };
        let segment = path.path.segments.last().unwrap();
        if segment.ident == "Context" {
            if !matches!(reference, Some(r) if r.mutability.is_none()) {
                return Err(syn::Error::new_spanned(&arg.ty, "context must be &Context"));
            }
            if arg.attrs.iter().any(|a| {
                ["channel", "capacity", "trigger", "keep_across_runs"]
                    .iter()
                    .any(|name| a.path().is_ident(name))
            }) {
                return Err(syn::Error::new_spanned(
                    arg,
                    "context is not a channel endpoint",
                ));
            }
            arguments.push(None);
            continue;
        }
        let kind = match segment.ident.to_string().as_str() {
            "Input" | "OptionalInput" | "InputSpan" if reference.is_none() => EndpointKind::Input,
            "RequiredInput" if reference.is_none() => EndpointKind::Required,
            "Output" if reference.is_none() => EndpointKind::Output,
            "OutputSpan" if reference.is_none() => EndpointKind::OutputSpan,
            "OutputUninit" if reference.is_none() => EndpointKind::Uninit,
            "Iox2OptionalInput" if reference.is_none() => EndpointKind::IoxInput,
            "Iox2SpanInput" if reference.is_none() => EndpointKind::IoxSpan,
            "Iox2Output" if reference.is_none() => EndpointKind::IoxOutput,
            "Iox2Event" if reference.is_none() => EndpointKind::IoxEvent,
            "Iox2NotifyOutput" if reference.is_none() => EndpointKind::IoxNotifier,
            "Publisher" if matches!(reference, Some(r) if r.mutability.is_some()) => {
                EndpointKind::Publisher
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    &arg.ty,
                    "expected Input, RequiredInput, OptionalInput, InputSpan, Output, OutputSpan, OutputUninit, &mut Publisher, or &Context",
                ));
            }
        };
        let mut forwardable = false;
        let mut payload = if matches!(kind, EndpointKind::IoxEvent | EndpointKind::IoxNotifier) {
            if let PathArguments::AngleBracketed(args) = &segment.arguments
                && args
                    .args
                    .iter()
                    .any(|a| !matches!(a, GenericArgument::Lifetime(_)))
            {
                return Err(syn::Error::new_spanned(
                    &arg.ty,
                    "event endpoints take no payload type",
                ));
            }
            parse_quote!(())
        } else {
            let PathArguments::AngleBracketed(generics) = &segment.arguments else {
                return Err(syn::Error::new_spanned(
                    &arg.ty,
                    "endpoint requires one payload type",
                ));
            };
            let payloads: Vec<_> = generics
                .args
                .iter()
                .filter_map(|a| match a {
                    GenericArgument::Type(t) => Some(t),
                    _ => None,
                })
                .collect();
            if payloads.len() == 2 && matches!(kind, EndpointKind::Input | EndpointKind::Required) {
                match payloads[1] {
                    Type::Path(path)
                        if path.path.segments.last().unwrap().ident == "Forwardable" =>
                    {
                        forwardable = true
                    }
                    Type::Path(path) if path.path.segments.last().unwrap().ident == "ReadOnly" => {}
                    _ => {
                        return Err(syn::Error::new_spanned(
                            payloads[1],
                            "expected Forwardable or ReadOnly input capability",
                        ));
                    }
                }
            } else if payloads.len() != 1 {
                return Err(syn::Error::new_spanned(
                    &arg.ty,
                    "endpoint requires one payload type",
                ));
            }
            payloads[0].clone()
        };
        StorageLifetimes.visit_type_mut(&mut payload);
        let mut channel = None;
        let mut capacity = None;
        let mut trigger = None;
        let mut keep = None;
        let mut forwards = Vec::new();
        for attr in &arg.attrs {
            if attr.path().is_ident("forwards") {
                let mappings = attr.parse_args_with(syn::punctuated::Punctuated::<syn::MetaNameValue,syn::Token![,]>::parse_terminated)?;
                for mapping in mappings {
                    let Some(field) = mapping.path.get_ident() else {
                        return Err(syn::Error::new_spanned(
                            mapping,
                            "expected source field name",
                        ));
                    };
                    let syn::Expr::Path(input) = &mapping.value else {
                        return Err(syn::Error::new_spanned(
                            mapping,
                            "expected input argument name",
                        ));
                    };
                    let Some(input) = input.path.get_ident() else {
                        return Err(syn::Error::new_spanned(
                            mapping,
                            "expected input argument name",
                        ));
                    };
                    forwards.push((field.clone(), input.clone()));
                }
                continue;
            }
            let destination = if attr.path().is_ident("channel") {
                &mut channel
            } else if attr.path().is_ident("capacity") {
                &mut capacity
            } else if attr.path().is_ident("trigger") {
                &mut trigger
            } else if attr.path().is_ident("keep_across_runs") {
                &mut keep
            } else {
                continue;
            };
            if destination.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "duplicate endpoint attribute",
                ));
            }
            *destination = Some(attr.parse_args::<syn::Expr>()?);
        }
        if (trigger.is_some() || keep.is_some())
            && !matches!(kind, EndpointKind::Input | EndpointKind::Required)
        {
            return Err(syn::Error::new_spanned(
                &arg.ty,
                "input policies apply to native inputs only",
            ));
        }
        arg.attrs.retain(|a| {
            ![
                "channel",
                "capacity",
                "trigger",
                "keep_across_runs",
                "forwards",
            ]
            .iter()
            .any(|name| a.path().is_ident(name))
        });
        let capacity = capacity.unwrap_or_else(|| {
            if segment.ident == "InputSpan"
                || segment.ident == "Iox2SpanInput"
                || segment.ident == "OutputSpan"
            {
                parse_quote!(4)
            } else {
                parse_quote!(1)
            }
        });
        arguments.push(Some(endpoints.len()));
        endpoints.push(Endpoint {
            forwardable,
            forwards,
            name,
            payload,
            kind,
            channel,
            capacity,
            trigger: trigger.unwrap_or_else(|| parse_quote!(true)),
            keep: keep.unwrap_or_else(|| parse_quote!(true)),
        });
    }
    let fallible = match &run.sig.output {
        ReturnType::Default => false,
        ReturnType::Type(_, ty) if matches!(ty.as_ref(), Type::Tuple(t) if t.elems.is_empty()) => {
            false
        }
        ReturnType::Type(_, ty) => {
            let Type::Path(path) = ty.as_ref() else {
                return Err(syn::Error::new_spanned(
                    ty,
                    "run must return () or Result<(), LoanError>",
                ));
            };
            if path.path.segments.last().unwrap().ident != "Result" {
                return Err(syn::Error::new_spanned(
                    ty,
                    "run must return () or Result<(), LoanError>",
                ));
            }
            true
        }
    };

    Ok(TaskDefinition {
        item,
        name: task_name,
        endpoints,
        arguments,
        fallible,
    })
}

fn validate_endpoints(endpoints: &[Endpoint]) -> syn::Result<()> {
    for endpoint in endpoints {
        if !endpoint.forwards.is_empty()
            && !matches!(
                endpoint.kind,
                EndpointKind::Output
                    | EndpointKind::Uninit
                    | EndpointKind::Publisher
                    | EndpointKind::OutputSpan
            )
        {
            return Err(syn::Error::new_spanned(
                &endpoint.name,
                "forwarding mappings belong to native outputs",
            ));
        }
        if !endpoint.forwards.is_empty()
            && !matches!(&endpoint.payload,Type::Path(path) if path.path.segments.last().is_some_and(|s| s.ident == "Forwarded"))
        {
            return Err(syn::Error::new_spanned(
                &endpoint.name,
                "forwarding mappings require a Forwarded output",
            ));
        }
        for (_, input) in &endpoint.forwards {
            if !endpoints.iter().any(|p| p.name == *input && p.forwardable) {
                return Err(syn::Error::new_spanned(
                    input,
                    "forwarding source must name a Forwardable input",
                ));
            }
        }
    }
    Ok(())
}

fn generate_registration(
    task: &syn::Ident,
    declaration: &syn::Ident,
    endpoints: &[Endpoint],
) -> proc_macro2::TokenStream {
    fn forwarded(payload: &Type) -> Option<(Type, Type)> {
        let Type::Path(path) = payload else {
            return None;
        };
        let segment = path.path.segments.last()?;
        if segment.ident != "Forwarded" {
            return None;
        }
        let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
            return None;
        };
        let types: Vec<_> = args
            .args
            .iter()
            .filter_map(|arg| {
                if let syn::GenericArgument::Type(ty) = arg {
                    Some(ty.clone())
                } else {
                    None
                }
            })
            .collect();
        (types.len() == 2).then(|| {
            let source = canonical_type(&types[1]);
            (types[0].clone(), source)
        })
    }
    struct Borrowed(bool);
    impl VisitMut for Borrowed {
        fn visit_lifetime_mut(&mut self, lifetime: &mut syn::Lifetime) {
            self.0 |= lifetime.ident != "static";
        }
    }
    for endpoint in endpoints {
        let mut endpoint_borrowed = Borrowed(false);
        endpoint_borrowed.visit_type_mut(&mut endpoint.payload.clone());
        if endpoint_borrowed.0 && forwarded(&endpoint.payload).is_none() {
            return quote!();
        }
    }
    let mut channel_names = Vec::new();
    let mut keys = Vec::new();
    let mut bindings = Vec::new();
    let mut schema = Vec::new();
    let mut captures = Vec::new();
    let mut rebuilt_keys = Vec::new();
    let mut mappings = Vec::new();
    for (index, endpoint) in endpoints.iter().enumerate() {
        let Endpoint {
            name,
            payload,
            capacity,
            trigger,
            keep,
            ..
        } = endpoint;
        let channel = endpoint
            .channel
            .as_ref()
            .map(|c| quote!(#c))
            .unwrap_or_else(|| quote!(stringify!(#name)));
        let direction = if matches!(
            endpoint.kind,
            EndpointKind::Input
                | EndpointKind::Required
                | EndpointKind::IoxInput
                | EndpointKind::IoxSpan
                | EndpointKind::IoxEvent
        ) {
            quote!(Input)
        } else {
            quote!(Output)
        };
        schema.push(quote!((stringify!(#name), ::task_graph::EndpointDirection::#direction)));
        channel_names.push(quote!(__cfw_overrides.resolve(stringify!(#name), ::task_graph::EndpointDirection::#direction, || {
            let value = #channel;
            ::core::convert::AsRef::<str>::as_ref(&value).to_owned()
        })?));
        let family = forwarded(payload);
        let (key, binding) = if let Some((message, source)) = &family {
            if !matches!(endpoint.kind, EndpointKind::Input | EndpointKind::Required) {
                let fields = endpoint.forwards.iter().map(|(field,input)| {
                    let (idx, input_endpoint) = endpoints.iter().enumerate().find(|(_,p)| p.name == *input).unwrap();
                    let ty = canonical_type(&input_endpoint.payload);
                    quote!((stringify!(#field),stringify!(#input),__cfw_names[#idx].as_str(),::std::any::TypeId::of::<#ty>()))
                });
                mappings.push(quote!(__cfw_plan.map_forwarding::<#source>(stringify!(#name),&__cfw_names[#index],&[#(#fields),*])?;));
            }
            match endpoint.kind {
                EndpointKind::Input | EndpointKind::Required => (
                    quote!(__cfw_plan.bundle_subscriber::<#message,#source>(&__cfw_names[#index], #capacity, ::task::SubscriberPolicy { trigger: #trigger, keep_across_runs: #keep })),
                    quote!(__cfw_bindings.bundle::<#message,#source>(&__cfw_names[#index])?),
                ),
                EndpointKind::IoxInput
                | EndpointKind::IoxSpan
                | EndpointKind::IoxOutput
                | EndpointKind::IoxEvent
                | EndpointKind::IoxNotifier => return quote!(),
                _ => (
                    quote!(__cfw_plan.bundle_publisher::<#message,#source>(&__cfw_names[#index], #capacity)),
                    quote!(__cfw_bindings.bundle::<#message,#source>(&__cfw_names[#index])?),
                ),
            }
        } else {
            match endpoint.kind {
                EndpointKind::IoxInput | EndpointKind::IoxSpan => (
                    quote!(__cfw_plan.ipc_subscriber::<#payload>(&__cfw_names[#index], #capacity)),
                    quote!(__cfw_bindings.ipc::<#payload>(&__cfw_names[#index])?),
                ),
                EndpointKind::IoxOutput => (
                    quote!(__cfw_plan.ipc_publisher::<#payload>(&__cfw_names[#index], #capacity)),
                    quote!(__cfw_bindings.ipc::<#payload>(&__cfw_names[#index])?),
                ),
                EndpointKind::IoxEvent => (
                    quote!(__cfw_plan.event(&__cfw_names[#index], #capacity)),
                    quote!(__cfw_bindings.events(&__cfw_names[#index])?),
                ),
                EndpointKind::IoxNotifier => (
                    quote!(__cfw_plan.notifier(&__cfw_names[#index])),
                    quote!(__cfw_bindings.events(&__cfw_names[#index])?),
                ),
                EndpointKind::Input | EndpointKind::Required => (
                    quote!(__cfw_plan.subscriber::<#payload>(&__cfw_names[#index], #capacity, ::task::SubscriberPolicy { trigger: #trigger, keep_across_runs: #keep })),
                    quote!(__cfw_bindings.native::<#payload>(&__cfw_names[#index])?),
                ),
                _ => (
                    quote!(__cfw_plan.publisher::<#payload>(&__cfw_names[#index], #capacity)),
                    quote!(__cfw_bindings.native::<#payload>(&__cfw_names[#index])?),
                ),
            }
        };
        if let Some((message, source)) = &family {
            let retype = if matches!(endpoint.kind, EndpointKind::Input | EndpointKind::Required) {
                quote!(subscriber_key)
            } else {
                quote!(publisher_key)
            };
            rebuilt_keys.push(
                quote!(::task_graph::bundle::#retype::<#message,#source>(__cfw_declaration.#name)),
            );
        } else {
            rebuilt_keys.push(quote!(__cfw_declaration.#name));
        }
        let forwardable = endpoint.forwardable;
        keys.push(quote!({
            let key = #key.map_err(|error| ::task_graph::BuildError(format!("endpoint '{}': {}", stringify!(#name), error)))?;
            __cfw_plan.annotate_last_endpoint(stringify!(#name), #forwardable);
            key
        }));
        bindings.push(binding);
        let capture_method = match endpoint.kind {
            EndpointKind::IoxEvent | EndpointKind::IoxNotifier => None,
            EndpointKind::IoxInput | EndpointKind::IoxSpan | EndpointKind::IoxOutput => {
                Some(quote!(register_ipc))
            }
            _ => Some(quote!(register_native)),
        };
        if let Some((message, source)) = family {
            captures.push(quote!(::task_graph::bundle::CaptureProbe::<#message,#source>::default().register(__cfw_plan, &__cfw_names[#index])?;));
        } else if let Some(method) = capture_method {
            captures.push(quote!(::task_graph::capture::CaptureProbe::<#payload>::default().#method(__cfw_plan, &__cfw_names[#index])?;));
            captures.push(quote!(::task_graph::replay::DecodeProbe::<#payload>::default().#method(__cfw_plan, &__cfw_names[#index])?;));
        }
    }
    quote! {
        impl ::task_graph::Task for #task {
            fn register(self: Box<Self>, __cfw_plan: &mut ::task_graph::GraphPlan, __cfw_overrides: &::task_graph::ChannelOverrides) -> Result<::task_graph::TaskBinding, ::task_graph::BuildError> {
                __cfw_overrides.validate(&[#(#schema),*])?;
                let __cfw_names: Vec<String> = vec![#(#channel_names),*];
                let __cfw_declaration = #declaration::from_keys(#(#keys),*);
                use ::task_graph::capture::MaybeCapture as _;
                use ::task_graph::replay::MaybeDecode as _;
                use ::task_graph::bundle::MaybeCapture as _;
                #(#captures)*
                #(#mappings)*
                Ok(Box::new(move |__cfw_bindings| {
                    let declaration = #declaration::from_keys(#(#rebuilt_keys),*);
                    Ok(Box::new((*self).bind(declaration, #(#bindings),*)?))
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsupported_endpoints_and_duplicate_attributes() {
        for source in [
            "impl Task { fn run(&mut self, input: Unsupported<u64>) {} }",
            "impl Task { fn run(&mut self, #[capacity(1)] #[capacity(2)] input: Input<u64>) {} }",
            "impl Task { fn run(&mut self, input: Input<u64, u32>) {} }",
            "impl Task { fn run(&mut self, #[trigger(false)] output: Output<u64>) {} }",
            "impl Task { fn run(&mut self, #[keep_across_runs(false)] context: &Context) {} }",
            "impl Task { fn run(&self, input: Input<u64>, #[forwards(source = input)] output: Output<u64>) {} }",
            "impl Task { fn run(&self, input: Input<u64,Forwardable>, #[forwards(source = missing)] output: Output<u64>) {} }",
            "impl Task { fn run(&self, input: Input<u64,Forwardable>, #[forwards(source = input)] output: Output<u64>) {} }",
        ] {
            assert!(expand(syn::parse_str(source).unwrap()).is_err());
        }
    }

    #[test]
    fn rejects_invalid_source_bundles() {
        for source in [
            "struct Sources<'a> {}",
            "struct Sources<'a>(ArenaReaderPtr<'a,Message<u64>>);",
            "struct Sources<'a> { raw: &'a u64 }",
            "struct Sources<'a> { wrong: ArenaReaderPtr<'static,Message<u64>> }",
            "struct Sources<'a> { wrong: ArenaReaderPtr<'a,u64> }",
            "struct Sources<'a,T> { generic: ArenaReaderPtr<'a,Message<T>> }",
        ] {
            assert!(
                derive_sources(syn::parse_str(source).unwrap()).is_err(),
                "{source}"
            );
        }
    }
}
