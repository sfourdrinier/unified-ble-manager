#!/bin/sh
# Synthetic ordered group requests through the real Java/JNI/ForeignRadio boundary.
set -eu
cd "$(dirname "$0")/../.."
cargo rustc -p ubm5_jni_echo --lib -- --cfg ubm_jni_roundtrip
mkdir -p bindings/jni/target/jvm-mobile
javac -d bindings/jni/target/jvm-mobile android/src/main/java/com/ubm/core/MobileCoreBridge.java bindings/jni/java/com/ubm/core/TestOrdered.java
java -Djava.library.path="${CARGO_TARGET_DIR:-target}/debug" -cp bindings/jni/target/jvm-mobile com.ubm.core.TestOrdered
