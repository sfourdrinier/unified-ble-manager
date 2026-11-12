#!/bin/sh
# Synthetic JNI v2 descriptor and completion round trip; does not use a radio.
set -eu
cd "$(dirname "$0")/../.."
cargo rustc --offline -p ubm5_jni_echo --lib -- --cfg ubm_jni_roundtrip
fixture_dir="${TMPDIR:-/tmp}/ubm-slot-wire-jvm-$$"
trap 'rm -rf "$fixture_dir"' EXIT HUP INT TERM
mkdir -p "$fixture_dir"
javac -d "$fixture_dir" android/src/main/java/com/ubm/core/MobileCoreBridge.java bindings/jni/java-slot/com/ubm/core/TestSlotWire.java
java -Djava.library.path="${CARGO_TARGET_DIR:-target}/debug" -cp "$fixture_dir" com.ubm.core.TestSlotWire
