// The bridge is optional so the default workspace build needs no C++ toolchain.
#[cfg(not(feature = "cxx"))]
fn main() {}

#[cfg(feature = "cxx")]
fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    cxx_build::bridge("src/lib.rs")
        .std("c++17")
        .compile("task_cxx");
}
