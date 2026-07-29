extern crate proc_macro;

use proc_macro::TokenStream;
use quote::quote;
use syn::{FnArg, Ident, ItemImpl, PatType, parse_macro_input, parse_quote};

#[derive(Clone)]
enum InputKind {
    Required,
    Optional,
    Span,
}

#[derive(Clone)]
enum OutputKind {
    Default,
    Span,
}

#[derive(Clone, PartialEq)]
enum TypeForm {
    Value,
    RefMut,
    Ref_,
}

#[derive(Clone)]
enum PortKind {
    Sub { msg: Ident, ikind: InputKind },
    ForwardableSub { msg: Ident, ikind: InputKind },
    Pub { msg: Ident, okind: OutputKind },
    ForwardingPub { user_data: Ident, forwarded: Ident },
    Context,
}

#[derive(Clone)]
struct SigArg {
    port_kind: PortKind,
    field_name: Ident,
}

#[derive(Clone)]
struct MacroCallbackSignature {
    pub callback_type: Ident,
    pub arguments: Vec<SigArg>,
}

fn extract_two_idents_from_path(
    type_path: &syn::TypePath,
    span_ty: &syn::Type,
) -> Result<(Ident, Ident), syn::Error> {
    let last =
        type_path.path.segments.last().ok_or_else(|| {
            syn::Error::new_spanned(span_ty, "expected at least one path segment")
        })?;
    let angle_args = match &last.arguments {
        syn::PathArguments::AngleBracketed(a) => a,
        _ => {
            return Err(syn::Error::new_spanned(
                span_ty,
                "expected angle-bracket generics (e.g. ForwardingOutput<A, B>)",
            ));
        }
    };
    let args: Vec<_> = angle_args.args.iter().collect();
    if args.len() < 2 {
        return Err(syn::Error::new_spanned(
            span_ty,
            "expected two generic type arguments",
        ));
    }
    let to_ident = |arg: &&syn::GenericArgument| -> Result<Ident, syn::Error> {
        if let syn::GenericArgument::Type(syn::Type::Path(p)) = arg {
            p.path.get_ident().cloned().ok_or_else(|| {
                syn::Error::new_spanned(span_ty, "type argument must be a simple identifier")
            })
        } else {
            Err(syn::Error::new_spanned(
                span_ty,
                "type argument must be a simple type path",
            ))
        }
    };
    Ok((to_ident(&args[0])?, to_ident(&args[1])?))
}

fn get_two_message_types(pat_ty: &PatType) -> Result<(Ident, Ident), syn::Error> {
    let type_path = match pat_ty.ty.as_ref() {
        syn::Type::Path(p) => p,
        _ => {
            return Err(syn::Error::new_spanned(
                &pat_ty.ty,
                "expected a path type (e.g. ForwardingOutput<A, B>)",
            ));
        }
    };
    extract_two_idents_from_path(type_path, &pat_ty.ty)
}

fn get_message_type(pat_ty: &PatType) -> Result<Ident, syn::Error> {
    let type_path = match pat_ty.ty.as_ref() {
        syn::Type::Path(p) => p,
        _ => return Err(syn::Error::new_spanned(&pat_ty.ty, "expected a path type")),
    };
    let last =
        type_path.path.segments.last().ok_or_else(|| {
            syn::Error::new_spanned(&pat_ty.ty, "expected at least one path segment")
        })?;
    let angle_args = match &last.arguments {
        syn::PathArguments::AngleBracketed(a) => a,
        _ => {
            return Err(syn::Error::new_spanned(
                &pat_ty.ty,
                "expected angle-bracket generic (e.g. RequiredInput<MyType>)",
            ));
        }
    };
    let last_arg = angle_args.args.last().ok_or_else(|| {
        syn::Error::new_spanned(&pat_ty.ty, "expected at least one generic argument")
    })?;
    match last_arg {
        syn::GenericArgument::Type(syn::Type::Path(p)) => {
            p.path.get_ident().cloned().ok_or_else(|| {
                syn::Error::new_spanned(&pat_ty.ty, "message type must be a simple identifier")
            })
        }
        _ => Err(syn::Error::new_spanned(
            &pat_ty.ty,
            "generic argument must be a simple type path",
        )),
    }
}

