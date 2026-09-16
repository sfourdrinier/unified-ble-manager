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
    const pkg = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
    expect(pkg.files).toContain('android')
    expect(pkg.files).not.toContain('!android/src/main/jniLibs')
    expect(pkg.files).toContain('!android/build')
    for (const abi of ['arm64-v8a', 'x86_64']) {
      expect(
        fs.existsSync(path.join(root, 'android', 'src', 'main', 'jniLibs', abi, 'libubm5_jni_echo.so'))
      ).toBe(true)
    }
    expect(fs.existsSync(path.join(root, 'android', 'src', 'main', 'jniLibs', 'build-identity.txt'))).toBe(
      true
    )
  })

  test('Gradle verifies prebuilts in packed consumers and fails loud without them', () => {
    const buildGradle = fs.readFileSync(path.join(root, 'android/build.gradle'), 'utf8')
    // .git (not source presence) selects source builds: the npm artifact
    // ships sources AND prebuilts, and packed consumers need no NDK/Rust.
    expect(buildGradle).toContain('def ubmRustDevCheckout =')
    expect(buildGradle).toContain('def ubmRustPrebuiltDir = file("src/main/jniLibs")')
    expect(buildGradle).toContain('def ubmRustPrebuiltIdentity =')
    // Packed variants package the committed tree; a packed tree without
    // prebuilts fails LOUD (broken artifact), never silently.
    expect(buildGradle).toContain('android.sourceSets.main.jniLibs.srcDirs = [file("src/main/jniLibs")]')
    expect(buildGradle).toContain('packed-consumer context')
    expect(buildGradle).toContain('no committed prebuilts')
  })
})
