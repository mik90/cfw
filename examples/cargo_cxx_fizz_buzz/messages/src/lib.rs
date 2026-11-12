//! Payload types shared between Rust tasks and C++ tasks.
//!
//! The definitions live in the cxx bridge so both languages see the same layout;
//! task bridges alias these types via `type Vec3 = ::cxx_fizz_buzz_messages::Vec3;`.
//!
//! This is a separate crate on purpose: it exercises the real linking/including
//! setup. Payload structs must be declared in a cxx bridge, and cross-crate
//! aliasing only works when this crate exposes its generated header via `links`
//! and task bridges `include!` it.
//!
//! The whole crate is gated behind the `cxx` feature so default workspace builds
//! need no C++ toolchain.
#![cfg(feature = "cxx")]

pub use ffi::Vec3;

#[cxx::bridge(namespace = "cfw::messages")]
mod ffi {
    /// A 3D vector shared verbatim between Rust and C++.
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct Vec3 {
        x: f64,
        y: f64,
        z: f64,
    }
}

// Bridge-declared structs cannot derive serde, so logging/replay support is
// written by hand. Simple payloads can serialize a tuple of their fields.
impl task::loggable::Loggable for Vec3 {
    type Context<'a>
        = ()
    where
        Self: 'a;

    fn serialize(
        &self,
        writer: &mut dyn std::io::Write,
    ) -> Result<(), task::loggable::SerializeError> {
        serde_json::to_writer(writer, &(self.x, self.y, self.z)).map_err(Into::into)
    }

    fn deserialize_with_ctx<'a>(
        bytes: &[u8],
        _: (),
    ) -> Result<Self, task::loggable::DeserializeError>
    where
        Self: 'a,
    {
        let (x, y, z): (f64, f64, f64) = serde_json::from_slice(bytes)?;
        Ok(Self { x, y, z })
    }
}

#[cfg(test)]
mod tests {
    use task::loggable::Loggable;

    use super::Vec3;

    #[test]
    fn shared_struct_payload_loggable_round_trip() {
        let value = Vec3 {
            x: 1.5,
            y: -2.0,
            z: 0.25,
        };
        let mut bytes = Vec::new();
        value.serialize(&mut bytes).unwrap();
        let decoded = Vec3::deserialize(&bytes).unwrap();
        assert_eq!(decoded, value);
    }
}
