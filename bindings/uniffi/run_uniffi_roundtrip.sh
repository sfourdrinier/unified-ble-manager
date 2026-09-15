#!/bin/sh
# UniFFI binding test: Rust gates, scaffolding proof, pinned codegen for
# Kotlin/Swift/Python, reproducibility diff, and the real Python exchange
# through the generated scaffolding. Fails loudly on any step.
set -eu
cd "$(dirname "$0")"

BINDGEN="${UNIFFI_BINDGEN:-/tmp/ubm-tools/bin/uniffi-bindgen}"
if ! [ -x "$BINDGEN" ]; then
  echo "installing pinned uniffi-bindgen 0.32.1"
  cargo install uniffi --version =0.32.1 --features cli --root /tmp/ubm-tools --locked
fi
"$BINDGEN" --version

echo "--- uniffi: Rust gates"
cargo fmt --check
cargo check
cargo clippy --all-targets -- -D warnings
cargo test

echo "--- uniffi: build cdylib + generate bindings (pinned codegen)"
cargo build
LIB=target/debug/libubm5_uniffi_echo.so
REGEN=target/regen-bindings
rm -rf "$REGEN"
mkdir -p "$REGEN/kotlin" "$REGEN/swift" "$REGEN/python"
"$BINDGEN" generate --library "$LIB" --language kotlin --out-dir "$REGEN/kotlin"
"$BINDGEN" generate --library "$LIB" --language swift --out-dir "$REGEN/swift"
"$BINDGEN" generate --library "$LIB" --language python --out-dir "$REGEN/python"

echo "--- uniffi: reproducibility (regen must match committed recipe)"
diff -r generated/kotlin "$REGEN/kotlin" && echo "kotlin reproducible"
diff -r generated/swift "$REGEN/swift" && echo "swift reproducible"
diff -r generated/python "$REGEN/python" && echo "python reproducible"

echo "--- uniffi: Python exchange through the generated scaffolding"
PYRUN=target/pyrun
rm -rf "$PYRUN"
mkdir -p "$PYRUN"
cp "$REGEN/python/ubm_echo.py" "$PYRUN/"
cp "$LIB" "$PYRUN/"
python3 tests/python_roundtrip.py "$PYRUN"

echo "--- uniffi: versions"
rustc --version
cargo --version
python3 --version
grep -A2 'name = "uniffi"' Cargo.lock | head -3
