// __tests__/NativeBuildIdentity.test.js
//
// PR210-18: one implementation of native build identity. The source digest
// binds each binding crate to its transitive path dependencies (parsed from
// Cargo.toml, never a hand-maintained list), its build script, the shared
// identity helper, the workspace manifest, the lockfile and the toolchain
// pin. The binding schema binds the wrapper-side declaration files that must
// match the compiled binary (closes T1). Staging checks bind the Apple
// XCFramework and the committed Android jniLibs to those digests.

'use strict'

const crypto = require('crypto')
const fs = require('fs')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

const identity = require('../scripts/release/native-build-identity')

const repoRoot = path.join(__dirname, '..')
const cli = path.join(repoRoot, 'scripts', 'release', 'native-build-identity.js')

function writeFile(root, relative, content) {
  const absolute = path.join(root, relative)
  fs.mkdirSync(path.dirname(absolute), { recursive: true })
  fs.writeFileSync(absolute, content)
  return absolute
}

function sha256(content) {
  return crypto.createHash('sha256').update(content).digest('hex')
}

function fixtureRoot() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-identity-'))
  writeFile(
    root,
    'Cargo.toml',
    '[workspace]\nmembers = ["crates/*", "bindings/*"]\n\n[workspace.dependencies]\nubm-shared = { path = "crates/ubm-shared" }\n'
  )
  writeFile(root, 'Cargo.lock', '# lock v1\n')
  writeFile(root, 'rust-toolchain.toml', '[toolchain]\nchannel = "1.98.1"\n')
  writeFile(root, 'bindings/ubm_build_identity.rs', '// helper\n')
  writeFile(root, 'crates/ubm-core/Cargo.toml', '[package]\nname = "ubm-core"\n')
  writeFile(root, 'crates/ubm-core/src/lib.rs', 'pub mod contracts;\n')
  writeFile(root, 'crates/ubm-core/src/contracts.rs', 'pub const CONTRACT_REVISION: &str = "C-UBM.0.1.2-DRAFT";\n')
  writeFile(root, 'crates/ubm-core/tests/it.rs', '#[test] fn t() {}\n')
  writeFile(
    root,
    'crates/ubm-fake-radio/Cargo.toml',
    '[package]\nname = "ubm-fake-radio"\n\n[dependencies]\nubm-core = { path = "../ubm-core" }\n\n[dev-dependencies]\nubm-testonly = { path = "../ubm-testonly" }\n'
  )
  writeFile(root, 'crates/ubm-fake-radio/src/lib.rs', '// fake\n')
  writeFile(root, 'crates/ubm-testonly/Cargo.toml', '[package]\nname = "ubm-testonly"\n')
  writeFile(root, 'crates/ubm-testonly/src/lib.rs', '// test only\n')
  writeFile(root, 'crates/ubm-shared/Cargo.toml', '[package]\nname = "ubm-shared"\n')
  writeFile(root, 'crates/ubm-shared/src/lib.rs', '// shared\n')
  writeFile(
    root,
    'crates/ubm-desktop/Cargo.toml',
    '[package]\nname = "ubm-desktop"\n\n[dependencies.ubm-core]\npath = "../ubm-core"\n'
  )
  writeFile(root, 'crates/ubm-desktop/src/lib.rs', '// desktop\n')
  // napi: inline + table + workspace-inherited path dependencies.
  writeFile(
    root,
    'bindings/napi/Cargo.toml',
    '[package]\nname = "ubm5_napi_echo"\n\n[dependencies]\nnapi = "=2.16.17"\nubm-desktop = { path = "../../crates/ubm-desktop" }\nubm-shared = { workspace = true }\n\n[dependencies.ubm-fake-radio]\npath = "../../crates/ubm-fake-radio"\n'
  )
  writeFile(root, 'bindings/napi/build.rs', 'fn main() {}\n')
  writeFile(root, 'bindings/napi/src/lib.rs', '// napi lib\n')
  writeFile(root, 'bindings/napi/src/dispatch.rs', '// napi dispatch\n')
  writeFile(root, 'bindings/napi/run_napi_roundtrip.sh', 'echo not an input\n')
  // jni: JNI class discovered from the exported symbols.
  writeFile(
    root,
    'bindings/jni/Cargo.toml',
    '[package]\nname = "ubm5_jni_echo"\n\n[dependencies]\nubm-core = { path = "../../crates/ubm-core" }\n\n[target.\'cfg(target_os = "android")\'.dependencies]\nubm-fake-radio = { path = "../../crates/ubm-fake-radio" }\n'
  )
  writeFile(root, 'bindings/jni/build.rs', 'fn main() {}\n')
  writeFile(
    root,
    'bindings/jni/src/lib.rs',
    'pub extern "system" fn Java_com_ubm_echo_EchoBridge_nativeOpen() {}\npub extern "system" fn Java_com_ubm_gatt_GattBridge_nativeDrainGattEvents() {}\n'
  )
  writeFile(
    root,
    'android/src/main/java/com/ubm/echo/EchoBridge.java',
    'public static native long nativeOpen(String r);\n'
  )
  writeFile(
    root,
    'android/src/main/java/com/ubm/gatt/GattBridge.java',
    'public static native String nativeDrainGattEvents(long h);\n'
  )
  writeFile(root, 'bindings/jni/java/com/ubm/echo/EchoBridge.java', 'public static native long nativeOpen(String r);\n')
  // uniffi: UDL + generated Swift surface.
  writeFile(
    root,
    'bindings/uniffi/Cargo.toml',
    '[package]\nname = "ubm5_uniffi_echo"\n\n[dependencies]\nubm-core = { path = "../../crates/ubm-core" }\n\n[build-dependencies]\nubm-shared = { path = "../../crates/ubm-shared" }\n'
  )
  writeFile(root, 'bindings/uniffi/build.rs', 'fn main() {}\n')
  writeFile(root, 'bindings/uniffi/src/lib.rs', '// uniffi\n')
  writeFile(root, 'bindings/uniffi/src/ubm_echo.udl', 'namespace ubm_echo {};\n')
  writeFile(root, 'bindings/uniffi/generated/swift/ubm_echo.swift', '// swift\n')
  writeFile(root, 'bindings/uniffi/generated/swift/ubm_echoFFI.h', '// header\n')
  writeFile(root, 'bindings/uniffi/generated/swift/ubm_echoFFI.modulemap', 'module ubm_echoFFI {}\n')
  writeFile(root, 'bindings/uniffi/tests/swift.rs', '// not an input\n')
  return root
}

