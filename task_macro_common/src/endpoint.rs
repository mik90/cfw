//! Endpoint model shared by the attribute and declared task APIs.

use quote::ToTokens;
use syn::{Ident, ItemImpl, LitBool, LitInt, LitStr, Type, parse_quote};

#[derive(Clone, Copy)]
pub enum EndpointKind {
    Input,
    Required,
    Output,
    OutputDefault,
    Uninit,
    OutputSpan,
    IoxInput,
    IoxSpan,
    IoxOutput,
    IoxEvent,
    IoxNotifier,
}

#[derive(Clone)]
pub struct ForwardedFamily {
    pub message: Type,
    pub sources: Type,
}

pub struct Endpoint {
    pub forwardable: bool,
    pub forwards: Vec<(syn::Ident, syn::Ident)>,
    pub forwarded: Option<ForwardedFamily>,
    pub name: syn::Ident,
    pub payload: Type,
    pub kind: EndpointKind,
    pub channel: Option<syn::Expr>,
    pub capacity: syn::Expr,
    pub trigger: syn::Expr,
    pub keep: syn::Expr,
}

pub struct TaskDefinition {
    pub item: Option<ItemImpl>,
    pub name: syn::Ident,
    pub self_ty: proc_macro2::TokenStream,
    pub endpoints: Vec<Endpoint>,
    pub arguments: Vec<Option<usize>>,
    pub fallible: bool,
}

#[derive(Clone)]
pub struct EndpointSpec {
    pub name: Ident,
    pub ty: Type,
    pub channel: Option<LitStr>,
    pub capacity: Option<LitInt>,
    pub trigger: Option<LitBool>,
    pub keep_across_runs: Option<LitBool>,
    pub forwardable: Option<LitBool>,
    pub forwards: Vec<(Ident, Ident)>,
    pub forwarded: Option<ForwardedFamily>,
}

pub(crate) fn span_endpoint(segment: &syn::PathSegment) -> bool {
    matches!(
        segment.ident.to_string().as_str(),
        "InputSpan" | "OutputSpan" | "Iox2SpanInput"
    )
}

/// Trigger inputs consume by default so retained messages cannot retrigger them.
pub(crate) fn default_keep(trigger: &syn::Expr) -> syn::Expr {
    parse_quote!(!(#trigger))
}

pub(crate) fn validate_endpoint_policies(
    kind: EndpointKind,
    has_trigger: bool,
    has_keep: bool,
    tokens: impl ToTokens,
) -> syn::Result<()> {
    if has_trigger && !matches!(kind, EndpointKind::Input | EndpointKind::Required) {
        return Err(syn::Error::new_spanned(
            tokens,
            "trigger applies to native inputs only",
        ));
    }
    if has_keep
        && !matches!(
            kind,
            EndpointKind::Input
                | EndpointKind::Required
                | EndpointKind::IoxInput
                | EndpointKind::IoxSpan
        )
    {
        return Err(syn::Error::new_spanned(
            tokens,
            "keep_across_runs applies to native or iox2 data inputs only",
        ));
    }
    Ok(())
}

pub fn validate_endpoints(endpoints: &[Endpoint]) -> syn::Result<()> {
    for endpoint in endpoints {
        if !endpoint.forwards.is_empty()
            && !matches!(
                endpoint.kind,
                EndpointKind::Output
                    | EndpointKind::OutputDefault
                    | EndpointKind::Uninit
                    | EndpointKind::OutputSpan
            )
        {
            return Err(syn::Error::new_spanned(
                &endpoint.name,
                "forwarding mappings belong to native outputs",
            ));
        }
        if !endpoint.forwards.is_empty()
            && endpoint.forwarded.is_none()
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
