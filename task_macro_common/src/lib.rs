//! Shared parsing and code generation for cfw's task proc macros.
//!
//! `task_macros` and `task_cxx_macros` are thin proc-macro facades over this
//! crate; the public surface here is what they consume.

mod attribute;
mod declared;
mod endpoint;
mod forwarding;
pub mod generate;
mod registration;
mod sources;

pub use attribute::expand;
pub use declared::{DeclaredTask, declared_endpoint, expand_declared, parse_endpoint_block};
pub use endpoint::{
    Endpoint, EndpointKind, EndpointSpec, ForwardedFamily, TaskDefinition, validate_endpoints,
};
pub use registration::generate_registration;
pub use sources::derive_sources;