function withFixture(run) {
  const root = fixtureRoot()
  try {
    run(root)
  } finally {
    fs.rmSync(root, { recursive: true, force: true })
  }
}

describe('path dependency graph (parsed, never hand-maintained)', () => {
  test('napi resolves inline, table and workspace-inherited path deps transitively', () => {
    withFixture(root => {
      expect(identity.resolvePathDependencies(root, 'bindings/napi')).toEqual([
        'crates/ubm-core',
        'crates/ubm-desktop',
        'crates/ubm-fake-radio',
        'crates/ubm-shared'
      ])
    })
  })

  test('target-specific and build dependencies count; dev dependencies never do', () => {
    withFixture(root => {
      expect(identity.resolvePathDependencies(root, 'bindings/jni')).toEqual([
        'crates/ubm-core',
        'crates/ubm-fake-radio'
      ])
      expect(identity.resolvePathDependencies(root, 'bindings/uniffi')).toEqual([
        'crates/ubm-core',
        'crates/ubm-shared'
      ])
    })
  })

  test('a path dependency without a manifest fails closed', () => {
    withFixture(root => {
      fs.rmSync(path.join(root, 'crates/ubm-desktop/Cargo.toml'))
      expect(() => identity.resolvePathDependencies(root, 'bindings/napi')).toThrow(/crates\/ubm-desktop\/Cargo\.toml/)
    })
  })

  test('the real mobile graphs match the manifests (ubm-mobile is the production owner)', () => {
    const mobileGraph = ['crates/ubm-core', 'crates/ubm-desktop', 'crates/ubm-fake-radio', 'crates/ubm-mobile']
    expect(identity.resolvePathDependencies(repoRoot, 'bindings/jni')).toEqual(mobileGraph)
    expect(identity.resolvePathDependencies(repoRoot, 'bindings/uniffi')).toEqual(mobileGraph)
  })
})

