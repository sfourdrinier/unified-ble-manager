# android/consumer-rules.pro
#
# Consumer R8/ProGuard rules, applied automatically to any app that links this
# library with minification enabled. The Rust cdylib (libubm5_jni_echo.so) and
# the C++ protocol library (libunified_ble_native_protocol.so) resolve Java
# classes and members by name through JNI, so shrinking or renaming any of
# them fails at runtime (for example `platform.failure|platform|
# mobile.host.install|jni-call-failed`). The scan in
# __tests__/AndroidConsumerKeepRules.test.js keeps this file aligned with the
# shipped binaries and JNI sources.

# Rust JNI bridges: native entry points are exported as Java_com_ubm_*, the
# libraries throw com/ubm/echo/EchoException and
# com/ubm/core/MobileCoreBridge$MobileCoreException by name, and they call the
# RadioHost / WakeListener / InvokeCallback methods by name.
-keep class com.ubm.** { *; }

# Host callbacks the Rust core invokes by method name on the objects it is
# handed (implemented outside com.ubm).
-keepclassmembers class * implements com.ubm.core.MobileCoreBridge$RadioHost { <methods>; }
-keepclassmembers class * implements com.ubm.core.MobileCoreBridge$WakeListener { <methods>; }
-keepclassmembers class * implements com.ubm.core.MobileCoreBridge$InvokeCallback { <methods>; }

# C++ protocol library: FindClass + GetStaticMethodID on the JSI binding
# (close, dispatchNative, emitCurrentAdapterState).
-keep class com.sfourdrinier.unifiedblemanager.protocol.UnifiedBleProtocolJsiBinding {
    static void close(long);
    static void dispatchNative(long, byte[]);
    static void emitCurrentAdapterState(long);
}

# Every JNI-registered native method keeps its name (Java_<class>_<method>).
-keepclasseswithmembernames class com.ubm.**, com.sfourdrinier.unifiedblemanager.** {
    native <methods>;
}
