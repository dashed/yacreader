fn main() {
    // Standalone `cargo build/test --features ffi` compiles the cxx C++ glue
    // here so the crate is self-contained. Under CMake/Corrosion the bridge is
    // generated and compiled by `corrosion_add_cxxbridge` instead; that build
    // sets STUMP_SYNC_SKIP_CXX_BUILD=1 so we skip it here and avoid duplicate
    // cxx shim symbols at link time.
    println!("cargo:rerun-if-env-changed=STUMP_SYNC_SKIP_CXX_BUILD");
    let skip_cxx = std::env::var_os("STUMP_SYNC_SKIP_CXX_BUILD").is_some();
    if std::env::var("CARGO_FEATURE_FFI").is_ok() && !skip_cxx {
        cxx_build::bridge("src/lib.rs")
            .flag_if_supported("-std=c++17")
            .compile("stump_sync_cxx");
    }
}