fn field_name(pat_ty: &PatType) -> Result<Ident, syn::Error> {
    if let syn::Pat::Ident(pat_ident) = pat_ty.pat.as_ref() {
        Ok(pat_ident.ident.clone())
    } else {
        Err(syn::Error::new_spanned(
            &pat_ty.pat,
            "argument pattern must be a simple identifier",
        ))
    }
}

fn find_signature(item_impl: &ItemImpl) -> Result<MacroCallbackSignature, syn::Error> {
    let struct_ident = match item_impl.self_ty.as_ref() {
        syn::Type::Path(p) => p
            .path
            .get_ident()
            .ok_or_else(|| {
                syn::Error::new_spanned(&item_impl.self_ty, "expected a simple struct identifier")
            })?
            .clone(),
        _ => {
            return Err(syn::Error::new_spanned(
                &item_impl.self_ty,
                "expected a path for the impl type",
            ));
        }
    };

    let run_fn = item_impl
        .items
        .iter()
        .find_map(|item| {
            if let syn::ImplItem::Fn(f) = item {
                if f.sig.ident == "run" { Some(f) } else { None }
            } else {
                None
            }
        })
        .ok_or_else(|| {
            syn::Error::new_spanned(item_impl, "impl block must contain a run() function")
        })?;

    let mut arguments = Vec::new();
    for arg in run_fn.sig.inputs.iter() {
        let pat_ty = match arg {
            FnArg::Typed(t) => t,
            FnArg::Receiver(_) => continue,
        };

        let fname = field_name(pat_ty)?;

        let (type_path, form) = match pat_ty.ty.as_ref() {
            syn::Type::Path(p) => (p, TypeForm::Value),
            syn::Type::Reference(r) => {
                if r.mutability.is_some() {
                    match r.elem.as_ref() {
                        syn::Type::Path(p) => (p, TypeForm::RefMut),
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &pat_ty.ty,
                                "expected a path type inside &mut reference",
                            ));
                        }
                    }
                } else {
                    match r.elem.as_ref() {
                        syn::Type::Path(p) => (p, TypeForm::Ref_),
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &pat_ty.ty,
                                "expected a path type inside & reference",
                            ));
                        }
                    }
                }
            }
            _ => return Err(syn::Error::new_spanned(&pat_ty.ty, "expected a path type")),
        };
        let last = type_path.path.segments.last().ok_or_else(|| {
            syn::Error::new_spanned(&pat_ty.ty, "expected at least one path segment")
        })?;

        let port_kind = match (last.ident.to_string().as_str(), &form) {
            ("RequiredInput", TypeForm::Value) => PortKind::Sub {
                msg: get_message_type(pat_ty)?,
                ikind: InputKind::Required,
            },
            ("OptionalInput", TypeForm::Value) => PortKind::Sub {
                msg: get_message_type(pat_ty)?,
                ikind: InputKind::Optional,
            },
            ("InputSpan", TypeForm::Value) => PortKind::Sub {
                msg: get_message_type(pat_ty)?,
                ikind: InputKind::Span,
            },
            ("ForwardableRequiredInput", TypeForm::Value) => PortKind::ForwardableSub {
                msg: get_message_type(pat_ty)?,
                ikind: InputKind::Required,
            },
            ("ForwardableOptionalInput", TypeForm::Value) => PortKind::ForwardableSub {
                msg: get_message_type(pat_ty)?,
                ikind: InputKind::Optional,
            },
            ("ForwardableInputSpan", TypeForm::Value) => PortKind::ForwardableSub {
                msg: get_message_type(pat_ty)?,
                ikind: InputKind::Span,
            },
            ("Output", TypeForm::Value) => PortKind::Pub {
                msg: get_message_type(pat_ty)?,
                okind: OutputKind::Default,
            },
            ("OutputSpan", TypeForm::Value) => PortKind::Pub {
                msg: get_message_type(pat_ty)?,
                okind: OutputKind::Span,
            },
            ("ForwardingOutput", TypeForm::Value) => {
                let (user_data, forwarded) = get_two_message_types(pat_ty)?;
                PortKind::ForwardingPub {
                    user_data,
                    forwarded,
                }
            }
            ("Context", TypeForm::Value) => PortKind::Context,
            ("Context", TypeForm::Ref_) => PortKind::Context,
            _ => {
                return Err(syn::Error::new_spanned(
                    &last.ident,
                    format!(
                        "unknown callback argument type '{}'; expected RequiredInput, OptionalInput, InputSpan, ForwardableRequiredInput, ForwardableOptionalInput, ForwardableInputSpan, Output, OutputSpan, ForwardingOutput, Context, or &Context",
                        last.ident
                    ),
                ));
            }
        };
        arguments.push(SigArg {
            port_kind,
            field_name: fname,
        });
    }

    Ok(MacroCallbackSignature {
        callback_type: struct_ident,
        arguments,
    })
}

