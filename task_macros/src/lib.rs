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

/// The ident is the message type generic argument (e.g. `i32` in `RequiredInput<i32>`).
#[derive(Clone)]
enum ArgumentKind {
    Input((syn::Ident, InputKind)),
    Output((syn::Ident, OutputKind)),
    Context,
}

#[derive(Clone)]
struct MacroCallbackSignature {
    pub callback_type: syn::Ident,
    pub arguments: Vec<ArgumentKind>,
}

impl MacroCallbackSignature {
    fn build_subscribers_impl(&self) -> syn::ImplItem {
        let subscribers: Vec<syn::Expr> = self
            .arguments
            .iter()
            .filter_map(|arg| {
                let (msg, kind_tokens) = match arg {
                    ArgumentKind::Input((msg, InputKind::Required)) => {
                        (msg, quote!(InputKind::Required))
                    }
                    ArgumentKind::Input((msg, InputKind::Optional)) => {
                        (msg, quote!(InputKind::Optional))
                    }
                    ArgumentKind::Input((msg, InputKind::Span)) => {
                        (msg, quote!(InputKind::Span))
                    }
                    _ => return None,
                };
                Some(parse_quote!(
                    Box::new(task::subscriber::Subscriber::<#msg>::new(#kind_tokens.into()))
                ))
            })
            .collect();

        parse_quote! {
            fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
                vec![#(#subscribers),*]
            }
        }
    }

    fn build_publishers_impl(&self) -> syn::ImplItem {
        let publishers: Vec<syn::Expr> = self
            .arguments
            .iter()
            .filter_map(|arg| {
                let (msg, kind_tokens) = match arg {
                    ArgumentKind::Output((msg, OutputKind::Default)) => {
                        (msg, quote!(OutputKind::Default))
                    }
                    ArgumentKind::Output((msg, OutputKind::Span)) => {
                        (msg, quote!(OutputKind::Span))
                    }
                    _ => return None,
                };
                Some(parse_quote!(
                    Box::new(task::publisher::Publisher::<#msg>::new(#kind_tokens.into()))
                ))
            })
            .collect();

        parse_quote! {
            fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
                vec![#(#publishers),*]
            }
        }
    }
}

fn get_message_type(pat_ty: &PatType) -> Result<Ident, syn::Error> {
    let type_path = match pat_ty.ty.as_ref() {
        syn::Type::Path(p) => p,
        _ => return Err(syn::Error::new_spanned(&pat_ty.ty, "expected a path type")),
    };
    let last = type_path
        .path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new_spanned(&pat_ty.ty, "expected at least one path segment"))?;
    let angle_args = match &last.arguments {
        syn::PathArguments::AngleBracketed(a) => a,
        _ => {
            return Err(syn::Error::new_spanned(
                &pat_ty.ty,
                "expected angle-bracket generic (e.g. RequiredInput<MyType>)",
            ))
        }
    };
    let last_arg = angle_args
        .args
        .last()
        .ok_or_else(|| syn::Error::new_spanned(&pat_ty.ty, "expected at least one generic argument"))?;
    match last_arg {
        syn::GenericArgument::Type(syn::Type::Path(p)) => p
            .path
            .get_ident()
            .cloned()
            .ok_or_else(|| syn::Error::new_spanned(&pat_ty.ty, "message type must be a simple identifier")),
        _ => Err(syn::Error::new_spanned(
            &pat_ty.ty,
            "generic argument must be a simple type path",
        )),
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
            ))
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
        .ok_or_else(|| syn::Error::new_spanned(item_impl, "impl block must contain a run() function"))?;

    let mut arguments = Vec::new();
    for arg in run_fn.sig.inputs.iter() {
        let pat_ty = match arg {
            FnArg::Typed(t) => t,
            FnArg::Receiver(_) => continue,
        };
        let type_path = match pat_ty.ty.as_ref() {
            syn::Type::Path(p) => p,
            _ => return Err(syn::Error::new_spanned(&pat_ty.ty, "expected a path type")),
        };
        let last = type_path
            .path
            .segments
            .last()
            .ok_or_else(|| syn::Error::new_spanned(&pat_ty.ty, "expected at least one path segment"))?;

        let kind = match last.ident.to_string().as_str() {
            "RequiredInput" => ArgumentKind::Input((get_message_type(pat_ty)?, InputKind::Required)),
            "OptionalInput" => ArgumentKind::Input((get_message_type(pat_ty)?, InputKind::Optional)),
            "InputSpan" => ArgumentKind::Input((get_message_type(pat_ty)?, InputKind::Span)),
            "Output" => ArgumentKind::Output((get_message_type(pat_ty)?, OutputKind::Default)),
            "OutputSpan" => ArgumentKind::Output((get_message_type(pat_ty)?, OutputKind::Span)),
            "Context" => ArgumentKind::Context,
            _ => {
                return Err(syn::Error::new_spanned(
                    &last.ident,
                    format!(
                        "unknown task argument type '{}'; expected RequiredInput, OptionalInput, InputSpan, Output, OutputSpan, or Context",
                        last.ident
                    ),
                ))
            }
        };
        arguments.push(kind);
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

    let mut callback_arguments =
        syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::new();
    let mut subscriber_index: usize = 0;
    let mut publisher_index: usize = 0;

    for arg in sig.arguments.iter() {
        let expr: syn::Expr = match arg {
            ArgumentKind::Input((msg, kind)) => {
                let constructor = match kind {
                    InputKind::Required => quote!(RequiredInput::<#msg>::new_downcasted),
                    InputKind::Optional => quote!(OptionalInput::<#msg>::new_downcasted),
                    InputKind::Span => quote!(InputSpan::<#msg>::new_downcasted),
                };
                let expr = parse_quote!(#constructor(&mut *subscribers[#subscriber_index]));
                subscriber_index += 1;
                expr
            }
            ArgumentKind::Output((msg, kind)) => {
                let constructor = match kind {
                    OutputKind::Default => quote!(Output::<#msg>::new_downcasted),
                    OutputKind::Span => quote!(OutputSpan::<#msg>::new_downcasted),
                };
                let expr = parse_quote!(#constructor(&mut *publishers[#publisher_index]));
                publisher_index += 1;
                expr
            }
            ArgumentKind::Context => parse_quote!(ctx),
        };
        callback_arguments.push(expr);
    }

    let build_subscribers_impl = sig.build_subscribers_impl();
    let build_publishers_impl = sig.build_publishers_impl();
    let struct_name = &sig.callback_type;

    let quoted = quote! {
        use task::subscriber::{RequiredInput, OptionalInput, InputSpan};
        use task::publisher::{Output, OutputSpan};
        use task::generic_publisher::GenericPublisher;
        use task::generic_subscriber::GenericSubscriber;
        use task::callback::{Run, GenericCallback, CallbackSignature, InputKind, OutputKind};

        #item_impl

        impl GenericCallback for #struct_name {
            fn run_generic(
                &mut self,
                subscribers: &mut [Box<dyn GenericSubscriber>],
                publishers: &mut [Box<dyn GenericPublisher>],
                ctx: &task::context::Context,
            ) -> task::callback::Run {
                self.run(#callback_arguments);
                task::callback::Run::new(1)
            }

            #build_subscribers_impl

            #build_publishers_impl
        }
    };

    TokenStream::from(quoted)
}
