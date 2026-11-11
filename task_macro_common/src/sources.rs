//! `ForwardedSources` derive implementation.

use quote::quote;
use syn::{GenericArgument, PathArguments, Type};

use crate::forwarding::canonical_type;

pub fn derive_sources(item: syn::DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &item.ident;
    if item.generics.params.len() != 1
        || item.generics.lifetimes().count() != 1
        || item.generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(
            &item,
            "source bundles require exactly one lifetime parameter",
        ));
    }
    let lifetime = &item.generics.lifetimes().next().unwrap().lifetime;
    let syn::Data::Struct(data) = &item.data else {
        return Err(syn::Error::new_spanned(
            &item,
            "expected a named source struct",
        ));
    };
    let syn::Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &item,
            "expected named source fields",
        ));
    };
    if fields.named.is_empty() {
        return Err(syn::Error::new_spanned(
            &item,
            "source bundles cannot be empty",
        ));
    }
    let mut names = Vec::new();
    let mut types = Vec::new();
    for field in &fields.named {
        let Type::Path(path) = &field.ty else {
            return Err(syn::Error::new_spanned(
                field,
                "expected ArenaReaderPtr<'graph_storage, Message<T>>",
            ));
        };
        let segment = path.path.segments.last().unwrap();
        let PathArguments::AngleBracketed(args) = &segment.arguments else {
            return Err(syn::Error::new_spanned(
                field,
                "expected ArenaReaderPtr<'graph_storage, Message<T>>",
            ));
        };
        if segment.ident != "ArenaReaderPtr"
            || args.args.len() != 2
            || !matches!(args.args.first(), Some(GenericArgument::Lifetime(l)) if l == lifetime)
        {
            return Err(syn::Error::new_spanned(
                field,
                "expected ArenaReaderPtr using the bundle lifetime",
            ));
        }
        let Some(GenericArgument::Type(Type::Path(message))) = args.args.last() else {
            return Err(syn::Error::new_spanned(field, "expected Message<T>"));
        };
        let message = message.path.segments.last().unwrap();
        let PathArguments::AngleBracketed(args) = &message.arguments else {
            return Err(syn::Error::new_spanned(field, "expected Message<T>"));
        };
        let Some(GenericArgument::Type(payload)) = args.args.first() else {
            return Err(syn::Error::new_spanned(field, "expected Message<T>"));
        };
        if message.ident != "Message" || args.args.len() != 1 {
            return Err(syn::Error::new_spanned(field, "expected Message<T>"));
        }
        names.push(field.ident.as_ref().unwrap());
        types.push(payload);
    }
    let indices: Vec<_> = (0..names.len()).map(syn::Index::from).collect();
    let static_types: Vec<_> = types.iter().map(|ty| canonical_type(ty)).collect();
    let count = names.len();
    Ok(quote! {
        // SAFETY: This bundle consists exclusively of arena handles whose only
        // rebound parameter is their common source-storage lifetime.
        unsafe impl ::task::ForwardedSources for #name<'static> {
            type Bound<'a> = #name<'a>;
            fn fields() -> Vec<(&'static str, ::std::any::TypeId)> {
                vec![#((stringify!(#names), ::std::any::TypeId::of::<#static_types>())),*]
            }
        }
        impl<#lifetime> ::task::forwarded_message::SerializableForward for #name<#lifetime> {
            type Context<'a> = (#(&'a dyn ::task::loggable::MessageLog<#lifetime, #types>,)*) where Self: 'a;
            fn headers(&self) -> Vec<(&'static str, ::task::message::MessageHeader)> {
                vec![#((stringify!(#names), self.#names.header)),*]
            }
            fn resolve<'a>(headers: &[(String, ::task::message::MessageHeader)], context: Self::Context<'a>) -> Result<Self, ::task::loggable::DeserializeError> where Self: 'a {
                if headers.len() != #count { return Err("forwarded source field count mismatch".into()); }
                Ok(Self { #(#names: {
                    if headers[#indices].0 != stringify!(#names) { return Err("forwarded source field name mismatch".into()); }
                    context.#indices.lookup(&headers[#indices].1).ok_or("forwarded source publication missing")?
                }),* })
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_source_bundles() {
        for source in [
            "struct Sources<'a> {}",
            "struct Sources<'a>(ArenaReaderPtr<'a,Message<u64>>);",
            "struct Sources<'a> { raw: &'a u64 }",
            "struct Sources<'a> { wrong: ArenaReaderPtr<'static,Message<u64>> }",
            "struct Sources<'a> { wrong: ArenaReaderPtr<'a,u64> }",
            "struct Sources<'a,T> { generic: ArenaReaderPtr<'a,Message<T>> }",
        ] {
            assert!(
                derive_sources(syn::parse_str(source).unwrap()).is_err(),
                "{source}"
            );
        }
    }
}