#[proc_macro_attribute]
pub fn task_callback(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item_impl = parse_macro_input!(item as ItemImpl);

    let sig = match find_signature(&item_impl) {
        Ok(s) => s,
        Err(e) => return e.to_compile_error().into(),
    };

    let struct_name = &sig.callback_type;
    let ports_name = Ident::new(&format!("{}Ports", struct_name), struct_name.span());
    let callback_name = Ident::new(&format!("{}Callback", struct_name), struct_name.span());

    // ── Per-field code bits ──
    let mut field_defs: Vec<syn::Field> = Vec::new();
    let mut field_ctors: Vec<syn::FieldValue> = Vec::new(); // for ports_name constructor
    let mut run_args: Vec<syn::Expr> = Vec::new();
    let mut for_each_sub_stmts: Vec<syn::Stmt> = Vec::new();
    let mut for_each_pub_stmts: Vec<syn::Stmt> = Vec::new();
    let mut for_each_sub_mut_stmts: Vec<syn::Stmt> = Vec::new();
    let mut for_each_pub_mut_stmts: Vec<syn::Stmt> = Vec::new();
    let mut port_mut_stmts: Vec<syn::Stmt> = Vec::new();
    let mut drain_stmts: Vec<syn::Stmt> = Vec::new();
    let mut flush_stmts: Vec<syn::Stmt> = Vec::new();
    let mut flush_logged_stmts: Vec<syn::Stmt> = Vec::new();
    let mut sub_exec_terms: Vec<syn::Expr> = Vec::new();
    let mut able_terms: Vec<syn::Expr> = Vec::new();
    let mut input_ready_terms: Vec<syn::Expr> = Vec::new();
    let mut registrations: Vec<syn::Stmt> = Vec::new();
    let mut drop_stmts: Vec<syn::Stmt> = Vec::new();

    for sig_arg in sig.arguments.iter() {
        let fname = &sig_arg.field_name;
        match &sig_arg.port_kind {
            PortKind::Sub { msg, ikind } => {
                let cfg: syn::Expr = match ikind {
                    InputKind::Required => parse_quote!(task::callback::InputKind::Required.into()),
                    InputKind::Optional => parse_quote!(task::callback::InputKind::Optional.into()),
                    InputKind::Span => parse_quote!(task::callback::InputKind::Span.into()),
                };
                field_defs.push(parse_quote!(pub #fname: task::subscriber::Subscriber<#msg>));
                field_ctors
                    .push(parse_quote!(#fname: task::subscriber::Subscriber::<#msg>::new(#cfg)));

                let ctor: syn::Expr = match ikind {
                    InputKind::Required => parse_quote!(RequiredInput::new(&self.ports.#fname)),
                    InputKind::Optional => parse_quote!(OptionalInput::new(&self.ports.#fname)),
                    InputKind::Span => parse_quote!(InputSpan::new(&self.ports.#fname)),
                };
                run_args.push(ctor);

                for_each_sub_stmts.push(parse_quote!(f(&self.ports.#fname);));
                for_each_sub_mut_stmts.push(parse_quote!(f(&mut self.ports.#fname);));
                port_mut_stmts.push(parse_quote!(f(PortMut::Subscriber(&mut self.ports.#fname));));
                drain_stmts.push(
                    parse_quote!(GenericSubscriber::drain_writer_to_reader(&self.ports.#fname);),
                );
                sub_exec_terms
                    .push(parse_quote!(GenericSubscriber::requests_execution(&self.ports.#fname)));
                able_terms.push(parse_quote!(GenericSubscriber::able_to_run(&self.ports.#fname)));
                input_ready_terms.push(parse_quote!(
                    self.ports.#fname.config().is_optional || GenericSubscriber::has_data_available(&self.ports.#fname)
                ));
                registrations.push(parse_quote!(
                    task::channel_registry::Probe::<#msg>::new().try_register(registry);
                ));
                drop_stmts.push(parse_quote!(
                    GenericSubscriber::cleanup_buffers(&self.ports.#fname);
                ));
            }
            PortKind::ForwardableSub { msg, ikind } => {
                let cfg: syn::Expr = match ikind {
                    InputKind::Required => parse_quote!(task::callback::InputKind::Required.into()),
                    InputKind::Optional => parse_quote!(task::callback::InputKind::Optional.into()),
                    InputKind::Span => parse_quote!(task::callback::InputKind::Span.into()),
                };
                field_defs
                    .push(parse_quote!(pub #fname: task::subscriber::ForwardableSubscriber<#msg>));
                field_ctors.push(parse_quote!(#fname: task::subscriber::ForwardableSubscriber::<#msg>::new(#cfg)));

                let ctor: syn::Expr = match ikind {
                    InputKind::Required => {
                        parse_quote!(ForwardableRequiredInput::new(&self.ports.#fname))
                    }
                    InputKind::Optional => {
                        parse_quote!(ForwardableOptionalInput::new(&self.ports.#fname))
                    }
                    InputKind::Span => parse_quote!(ForwardableInputSpan::new(&self.ports.#fname)),
                };
                run_args.push(ctor);

                for_each_sub_stmts.push(parse_quote!(f(&self.ports.#fname);));
                for_each_sub_mut_stmts.push(parse_quote!(f(&mut self.ports.#fname);));
                port_mut_stmts.push(parse_quote!(f(PortMut::Subscriber(&mut self.ports.#fname));));
                drain_stmts.push(
                    parse_quote!(GenericSubscriber::drain_writer_to_reader(&self.ports.#fname);),
                );
                sub_exec_terms
                    .push(parse_quote!(GenericSubscriber::requests_execution(&self.ports.#fname)));
                able_terms.push(parse_quote!(GenericSubscriber::able_to_run(&self.ports.#fname)));
                input_ready_terms.push(parse_quote!(
                    self.ports.#fname.config().is_optional || GenericSubscriber::has_data_available(&self.ports.#fname)
                ));
                registrations.push(parse_quote!(
                    task::channel_registry::Probe::<#msg>::new().try_register(registry);
                ));
                drop_stmts.push(parse_quote!(
                    GenericSubscriber::cleanup_buffers(&self.ports.#fname);
                ));
            }
            PortKind::Pub { msg, okind } => {
                let cfg: syn::Expr = match okind {
                    OutputKind::Default => parse_quote!(task::callback::OutputKind::Default.into()),
                    OutputKind::Span => parse_quote!(task::callback::OutputKind::Span.into()),
                };
                field_defs.push(parse_quote!(pub #fname: task::publisher::Publisher<#msg>));
                field_ctors
                    .push(parse_quote!(#fname: task::publisher::Publisher::<#msg>::new(#cfg)));

                let ctor: syn::Expr = match okind {
                    OutputKind::Default => {
                        parse_quote!(Output::new_default(&mut self.ports.#fname))
                    }
                    OutputKind::Span => parse_quote!(OutputSpan::new(&mut self.ports.#fname)),
                };
                run_args.push(ctor);

                for_each_pub_stmts.push(parse_quote!(f(&self.ports.#fname);));
                for_each_pub_mut_stmts.push(parse_quote!(f(&mut self.ports.#fname);));
                port_mut_stmts.push(parse_quote!(f(PortMut::Publisher(&mut self.ports.#fname));));
                flush_stmts.push(parse_quote!(GenericPublisher::flush_loaned_values(&mut self.ports.#fname, timestamp);));
                flush_logged_stmts.push(parse_quote!(
                    GenericPublisher::flush_loaned_values_logged(&mut self.ports.#fname, timestamp, &mut |h| hook(ordinal, h));
                ));
                flush_logged_stmts.push(parse_quote!(ordinal += 1;));
                registrations.push(parse_quote!(
                    task::channel_registry::Probe::<#msg>::new().try_register(registry);
                ));
            }
            PortKind::ForwardingPub {
                user_data,
                forwarded,
            } => {
                field_defs.push(parse_quote!(pub #fname: task::publisher::ForwardingPublisher<#user_data, #forwarded>));
                field_ctors.push(parse_quote!(#fname: task::publisher::ForwardingPublisher::<#user_data, #forwarded>::new(
                    task::callback::OutputKind::Default.into(), vec![]
                )));

                run_args.push(parse_quote!(ForwardingOutput::new(&mut self.ports.#fname)));

                for_each_pub_stmts.push(parse_quote!(f(&self.ports.#fname);));
                for_each_pub_mut_stmts.push(parse_quote!(f(&mut self.ports.#fname);));
                port_mut_stmts.push(parse_quote!(f(PortMut::Publisher(&mut self.ports.#fname));));
                flush_stmts.push(parse_quote!(GenericPublisher::flush_loaned_values(&mut self.ports.#fname, timestamp);));
                flush_logged_stmts.push(parse_quote!(
                    GenericPublisher::flush_loaned_values_logged(&mut self.ports.#fname, timestamp, &mut |h| hook(ordinal, h));
                ));
                flush_logged_stmts.push(parse_quote!(ordinal += 1;));
                registrations.push(parse_quote!(
                    task::channel_registry::Probe::<
                        task::forwarded_message::ForwardedMessage<#user_data, #forwarded>
                    >::new().try_register(registry);
                ));
            }
            PortKind::Context => {
                run_args.push(parse_quote!(ctx));
            }
        }
    }

    let flush_logged_body: syn::Block = {
        let mut stmts: Vec<syn::Stmt> = Vec::new();
        stmts.push(parse_quote!(let mut ordinal = 0usize;));
        stmts.extend(flush_logged_stmts);
        parse_quote!({ #(#stmts)* })
    };

    let sub_exec_body: syn::Block = {
        if sub_exec_terms.is_empty() {
            parse_quote!({ false })
        } else {
            parse_quote!({ false #( || #sub_exec_terms )* })
        }
    };
    let able_body: syn::Block = {
        if able_terms.is_empty() {
            parse_quote!({ true })
        } else {
            parse_quote!({ true #( && #able_terms )* })
        }
    };
    let input_ready_body: syn::Block = {
        if input_ready_terms.is_empty() {
            parse_quote!({ true })
        } else {
            parse_quote!({ true #( && #input_ready_terms )* })
        }
    };

    let tokens = quote! {
        #item_impl

        #[allow(non_camel_case_types)]
        pub struct #ports_name {
            #(#field_defs,)*
        }

        pub struct #callback_name {
            user: #struct_name,
            ports: #ports_name,
        }

        impl #struct_name {
            pub fn build(
                self,
                registry: &mut task::channel_registry::ChannelRegistry,
            ) -> #callback_name {
                use task::channel_registry::MaybeRegister as _;
                #(#registrations)*

                #callback_name {
                    user: self,
                    ports: #ports_name {
                        #(#field_ctors,)*
                    },
                }
            }
        }

        const _: () = {
            use task::callback::{Run, Callback, PortMut};
            use task::generic_subscriber::GenericSubscriber;
            use task::generic_publisher::GenericPublisher;
            use task::input::{RequiredInput, OptionalInput, InputSpan, ForwardableRequiredInput, ForwardableOptionalInput, ForwardableInputSpan};
            use task::output::{Output, OutputSpan, ForwardingOutput};

            impl Callback for #callback_name {
                fn run(&mut self, ctx: &task::context::Context) -> Run {
                    self.user.run(#(#run_args),*);
                    Run::new(1)
                }

                fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
                    #(#for_each_sub_stmts)*
                }
                fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
                    #(#for_each_pub_stmts)*
                }
                fn for_each_subscriber_mut<'a>(&'a mut self, f: &mut dyn FnMut(&'a mut dyn GenericSubscriber)) {
                    #(#for_each_sub_mut_stmts)*
                }
                fn for_each_publisher_mut<'a>(&'a mut self, f: &mut dyn FnMut(&'a mut dyn GenericPublisher)) {
                    #(#for_each_pub_mut_stmts)*
                }
                fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
                    #(#port_mut_stmts)*
                }

                fn drain_subscribers(&self) {
                    #(#drain_stmts)*
                }
                fn flush_publishers(&mut self, timestamp: task::time::FrameworkTime) {
                    #(#flush_stmts)*
                }
                fn flush_publishers_logged(&mut self, timestamp: task::time::FrameworkTime, hook: &mut dyn FnMut(usize, &task::message::MessageHeader)) #flush_logged_body
                fn subscribers_request_execution(&self) -> bool #sub_exec_body
                fn able_to_run(&self) -> bool #able_body
                fn required_inputs_ready(&self) -> bool #input_ready_body
            }

            impl Drop for #callback_name {
                fn drop(&mut self) {
                    #(#drop_stmts)*
                }
            }
        };
    };

    TokenStream::from(tokens)
}