// PR210-18 hardening: a [patch.*] path crate (vendored btleplug) is an input
// of exactly the bindings whose resolved feature graph links it. Optional
// dependencies count only when a feature activates them, as in cargo.
function patchedFixture(root) {
  writeFile(
    root,
    'Cargo.toml',
    '[workspace]\nmembers = ["crates/*", "bindings/*"]\n\n[workspace.dependencies]\nubm-shared = { path = "crates/ubm-shared" }\n\n[patch.crates-io]\nbtleplug = { path = "vendor/btleplug" }\n'
  )
  writeFile(
    root,
    'crates/ubm-desktop/Cargo.toml',
    [
      '[package]',
      'name = "ubm-desktop"',
      '',
      '[features]',
      'default = ["btleplug"]',
      'btleplug = [',
      '    "dep:btleplug",',
      ']',
      '',
      '[dependencies]',
      'ubm-core = { path = "../ubm-core" }',
      'btleplug = { version = "0.12", optional = true }',
      '',
      '[target.\'cfg(target_os = "windows")\'.dependencies]',
      'windows = { version = "0.62", optional = true, features = [',
      '    "Devices_Bluetooth",',
      '] }',
      ''
    ].join('\n')
  )
  writeFile(root, 'crates/ubm-desktop/build.rs', 'fn main() {}\n')
  writeFile(
    root,
    'vendor/btleplug/Cargo.toml',
    '[package]\nname = "btleplug"\nbuild = "build.rs"\n\n[lib]\npath = "src/lib.rs"\n\n[dependencies.tokio]\nversion = "1"\n'
  )
  writeFile(root, 'vendor/btleplug/build.rs', 'fn main() {}\n')
  writeFile(root, 'vendor/btleplug/src/lib.rs', '// vendored\n')
  writeFile(root, 'vendor/btleplug/UBM_PATCHES.md', 'not an input\n')
  writeFile(
    root,
    'crates/ubm-mobile/Cargo.toml',
    '[package]\nname = "ubm-mobile"\n\n[dependencies]\nubm-core = { path = "../ubm-core" }\nubm-desktop = { path = "../ubm-desktop", default-features = false }\n'
  )
  writeFile(root, 'crates/ubm-mobile/src/lib.rs', '// mobile owner\n')
  writeFile(
    root,
    'bindings/jni/Cargo.toml',
    '[package]\nname = "ubm5_jni_echo"\n\n[dependencies]\nubm-core = { path = "../../crates/ubm-core" }\nubm-mobile = { path = "../../crates/ubm-mobile" }\nubm-desktop = { path = "../../crates/ubm-desktop", default-features = false }\n'
  )
}

