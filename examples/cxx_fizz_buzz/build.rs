// The bridge is optional so the default workspace build needs no C++ toolchain.
#[cfg(not(feature = "cxx"))]
fn main() {}

#[cfg(feature = "cxx")]
fn main() {
    println!("cargo:rerun-if-changed=src/bridge.rs");
    println!("cargo:rerun-if-changed=src/fizz_buzz.cpp");
    println!("cargo:rerun-if-changed=src/fizz_buzz.hpp");

    task_cxx_codegen::generate_bridge_file(
        "src/bridge.rs",
        "gen/bridge.rs",
        &task_cxx_codegen::BridgeConfig {
            namespace: "cfw::fizz_buzz",
            headers: &["cxx_fizz_buzz/src/fizz_buzz.hpp"],
            payloads: &[task_cxx_codegen::PayloadAlias {
                namespace: "cfw::messages",
                header: "cxx_fizz_buzz_messages/src/lib.rs.h",
                rust_type: "Vec3",
                rust_path: "::cxx_fizz_buzz_messages::Vec3",
            }],
        },
    )
    .unwrap();

    cxx_build::bridge("gen/bridge.rs")
        .file("src/fizz_buzz.cpp")
        .std("c++17")
        .compile("cxx_fizz_buzz");
}
