// The bridge is optional so the default workspace build needs no C++ toolchain.
#[cfg(not(feature = "cxx"))]
fn main() {}

#[cfg(feature = "cxx")]
fn main() {
    println!("cargo:rerun-if-changed=src/tasks.rs");
    println!("cargo:rerun-if-changed=src/input_span.cpp");
    println!("cargo:rerun-if-changed=src/input_span.hpp");

    task_cxx_codegen::generate_bridge_file("src/tasks.rs", "gen/bridge.rs").unwrap();

    cxx_build::bridge("gen/bridge.rs")
        .file("src/input_span.cpp")
        .std("c++17")
        .compile("task_cxx_tests");
}
