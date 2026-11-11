// The bridge is optional so the default workspace build needs no C++ toolchain.
#[cfg(not(feature = "cxx"))]
fn main() {}

#[cfg(feature = "cxx")]
fn main() {
    println!("cargo:rerun-if-changed=src/bridge.rs");
    println!("cargo:rerun-if-changed=src/fizz_buzz.cpp");
    println!("cargo:rerun-if-changed=src/fizz_buzz.hpp");

    cxx_build::bridge("src/bridge.rs")
        .file("src/fizz_buzz.cpp")
        .std("c++17")
        .compile("cxx_fizz_buzz");
}