describe('patched and vendored crates, implicit build scripts', () => {
  test('a [patch] path crate joins only the bindings whose feature graph activates it', () => {
    withFixture(root => {
      patchedFixture(root)
      expect(identity.resolvePathDependencies(root, 'bindings/napi')).toContain('vendor/btleplug')
      expect(identity.resolvePathDependencies(root, 'bindings/jni')).toEqual([
        'crates/ubm-core',
        'crates/ubm-desktop',
        'crates/ubm-mobile'
      ])
    })
  })

  test('a vendored source, build script or manifest edit changes the digest of the bindings that link it', () => {
    withFixture(root => {
      patchedFixture(root)
      for (const relative of ['vendor/btleplug/src/lib.rs', 'vendor/btleplug/build.rs', 'vendor/btleplug/Cargo.toml']) {
        const napi = identity.computeBindingIdentity(root, 'napi').sourceDigest
        const jni = identity.computeBindingIdentity(root, 'jni').sourceDigest
        fs.appendFileSync(path.join(root, relative), '\n# edit\n')
        expect(identity.computeBindingIdentity(root, 'napi').sourceDigest).not.toBe(napi)
        expect(identity.computeBindingIdentity(root, 'jni').sourceDigest).toBe(jni)
      }
    })
  })

  test('non-build files beside the vendored crate are not inputs', () => {
    withFixture(root => {
      patchedFixture(root)
      const before = identity.computeBindingIdentity(root, 'napi').sourceDigest
      fs.appendFileSync(path.join(root, 'vendor/btleplug/UBM_PATCHES.md'), 'edit\n')
      expect(identity.computeBindingIdentity(root, 'napi').sourceDigest).toBe(before)
    })
  })

  test('a feature a dependent requests activates the optional dependency', () => {
    withFixture(root => {
      patchedFixture(root)
      writeFile(
        root,
        'crates/ubm-mobile/Cargo.toml',
        '[package]\nname = "ubm-mobile"\n\n[dependencies]\nubm-desktop = { path = "../ubm-desktop", default-features = false, features = ["btleplug"] }\n'
      )
      expect(identity.resolvePathDependencies(root, 'bindings/jni')).toContain('vendor/btleplug')
    })
  })

  test('an implicit build.rs (no build = key) is digested', () => {
    withFixture(root => {
      patchedFixture(root)
      expect(identity.bindingSourceInputs(root, 'jni')).toContain('crates/ubm-desktop/build.rs')
      const before = identity.computeBindingIdentity(root, 'jni').sourceDigest
      fs.appendFileSync(path.join(root, 'crates/ubm-desktop/build.rs'), '// edit\n')
      expect(identity.computeBindingIdentity(root, 'jni').sourceDigest).not.toBe(before)
    })
  })

  test('the mobile owner crate is an input of jni', () => {
    withFixture(root => {
      patchedFixture(root)
      const before = identity.computeBindingIdentity(root, 'jni').sourceDigest
      fs.appendFileSync(path.join(root, 'crates/ubm-mobile/src/lib.rs'), '// edit\n')
      expect(identity.computeBindingIdentity(root, 'jni').sourceDigest).not.toBe(before)
    })
  })

  test('a patch entry whose directory has no manifest fails closed', () => {
    withFixture(root => {
      patchedFixture(root)
      fs.rmSync(path.join(root, 'vendor/btleplug/Cargo.toml'))
      expect(() => identity.resolvePathDependencies(root, 'bindings/napi')).toThrow(/vendor\/btleplug\/Cargo\.toml/)
    })
  })

  test('the real graphs equal cargo tree (normal + build edges, all targets)', () => {
    const toolchain = /^channel\s*=\s*"([^"]+)"/m.exec(
      fs.readFileSync(path.join(repoRoot, 'rust-toolchain.toml'), 'utf8')
    )[1]
    for (const [binding, packageName] of [
      ['napi', 'ubm5_napi_echo'],
      ['jni', 'ubm5_jni_echo'],
      ['uniffi', 'ubm5_uniffi_echo']
    ]) {
      const tree = spawnSync(
        'cargo',
        [
          'tree',
          '--offline',
          '--locked',
          '-p',
          packageName,
          '-e',
          'normal,build',
          '--target',
          'all',
          '--prefix',
          'none'
        ],
        { cwd: repoRoot, encoding: 'utf8', env: { ...process.env, RUSTUP_TOOLCHAIN: toolchain } }
      )
      expect(tree.stderr).not.toMatch(/error/)
      expect(tree.status).toBe(0)
      const local = new Set()
      for (const line of tree.stdout.split('\n')) {
        const match = /\((\/[^)]+)\)/.exec(line)
        if (match === null) continue
        const relative = path.relative(repoRoot, match[1]).split(path.sep).join('/')
        if (relative !== identity.BINDINGS[binding].crateDir) local.add(relative)
      }
      expect(identity.resolvePathDependencies(repoRoot, identity.BINDINGS[binding].crateDir)).toEqual([...local].sort())
    }
  }, 120000)
})

