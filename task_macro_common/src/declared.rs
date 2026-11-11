//! Declarative task-spec parsing (`define_task!` and `define_cxx_task!`).

use syn::parse::{Parse, ParseStream};
use syn::{
    GenericArgument, Ident, LitBool, LitInt, LitStr, PathArguments, Token, Type, braced,
    parse_quote,
};

use crate::endpoint::{
    Endpoint, EndpointKind, EndpointSpec, ForwardedFamily, TaskDefinition, default_keep,
    span_endpoint, validate_endpoint_policies, validate_endpoints,
};
use crate::generate;

pub struct DeclaredTask {
    api: syn::Path,
    name: Ident,
    payloads_module: Ident,
    subscribers: Vec<EndpointSpec>,
    publishers: Vec<EndpointSpec>,
    payloads: Vec<(Ident, syn::Path)>,
}

impl Parse for DeclaredTask {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let api = parse_key::<syn::Path>(input, "api")?;
        input.parse::<Token![,]>()?;
        let name = parse_key::<Ident>(input, "name")?;
        input.parse::<Token![,]>()?;
        let payloads_module = parse_key::<Ident>(input, "payloads_module")?;
        input.parse::<Token![,]>()?;
        let subscribers = parse_endpoint_block(input, "subscribers")?;
        input.parse::<Token![,]>()?;
        let publishers = parse_endpoint_block(input, "publishers")?;
        input.parse::<Token![,]>()?;
        let payloads = parse_payload_block(input, "payloads")?;
        let _ = input.parse::<Token![,]>();
        if !input.is_empty() {
            return Err(input.error("unexpected tokens after task definition"));
        }
        Ok(Self {
            api,
            name,
            payloads_module,
            subscribers,
            publishers,
            payloads,
        })
    }
}

fn parse_key<T: Parse>(input: ParseStream, key: &str) -> syn::Result<T> {
    let ident: Ident = input.parse()?;
    if ident != key {
        return Err(syn::Error::new_spanned(ident, format!("expected '{key}'")));
    }
    input.parse::<Token![:]>()?;
    input.parse()
}

fn parse_forward_mappings(input: ParseStream) -> syn::Result<Vec<(Ident, Ident)>> {
    let content;
    braced!(content in input);
    let mut mappings = Vec::new();
    while !content.is_empty() {
        let field: Ident = content.parse()?;
        content.parse::<Token![:]>()?;
        let source: Ident = content.parse()?;
        mappings.push((field, source));
        if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
        } else {
            break;
        }
    }
    Ok(mappings)
}

pub fn parse_endpoint_block(input: ParseStream, key: &str) -> syn::Result<Vec<EndpointSpec>> {
    let ident: Ident = input.parse()?;
    if ident != key {
        return Err(syn::Error::new_spanned(ident, format!("expected '{key}'")));
    }
    input.parse::<Token![:]>()?;
    let content;
    braced!(content in input);
    let mut endpoints = Vec::new();
    while !content.is_empty() {
        let name: Ident = content.parse()?;
        content.parse::<Token![:]>()?;
        let ty: Type = content.parse()?;
        let policy;
        braced!(policy in content);
        let mut channel = None;
        let mut capacity = None;
        let mut trigger = None;
        let mut keep_across_runs = None;
        let mut forwardable = None;
        let mut forwards = Vec::new();
        let mut forwarded_message = None;
        let mut forwarded_sources = None;
        while !policy.is_empty() {
            let key: Ident = policy.parse()?;
            policy.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "channel" => channel = Some(policy.parse::<LitStr>()?),
                "capacity" => capacity = Some(policy.parse::<LitInt>()?),
                "trigger" => trigger = Some(policy.parse::<LitBool>()?),
                "keep_across_runs" => keep_across_runs = Some(policy.parse::<LitBool>()?),
                "forwardable" => forwardable = Some(policy.parse::<LitBool>()?),
                "forwards" => forwards = parse_forward_mappings(&policy)?,
                "forwarded_message" => forwarded_message = Some(policy.parse::<Type>()?),
                "forwarded_sources" => forwarded_sources = Some(policy.parse::<Type>()?),
                _ => {
                    return Err(syn::Error::new_spanned(
                        key,
                        "expected channel, capacity, trigger, keep_across_runs, forwardable, forwards, forwarded_message, or forwarded_sources",
                    ));
                }
            }
            if policy.peek(Token![,]) {
                policy.parse::<Token![,]>()?;
            } else {
                break;
            }
        }
        let forwarded = match (forwarded_message, forwarded_sources) {
            (Some(message), Some(sources)) => Some(ForwardedFamily { message, sources }),
            (None, None) => None,
            _ => {
                return Err(syn::Error::new_spanned(
                    &ty,
                    "forwarded_message and forwarded_sources must be provided together",
                ));
            }
        };
        endpoints.push(EndpointSpec {
            name,
            ty,
            channel,
            capacity,
            trigger,
            keep_across_runs,
            forwardable,
            forwards,
            forwarded,
        });
        if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
        } else {
            break;
        }
    }
    Ok(endpoints)
}

