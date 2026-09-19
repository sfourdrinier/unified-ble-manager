#!/bin/sh
# JVM exchange through the real JNI mobile surface (com.ubm.core.MobileCoreBridge).
# Builds the cdylib, compiles the facade + TestMobile, runs it. Requires a JDK.
set -eu
cd "$(dirname "$0")"
ROOT="../.."
command -v javac >/dev/null 2>&1 || { echo "NO JDK: javac missing"; exit 1; }
cargo build -p ubm5_jni_echo --locked
LIBDIR="$(pwd)/$ROOT/target/debug"
rm -rf target/jvm-mobile
mkdir -p target/jvm-mobile
javac -d target/jvm-mobile java/com/ubm/core/MobileCoreBridge.java java/com/ubm/core/TestMobile.java
java -Djava.library.path="$LIBDIR" -cp target/jvm-mobile com.ubm.core.TestMobile
