const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')

describe('Android Rust cdylib packaging (UBM 5.0 HOST-ANDROID)', () => {
  // F01 supersedes the skip-strategy: the npm artifact ships COMMITTED
  // release prebuilts (src/main/jniLibs/<abi>/libubm5_jni_echo.so plus
  // build-identity.txt) and Gradle VERIFIES them in packed consumers, so
  // the bridge natives LOAD — never skip, never silently. Staged local
  // outputs (android/build) stay excluded.
  test('packed artifact ships the committed jniLibs prebuilts, not staged outputs', () => {
    // PR210-18: the committed identity is JSON (build-identity.json, with
    // sourceDigest) — it replaced build-identity.txt.
    const pkg = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
    expect(pkg.files).toContain('android')
    expect(pkg.files).not.toContain('!android/src/main/jniLibs')
    expect(pkg.files).toContain('!android/build')
    for (const abi of ['arm64-v8a', 'x86_64']) {
      expect(
        fs.existsSync(path.join(root, 'android', 'src', 'main', 'jniLibs', abi, 'libubm5_jni_echo.so'))
      ).toBe(true)
    }
    expect(fs.existsSync(path.join(root, 'android', 'src', 'main', 'jniLibs', 'build-identity.json'))).toBe(
      true
    )
    expect(fs.existsSync(path.join(root, 'android', 'src', 'main', 'jniLibs', 'build-identity.txt'))).toBe(
      false
    )
  })

  test('Gradle verifies prebuilts in packed consumers and fails loud without them', () => {
    const buildGradle = fs.readFileSync(path.join(root, 'android/build.gradle'), 'utf8')
    // PR210-19 (ADR D2.6): the mode is explicit configuration, identical to
    // the podspec: unset/empty -> prebuilt, `prebuilt`/`source` explicit,
    // anything else a GradleException. Previously an unset variable fell
    // back to source mode when Rust sources AND ../.git existed; that
    // inference is gone (these expectations replace the old
    // `ubmRustSourcesPresent && ...("../.git")` / `ubmRustDevCheckout` ones).
    expect(buildGradle).toContain('def ubmNativeBuildMode')
    expect(buildGradle).toContain("UBM_NATIVE_BUILD must be unset, 'prebuilt' or 'source'")
    expect(buildGradle).not.toMatch(/resolve\("\.\.\/\.git"\)/)
    expect(buildGradle).not.toContain('ubmRustDevCheckout')
    expect(buildGradle).not.toContain('toLowerCase')
    expect(buildGradle).toContain('def ubmRustSourceBuild = ubmNativeBuildMode == "source"')
    expect(buildGradle).toContain('def ubmRustPrebuiltDir = file("src/main/jniLibs")')
    expect(buildGradle).toContain('def ubmRustPrebuiltIdentity = new File(ubmRustPrebuiltDir, "build-identity.json")')
    // Packed variants package the committed tree; a packed tree without
    // prebuilts fails LOUD (broken artifact), never silently.
    expect(buildGradle).toContain('android.sourceSets.main.jniLibs.srcDirs = [file("src/main/jniLibs")]')
    expect(buildGradle).toContain('no committed prebuilts')
    // D2(iii): the 16 KB page-size gate is wired into both paths (hard in
    // source builds, opportunistic --offline-ok over packed prebuilts).
    expect(buildGradle).toContain('def ubmRust16kScript = file("check-elf-16k-pages.sh")')
    expect(buildGradle).toContain('inputs.file(ubmRust16kScript)')
    expect(buildGradle).toContain('16 KB page check')
  })

  // F20 + PR210-19: the Gradle input graph is the identity library's input
  // list (the crate, its TRANSITIVE path dependencies parsed from the
  // manifests, the workspace manifest, lockfile, toolchain pin and the JNI
  // binding schema) — no hand-maintained list that can drift from
  // Cargo.toml. Previously build.gradle declared ubmRustInputDirs /
  // ubmRustInputFiles by hand; that list is deleted.
  test('Gradle takes its Rust input graph from the identity library', () => {
    const buildGradle = fs.readFileSync(path.join(root, 'android/build.gradle'), 'utf8')
    expect(buildGradle).not.toContain('def ubmRustInputDirs')
    expect(buildGradle).toContain('scripts/release/native-build-identity.js')
    expect(buildGradle).toContain('"--inputs", "jni"')
    expect(buildGradle).toContain('ubmRustInputFiles.each { input -> inputs.file(input) }')
    const identity = require('../scripts/release/native-build-identity')
    const inputs = identity.bindingSourceInputs(root, 'jni')
    for (const expected of [
      'bindings/jni/Cargo.toml',
      'bindings/jni/src/lib.rs',
      'crates/ubm-core/Cargo.toml',
      'crates/ubm-core/src/lib.rs',
      'crates/ubm-fake-radio/Cargo.toml',
      'Cargo.toml',
      'Cargo.lock',
      'rust-toolchain.toml'
    ]) {
      expect(inputs).toContain(expected)
    }
    // The build script itself is an input (a script fix rebuilds).
    expect(buildGradle).toContain('inputs.file(ubmRustScript)')
    const scriptDef = buildGradle.match(/def ubmRustScript = file\("([^"]+)"\)/)
    expect(scriptDef).not.toBeNull()
    expect(fs.existsSync(path.join(root, 'android', scriptDef[1]))).toBe(true)
  })

  test('the canonical builder seals the identity and exposes a direct prepare step', () => {
    const builder = fs.readFileSync(path.join(root, 'android/build-rust-cdylib.sh'), 'utf8')
    expect(builder).toContain('--write --print-env jni')
    expect(builder).toContain('export UBM_BUILD_SOURCE_DIGEST UBM_BUILD_BINDING_SCHEMA')
    expect(builder).toContain('--prepare')
    expect(builder).toContain('LLVM_NM="$NDK/toolchains/llvm/prebuilt/$HOST_TAG/bin/llvm-nm"')
    expect(builder).toContain('"$LLVM_NM" -D --defined-only "$BUILT"')
    expect(builder).not.toMatch(/(^|\s)nm -D --defined-only/u)
    expect(builder).toContain('case "${CARGO_TARGET_DIR:-}" in')
    expect(builder).toContain('") CARGO_TARGET_ROOT="$ROOT/target" ;;')
    expect(builder).toContain('/*) CARGO_TARGET_ROOT="$CARGO_TARGET_DIR" ;;')
    expect(builder).toContain('*) CARGO_TARGET_ROOT="$ROOT/$CARGO_TARGET_DIR" ;;')
    expect(builder).toContain('BUILT="$CARGO_TARGET_ROOT/$TARGET/$PROFILE/$LIB"')
    expect(builder).not.toContain('BUILT="$ROOT/target/$TARGET/$PROFILE/$LIB"')
    expect(builder).toContain('PINNED_RUSTC="$(rustup which --toolchain "$PINNED_TOOLCHAIN" rustc)"')
    expect(builder).toContain('RUSTC="$PINNED_RUSTC"')
    const pkg = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
    expect(pkg.scripts['native:android:prepare']).toBe('sh android/build-rust-cdylib.sh --prepare')
    const refresh = fs.readFileSync(path.join(root, 'android/refresh-prebuilt-jniLibs.sh'), 'utf8')
    expect(refresh).toContain('--write-android-identity')
    expect(refresh).toContain('--check-android-prebuilts')
    expect(refresh).not.toContain('build-identity.txt')
  })
})
