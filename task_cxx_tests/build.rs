// The bridge is optional so the default workspace build needs no C++ toolchain.
#[cfg(not(feature = "cxx"))]
fn main() {}

#[cfg(feature = "cxx")]
fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=src/input_span.cpp");
    println!("cargo:rerun-if-changed=src/input_span.hpp");

    cxx_build::bridge("src/lib.rs")
        .file("src/input_span.cpp")
        .std("c++17")
        .compile("task_cxx_tests");
}
