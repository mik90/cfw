//! Typed declarations, endpoint collections, and method invocation adapters.
use super::{Endpoint, EndpointKind, TaskDefinition, generate_registration};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

pub(super) fn task(task: &TaskDefinition) -> TokenStream {
    let item = &task.item;
    let declaration_name = format_ident!("{}Declaration", task.name);
    let inputs_name = format_ident!("{}Inputs", task.name);
    let outputs_name = format_ident!("{}Outputs", task.name);
    let declaration = declaration(task, &declaration_name);
    let registration = generate_registration(&task.name, &declaration_name, &task.endpoints);
    let inputs = collection(&task.endpoints, &inputs_name, true);
    let outputs = collection(&task.endpoints, &outputs_name, false);
    let binding = binding(task, &declaration_name, &inputs_name, &outputs_name);
    quote! { #item #declaration #registration #inputs #outputs #binding }
}

fn arity(count: usize) -> TokenStream {
    if count > 7 {
        quote!(#[expect(clippy::too_many_arguments, reason = "one typed argument per declared endpoint")])
    } else {
        quote!()
    }
}

fn declaration(task: &TaskDefinition, declaration: &syn::Ident) -> TokenStream {
    let task_name = &task.name;
    let fields: Vec<_> = task
        .endpoints
        .iter()
        .map(|p| {
            let name = &p.name;
            let key = p.key_type();
            quote!(#name: #key)
        })
        .collect();
    let accessors = task.endpoints.iter().map(|p| {
        let name = &p.name;
        let accessor = format_ident!("{}_key", name);
        let key = p.key_type();
        quote!(#[cfg(test)] pub(crate) fn #accessor(&self) -> &#key { &self.#name })
    });
    let names = task.endpoints.iter().map(|p| &p.name);
    let params = task.endpoints.iter().map(|p| {
        let name = &p.name;
        let plan = p.plan_type();
        quote!(#name: #plan)
    });
    let validation = task.endpoints.iter().filter_map(|p| {
        let channel = p.channel.as_ref()?;
        let name = &p.name;
        Some(quote! {
            {
                let expected_value = #channel;
                let expected: &str = ::core::convert::AsRef::<str>::as_ref(&expected_value);
                if #name.name() != expected {
                    return Err(::task::DeclarationError { field: stringify!(#name), expected: expected.into(), actual: #name.name().into() });
                }
            }
        })
    });
    let endpoints = task.endpoints.iter().map(Endpoint::declare);
    let arity = arity(task.endpoints.len());
    quote! {
        pub(crate) struct #declaration<'storage> {
            #(#fields,)*
            __cfw_lifetime: ::core::marker::PhantomData<&'storage ()>,
        }
        #[allow(dead_code)]
        impl<'storage> #declaration<'storage> {
            #(#accessors)*
            #arity
            pub(crate) fn from_keys(#(#fields),*) -> Self {
                Self { #(#names,)* __cfw_lifetime: ::core::marker::PhantomData }
            }
        }
        impl #task_name {
            #[cfg(test)]
            #[allow(dead_code)]
            #arity
            pub(crate) fn declare<'storage>(#(#params),*) -> Result<#declaration<'storage>, ::task::DeclarationError> {
                #(#validation)*
                Ok(#declaration { #(#endpoints,)* __cfw_lifetime: ::core::marker::PhantomData })
            }
        }
    }
}

fn collection(endpoints: &[Endpoint], name: &syn::Ident, input: bool) -> TokenStream {
    let endpoints: Vec<_> = endpoints.iter().filter(|p| p.is_input() == input).collect();
    let fields = endpoints.iter().map(|p| {
        let name = &p.name;
        let endpoint = p.endpoint_type();
        quote!(#name: #endpoint)
    });
    let names: Vec<_> = endpoints.iter().map(|p| &p.name).collect();
    let (set, visitor, visitor_mut) = if input {
        (
            quote!(Inputs),
            quote!(InputVisitor),
            quote!(InputVisitorMut),
        )
    } else {
        (
            quote!(Outputs),
            quote!(OutputVisitor),
            quote!(OutputVisitorMut),
        )
    };
    quote! {
        pub(crate) struct #name<'storage> {
            #(#fields,)*
            __cfw_lifetime: ::core::marker::PhantomData<&'storage ()>,
        }
        impl ::task::callback::#set for #name<'_> {
            fn visit(&self, visitor: &mut impl ::task::callback::#visitor) {
                #(visitor.visit(&self.#names);)*
            }
            fn visit_mut(&mut self, visitor: &mut impl ::task::callback::#visitor_mut) {
                #(visitor.visit(&mut self.#names);)*
            }
        }
    }
}

fn binding(
    task: &TaskDefinition,
    declaration: &syn::Ident,
    inputs: &syn::Ident,
    outputs: &syn::Ident,
) -> TokenStream {
    let task_name = &task.name;
    let params = task.endpoints.iter().map(|p| {
        let name = &p.name;
        let bindings = p.bindings_type();
        quote!(#name: #bindings)
    });
    let input_fields = task
        .endpoints
        .iter()
        .filter(|p| p.is_input())
        .map(Endpoint::bind);
    let output_fields = task
        .endpoints
        .iter()
        .filter(|p| !p.is_input())
        .map(Endpoint::bind);
    let invocation = invocation(task);
    let arity = arity(task.endpoints.len() + 2);
    quote! {
        impl #task_name {
            #arity
            pub(crate) fn bind<'storage>(self, __cfw_declaration: #declaration<'storage>, #(#params),*) -> Result<impl ::task::Callback + 'storage, ::task::EndpointError> {
                let __cfw_inputs = #inputs { #(#input_fields,)* __cfw_lifetime: ::core::marker::PhantomData };
                let __cfw_outputs = #outputs { #(#output_fields,)* __cfw_lifetime: ::core::marker::PhantomData };
                #[allow(unused_mut)]
                let mut __cfw_user = self;
                Ok(::task::CallbackNode::new(__cfw_inputs, __cfw_outputs,
                    move |__cfw_inputs: &#inputs<'storage>, __cfw_outputs: &mut #outputs<'storage>, __cfw_context: &::task::Context| {
                        #invocation
                    }
                ))
            }
        }
    }
}

fn invocation(task: &TaskDefinition) -> TokenStream {
    let arguments = task.arguments.iter().map(|arg| match arg {
        Some(index) => task.endpoints[*index].argument(),
        None => quote!(__cfw_context),
    });
    if task.fallible {
        quote!(__cfw_user.run(#(#arguments),*))
    } else {
        quote! { __cfw_user.run(#(#arguments),*); Ok(()) }
    }
}

impl Endpoint {
    pub(super) fn is_input(&self) -> bool {
        matches!(
            self.kind,
            EndpointKind::Input
                | EndpointKind::Required
                | EndpointKind::IoxInput
                | EndpointKind::IoxSpan
                | EndpointKind::IoxEvent
        )
    }
    fn key_type(&self) -> TokenStream {
        let payload = &self.payload;
        match self.kind {
            EndpointKind::IoxInput | EndpointKind::IoxSpan => {
                quote!(::task::iox2::Iox2SubscriberKey<#payload>)
            }
            EndpointKind::IoxOutput => quote!(::task::iox2::Iox2PublisherKey<#payload>),
            EndpointKind::IoxEvent => quote!(::task::iox2::Iox2EventKey),
            EndpointKind::IoxNotifier => quote!(::task::iox2::Iox2NotifierKey),
            EndpointKind::Input | EndpointKind::Required => quote!(::task::SubscriberKey<#payload>),
            _ => quote!(::task::PublisherKey<#payload>),
        }
    }
    fn plan_type(&self) -> TokenStream {
        let payload = &self.payload;
        match self.kind {
            EndpointKind::IoxInput | EndpointKind::IoxSpan | EndpointKind::IoxOutput => {
                quote!(&mut ::task::iox2::Iox2ChannelPlan<#payload>)
            }
            EndpointKind::IoxEvent | EndpointKind::IoxNotifier => {
                quote!(&mut dyn ::task::iox2::Iox2EventPlan)
            }
            _ => quote!(&mut ::task::ChannelPlan<#payload>),
        }
    }
    fn bindings_type(&self) -> TokenStream {
        let payload = &self.payload;
        match self.kind {
            EndpointKind::IoxInput | EndpointKind::IoxSpan | EndpointKind::IoxOutput => {
                quote!(&::task::iox2::Iox2Bindings<#payload>)
            }
            EndpointKind::IoxEvent | EndpointKind::IoxNotifier => {
                quote!(&dyn ::task::iox2::Iox2EventBindings)
            }
            _ => quote!(&::task::ChannelBindings<'storage, #payload>),
        }
    }
    fn endpoint_type(&self) -> TokenStream {
        let payload = &self.payload;
        match self.kind {
            EndpointKind::Input => quote!(::task::Subscriber<'storage, #payload>),
            EndpointKind::Required => {
                quote!(::task::callback::Required<::task::Subscriber<'storage, #payload>>)
            }
            EndpointKind::IoxInput | EndpointKind::IoxSpan => {
                quote!(::task::iox2::Iox2Subscriber<#payload>)
            }
            EndpointKind::IoxOutput => quote!(::task::iox2::Iox2Publisher<#payload>),
            EndpointKind::IoxEvent => quote!(::task::iox2::Iox2EventSubscriber),
            EndpointKind::IoxNotifier => quote!(::task::iox2::Iox2Notifier),
            _ => quote!(::task::Publisher<'storage, #payload>),
        }
    }
    fn declare(&self) -> TokenStream {
        let Self {
            name,
            capacity,
            trigger,
            keep,
            ..
        } = self;
        let expression = match self.kind {
            EndpointKind::IoxEvent => quote!(#name.events(#capacity)),
            EndpointKind::IoxNotifier => quote!(#name.notifier()),
            EndpointKind::IoxInput | EndpointKind::IoxSpan => quote!(#name.subscriber(#capacity)),
            EndpointKind::Input | EndpointKind::Required => {
                quote!(#name.subscriber_with_policy(#capacity, ::task::SubscriberPolicy { trigger: #trigger, keep_across_runs: #keep }))
            }
            _ => quote!(#name.publisher(#capacity)),
        };
        quote!(#name: #expression)
    }
    fn bind(&self) -> TokenStream {
        let name = &self.name;
        let take = match self.kind {
            EndpointKind::IoxEvent => quote!(take_event),
            EndpointKind::IoxNotifier => quote!(take_notifier),
            _ if self.is_input() => quote!(take_subscriber),
            _ => quote!(take_publisher),
        };
        let endpoint = quote!(#name.#take(&__cfw_declaration.#name)?);
        if matches!(self.kind, EndpointKind::Required) {
            quote!(#name: ::task::callback::Required(#endpoint))
        } else {
            quote!(#name: #endpoint)
        }
    }
    fn argument(&self) -> TokenStream {
        let name = &self.name;
        match self.kind {
            EndpointKind::Input if self.forwardable => {
                quote!(__cfw_inputs.#name.input_forwardable())
            }
            EndpointKind::Input => quote!(__cfw_inputs.#name.input_readonly()),
            EndpointKind::Required if self.forwardable => {
                quote!(::task::RequiredInput::new_forwardable(&__cfw_inputs.#name.0))
            }
            EndpointKind::Required => quote!(::task::RequiredInput::new(&__cfw_inputs.#name.0)),
            EndpointKind::Output => {
                quote!(__cfw_outputs.#name.loan(::core::default::Default::default())?)
            }
            EndpointKind::OutputSpan => quote!(__cfw_outputs.#name.span()),
            EndpointKind::Uninit => quote!(__cfw_outputs.#name.loan_uninit()?),
            EndpointKind::Publisher => quote!(&mut __cfw_outputs.#name),
            EndpointKind::IoxInput => {
                quote!(::task::iox2::Iox2OptionalInput::new(&__cfw_inputs.#name))
            }
            EndpointKind::IoxSpan => quote!(::task::iox2::Iox2SpanInput::new(&__cfw_inputs.#name)),
            EndpointKind::IoxEvent => quote!(::task::iox2::Iox2Event::new(&__cfw_inputs.#name)),
            EndpointKind::IoxOutput => {
                quote!(::task::iox2::Iox2Output::new_default(&mut __cfw_outputs.#name)?)
            }
            EndpointKind::IoxNotifier => {
                quote!(::task::iox2::Iox2NotifyOutput::new(&mut __cfw_outputs.#name))
            }
        }
    }
}
