use iceoryx2::prelude::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Value(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ZeroCopySend)]
#[repr(C)]
pub struct Iox2(pub u64);
