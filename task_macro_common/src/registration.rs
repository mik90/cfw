//! `task_graph::Task` implementation generation.

use quote::quote;
use syn::visit_mut::VisitMut;

use crate::endpoint::{Endpoint, EndpointKind, ForwardedFamily};
use crate::forwarding::{Borrowed, canonical_type, forwarded_family};

pub fn generate_registration(
    self_ty: &proc_macro2::TokenStream,
    declaration: &syn::Ident,
    endpoints: &[Endpoint],
) -> proc_macro2::TokenStream {
    for endpoint in endpoints {
        let mut endpoint_borrowed = Borrowed(false);
        endpoint_borrowed.visit_type_mut(&mut endpoint.payload.clone());
        if endpoint_borrowed.0
            && endpoint.forwarded.is_none()
            && forwarded_family(&endpoint.payload).is_none()
        {
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
            ::task_graph::ChannelName::new(::core::convert::AsRef::<str>::as_ref(&value))
        })?));
        let family = endpoint
            .forwarded
            .clone()
            .or_else(|| forwarded_family(payload));
        let (key, binding) = if let Some(ForwardedFamily { message, sources }) = &family {
            if !matches!(endpoint.kind, EndpointKind::Input | EndpointKind::Required) {
                let fields = endpoint.forwards.iter().map(|(field,input)| {
                    let (idx, input_endpoint) = endpoints.iter().enumerate().find(|(_,p)| p.name == *input).unwrap();
                    let ty = canonical_type(&input_endpoint.payload);
                    quote!((stringify!(#field),stringify!(#input),__cfw_names[#idx].as_str(),::std::any::TypeId::of::<#ty>()))
                });
                mappings.push(quote!(__cfw_plan.map_forwarding::<#sources>(stringify!(#name),&__cfw_names[#index],&[#(#fields),*])?;));
            }
            match endpoint.kind {
                EndpointKind::Input | EndpointKind::Required => (
                    quote!(__cfw_plan.bundle_subscriber::<#message,#sources>(&__cfw_names[#index], #capacity, ::task::SubscriberPolicy { trigger: #trigger, keep_across_runs: #keep })),
                    quote!(__cfw_bindings.bundle::<#message,#sources>(&__cfw_names[#index])?),
                ),
                EndpointKind::IoxInput
                | EndpointKind::IoxSpan
                | EndpointKind::IoxOutput
                | EndpointKind::IoxEvent
                | EndpointKind::IoxNotifier => return quote!(),
                _ => (
                    quote!(__cfw_plan.bundle_publisher::<#message,#sources>(&__cfw_names[#index], #capacity)),
                    quote!(__cfw_bindings.bundle::<#message,#sources>(&__cfw_names[#index])?),
                ),
            }
        } else {
            match endpoint.kind {
                EndpointKind::IoxInput | EndpointKind::IoxSpan => (
                    quote!(__cfw_plan.ipc_subscriber_with_keep::<#payload>(&__cfw_names[#index], #capacity, #keep)),
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
        if let Some(ForwardedFamily { message, sources }) = &family {
            let retype = if matches!(endpoint.kind, EndpointKind::Input | EndpointKind::Required) {
                quote!(subscriber_key)
            } else {
                quote!(publisher_key)
            };
            rebuilt_keys.push(
                quote!(::task_graph::bundle::#retype::<#message,#sources>(__cfw_declaration.#name)),
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
                Some(quote!(register_iox2))
            }
            _ => Some(quote!(register_native)),
        };
        if let Some(ForwardedFamily { message, sources }) = family {
            captures.push(quote!(::task_graph::bundle::CaptureProbe::<#message,#sources>::default().register(__cfw_plan, &__cfw_names[#index])?;));
        } else if let Some(method) = capture_method {
            captures.push(quote!(::task_graph::capture::CaptureProbe::<#payload>::default().#method(__cfw_plan, &__cfw_names[#index])?;));
            captures.push(quote!(::task_graph::replay::DecodeProbe::<#payload>::default().#method(__cfw_plan, &__cfw_names[#index])?;));
        }
    }
    quote! {
        impl ::task_graph::Task for #self_ty {
            fn register(self: Box<Self>, __cfw_plan: &mut ::task_graph::GraphPlan, __cfw_overrides: &::task_graph::ChannelOverrides) -> Result<::task_graph::TaskBinding, ::task_graph::BuildError> {
                __cfw_overrides.validate(&[#(#schema),*])?;
                let __cfw_names: Vec<::task_graph::ChannelName> = vec![#(#channel_names),*];
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
