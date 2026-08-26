#!/usr/bin/env bash
# Run the Linux CI jobs against a CLEAN checkout, outside the working tree,
# in parallel, with shared caches. A pre-push filter, not a CI replacement.
#
#   scripts/ci/preflight.sh                 # HEAD
#   scripts/ci/preflight.sh origin/main     # any ref
#   scripts/ci/preflight.sh --serial        # one job at a time (readable logs)
#   scripts/ci/preflight.sh --keep          # leave the workspace for inspection
#
# WHY A SEPARATE CHECKOUT. CI runs `actions/checkout`, i.e. a pristine tree at
# one commit. Running the same commands in your working tree tests something
# else: uncommitted edits, stale build output, untracked files that a fresh
# clone would not have. Every "passes locally, fails in CI" starts here. This
# creates a detached worktree under the cache dir, runs there, and removes it.
#
# WHY IT IS FAST. The worktree shares the object store (no clone), and the two
# expensive caches are shared across runs rather than rebuilt per checkout:
# ~2.8 GB of Cargo target output and the pnpm content-addressed store. The two
# independent jobs then run concurrently.
#
# WHAT IT DOES NOT COVER, and why you still push:
#   - the windows-latest and macos-latest legs of `package` and `tauri-plugin`
#   - the `apple` job entirely
#   - CoreBluetooth and WinRT native boundaries (OS-gated)
#   - the Node version matrix (CI pins 22 and 24; this uses your local Node)
#   - the runner OS: hosted `ubuntu-latest` is 24.04, this box is newer
# Treat green here as "worth pushing", never as "CI will pass".

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/ubm-preflight"
REF="HEAD"
SERIAL=0
KEEP=0
FAST=0

for arg in "$@"; do
  case "$arg" in
    --serial) SERIAL=1 ;;
    --fast) FAST=1 ;;
    --keep) KEEP=1 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    -*) echo "unknown flag: $arg" >&2; exit 2 ;;
    *) REF="$arg" ;;
  esac
done

cd "$REPO" || exit 1
SHA="$(git rev-parse --short "$REF" 2>/dev/null)" || { echo "unknown ref: $REF" >&2; exit 2; }
WORK="$CACHE/tree-$SHA"

# Shared across runs: rebuilding 2.8 GB of Cargo output per checkout is what
# makes a clean-tree run feel expensive, and it is pure waste - the artefacts
# are keyed by content, not by directory.
export CARGO_TARGET_DIR="$CACHE/cargo-target"
mkdir -p "$CARGO_TARGET_DIR"

cleanup() {
  [ "$KEEP" -eq 1 ] && { echo "workspace kept: $WORK"; return; }
  git -C "$REPO" worktree remove --force "$WORK" >/dev/null 2>&1
}
trap cleanup EXIT

printf '\033[1mpreflight\033[0m %s (%s)\n' "$REF" "$SHA"
echo "  workspace: $WORK"
echo "  cargo target (shared): $CARGO_TARGET_DIR"

git worktree remove --force "$WORK" >/dev/null 2>&1
git worktree add --detach "$WORK" "$SHA" >/dev/null 2>&1 || {
  echo "could not create a clean worktree at $WORK" >&2; exit 1; }

cd "$WORK" || exit 1
corepack enable >/dev/null 2>&1

started=$(date +%s)
echo
printf '\033[1m▶ install (frozen lockfile)\033[0m\n'
if ! pnpm install --frozen-lockfile > "$CACHE/install.log" 2>&1; then
  tail -20 "$CACHE/install.log"; echo "install failed"; exit 1
fi
printf '  ✓ %ss\n' "$(( $(date +%s) - started ))"

# The `package` and `tauri-plugin` jobs share no state, exactly as in CI where
# they are separate jobs, so running them concurrently is faithful rather than
# a shortcut.
run_package() {
  cd "$WORK" || return 1
  set -e
  pnpm test:package
  pnpm validate:evidence
  pnpm test:plugin
  pnpm test:native-protocol
  pnpm lint
  pnpm prepack
  pnpm run docs:check
  pnpm build:example:web
  pnpm performance:check
  pnpm release:artifacts:check
  node example-electron/smoke.js
  node scripts/ci/check-host-exports.js
  node scripts/ci/bluez-soft-probe.js
  node scripts/ci/pack-install-smoke.js
  node scripts/ci/packed-host-consumer-check.js
  node scripts/ci/g6a-packed-consumer-proof.js
}

