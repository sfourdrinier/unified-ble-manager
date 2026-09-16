const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')

describe('Android Rust cdylib packaging (UBM 5.0 HOST-ANDROID)', () => {
  test('packed artifact excludes staged jniLibs outputs', () => {
    const pkg = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
    expect(pkg.files).toContain('!android/src/main/jniLibs')
  })

  test('Gradle cdylib task skips loudly without Rust sources (packed consumers)', () => {
    const buildGradle = fs.readFileSync(path.join(root, 'android/build.gradle'), 'utf8')
    // Presence probe computed once, outside the task (projectDir-relative:
    // rootDir is example/android when the library is included as a module).
    expect(buildGradle).toContain('def ubmRustSourcesPresent =')
    expect(buildGradle).toContain('projectDir.toPath().resolve("../bindings/jni/src")')
    // Rust source inputs are conditional; the task SKIPs (never fails
    // validation) in packed-consumer checkouts that have no bindings/.
    expect(buildGradle).toContain('if (ubmRustSourcesPresent) {')
    expect(buildGradle).toContain('buildUbmRustCdylib SKIPPED')
    expect(buildGradle).toContain('packed-consumer context')
    // The skip decision itself is a tracked input so context switches rerun.
    expect(buildGradle).toContain('inputs.property("ubmRustSourcesPresent"')
  })
})
