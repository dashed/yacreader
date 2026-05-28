fn main() {
    if std::env::var("CARGO_FEATURE_FFI").is_ok() {
        cxx_build::bridge("src/lib.rs")
            .flag_if_supported("-std=c++17")
            .compile("stump_sync_cxx");
    }
}