describe('source digest', () => {
  test('is sha256 over sorted relpath NUL sha256 NEWLINE lines', () => {
    withFixture(root => {
      const inputs = identity.bindingSourceInputs(root, 'uniffi')
      expect(inputs).toEqual([...inputs].sort())
      expect(inputs).toEqual([
        'Cargo.lock',
        'Cargo.toml',
        'bindings/ubm_build_identity.rs',
        'bindings/uniffi/Cargo.toml',
        'bindings/uniffi/build.rs',
        'bindings/uniffi/src/lib.rs',
        'bindings/uniffi/src/ubm_echo.udl',
        'crates/ubm-core/Cargo.toml',
        'crates/ubm-core/src/contracts.rs',
        'crates/ubm-core/src/lib.rs',
        'crates/ubm-shared/Cargo.toml',
        'crates/ubm-shared/src/lib.rs',
        'rust-toolchain.toml'
      ])
      const expected = sha256(
        inputs.map(relative => `${relative}\0${sha256(fs.readFileSync(path.join(root, relative)))}\n`).join('')
      )
      expect(identity.computeBindingIdentity(root, 'uniffi').sourceDigest).toBe(expected)
    })
  })

  test.each([
    ['a transitive dependency source', 'crates/ubm-core/src/lib.rs'],
    ['the lockfile', 'Cargo.lock'],
    ['the toolchain pin', 'rust-toolchain.toml'],
    ['the build script', 'bindings/napi/build.rs'],
    ['the shared identity helper', 'bindings/ubm_build_identity.rs'],
    ['the workspace manifest', 'Cargo.toml']
  ])('changes when %s changes', (_label, relative) => {
    withFixture(root => {
      const before = identity.computeBindingIdentity(root, 'napi').sourceDigest
      fs.appendFileSync(path.join(root, relative), '\n// edit\n')
      expect(identity.computeBindingIdentity(root, 'napi').sourceDigest).not.toBe(before)
    })
  })

  test('a new file under a dependency src/ is an input (fail closed on additions)', () => {
    withFixture(root => {
      const before = identity.computeBindingIdentity(root, 'jni').sourceDigest
      writeFile(root, 'crates/ubm-core/src/new_module.rs', '// new\n')
      expect(identity.computeBindingIdentity(root, 'jni').sourceDigest).not.toBe(before)
    })
  })

  test('tests, dev-dependencies and non-build files are not inputs', () => {
    withFixture(root => {
      const before = identity.computeBindingIdentity(root, 'napi').sourceDigest
      fs.appendFileSync(path.join(root, 'crates/ubm-core/tests/it.rs'), '// edit\n')
      fs.appendFileSync(path.join(root, 'crates/ubm-testonly/src/lib.rs'), '// edit\n')
      fs.appendFileSync(path.join(root, 'bindings/napi/run_napi_roundtrip.sh'), '# edit\n')
      expect(identity.computeBindingIdentity(root, 'napi').sourceDigest).toBe(before)
    })
  })

  test('a missing workspace input fails closed', () => {
    withFixture(root => {
      fs.rmSync(path.join(root, 'rust-toolchain.toml'))
      expect(() => identity.computeBindingIdentity(root, 'napi')).toThrow(/rust-toolchain\.toml/)
    })
  })
})

describe('binding schema (T1)', () => {
  test('napi binds dispatch.rs and lib.rs', () => {
    withFixture(root => {
      expect(identity.bindingSchemaInputs(root, 'napi')).toEqual([
        'bindings/napi/src/dispatch.rs',
        'bindings/napi/src/lib.rs'
      ])
      const before = identity.computeBindingIdentity(root, 'napi').bindingSchema
      fs.appendFileSync(path.join(root, 'bindings/napi/src/dispatch.rs'), '// op\n')
      expect(identity.computeBindingIdentity(root, 'napi').bindingSchema).not.toBe(before)
    })
  })

  test('uniffi binds the UDL and the generated Swift surface', () => {
    withFixture(root => {
      expect(identity.bindingSchemaInputs(root, 'uniffi')).toEqual([
        'bindings/uniffi/generated/swift/ubm_echo.swift',
        'bindings/uniffi/generated/swift/ubm_echoFFI.h',
        'bindings/uniffi/generated/swift/ubm_echoFFI.modulemap',
        'bindings/uniffi/src/ubm_echo.udl'
      ])
      const before = identity.computeBindingIdentity(root, 'uniffi')
      fs.appendFileSync(path.join(root, 'bindings/uniffi/generated/swift/ubm_echo.swift'), '// regen\n')
      const after = identity.computeBindingIdentity(root, 'uniffi')
      expect(after.bindingSchema).not.toBe(before.bindingSchema)
      // Generated Swift is compiled in the consumer, not into the binary.
      expect(after.sourceDigest).toBe(before.sourceDigest)
    })
  })

  test('jni binds lib.rs plus every Java class its exported symbols name', () => {
    withFixture(root => {
      expect(identity.bindingSchemaInputs(root, 'jni')).toEqual([
        'android/src/main/java/com/ubm/echo/EchoBridge.java',
        'android/src/main/java/com/ubm/gatt/GattBridge.java',
        'bindings/jni/java/com/ubm/echo/EchoBridge.java',
        'bindings/jni/src/lib.rs'
      ])
    })
  })

  test('a JNI class without its Java declaration fails closed', () => {
    withFixture(root => {
      fs.rmSync(path.join(root, 'android/src/main/java/com/ubm/gatt/GattBridge.java'))
      expect(() => identity.bindingSchemaInputs(root, 'jni')).toThrow(/GattBridge\.java/)
    })
  })
})

