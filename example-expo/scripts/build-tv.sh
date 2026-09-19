#!/usr/bin/env bash
# example-expo/scripts/build-tv.sh — Apple TV (tvOS) variant of example-expo.
#
# One source tree, not a fork: this stages a generated copy of example-expo at
# example-expo/ios-tv (gitignored, like ios/ and android/), applies only the
# TV build inputs (react-native-tvos alias, @react-native-tvos/config-tv,
# Metro shared-driver path), and prebuilds with EXPO_TV=1 so the native
# project targets tvOS.
#
# It NEVER touches example-expo/ios or example-expo/android: the phones keep
# building from those directories while the TV builds from ios-tv/.
#
# TV dependency versions (pinned here, sources in docs):
# - react-native via npm:react-native-tvos@0.86-stable (== 0.86.3-0), the
#   tvOS fork release matching Expo SDK 57 / React Native 0.86.3.
#   Source: Expo guide "Build Expo apps for TV" (SDK-version match rule) and
#   the 0.86-stable dist-tag on npm.
# - @react-native-tvos/config-tv pinned below (peer: expo >= 52).
#   Source: npm @react-native-tvos/config-tv dist-tags (latest).
#
# Usage:
#   bash example-expo/scripts/build-tv.sh stage     # rsync sources -> ios-tv, apply TV inputs, drop the staged library copy
#   bash example-expo/scripts/build-tv.sh install   # pnpm install in ios-tv (re-resolves the library from the repo)
#   bash example-expo/scripts/build-tv.sh verify-identity # staged library identity equals the repo (finding 176)
#   bash example-expo/scripts/build-tv.sh prebuild  # EXPO_TV=1 expo prebuild --platform ios + pod install
#   bash example-expo/scripts/build-tv.sh bundle-url # point staged AppDelegate at the TV Metro (TV_METRO_PORT wins)
#   bash example-expo/scripts/build-tv.sh build     # Debug .app for a real Apple TV (needs DEVELOPMENT_TEAM)
#   bash example-expo/scripts/build-tv.sh metro     # serve the staged TV bundle (TV_METRO_PORT)
#   bash example-expo/scripts/build-tv.sh all       # stage..build
#
# Env (defaults match this repo's LAN setup; the phone Metro stays on 8082,
# the TV tree gets its own Metro because it resolves react-native-tvos):
#   TV_METRO_PORT=8081 TV_LAN_HOST=192.168.68.116 DEVELOPMENT_TEAM=<team> (build only)
#
# Signing: DEVELOPMENT_TEAM is passed on the xcodebuild command line only and
# is never written into any file.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
ROOT="$(cd "${APP_DIR}/.." && pwd)"
STAGE="${TV_STAGE_DIR:-${APP_DIR}/ios-tv}"

TV_METRO_PORT="${TV_METRO_PORT:-8081}"
TV_LAN_HOST="${TV_LAN_HOST:-192.168.68.116}"
TVOS_ALIAS="${TVOS_ALIAS:-npm:react-native-tvos@0.86-stable}"
CONFIG_TV_VERSION="${CONFIG_TV_VERSION:-0.1.6}"
# The Apple TV this repo builds for ("Office", Apple TV 4K 3rd gen).
TV_DEVICE_ID="${TV_DEVICE_ID:-27C3EE87-9EB5-54C1-8CAB-52D33CB077C9}"
TV_BUNDLE_ID="${TV_BUNDLE_ID:-com.sfourdrinier.bleplxexample}"

