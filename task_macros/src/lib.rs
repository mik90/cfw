// TODO(port-callback-surface): Audit remaining forwarding/span-output conveniences;
// migrate test_tasks and remaining examples to declarations and factories.
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::visit_mut::VisitMut;
use syn::{
    FnArg, GenericArgument, ItemImpl, Pat, PathArguments, ReturnType, Type, parse_macro_input,
    parse_quote,
};

/// Generate typed endpoint declarations and storage-borrowing callback construction.
/// `Task::declare(plans...)` creates keys before allocation; `task.bind(
/// declaration, bindings...)` creates the callback afterward. Use `Declaration::from_keys`
/// when several ports share a channel plan. The executor manages update/flush.
#[proc_macro_attribute]
pub fn task_callback(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemImpl);
    expand(item)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[derive(Clone, Copy)]
enum PortKind {
    Input,
    Required,
    Output,
    Uninit,
    Publisher,
    IoxInput,
    IoxSpan,
    IoxOutput,
    IoxEvent,
    IoxNotifier,
}

struct Port {
    name: syn::Ident,
    payload: Type,
    kind: PortKind,
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

fn expand(mut item: ItemImpl) -> syn::Result<proc_macro2::TokenStream> {
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
    let declaration = format_ident!("{}Declaration", task_name);
    let callback = format_ident!("{}Callback", task_name);
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

    let mut ports = Vec::new();
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
                "expected a typed input or output port",
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
            arguments.push(quote!(__cfw_context));
            continue;
        }
        let kind = match segment.ident.to_string().as_str() {
            "Input" | "OptionalInput" | "InputSpan" if reference.is_none() => PortKind::Input,
            "RequiredInput" if reference.is_none() => PortKind::Required,
            "Output" if reference.is_none() => PortKind::Output,
            "OutputUninit" if reference.is_none() => PortKind::Uninit,
            "Iox2OptionalInput" if reference.is_none() => PortKind::IoxInput,
            "Iox2SpanInput" if reference.is_none() => PortKind::IoxSpan,
            "Iox2Output" if reference.is_none() => PortKind::IoxOutput,
            "Iox2Event" if reference.is_none() => PortKind::IoxEvent,
            "Iox2NotifyOutput" if reference.is_none() => PortKind::IoxNotifier,
            "Publisher" if matches!(reference, Some(r) if r.mutability.is_some()) => {
                PortKind::Publisher
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    &arg.ty,
                    "expected Input, RequiredInput, OptionalInput, InputSpan, Output, OutputUninit, &mut Publisher, or &Context",
                ));
            }
        };
        let mut payload = if matches!(kind, PortKind::IoxEvent | PortKind::IoxNotifier) {
            if let PathArguments::AngleBracketed(args) = &segment.arguments
                && args
                    .args
                    .iter()
                    .any(|a| !matches!(a, GenericArgument::Lifetime(_)))
            {
                return Err(syn::Error::new_spanned(
                    &arg.ty,
                    "event ports take no payload type",
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
            if payloads.len() != 1 {
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
        for attr in &arg.attrs {
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
            && !matches!(kind, PortKind::Input | PortKind::Required)
        {
            return Err(syn::Error::new_spanned(
                &arg.ty,
                "input policies apply to native inputs only",
            ));
        }
        arg.attrs.retain(|a| {
            !["channel", "capacity", "trigger", "keep_across_runs"]
                .iter()
                .any(|name| a.path().is_ident(name))
        });
        let capacity = capacity.unwrap_or_else(|| {
            if segment.ident == "InputSpan" || segment.ident == "Iox2SpanInput" {
                parse_quote!(4)
            } else {
                parse_quote!(1)
            }
        });
        arguments.push(match kind {
            PortKind::Input => quote!(self.#name.input()),
            PortKind::Required => quote!(::task::RequiredInput::new(&self.#name)),
            PortKind::Output => quote!(self.#name.loan(::core::default::Default::default())?),
            PortKind::Uninit => quote!(self.#name.loan_uninit()?),
            PortKind::Publisher => quote!(&mut self.#name),
            PortKind::IoxInput => quote!(::task::iox2::Iox2OptionalInput::new(&self.#name)),
            PortKind::IoxSpan => quote!(::task::iox2::Iox2SpanInput::new(&self.#name)),
            PortKind::IoxEvent => quote!(::task::iox2::Iox2Event::new(&self.#name)),
            PortKind::IoxOutput => quote!(::task::iox2::Iox2Output::new_default(&mut self.#name)?),
            PortKind::IoxNotifier => quote!(::task::iox2::Iox2NotifyOutput::new(&mut self.#name)),
        });
        ports.push(Port {
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

    let automatic = automatic_registration(&task_name, &declaration, &ports);
    let mut key_fields = Vec::new();
    let mut key_params = Vec::new();
    let mut plan_params = Vec::new();
    let mut validation = Vec::new();
    let mut registration = Vec::new();
    let mut binding_params = Vec::new();
    let mut construction = Vec::new();
    let mut endpoint_fields = Vec::new();
    let mut updates = Vec::new();
    let mut finish_inputs = Vec::new();
    let mut wake = Vec::new();
    let mut pending = Vec::new();
    let mut channel_names = Vec::new();
    let mut ready = Vec::new();
    let mut available = Vec::new();
    let mut flush = Vec::new();
    let mut discard = Vec::new();
    let mut names = Vec::new();
    let mut events = Vec::new();
    for port in ports {
        let Port {
            name,
            payload,
            kind,
            channel,
            capacity,
            trigger,
            keep,
        } = port;
        let input = matches!(kind, PortKind::Input | PortKind::Required);
        channel_names.push(quote!(visit(self.#name.channel_name());));
        let (key, plan, bindings) = match kind {
            PortKind::IoxInput | PortKind::IoxSpan => (
                quote!(::task::iox2::Iox2SubscriberKey<#payload>),
                quote!(&mut ::task::iox2::Iox2ChannelPlan<#payload>),
                quote!(&::task::iox2::Iox2Bindings<#payload>),
            ),
            PortKind::IoxOutput => (
                quote!(::task::iox2::Iox2PublisherKey<#payload>),
                quote!(&mut ::task::iox2::Iox2ChannelPlan<#payload>),
                quote!(&::task::iox2::Iox2Bindings<#payload>),
            ),
            PortKind::IoxEvent => (
                quote!(::task::iox2::Iox2EventKey),
                quote!(&mut dyn ::task::iox2::Iox2EventPlan),
                quote!(&dyn ::task::iox2::Iox2EventBindings),
            ),
            PortKind::IoxNotifier => (
                quote!(::task::iox2::Iox2NotifierKey),
                quote!(&mut dyn ::task::iox2::Iox2EventPlan),
                quote!(&dyn ::task::iox2::Iox2EventBindings),
            ),
            _ => (
                if input {
                    quote!(::task::SubscriberKey<#payload>)
                } else {
                    quote!(::task::PublisherKey<#payload>)
                },
                quote!(&mut ::task::ChannelPlan<#payload>),
                quote!(&::task::EndpointBindings<'storage, #payload>),
            ),
        };
        key_fields.push(quote!(#name: #key));
        key_params.push(quote!(#name: #key));
        plan_params.push(quote!(#name: #plan));
        binding_params.push(quote!(#name: #bindings));
        if let Some(channel) = channel {
            validation.push(quote! {
                {
                    let expected_value = #channel;
                    let expected: &str = ::core::convert::AsRef::<str>::as_ref(&expected_value);
                    if #name.name() != expected {
                        return Err(::task::DeclarationError { field: stringify!(#name), expected: expected.into(), actual: #name.name().into() });
                    }
                }
            });
        }
        if matches!(kind, PortKind::IoxEvent) {
            registration.push(quote!(#name: #name.events(#capacity)));
            construction.push(quote!(#name: #name.take_event(&self.#name)?));
            endpoint_fields.push(quote!(#name: ::task::iox2::Iox2EventSubscriber));
            updates.push(quote!(self.#name.update();));
            wake.push(quote!(self.#name.set_waker(wake.clone());));
            pending.push(quote!(self.#name.has_pending()));
            events.push(quote!(if let Some(event) = self.#name.take_registration() { registrations.push(event); }));
        } else if matches!(kind, PortKind::IoxNotifier) {
            registration.push(quote!(#name: #name.notifier()));
            construction.push(quote!(#name: #name.take_notifier(&self.#name)?));
            endpoint_fields.push(quote!(#name: ::task::iox2::Iox2Notifier));
            flush.push(quote!(self.#name.flush(timestamp);));
            discard.push(quote!(self.#name.discard_pending();));
        } else if matches!(kind, PortKind::IoxInput | PortKind::IoxSpan) {
            registration.push(quote!(#name: #name.subscriber(#capacity)));
            construction.push(quote!(#name: #name.take_subscriber(&self.#name)?));
            endpoint_fields.push(quote!(#name: ::task::iox2::Iox2Subscriber<#payload>));
            updates.push(quote!(self.#name.update();));
        } else if matches!(kind, PortKind::IoxOutput) {
            registration.push(quote!(#name: #name.publisher(#capacity)));
            construction.push(quote!(#name: #name.take_publisher(&self.#name)?));
            endpoint_fields.push(quote!(#name: ::task::iox2::Iox2Publisher<#payload>));
            flush.push(quote!(self.#name.flush(timestamp);));
            discard.push(quote!(self.#name.discard_pending();));
        } else if input {
            registration.push(quote!(#name: #name.subscriber_with_policy(#capacity, ::task::SubscriberPolicy { trigger: #trigger, keep_across_runs: #keep })));
            construction.push(quote!(#name: #name.take_subscriber(&self.#name)?));
            endpoint_fields.push(quote!(#name: ::task::Subscriber<'storage, #payload>));
            updates.push(quote!(self.#name.update();));
            finish_inputs.push(quote!(self.#name.finish_iteration();));
            wake.push(quote!(self.#name.set_waker(wake.clone());));
            pending.push(quote!(self.#name.requests_execution()));
            if matches!(kind, PortKind::Required) {
                available.push(quote!(self.#name.has_pending()));
                ready.push(quote!(!self.#name.is_empty()));
            }
        } else {
            registration.push(quote!(#name: #name.publisher(#capacity)));
            construction.push(quote!(#name: #name.take_publisher(&self.#name)?));
            endpoint_fields.push(quote!(#name: ::task::Publisher<'storage, #payload>));
            flush.push(quote!(self.#name.flush(timestamp);));
            discard.push(quote!(self.#name.discard_pending();));
        }
        names.push(name);
    }
    let result = if fallible {
        quote!(self.__cfw_user.run(#(#arguments),*))
    } else {
        quote! { self.__cfw_user.run(#(#arguments),*); Ok(()) }
    };
    let event_method = if events.is_empty() {
        quote!()
    } else {
        quote! {
            fn take_iox2_events(&mut self) -> Vec<::task::iox2::Iox2EventRegistration> {
                let mut registrations = Vec::new(); #(#events)* registrations
            }
        }
    };
    Ok(quote! {
        #item
        #automatic

        pub struct #declaration<'storage> {
            #(#key_fields,)*
            __cfw_lifetime: ::core::marker::PhantomData<&'storage ()>,
        }
        pub struct #callback<'storage> {
            __cfw_user: #task_name,
            #(#endpoint_fields,)*
            __cfw_lifetime: ::core::marker::PhantomData<&'storage ()>,
        }
        impl #task_name {
            pub fn declare<'storage>(#(#plan_params),*) -> Result<#declaration<'storage>, ::task::DeclarationError> {
                #(#validation)*
                Ok(#declaration { #(#registration,)* __cfw_lifetime: ::core::marker::PhantomData })
            }
            pub fn bind<'storage>(self, __cfw_declaration: #declaration<'storage>, #(#binding_params),*) -> Result<#callback<'storage>, ::task::EndpointError> {
                __cfw_declaration.build(self, #(#names),*)
            }
        }
        impl<'storage> #declaration<'storage> {
            pub fn from_keys(#(#key_params),*) -> Self {
                Self { #(#names,)* __cfw_lifetime: ::core::marker::PhantomData }
            }
            fn build(self, __cfw_user: #task_name, #(#binding_params),*) -> Result<#callback<'storage>, ::task::EndpointError> {
                Ok(#callback { __cfw_user, #(#construction,)* __cfw_lifetime: ::core::marker::PhantomData })
            }
        }
        impl<'storage> ::task::Callback for #callback<'storage> {
            #event_method
            fn set_waker(&mut self, wake: ::task::wake::WakeHandle) { #(#wake)* }
            fn has_pending_inputs(&self) -> bool { false #(|| #pending)* }
            fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) { #(#channel_names)* }
            fn update_inputs(&mut self) { #(#updates)* }
            fn finish_inputs(&mut self) { #(#finish_inputs)* }
            fn required_inputs_ready(&self) -> bool { true #(&& #ready)* }
            fn required_inputs_available(&self) -> bool { true #(&& #available)* }
            fn run(&mut self, __cfw_context: &::task::Context) -> Result<(), ::task::LoanError> { #result }
            fn flush_outputs(&mut self, timestamp: ::task::time::FrameworkTime) { #(#flush)* }
            fn discard_outputs(&mut self) { #(#discard)* }
        }
    })
}

fn automatic_registration(
    task: &syn::Ident,
    declaration: &syn::Ident,
    ports: &[Port],
) -> proc_macro2::TokenStream {
    struct Borrowed(bool);
    impl VisitMut for Borrowed {
        fn visit_lifetime_mut(&mut self, lifetime: &mut syn::Lifetime) {
            self.0 |= lifetime.ident != "static";
        }
    }
    let mut borrowed = Borrowed(false);
    for port in ports {
        borrowed.visit_type_mut(&mut port.payload.clone());
    }
    if borrowed.0 {
        return quote!();
    }
    let factory = format_ident!("{}AutomaticFactory", task);
    let mut channel_names = Vec::new();
    let mut keys = Vec::new();
    let mut bindings = Vec::new();
    for (index, port) in ports.iter().enumerate() {
        let Port {
            name,
            payload,
            capacity,
            trigger,
            keep,
            ..
        } = port;
        let channel = port
            .channel
            .as_ref()
            .map(|c| quote!(#c))
            .unwrap_or_else(|| quote!(stringify!(#name)));
        channel_names.push(quote!({ let value = #channel; ::core::convert::AsRef::<str>::as_ref(&value).to_owned() }));
        let (key, binding) = match port.kind {
            PortKind::IoxInput | PortKind::IoxSpan => (
                quote!(__cfw_plan.ipc_subscriber::<#payload>(&__cfw_names[#index], #capacity)?),
                quote!(__cfw_bindings.ipc::<#payload>(&self.names[#index])?),
            ),
            PortKind::IoxOutput => (
                quote!(__cfw_plan.ipc_publisher::<#payload>(&__cfw_names[#index], #capacity)?),
                quote!(__cfw_bindings.ipc::<#payload>(&self.names[#index])?),
            ),
            PortKind::IoxEvent => (
                quote!(__cfw_plan.event(&__cfw_names[#index], #capacity)?),
                quote!(__cfw_bindings.events(&self.names[#index])?),
            ),
            PortKind::IoxNotifier => (
                quote!(__cfw_plan.notifier(&__cfw_names[#index])?),
                quote!(__cfw_bindings.events(&self.names[#index])?),
            ),
            PortKind::Input | PortKind::Required => (
                quote!(__cfw_plan.subscriber::<#payload>(&__cfw_names[#index], #capacity, ::task::SubscriberPolicy { trigger: #trigger, keep_across_runs: #keep })?),
                quote!(__cfw_bindings.native::<#payload>(&self.names[#index])?),
            ),
            _ => (
                quote!(__cfw_plan.publisher::<#payload>(&__cfw_names[#index], #capacity)?),
                quote!(__cfw_bindings.native::<#payload>(&self.names[#index])?),
            ),
        };
        keys.push(key);
        bindings.push(binding);
    }
    quote! {
        struct #factory { user: #task, declaration: #declaration<'static>, names: Vec<String> }
        impl ::task::automatic::Task for #task {
            fn register(self: Box<Self>, __cfw_plan: &mut ::task::automatic::NamedPlan) -> Result<Box<dyn ::task::automatic::TaskFactory>, ::task::automatic::BuildError> {
                let __cfw_names: Vec<String> = vec![#(#channel_names),*];
                let declaration = #declaration::from_keys(#(#keys),*);
                Ok(Box::new(#factory { user: *self, declaration, names: __cfw_names }))
            }
        }
        impl ::task::automatic::TaskFactory for #factory {
            fn build<'storage>(self: Box<Self>, __cfw_bindings: &::task::automatic::NamedBindings<'storage>) -> Result<Box<dyn ::task::Callback + 'storage>, ::task::automatic::BuildError> {
                Ok(Box::new(self.user.bind(self.declaration, #(#bindings),*)?))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsupported_ports_and_duplicate_attributes() {
        for source in [
            "impl Task { fn run(&mut self, input: Unsupported<u64>) {} }",
            "impl Task { fn run(&mut self, #[capacity(1)] #[capacity(2)] input: Input<u64>) {} }",
            "impl Task { fn run(&mut self, input: Input<u64, u32>) {} }",
            "impl Task { fn run(&mut self, #[trigger(false)] output: Output<u64>) {} }",
            "impl Task { fn run(&mut self, #[keep_across_runs(false)] context: &Context) {} }",
        ] {
            assert!(expand(syn::parse_str(source).unwrap()).is_err());
        }
    }
}