describe('generated TypeScript identity (src/generated/native-build-identity.ts)', () => {
  test('--write then --check passes; an edit makes --check fail with the rerun command', () => {
    withFixture(root => {
      identity.writeGenerated(root)
      expect(identity.checkGenerated(root)).toBe(true)
      const generated = fs.readFileSync(path.join(root, identity.GENERATED_RELATIVE), 'utf8')
      expect(generated).toContain("schema: 'ubm-native-build-identity/1'")
      expect(generated).toContain("contractRevision: 'C-UBM.0.1.2-DRAFT'")
      expect(generated).toContain(identity.computeBindingIdentity(root, 'jni').sourceDigest)
      fs.appendFileSync(path.join(root, 'crates/ubm-core/src/lib.rs'), '// edit\n')
      expect(() => identity.checkGenerated(root)).toThrow(/native-build-identity\.js --write/)
    })
  })

  test('a missing generated module fails --check', () => {
    withFixture(root => {
      expect(() => identity.checkGenerated(root)).toThrow(/missing/)
    })
  })

  test('declares a target set per binding', () => {
    const computed = identity.computeNativeBuildIdentity(repoRoot)
    expect(computed.bindings.jni.targets).toEqual(['aarch64-linux-android', 'x86_64-linux-android'])
    expect(computed.bindings.uniffi.targets).toEqual([
      'aarch64-apple-ios',
      'aarch64-apple-ios-sim',
      'aarch64-apple-tvos',
      'aarch64-apple-tvos-sim',
      'x86_64-apple-ios'
    ])
    expect(computed.bindings.napi.targets.length).toBeGreaterThan(0)
  })

  test('CLI --check reports the committed module is current', () => {
    const result = spawnSync(process.execPath, [cli, '--check'], { encoding: 'utf8' })
    expect(result.stderr).toBe('')
    expect(result.status).toBe(0)
    expect(result.stdout).toContain('native-build-identity: current')
  })

  test('prepack runs --check before bob compiles the module', () => {
    const buildPackage = fs.readFileSync(path.join(repoRoot, 'scripts', 'ci', 'build-package.js'), 'utf8')
    const check = buildPackage.indexOf("'scripts/release/native-build-identity.js', '--check'")
    expect(check).toBeGreaterThan(-1)
    expect(check).toBeLessThan(buildPackage.indexOf("'bob', 'build'"))
  })

  test('packed consumers receive the identity script (package.json files)', () => {
    const pkg = JSON.parse(fs.readFileSync(path.join(repoRoot, 'package.json'), 'utf8'))
    expect(pkg.files).toContain('scripts/release/native-build-identity.js')
  })

  test('CLI --print-env emits exactly the two cargo identity variables', () => {
    withFixture(root => {
      const result = spawnSync(process.execPath, [cli, '--root', root, '--print-env', 'jni'], { encoding: 'utf8' })
      expect(result.status).toBe(0)
      const expected = identity.computeBindingIdentity(root, 'jni')
      expect(result.stdout).toBe(
        `UBM_BUILD_SOURCE_DIGEST=${expected.sourceDigest}\nUBM_BUILD_BINDING_SCHEMA=${expected.bindingSchema}\n`
      )
    })
  })

  test('CLI rejects an unknown binding and an unknown argument', () => {
    const binding = spawnSync(process.execPath, [cli, '--print-env', 'wasm'], { encoding: 'utf8' })
    expect(binding.status).not.toBe(0)
    expect(binding.stderr).toMatch(/unknown binding 'wasm'/)
    const argument = spawnSync(process.execPath, [cli, '--bogus'], { encoding: 'utf8' })
    expect(argument.status).not.toBe(0)
    expect(argument.stderr).toMatch(/Unknown argument: --bogus/)
  })

  test('the identity script is self-contained (packed consumers ship only this file of scripts/)', () => {
    const source = fs.readFileSync(cli, 'utf8')
    const required = [...source.matchAll(/require\(\s*['"]([^'"]+)['"]\s*\)/g)].map(match => match[1])
    for (const specifier of required) {
      expect(specifier).toMatch(/^(node:)?(crypto|fs|path|child_process)$/)
    }
  })
})