run_tauri() {
  cd "$WORK" || return 1
  set -e
  cargo fmt --manifest-path native/tauri/Cargo.toml -- --check
  cargo test --manifest-path native/tauri/Cargo.toml
  cargo clippy --manifest-path native/tauri/Cargo.toml -- -D warnings
  cargo check --manifest-path example-tauri/src-tauri/Cargo.toml
}

# The two Android jobs are the slowest in CI and fully reproducible here, so
# they are the ones worth catching before a push. They need the Android SDK and
# a JDK; without them the run still reports the rest rather than failing.
run_android() {
  cd "$WORK" || return 1
  set -e
  export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
  export ANDROID_SDK_ROOT="$ANDROID_HOME"

  # classic-rn-android
  pnpm --dir example install --no-frozen-lockfile
  test -d example/node_modules/@react-native/gradle-plugin
  pnpm test:native-protocol:android
  (cd example/android && ./gradlew :app:assembleDebug --no-daemon --console=plain \
      -PreactNativeArchitectures=arm64-v8a)

  # expo-cng-android
  pnpm --dir example-expo install --no-frozen-lockfile
  (cd example-expo && npx expo install --fix)
  pnpm --dir example-expo exec tsc --noEmit -p tsconfig.json
  (cd example-expo && NODE_ENV=development npx expo prebuild --clean --no-install)
  (cd example-expo/android && NODE_ENV=development ./gradlew :app:assembleDebug \
      --no-daemon --console=plain)
}

android_ready() {
  [ -d "${ANDROID_HOME:-$HOME/Android/Sdk}" ] && command -v java >/dev/null 2>&1
}

pkg_started=$(date +%s)
if [ "$SERIAL" -eq 1 ]; then
  run_package > "$CACHE/package.log" 2>&1; pkg_status=$?
  pkg_elapsed=$(( $(date +%s) - pkg_started ))
  tau_started=$(date +%s)
  run_tauri > "$CACHE/tauri.log" 2>&1; tau_status=$?
  tau_elapsed=$(( $(date +%s) - tau_started ))
else
  printf '\033[1m▶ package + tauri-plugin (parallel)\033[0m\n'
  run_package > "$CACHE/package.log" 2>&1 &
  pkg_pid=$!
  run_tauri > "$CACHE/tauri.log" 2>&1 &
  tau_pid=$!
  wait "$pkg_pid"; pkg_status=$?
  wait "$tau_pid"; tau_status=$?
  pkg_elapsed=$(( $(date +%s) - pkg_started ))
  tau_elapsed=$pkg_elapsed
fi

and_started=$(date +%s)
if [ "$FAST" -eq 1 ]; then
  and_status=-1
elif ! android_ready; then
  and_status=-2
else
  printf '\033[1m▶ android (classic RN + Expo CNG)\033[0m\n'
  run_android > "$CACHE/android.log" 2>&1; and_status=$?
fi
and_elapsed=$(( $(date +%s) - and_started ))

report() {
  local name="$1" status="$2" elapsed="$3" log="$4"
  if [ "$status" -eq 0 ]; then
    printf '\033[32m  ✓ %-16s %ss\033[0m\n' "$name" "$elapsed"
  else
    printf '\033[31m  ✗ %-16s %ss  (exit %s)\033[0m\n' "$name" "$elapsed" "$status"
    tail -25 "$log" | sed 's/^/      /'
    printf '      full log: %s\n' "$log"
  fi
}

echo
printf '\033[1m── summary ──\033[0m\n'
report package "$pkg_status" "$pkg_elapsed" "$CACHE/package.log"
report tauri-plugin "$tau_status" "$tau_elapsed" "$CACHE/tauri.log"
case "$and_status" in
  -1) printf '\033[33m  – %-16s skipped (--fast)\033[0m\n' "android" ;;
  -2) printf '\033[33m  – %-16s skipped (no Android SDK or JDK)\033[0m\n' "android" ;;
  *) report android "$and_status" "$and_elapsed" "$CACHE/android.log" ;;
esac
printf '  total %ss\n' "$(( $(date +%s) - started ))"
echo
echo "Not covered: windows/macos legs, the apple job, CoreBluetooth and WinRT"
echo "boundaries, the Node matrix, and the runner OS. Push for those."

[ "$pkg_status" -eq 0 ] && [ "$tau_status" -eq 0 ] && [ "$and_status" -le 0 ]
