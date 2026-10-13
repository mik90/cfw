//! Typed declarations, endpoint collections, and method invocation adapters.
use super::{Port, PortKind, TaskDefinition, generate_registration};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

pub(super) fn task(task: &TaskDefinition) -> TokenStream {
    let item = &task.item;
    let declaration_name = format_ident!("{}Declaration", task.name);
    let inputs_name = format_ident!("{}Inputs", task.name);
    let outputs_name = format_ident!("{}Outputs", task.name);
    let declaration = declaration(task, &declaration_name);
    let registration = generate_registration(&task.name, &declaration_name, &task.ports);
    let inputs = collection(&task.ports, &inputs_name, true);
    let outputs = collection(&task.ports, &outputs_name, false);
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
        .ports
        .iter()
        .map(|p| {
            let name = &p.name;
            let key = p.key_type();
            quote!(#name: #key)
        })
        .collect();
    let accessors = task.ports.iter().map(|p| {
        let name = &p.name;
        let accessor = format_ident!("{}_key", name);
        let key = p.key_type();
        quote!(#[cfg(test)] pub(crate) fn #accessor(&self) -> &#key { &self.#name })
    });
    let names = task.ports.iter().map(|p| &p.name);
    let params = task.ports.iter().map(|p| {
        let name = &p.name;
        let plan = p.plan_type();
        quote!(#name: #plan)
    });
    let validation = task.ports.iter().filter_map(|p| {
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
    let endpoints = task.ports.iter().map(Port::declare);
    let arity = arity(task.ports.len());
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

fn collection(ports: &[Port], name: &syn::Ident, input: bool) -> TokenStream {
    let ports: Vec<_> = ports.iter().filter(|p| p.is_input() == input).collect();
    let fields = ports.iter().map(|p| {
        let name = &p.name;
        let endpoint = p.endpoint_type();
        quote!(#name: #endpoint)
    });
    let names: Vec<_> = ports.iter().map(|p| &p.name).collect();
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
    let params = task.ports.iter().map(|p| {
        let name = &p.name;
        let bindings = p.bindings_type();
        quote!(#name: #bindings)
    });
    let input_fields = task.ports.iter().filter(|p| p.is_input()).map(Port::bind);
    let output_fields = task.ports.iter().filter(|p| !p.is_input()).map(Port::bind);
    let invocation = invocation(task);
    let arity = arity(task.ports.len() + 2);
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
        Some(index) => task.ports[*index].argument(),
        None => quote!(__cfw_context),
    });
    if task.fallible {
        quote!(__cfw_user.run(#(#arguments),*))
    } else {
        quote! { __cfw_user.run(#(#arguments),*); Ok(()) }
    }
}

impl Port {
    pub(super) fn is_input(&self) -> bool {
        matches!(
            self.kind,
            PortKind::Input
                | PortKind::Required
                | PortKind::IoxInput
                | PortKind::IoxSpan
                | PortKind::IoxEvent
        )
    }
    fn key_type(&self) -> TokenStream {
        let payload = &self.payload;
        match self.kind {
            PortKind::IoxInput | PortKind::IoxSpan => {
                quote!(::task::iox2::Iox2SubscriberKey<#payload>)
            }
            PortKind::IoxOutput => quote!(::task::iox2::Iox2PublisherKey<#payload>),
            PortKind::IoxEvent => quote!(::task::iox2::Iox2EventKey),
            PortKind::IoxNotifier => quote!(::task::iox2::Iox2NotifierKey),
            PortKind::Input | PortKind::Required => quote!(::task::SubscriberKey<#payload>),
            _ => quote!(::task::PublisherKey<#payload>),
        }
    }
    fn plan_type(&self) -> TokenStream {
        let payload = &self.payload;
        match self.kind {
            PortKind::IoxInput | PortKind::IoxSpan | PortKind::IoxOutput => {
                quote!(&mut ::task::iox2::Iox2ChannelPlan<#payload>)
            }
            PortKind::IoxEvent | PortKind::IoxNotifier => {
                quote!(&mut dyn ::task::iox2::Iox2EventPlan)
            }
            _ => quote!(&mut ::task::ChannelPlan<#payload>),
        }
    }
    fn bindings_type(&self) -> TokenStream {
        let payload = &self.payload;
        match self.kind {
            PortKind::IoxInput | PortKind::IoxSpan | PortKind::IoxOutput => {
                quote!(&::task::iox2::Iox2Bindings<#payload>)
            }
            PortKind::IoxEvent | PortKind::IoxNotifier => {
                quote!(&dyn ::task::iox2::Iox2EventBindings)
            }
            _ => quote!(&::task::EndpointBindings<'storage, #payload>),
        }
    }
    fn endpoint_type(&self) -> TokenStream {
        let payload = &self.payload;
        match self.kind {
            PortKind::Input => quote!(::task::Subscriber<'storage, #payload>),
            PortKind::Required => {
                quote!(::task::callback::Required<::task::Subscriber<'storage, #payload>>)
            }
            PortKind::IoxInput | PortKind::IoxSpan => {
                quote!(::task::iox2::Iox2Subscriber<#payload>)
            }
            PortKind::IoxOutput => quote!(::task::iox2::Iox2Publisher<#payload>),
            PortKind::IoxEvent => quote!(::task::iox2::Iox2EventSubscriber),
            PortKind::IoxNotifier => quote!(::task::iox2::Iox2Notifier),
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
            PortKind::IoxEvent => quote!(#name.events(#capacity)),
            PortKind::IoxNotifier => quote!(#name.notifier()),
            PortKind::IoxInput | PortKind::IoxSpan => quote!(#name.subscriber(#capacity)),
            PortKind::Input | PortKind::Required => {
                quote!(#name.subscriber_with_policy(#capacity, ::task::SubscriberPolicy { trigger: #trigger, keep_across_runs: #keep }))
            }
            _ => quote!(#name.publisher(#capacity)),
        };
        quote!(#name: #expression)
    }
    fn bind(&self) -> TokenStream {
        let name = &self.name;
        let take = match self.kind {
            PortKind::IoxEvent => quote!(take_event),
            PortKind::IoxNotifier => quote!(take_notifier),
            _ if self.is_input() => quote!(take_subscriber),
            _ => quote!(take_publisher),
        };
        let endpoint = quote!(#name.#take(&__cfw_declaration.#name)?);
        if matches!(self.kind, PortKind::Required) {
            quote!(#name: ::task::callback::Required(#endpoint))
        } else {
            quote!(#name: #endpoint)
        }
    }
    fn argument(&self) -> TokenStream {
        let name = &self.name;
        match self.kind {
            PortKind::Input if self.forwardable => quote!(__cfw_inputs.#name.input_forwardable()),
            PortKind::Input => quote!(__cfw_inputs.#name.input_readonly()),
            PortKind::Required if self.forwardable => {
                quote!(::task::RequiredInput::new_forwardable(&__cfw_inputs.#name.0))
            }
            PortKind::Required => quote!(::task::RequiredInput::new(&__cfw_inputs.#name.0)),
            PortKind::Output => {
                quote!(__cfw_outputs.#name.loan(::core::default::Default::default())?)
            }
            PortKind::OutputSpan => quote!(__cfw_outputs.#name.span()),
            PortKind::Uninit => quote!(__cfw_outputs.#name.loan_uninit()?),
            PortKind::Publisher => quote!(&mut __cfw_outputs.#name),
            PortKind::IoxInput => quote!(::task::iox2::Iox2OptionalInput::new(&__cfw_inputs.#name)),
            PortKind::IoxSpan => quote!(::task::iox2::Iox2SpanInput::new(&__cfw_inputs.#name)),
            PortKind::IoxEvent => quote!(::task::iox2::Iox2Event::new(&__cfw_inputs.#name)),
            PortKind::IoxOutput => {
                quote!(::task::iox2::Iox2Output::new_default(&mut __cfw_outputs.#name)?)
            }
            PortKind::IoxNotifier => {
                quote!(::task::iox2::Iox2NotifyOutput::new(&mut __cfw_outputs.#name))
            }
        }
    }
}
