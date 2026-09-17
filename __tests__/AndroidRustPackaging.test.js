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
    // D2(iii): mode selection is explicit-first (UBM_NATIVE_BUILD), never
    // bare `.git` inference; unset backstops inference on BOTH Rust sources
    // AND .git. The npm artifact ships sources AND prebuilts, and packed
    // consumers need no NDK/Rust.
    expect(buildGradle).toContain('def ubmNativeBuildEnv =')
    expect(buildGradle).toContain("UBM_NATIVE_BUILD=source")
    expect(buildGradle).toContain("UBM_NATIVE_BUILD=prebuilt")
    expect(buildGradle).toContain('def ubmRustDevCheckout')
    expect(buildGradle).toContain('ubmRustSourcesPresent && projectDir.toPath().resolve("../.git")')
    expect(buildGradle).toContain('def ubmRustPrebuiltDir = file("src/main/jniLibs")')
    expect(buildGradle).toContain('def ubmRustPrebuiltIdentity =')
    // Packed variants package the committed tree; a packed tree without
    // prebuilts fails LOUD (broken artifact), never silently.
    expect(buildGradle).toContain('android.sourceSets.main.jniLibs.srcDirs = [file("src/main/jniLibs")]')
    expect(buildGradle).toContain('prebuilt context')
    expect(buildGradle).toContain('no committed prebuilts')
    // D2(iii): the 16 KB page-size gate is wired into both paths (hard in
    // source builds, opportunistic --offline-ok over packed prebuilts).
    expect(buildGradle).toContain('def ubmRust16kScript = file("check-elf-16k-pages.sh")')
    expect(buildGradle).toContain('inputs.file(ubmRust16kScript)')
    expect(buildGradle).toContain('16 KB page check')
  })

  // F20: the Gradle input graph must cover the cdylib crate plus its
  // TRANSITIVE path dependencies (from the manifests, not from memory), the
  // workspace manifest + lockfile + toolchain pin, and the build script
  // itself — so a core-only edit invalidates the staged .so. A full
  // Gradle rebuild-identity run needs SDK+NDK (host-gated); this pins the
  // graph structurally: removing `ubm-core/src` from the inputs, or adding
  // a path-dep without declaring it, fails here.
  test('Gradle inputs cover the transitive Rust path-dependency graph', () => {
    const jniDir = path.join(root, 'bindings', 'jni')
    // Repo-relative keys always use forward slashes: path.relative emits
    // backslashes on Windows, which would never match the Gradle-declared
    // (forward-slash) refs or the expected literals below.
    const repoKey = absolute => path.relative(root, absolute).split(path.sep).join('/')
    // Transitive path-deps from the manifests (single-line `{ path = ... }`
    // form, as written in this repo).
    const transitive = new Map() // crate dir (repo-relative) -> manifest path
    const visit = manifestPath => {
      const manifest = fs.readFileSync(manifestPath, 'utf8')
      const dir = path.dirname(manifestPath)
      const depPattern = /^\s*[A-Za-z0-9_-]+\s*=\s*\{[^}\n]*path\s*=\s*"([^"]+)"/gm
      for (const match of manifest.matchAll(depPattern)) {
        const depDir = path.normalize(path.join(dir, match[1]))
        const key = repoKey(depDir)
        if (!transitive.has(key)) {
          transitive.set(key, path.join(depDir, 'Cargo.toml'))
          visit(path.join(depDir, 'Cargo.toml'))
        }
      }
    }
    visit(path.join(jniDir, 'Cargo.toml'))
    expect([...transitive.keys()].sort()).toEqual(['crates/ubm-core', 'crates/ubm-fake-radio'])

    // Declared Gradle inputs: resolve `projectDir`-relative `../...` refs
    // plus the two hoisted variables.
    const buildGradle = fs.readFileSync(path.join(root, 'android/build.gradle'), 'utf8')
    const listBlock = name => {
      const match = buildGradle.match(new RegExp(`def ${name} = \\[([\\s\\S]*?)\\]`))
      expect(match).not.toBeNull()
      return match[1]
    }
    const resolveRef = ref => {
      if (ref === 'ubmRustSrcDir') return 'bindings/jni/src'
      if (ref === 'ubmRustManifest') return 'bindings/jni/Cargo.toml'
      const inline = ref.match(/resolve\("([^"]+)"\)/)
      expect(inline).not.toBeNull()
      return path.normalize(path.join('android', inline[1])).split(path.sep).join('/')
    }
    const declaredDirs = new Set(
      listBlock('ubmRustInputDirs')
        .split('\n')
        .map(line => line.trim().replace(/,$/, ''))
        .filter(line => line.length > 0 && !line.startsWith('//'))
        .map(resolveRef)
    )
    const declaredFiles = new Set(
      listBlock('ubmRustInputFiles')
        .split('\n')
        .map(line => line.trim().replace(/,$/, ''))
        .filter(line => line.length > 0 && !line.startsWith('//'))
        .map(resolveRef)
    )
    // The crate itself plus every transitive path-dep contributes its `src`
    // tree and its manifest.
    const expectedDirs = ['bindings/jni/src', ...[...transitive.keys()].map(key => `${key}/src`)]
    const expectedFiles = [
      'bindings/jni/Cargo.toml',
      ...[...transitive.keys()].map(key => `${key}/Cargo.toml`),
      'Cargo.toml',
      'Cargo.lock',
      'rust-toolchain.toml',
    ]
    for (const dir of expectedDirs) {
      expect([...declaredDirs]).toContain(dir)
    }
    for (const file of expectedFiles) {
      expect([...declaredFiles]).toContain(file)
    }
    // Every declared input must exist on disk: the Gradle guards
    // (`isDirectory`/`isFile`) silently skip missing paths, so a typo
    // would drop graph coverage without failing the build.
    for (const dir of declaredDirs) {
      expect(fs.statSync(path.join(root, dir)).isDirectory()).toBe(true)
    }
    for (const file of declaredFiles) {
      expect(fs.statSync(path.join(root, file)).isFile()).toBe(true)
    }
    // The build script itself is an input (a script fix rebuilds).
    expect(buildGradle).toContain('inputs.file(ubmRustScript)')
    const scriptDef = buildGradle.match(/def ubmRustScript = file\("([^"]+)"\)/)
    expect(scriptDef).not.toBeNull()
    expect(fs.existsSync(path.join(root, 'android', scriptDef[1]))).toBe(true)
  })
})