if [[ -n "${TV_STAGE_DIR:-}" ]]; then
  # Test/CI override: an absolute tmp dir, never the real tree.
  _tv_tmp="${TMPDIR:-/tmp}"
  _tv_tmp="${_tv_tmp%/}"
  case "${STAGE}" in
    /tmp/*|"$_tv_tmp"/*) ;;
    *) echo "error: TV_STAGE_DIR must be an absolute tmp dir (${STAGE})" >&2; exit 1 ;;
  esac
  unset _tv_tmp
elif [[ "${STAGE}" != "${APP_DIR}"/* ]]; then
  echo "error: stage dir escaped the app dir (${STAGE})" >&2
  exit 1
fi

cmd_stage() {
  mkdir -p "${STAGE}"
  # Sources only: the stage owns its node_modules (tvos alias) and its ios/
  # (tvOS prebuild). Excluded entries are protected from --delete, so a
  # re-stage never wipes a previous prebuild or install.
  rsync -a --delete \
    --exclude '/node_modules' \
    --exclude '/ios' \
    --exclude '/ios-tv' \
    --exclude '/android' \
    --exclude '/.expo' \
    --exclude '/dist' \
    --exclude '/web-build' \
    "${APP_DIR}/" "${STAGE}/"

  # Finding 176: pnpm reuses a present `file:` dependency directory, so a
  # re-stage must drop the staged unified-ble-manager copy. The next
  # `install` then resolves it fresh from the current repo instead of
  # keeping a stale build that fails closed with protocol.incompatible
  # native-identity.
  rm -rf "${STAGE}/node_modules/unified-ble-manager"

  # TV build inputs, applied to the staged copy only.
  node -e '
    const fs = require("node:fs");
    const path = require("node:path");
    const stage = process.argv[1];
    const root = process.argv[2];
    const tvosAlias = process.argv[3];
    const configTv = process.argv[4];
    const pkgPath = path.join(stage, "package.json");
    const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf8"));
    pkg.dependencies["react-native"] = tvosAlias;
    pkg.dependencies["unified-ble-manager"] = `file:${root}`;
    pkg.devDependencies = pkg.devDependencies ?? {};
    pkg.devDependencies["@react-native-tvos/config-tv"] = configTv;
    fs.writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + "\n");
    const appPath = path.join(stage, "app.json");
    const app = JSON.parse(fs.readFileSync(appPath, "utf8"));
    app.expo.plugins = app.expo.plugins ?? [];
    if (!app.expo.plugins.includes("@react-native-tvos/config-tv")) {
      app.expo.plugins.push("@react-native-tvos/config-tv");
    }
    fs.writeFileSync(appPath, JSON.stringify(app, null, 2) + "\n");
  ' "${STAGE}" "${ROOT}" "${TVOS_ALIAS}" "${CONFIG_TV_VERSION}"

  # The staged Metro still serves the shared driver, but from the repo path:
  # a relative ../examples-shared would resolve inside example-expo.
  python3 - "${STAGE}/metro.config.js" "${ROOT}/examples-shared" <<'EOF'
import sys
path, shared = sys.argv[1], sys.argv[2]
text = open(path).read()
needle = "path.resolve(projectRoot, '../examples-shared')"
assert needle in text, "metro shared-driver anchor not found"
open(path, "w").write(text.replace(needle, f"path.resolve('{shared}')"))
EOF
  # The stage sits one level deeper than example-expo, so the staged
  # src/driver/shared.ts must climb one more level to reach the same
  # shared driver. Without this both tsc and the TV Metro bundle resolve a
  # path that does not exist.
  python3 - "${STAGE}/src/driver/shared.ts" <<'EOF'
import sys
path = sys.argv[1]
text = open(path).read()
needle = "from '../../../examples-shared/driver/index.ts'"
assert needle in text, "shared.ts examples-shared anchor not found"
open(path, "w").write(text.replace(needle, "from '../../../../examples-shared/driver/index.ts'", 1))
EOF
  echo "staged TV app at ${STAGE}"
}

cmd_install() {
  # Packing the unified-ble-manager file: dep enumerates the whole checkout
  # (which currently holds hundreds of thousands of build-output files), so
  # the resolver needs heap headroom. No tree state is changed by this.
  (cd "${STAGE}" && NODE_OPTIONS=--max-old-space-size=8192 pnpm install --no-frozen-lockfile)
}

cmd_verify_identity() {
  # Finding 176: the staged build identity must equal the repo's, or the TV
  # app fails closed at runtime with protocol.incompatible native-identity.
  # Check it here — right after install — instead of on the Apple TV.
  local staged_lib="${STAGE}/node_modules/unified-ble-manager"
  local staged_identity="${staged_lib}/src/generated/native-build-identity.ts"
  local repo_identity="${ROOT}/src/generated/native-build-identity.ts"
  if [[ ! -f "${staged_identity}" ]]; then
    echo "error: staged unified-ble-manager has no src/generated/native-build-identity.ts (${staged_identity}): run install first" >&2
    exit 1
  fi
  if ! cmp -s "${repo_identity}" "${staged_identity}"; then
    echo "error: staged unified-ble-manager build identity is stale (diff ${repo_identity} ${staged_identity}): re-run stage, then install" >&2
    exit 1
  fi
  local repo_version staged_version
  repo_version="$(node -e "console.log(require('${ROOT}/package.json').version)")"
  staged_version="$(node -e "console.log(require('${staged_lib}/package.json').version)")"
  if [[ "${repo_version}" != "${staged_version}" ]]; then
    echo "error: staged unified-ble-manager version ${staged_version} != repo ${repo_version}: re-run stage, then install" >&2
    exit 1
  fi
  echo "staged unified-ble-manager identity matches the repo (version ${repo_version})"
}

cmd_prebuild() {
  (cd "${STAGE}" && EXPO_TV=1 npx expo prebuild --platform ios --clean --no-install)
  # react-native-tvos ships React-Core as a prebuilt tarball that pod install
  # caches under ~/Library/Caches/ReactNative (not writable from here), so
  # fetch it once to a writable cache and hand pod install the local file via
  # the fork's own RCT_TESTONLY_RNCORE_TARBALL_PATH switch. Debug tarball: this
  # script only builds Debug.
  local tv_version tarball_name tarball_url tarball_path
  tv_version="$(node -e "console.log(require('${STAGE}/node_modules/react-native/package.json').version)")"
  tarball_name="reactnative-core-${tv_version}-debug.tar.gz"
  tarball_url="https://repo1.maven.org/maven2/io/github/react-native-tvos/react-native-artifacts/${tv_version}/react-native-artifacts-${tv_version}-reactnative-core-debug.tar.gz"
  tarball_path="/tmp/tv-prebuilt-cache/${tarball_name}"
  mkdir -p "$(dirname "${tarball_path}")"
  if [[ ! -f "${tarball_path}" ]]; then
    curl -sSL -o "${tarball_path}" "${tarball_url}"
    curl -sSL "${tarball_url}.sha1" -o "${tarball_path}.sha1"
  fi
  # CocoaPods' home (~/.cocoapods) and download cache
  # (~/Library/Caches/CocoaPods) are not writable from here either.
  export CP_HOME_DIR="/tmp/tv-prebuilt-cache/cocoapods-home"
  export CP_CACHE_DIR="${CP_HOME_DIR}/cache"
  mkdir -p "${CP_HOME_DIR}"
  (cd "${STAGE}/ios" && RCT_TESTONLY_RNCORE_TARBALL_PATH="${tarball_path}" pod install)
}

cmd_bundle_url() {
  local delegate
  delegate="$(find "${STAGE}/ios" -maxdepth 2 -name AppDelegate.swift | head -1)"
  if [[ -z "${delegate}" ]]; then
    echo "error: no AppDelegate.swift under ${STAGE}/ios (run prebuild first)" >&2
    exit 1
  fi
  # Same mechanism as the phone build (example-expo/ios AppDelegate bundle URL
  # override), pointed at the TV Metro: the staged tree resolves
  # react-native-tvos, so it needs its own packager, while the driver server
  # stays shared on 8795 (derived from the bundle host).
  # Finding 176: TV_METRO_PORT wins every time. A stale override from a
  # previous run (for example the default 8081) is replaced, never kept.
  if grep -q 'jsLocation = "' "${delegate}"; then
    if grep -q "jsLocation = \"${TV_LAN_HOST}:${TV_METRO_PORT}\"" "${delegate}"; then
      echo "bundle URL override already present in ${delegate}"
      return 0
    fi
    python3 - "${delegate}" "${TV_LAN_HOST}:${TV_METRO_PORT}" <<'EOF'
import sys
path, location = sys.argv[1], sys.argv[2]
import re
text = open(path).read()
updated, count = re.subn(r'jsLocation = "[^"]*"', f'jsLocation = "{location}"', text, count=1)
assert count == 1, "bundle URL override not found"
open(path, "w").write(updated)
EOF
    echo "bundle URL override -> ${TV_LAN_HOST}:${TV_METRO_PORT} in ${delegate}"
    return 0
  fi
  python3 - "${delegate}" "${TV_LAN_HOST}:${TV_METRO_PORT}" <<'EOF'
import sys
path, location = sys.argv[1], sys.argv[2]
text = open(path).read()
anchor = "return RCTBundleURLProvider.sharedSettings().jsBundleURL(forBundleRoot:"
assert anchor in text, "AppDelegate bundleURL anchor not found"
insert = f'    RCTBundleURLProvider.sharedSettings().jsLocation = "{location}"\n    '
open(path, "w").write(text.replace(anchor, insert + anchor, 1))
EOF
  echo "bundle URL override -> ${TV_LAN_HOST}:${TV_METRO_PORT} in ${delegate}"
}

xcode_scheme() {
  local schemes scheme
  schemes="$(xcodebuild -list -json -project "${STAGE}/ios/"*.xcodeproj 2>/dev/null | python3 -c 'import json,sys; print("\n".join(json.load(sys.stdin)["project"]["schemes"]))')"
  scheme="$(printf '%s\n' "${schemes}" | grep -i -m1 'tv' || true)"
  if [[ -z "${scheme}" ]]; then
    scheme="$(printf '%s\n' "${schemes}" | head -1)"
  fi
  printf '%s' "${scheme}"
}

cmd_build() {
  if [[ -z "${DEVELOPMENT_TEAM:-}" ]]; then
    echo "error: DEVELOPMENT_TEAM is required (passed on the command line only)" >&2
    exit 1
  fi
  local scheme
  scheme="$(xcode_scheme)"
  echo "building scheme ${scheme} for Apple TV"
  (cd "${STAGE}/ios" && xcodebuild \
    -workspace ./*.xcworkspace \
    -scheme "${scheme}" \
    -configuration Debug \
    -destination 'generic/platform=tvOS' \
    -derivedDataPath build/tv-device \
    -allowProvisioningUpdates \
    "DEVELOPMENT_TEAM=${DEVELOPMENT_TEAM}" \
    build)
}

cmd_metro() {
  (cd "${STAGE}" && npx expo start --port "${TV_METRO_PORT}")
}

cmd_install_tv() {
  local app
  app="$(find "${STAGE}/ios/build/tv-device" -maxdepth 4 -name '*.app' -type d | head -1)"
  if [[ -z "${app}" ]]; then
    echo "error: no .app under ${STAGE}/ios/build/tv-device (run build first)" >&2
    exit 1
  fi
  xcrun devicectl device install app --device "${TV_DEVICE_ID}" "${app}"
}

cmd_launch_tv() {
  xcrun devicectl device process launch --device "${TV_DEVICE_ID}" "${TV_BUNDLE_ID}"
}

case "${1:-all}" in
  stage) cmd_stage ;;
  install) cmd_install ;;
  verify-identity) cmd_verify_identity ;;
  prebuild) cmd_prebuild ;;
  bundle-url) cmd_bundle_url ;;
  build) cmd_build ;;
  metro) cmd_metro ;;
  install-tv) cmd_install_tv ;;
  launch-tv) cmd_launch_tv ;;
  all) cmd_stage; cmd_install; cmd_verify_identity; cmd_prebuild; cmd_bundle_url; cmd_build ;;
  *) echo "usage: $0 [stage|install|verify-identity|prebuild|bundle-url|build|metro|install-tv|launch-tv|all]" >&2; exit 1 ;;
esac
