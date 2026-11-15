#!/bin/sh
# Test-only JS adapter → JNI → synthetic MobileSession marker path.
set -eu
cd "$(dirname "$0")/../.."
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/ubm-js-jni.XXXXXX")
trap 'rm -rf "$test_dir"' EXIT HUP INT TERM
build_dir="${CARGO_TARGET_DIR:-$test_dir/build}"
CARGO_TARGET_DIR="$build_dir" cargo build -p ubm5_jni_echo --features synthetic-marker-harness
mkdir -p "$test_dir/classes"
javac -d "$test_dir/classes" android/src/main/java/com/ubm/core/MobileCoreBridge.java bindings/jni/java/com/ubm/core/TestMobile.java bindings/jni/java/com/ubm/core/TestJsBridge.java
UBM_JS_JNI_LIBRARY="$build_dir/debug" UBM_JS_JNI_CLASSES="$test_dir/classes" node bindings/jni/js_marker_roundtrip.js
