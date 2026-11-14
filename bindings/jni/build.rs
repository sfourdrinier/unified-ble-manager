// bindings/jni/build.rs
//
// PR210-18: emits the `ubm-native-build-identity/1` constants
// ($OUT_DIR/ubm_build_identity.rs) from UBM_BUILD_SOURCE_DIGEST /
// UBM_BUILD_BINDING_SCHEMA ("unsealed" when unset).
include!("../ubm_build_identity.rs");

fn main() {
    // The JVM round-trip harness enables this via RUSTFLAGS; it is not a
    // Cargo feature because it must not affect published JNI feature sets.
    println!("cargo::rustc-check-cfg=cfg(ubm_jni_roundtrip)");
    ubm_build_identity::emit("jni");
}
