// bindings/jni/build.rs
//
// PR210-18: emits the `ubm-native-build-identity/1` constants
// ($OUT_DIR/ubm_build_identity.rs) from UBM_BUILD_SOURCE_DIGEST /
// UBM_BUILD_BINDING_SCHEMA ("unsealed" when unset).
include!("../ubm_build_identity.rs");

fn main() {
    ubm_build_identity::emit("jni");
}
