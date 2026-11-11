//! Thin proc-macro entry points over [`task_macro_common`].

use proc_macro::TokenStream;
use syn::parse_macro_input;

/// Derive a lifetime-indexed bundle of named ArenaReaderPtr<Message<T>> fields.
/// There is no field-count limit. Every field uses the struct's single lifetime;
/// nested forwarded payloads may use that lifetime as well.
///
/// Bound endpoints cannot promote source handles to static:
/// ```compile_fail
/// use task::{ArenaReaderPtr, Forwarded, Subscriber, SubscriberKey, message::Message};
/// use task_graph::GraphBindings;
/// use task_macros::ForwardedSources;
/// #[derive(ForwardedSources)]
/// struct Sources<'s> { source: ArenaReaderPtr<'s,Message<u64>> }
/// fn extend<'a>(bindings: &GraphBindings<'a>, key: SubscriberKey<Forwarded<bool,Sources<'static>>>)
///     -> Subscriber<'a,Forwarded<bool,Sources<'static>>> {
///     bindings.bundle::<bool,Sources<'static>>("output").unwrap()
///         .take_subscriber(&task_graph::bundle::subscriber_key::<bool,Sources<'static>>(key)).unwrap()
/// }
/// ```
#[proc_macro_derive(ForwardedSources)]
pub fn forwarded_sources(item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as syn::DeriveInput);
    task_macro_common::derive_sources(item)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Generate declaration-driven registration and a storage-borrowing callback.
/// Register tasks through `ApplicationBuilder` or `TaskRegistration`; channel
/// annotations supply default wiring and registrations support per-instance overrides.
/// Typed binding details stay within the declaring crate. Tests additionally get
/// `declare` and key accessors for checking low-level endpoint behavior.
/// Generated registration requires direct dependencies on `task` and `task_graph`.
#[proc_macro_attribute]
pub fn task_callback(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as syn::ItemImpl);
    task_macro_common::expand(item)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Generate a Bazel-facing task API crate: user-facing `Inputs`/`Outputs`,
/// a `Run` trait, and an exported `impl_task!` macro that implements
/// `task_graph::Task` for the user's concrete task type.
#[proc_macro]
pub fn define_task(input: TokenStream) -> TokenStream {
    let spec = parse_macro_input!(input as task_macro_common::DeclaredTask);
    task_macro_common::expand_declared(spec)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
