#!/bin/sh
# JNI binding test: Rust gates, cdylib build, javac harness, and the real JVM
# exchange through JNI. Fails loudly on any step. Requires a JDK.
# Workspace layout: this crate is a member of the root workspace, so the
# cdylib lands in the workspace target dir and the lockfile is the unified
# root Cargo.lock.
set -eu
cd "$(dirname "$0")"

ROOT="../.."

command -v javac >/dev/null 2>&1 || { echo "NO JDK: javac missing (recorded limitation)"; exit 1; }
command -v java >/dev/null 2>&1 || { echo "NO JDK: java missing (recorded limitation)"; exit 1; }

echo "--- jni: production guards (L1 safe-Rust + L2 doc names)"
python3 "$ROOT/bindings/guard_wiring.py"

echo "--- jni: Rust gates"
cargo fmt --check
cargo check -p ubm5_jni_echo --locked
cargo clippy -p ubm5_jni_echo --all-targets --locked -- -D warnings
cargo test -p ubm5_jni_echo --locked

echo "--- jni: build cdylib"
cargo build -p ubm5_jni_echo --locked
LIBDIR="$(pwd)/$ROOT/target/debug"
LIB="$LIBDIR/libubm5_jni_echo.so"
test -f "$LIB"

echo "--- jni: compile harness"
rm -rf target/jvm-classes
mkdir -p target/jvm-classes
javac -d target/jvm-classes java/com/ubm/echo/EchoBridge.java java/com/ubm/echo/EchoException.java java/com/ubm/echo/TestEcho.java java/com/ubm/gatt/GattBridge.java java/com/ubm/gatt/TestGatt.java

echo "--- jni: JVM exchange through JNI"
java -Djava.library.path="$LIBDIR" -cp target/jvm-classes com.ubm.echo.TestEcho

echo "--- jni: HOST-ANDROID GATT bridge exchange through JNI"
java -Djava.library.path="$LIBDIR" -cp target/jvm-classes com.ubm.gatt.TestGatt

echo "--- jni: versions"
rustc --version
cargo --version
javac -version 2>&1
java -version 2>&1 | head -1
grep -A2 'name = "jni"' "$ROOT/Cargo.lock" | head -3