function appleFixture(root, overrides = {}) {
  const current = identity.computeBindingIdentity(root, 'uniffi')
  const dir = path.join(root, 'ios', 'RustCore')
  const framework = path.join(dir, 'RustCore.xcframework')
  const plist = writeFile(root, 'ios/RustCore/RustCore.xcframework/Info.plist', '<plist>fixture</plist>\n')
  const libraries = identity.APPLE_DECLARED_LIBRARIES.map(declared => {
    const libraryPath = 'libubm5_uniffi_echo.a'
    const content = Buffer.from(`slice ${declared.libraryIdentifier}`)
    writeFile(root, path.join('ios/RustCore/RustCore.xcframework', declared.libraryIdentifier, libraryPath), content)
    return { ...declared, libraryPath, sha256: sha256(content), bytes: content.length }
  })
  const record = {
    schema: identity.APPLE_STAGING_SCHEMA,
    binding: 'uniffi',
    contractRevision: 'C-UBM.0.1.2-DRAFT',
    sourceDigest: current.sourceDigest,
    bindingSchema: current.bindingSchema,
    profile: 'release',
    infoPlistSha256: sha256(fs.readFileSync(plist)),
    libraries,
    ...overrides
  }
  writeFile(root, 'ios/RustCore/build-identity.json', `${JSON.stringify(record, null, 2)}\n`)
  return { dir, framework, record }
}

describe('Apple staging hash chain (--check-apple)', () => {
  test('a current, intact staging passes', () => {
    withFixture(root => {
      appleFixture(root)
      expect(identity.checkAppleStaging(root)).toBe(true)
    })
  })

  test('stale sources fail with the prepare command', () => {
    withFixture(root => {
      appleFixture(root)
      fs.appendFileSync(path.join(root, 'crates/ubm-core/src/lib.rs'), '// edit\n')
      expect(() => identity.checkAppleStaging(root)).toThrow(/sourceDigest[\s\S]*native:apple:prepare/)
    })
  })

  test('a regenerated Swift surface fails as a bindingSchema mismatch', () => {
    withFixture(root => {
      appleFixture(root)
      fs.appendFileSync(path.join(root, 'bindings/uniffi/generated/swift/ubm_echo.swift'), '// regen\n')
      expect(() => identity.checkAppleStaging(root)).toThrow(/bindingSchema/)
    })
  })

  test('a substituted slice fails and names it', () => {
    withFixture(root => {
      const { framework } = appleFixture(root)
      fs.appendFileSync(path.join(framework, 'tvos-arm64', 'libubm5_uniffi_echo.a'), 'x')
      expect(() => identity.checkAppleStaging(root)).toThrow(/tvos-arm64\/libubm5_uniffi_echo\.a/)
    })
  })

  test('an edited Info.plist fails', () => {
    withFixture(root => {
      const { framework } = appleFixture(root)
      fs.appendFileSync(path.join(framework, 'Info.plist'), '<!-- edit -->')
      expect(() => identity.checkAppleStaging(root)).toThrow(/Info\.plist sha256/)
    })
  })

  test('an omitted slice fails as a declared-set mismatch', () => {
    withFixture(root => {
      const { record } = appleFixture(root)
      appleFixture(root, { libraries: record.libraries.slice(1) })
      expect(() => identity.checkAppleStaging(root)).toThrow(/declared slice set/)
    })
  })

  test('an extra archive inside the framework fails', () => {
    withFixture(root => {
      const { framework } = appleFixture(root)
      writeFile(root, path.relative(root, path.join(framework, 'ios-arm64', 'extra.a')), 'x')
      expect(() => identity.checkAppleStaging(root)).toThrow(/undeclared archive/)
    })
  })

  test('a missing identity file fails with the prepare command', () => {
    withFixture(root => {
      expect(() => identity.checkAppleStaging(root)).toThrow(/build-identity\.json[\s\S]*native:apple:prepare/)
    })
  })

  test('the declared slice set matches ios/verify-rust-core.sh exactly', () => {
    const verifier = fs.readFileSync(path.join(repoRoot, 'ios', 'verify-rust-core.sh'), 'utf8')
    const block = /^DECLARED_LIBRARIES="([\s\S]*?)"$/m.exec(verifier)
    expect(block).not.toBeNull()
    const shellSet = block[1]
      .split('\n')
      .map(line => line.trim())
      .filter(line => line.length > 0)
      .sort()
    const jsSet = identity.APPLE_DECLARED_LIBRARIES.map(
      entry => `${entry.libraryIdentifier}|${entry.platform}|${entry.variant}|${entry.architectures.join(',')}`
    ).sort()
    expect(shellSet).toEqual(jsSet)
  })
})

