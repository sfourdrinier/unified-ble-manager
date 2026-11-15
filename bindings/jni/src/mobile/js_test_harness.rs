// bindings/jni/src/mobile/js_test_harness.rs
use super::*;

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_ubm_core_TestJsBridge_nativeEnableSyntheticMarker<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) {
    env.with_env(|_env| -> MobileResult<()> {
        require_host("mobile.test.marker")?.enable_synthetic_marker_for_test();
        Ok(())
    })
    .resolve_with::<ThrowMobile, _>(|| "mobile.test.marker")
}
