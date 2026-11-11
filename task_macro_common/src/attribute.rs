//! `#[task_callback]` attribute-macro expansion.

use quote::quote;
use syn::visit_mut::VisitMut;
use syn::{FnArg, GenericArgument, ItemImpl, Pat, PathArguments, ReturnType, Type, parse_quote};

use crate::endpoint::{
    Endpoint, EndpointKind, TaskDefinition, default_keep, span_endpoint,
    validate_endpoint_policies, validate_endpoints,
};
use crate::generate;

struct StorageLifetimes;
impl VisitMut for StorageLifetimes {
    fn visit_lifetime_mut(&mut self, lifetime: &mut syn::Lifetime) {
        if lifetime.ident != "static" {
            *lifetime = parse_quote!('graph_storage);
        }
    }
}

pub fn expand(item: ItemImpl) -> syn::Result<proc_macro2::TokenStream> {
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
            "OutputDefault" if reference.is_none() => EndpointKind::OutputDefault,
            "OutputSpan" if reference.is_none() => EndpointKind::OutputSpan,
            "Iox2OptionalInput" if reference.is_none() => EndpointKind::IoxInput,
            "Iox2SpanInput" if reference.is_none() => EndpointKind::IoxSpan,
            "Iox2Output" if reference.is_none() => EndpointKind::IoxOutput,
            "Iox2Event" if reference.is_none() => EndpointKind::IoxEvent,
            "Iox2NotifyOutput" if reference.is_none() => EndpointKind::IoxNotifier,
            _ => {
                return Err(syn::Error::new_spanned(
                    &arg.ty,
                    "expected Input, RequiredInput, OptionalInput, InputSpan, Output, OutputDefault, OutputSpan, or &Context",
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
        validate_endpoint_policies(kind, trigger.is_some(), keep.is_some(), &arg.ty)?;
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
        let span = span_endpoint(segment);
        let capacity = match capacity {
            Some(capacity) => capacity,
            None if span => {
                return Err(syn::Error::new_spanned(
                    &arg.ty,
                    "span endpoints require an explicit #[capacity(...)]",
                ));
            }
            None => parse_quote!(1),
        };
        arguments.push(Some(endpoints.len()));
        let trigger: syn::Expr = trigger.unwrap_or_else(|| parse_quote!(false));
        let keep = keep.unwrap_or_else(|| default_keep(&trigger));
        endpoints.push(Endpoint {
            forwardable,
            forwards,
            forwarded: None,
            name,
            payload,
            kind,
            channel,
            capacity,
            trigger,
            keep,
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
        item: Some(item),
        name: task_name.clone(),
        self_ty: quote!(#task_name),
        endpoints,
        arguments,
        fallible,
    })
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
            "impl Task { fn run(&mut self, input: InputSpan<u64>) {} }",
            "impl Task { fn run(&mut self, output: OutputSpan<u64>) {} }",
            "impl Task { fn run(&mut self, input: Iox2SpanInput<u64>) {} }",
        ] {
            assert!(expand(syn::parse_str(source).unwrap()).is_err());
        }
    }
}