function androidFixture(root, overrides = {}) {
  const current = identity.computeBindingIdentity(root, 'jni')
  const abis = identity.ANDROID_DECLARED_ABIS.map(({ abi, target }) => {
    const content = Buffer.from(`so ${abi}`)
    writeFile(root, `android/src/main/jniLibs/${abi}/libubm5_jni_echo.so`, content)
    return { abi, target, file: 'libubm5_jni_echo.so', sha256: sha256(content), bytes: content.length }
  })
  const record = {
    schema: identity.ANDROID_PREBUILT_SCHEMA,
    binding: 'jni',
    contractRevision: 'C-UBM.0.1.2-DRAFT',
    sourceDigest: current.sourceDigest,
    bindingSchema: current.bindingSchema,
    profile: 'release',
    abis,
    ...overrides
  }
  writeFile(root, 'android/src/main/jniLibs/build-identity.json', `${JSON.stringify(record, null, 2)}\n`)
  return record
}

describe('committed Android prebuilts (--check-android-prebuilts publish gate)', () => {
  test('prebuilts built from the current sources pass', () => {
    withFixture(root => {
      androidFixture(root)
      expect(identity.checkAndroidPrebuilts(root)).toBe(true)
    })
  })

  test('prebuilts older than the Rust sources fail with the refresh command', () => {
    withFixture(root => {
      androidFixture(root)
      fs.appendFileSync(path.join(root, 'crates/ubm-core/src/lib.rs'), '// edit\n')
      expect(() => identity.checkAndroidPrebuilts(root)).toThrow(/sourceDigest[\s\S]*refresh-prebuilt-jniLibs\.sh/)
    })
  })

  test('prebuilts that predate identity sealing (null sourceDigest) fail', () => {
    withFixture(root => {
      androidFixture(root, { sourceDigest: null })
      expect(() => identity.checkAndroidPrebuilts(root)).toThrow(/sourceDigest/)
    })
  })

  test('a tampered library fails the hash chain', () => {
    withFixture(root => {
      androidFixture(root)
      fs.appendFileSync(path.join(root, 'android/src/main/jniLibs/x86_64/libubm5_jni_echo.so'), 'x')
      expect(() => identity.checkAndroidPrebuilts(root)).toThrow(/x86_64/)
    })
  })

  test('the committed identity record is well-formed JSON with the declared ABIs', () => {
    const record = JSON.parse(
      fs.readFileSync(path.join(repoRoot, 'android', 'src', 'main', 'jniLibs', 'build-identity.json'), 'utf8')
    )
    expect(record.schema).toBe(identity.ANDROID_PREBUILT_SCHEMA)
    expect(record.binding).toBe('jni')
    expect(record.abis.map(entry => entry.abi).sort()).toEqual(
      identity.ANDROID_DECLARED_ABIS.map(entry => entry.abi).sort()
    )
    expect(Object.prototype.hasOwnProperty.call(record, 'sourceDigest')).toBe(true)
  })
})
