#!/bin/sh
# JNI binding test: Rust gates, cdylib build, javac harness, and the real JVM
# exchange through JNI. Fails loudly on any step. Requires a JDK.
set -eu
cd "$(dirname "$0")"

command -v javac >/dev/null 2>&1 || { echo "NO JDK: javac missing (recorded limitation)"; exit 1; }
command -v java >/dev/null 2>&1 || { echo "NO JDK: java missing (recorded limitation)"; exit 1; }

echo "--- jni: Rust gates"
cargo fmt --check
cargo check
cargo clippy --all-targets -- -D warnings
cargo test

echo "--- jni: build cdylib"
cargo build
LIBDIR="$(pwd)/target/debug"
LIB="$LIBDIR/libubm5_jni_echo.so"
test -f "$LIB"

echo "--- jni: compile harness"
rm -rf target/jvm-classes
mkdir -p target/jvm-classes
javac -d target/jvm-classes java/com/ubm/echo/EchoBridge.java java/com/ubm/echo/EchoException.java java/com/ubm/echo/TestEcho.java

echo "--- jni: JVM exchange through JNI"
java -Djava.library.path="$LIBDIR" -cp target/jvm-classes com.ubm.echo.TestEcho

echo "--- jni: versions"
rustc --version
cargo --version
javac -version 2>&1
java -version 2>&1 | head -1
grep -A2 'name = "jni"' Cargo.lock | head -3
