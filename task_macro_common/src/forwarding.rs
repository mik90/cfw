//! AST helpers for forwarded-message payloads.

use syn::visit_mut::VisitMut;
use syn::{Type, parse_quote};

use crate::endpoint::ForwardedFamily;

pub(crate) fn canonical_type(ty: &Type) -> Type {
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

pub(crate) struct Borrowed(pub(crate) bool);
impl VisitMut for Borrowed {
    fn visit_lifetime_mut(&mut self, lifetime: &mut syn::Lifetime) {
        self.0 |= lifetime.ident != "static";
    }
}

pub(crate) fn forwarded_family(payload: &Type) -> Option<ForwardedFamily> {
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
    (types.len() == 2).then(|| ForwardedFamily {
        message: types[0].clone(),
        sources: canonical_type(&types[1]),
    })
}