fn parse_payload_block(input: ParseStream, key: &str) -> syn::Result<Vec<(Ident, syn::Path)>> {
    let ident: Ident = input.parse()?;
    if ident != key {
        return Err(syn::Error::new_spanned(ident, format!("expected '{key}'")));
    }
    input.parse::<Token![:]>()?;
    let content;
    braced!(content in input);
    let mut payloads = Vec::new();
    while !content.is_empty() {
        let field: Ident = content.parse()?;
        content.parse::<Token![:]>()?;
        let path: syn::Path = content.parse()?;
        payloads.push((field, path));
        if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
        } else {
            break;
        }
    }
    Ok(payloads)
}

pub fn declared_endpoint(spec: EndpointSpec, input: bool) -> syn::Result<Endpoint> {
    let EndpointSpec {
        name,
        ty,
        channel,
        capacity,
        trigger,
        keep_across_runs,
        forwardable,
        forwards,
        forwarded,
    } = spec;
    let Type::Path(path) = &ty else {
        return Err(syn::Error::new_spanned(ty, "expected a typed endpoint"));
    };
    let segment = path.path.segments.last().unwrap();
    let kind = match (input, segment.ident.to_string().as_str()) {
        (true, "Input" | "OptionalInput" | "InputSpan") => EndpointKind::Input,
        (true, "RequiredInput") => EndpointKind::Required,
        (true, "Iox2OptionalInput") => EndpointKind::IoxInput,
        (true, "Iox2SpanInput") => EndpointKind::IoxSpan,
        (true, "Iox2Event") => EndpointKind::IoxEvent,
        (false, "Output") => EndpointKind::Output,
        (false, "OutputDefault") => EndpointKind::OutputDefault,
        (false, "OutputUninit") => EndpointKind::Uninit,
        (false, "OutputSpan") => EndpointKind::OutputSpan,
        (false, "Iox2Output") => EndpointKind::IoxOutput,
        (false, "Iox2NotifyOutput") => EndpointKind::IoxNotifier,
        _ => {
            return Err(syn::Error::new_spanned(
                &ty,
                "expected Input, OptionalInput, InputSpan, RequiredInput, Output, OutputDefault, OutputUninit, OutputSpan, Iox2OptionalInput, Iox2SpanInput, Iox2Output, Iox2Event, or Iox2NotifyOutput",
            ));
        }
    };
    if !input && (trigger.is_some() || keep_across_runs.is_some()) {
        return Err(syn::Error::new_spanned(
            &ty,
            "trigger and keep_across_runs apply to subscribers only",
        ));
    }
    validate_endpoint_policies(kind, trigger.is_some(), keep_across_runs.is_some(), &ty)?;
    let forwardable = forwardable.map(|value| value.value).unwrap_or(false);
    if forwardable && !matches!(kind, EndpointKind::Input | EndpointKind::Required) {
        return Err(syn::Error::new_spanned(
            &ty,
            "forwardable applies to native inputs only",
        ));
    }
    if !forwards.is_empty()
        && !matches!(
            kind,
            EndpointKind::Output
                | EndpointKind::OutputDefault
                | EndpointKind::Uninit
                | EndpointKind::OutputSpan
        )
    {
        return Err(syn::Error::new_spanned(
            &ty,
            "forwards applies to native outputs only",
        ));
    }
    if forwarded.is_some()
        && matches!(
            kind,
            EndpointKind::IoxInput
                | EndpointKind::IoxSpan
                | EndpointKind::IoxOutput
                | EndpointKind::IoxEvent
                | EndpointKind::IoxNotifier
        )
    {
        return Err(syn::Error::new_spanned(
            &ty,
            "forwarded endpoints are native only",
        ));
    }
    if !forwards.is_empty() && forwarded.is_none() {
        return Err(syn::Error::new_spanned(
            &ty,
            "forwards requires a forwarded channel",
        ));
    }
    let payload = if matches!(kind, EndpointKind::IoxEvent | EndpointKind::IoxNotifier) {
        if let PathArguments::AngleBracketed(generics) = &segment.arguments
            && generics
                .args
                .iter()
                .any(|argument| matches!(argument, GenericArgument::Type(_)))
        {
            return Err(syn::Error::new_spanned(
                &ty,
                "event endpoints take no payload type",
            ));
        }
        parse_quote!(())
    } else {
        let PathArguments::AngleBracketed(generics) = &segment.arguments else {
            return Err(syn::Error::new_spanned(
                &ty,
                "endpoint requires one payload type",
            ));
        };
        let payloads: Vec<_> = generics
            .args
            .iter()
            .filter_map(|argument| match argument {
                GenericArgument::Type(ty) => Some(ty.clone()),
                _ => None,
            })
            .collect();
        if payloads.len() != 1 {
            return Err(syn::Error::new_spanned(
                &ty,
                "endpoint requires exactly one payload type",
            ));
        }
        payloads[0].clone()
    };
    let span = span_endpoint(segment);
    let capacity = match capacity {
        Some(literal) => parse_quote!(#literal),
        None if span => {
            return Err(syn::Error::new_spanned(
                &ty,
                "span endpoints require an explicit #[capacity(...)]",
            ));
        }
        None => parse_quote!(1),
    };
    let trigger: syn::Expr = trigger
        .map(|literal| parse_quote!(#literal))
        .unwrap_or_else(|| parse_quote!(false));
    let keep = keep_across_runs
        .map(|literal| parse_quote!(#literal))
        .unwrap_or_else(|| default_keep(&trigger));
    Ok(Endpoint {
        forwardable,
        forwards,
        forwarded,
        name,
        payload,
        kind,
        channel: channel.map(|literal| parse_quote!(#literal)),
        capacity,
        trigger,
        keep,
    })
}

pub fn expand_declared(spec: DeclaredTask) -> syn::Result<proc_macro2::TokenStream> {
    let DeclaredTask {
        api,
        name,
        payloads_module,
        subscribers,
        publishers,
        payloads,
    } = spec;
    let mut endpoints = Vec::new();
    for subscriber in subscribers {
        endpoints.push(declared_endpoint(subscriber, true)?);
    }
    for publisher in publishers {
        endpoints.push(declared_endpoint(publisher, false)?);
    }
    validate_endpoints(&endpoints)?;
    let task = TaskDefinition {
        item: None,
        name,
        self_ty: dollar_ty(),
        endpoints,
        arguments: Vec::new(),
        fallible: false,
    };
    Ok(generate::bazel_api(
        &task,
        &api,
        &payloads_module,
        &payloads,
    ))
}

fn dollar_ty() -> proc_macro2::TokenStream {
    use proc_macro2::{Punct, Spacing, TokenTree};
    [
        TokenTree::Punct(Punct::new('$', Spacing::Alone)),
        TokenTree::Ident(Ident::new("ty", proc_macro2::Span::call_site())),
    ]
    .into_iter()
    .collect()
}
