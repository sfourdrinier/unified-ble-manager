// __tests__/NativeBuildRsIdentity.test.js
//
// PR210-18: every binding's build.rs emits the `ubm-native-build-identity/1`
// constants through the one shared helper (bindings/ubm_build_identity.rs).
// The executed half compiles a throwaway crate whose build script is that
// helper and whose binary prints ubm_build_identity_json(), so the frozen
// JSON shape, the "unsealed" default and the fail-closed validation are
// proven against the real helper, not a copy.

'use strict'

const fs = require('fs')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

const root = path.join(__dirname, '..')
const helper = path.join(root, 'bindings', 'ubm_build_identity.rs')
const DIGEST_A = 'a'.repeat(64)
const DIGEST_B = '0123456789abcdef'.repeat(4)

describe('binding build scripts', () => {
  test.each(['napi', 'jni', 'uniffi'])('%s build.rs emits its identity through the shared helper', binding => {
    const buildRs = fs.readFileSync(path.join(root, 'bindings', binding, 'build.rs'), 'utf8')
    expect(buildRs).toContain('include!("../ubm_build_identity.rs");')
    expect(buildRs).toContain(`ubm_build_identity::emit("${binding}");`)
  })

  test('the helper tracks both sealed variables and defaults to "unsealed"', () => {
    const source = fs.readFileSync(helper, 'utf8')
    expect(source).toContain('cargo:rerun-if-env-changed={name}')
    expect(source).toContain('["UBM_BUILD_SOURCE_DIGEST", "UBM_BUILD_BINDING_SCHEMA"]')
    expect(source).toContain('const UNSEALED: &str = "unsealed";')
  })
})

let fixture
let targetDir

function toolchain() {
  const pin = /^channel\s*=\s*"([^"]+)"/m.exec(fs.readFileSync(path.join(root, 'rust-toolchain.toml'), 'utf8'))
  return pin[1]
}

function runFixture(extraEnv) {
  const env = { ...process.env, CARGO_TARGET_DIR: targetDir, RUSTUP_TOOLCHAIN: toolchain() }
  delete env.UBM_BUILD_SOURCE_DIGEST
  delete env.UBM_BUILD_BINDING_SCHEMA
  return spawnSync('cargo', ['run', '--quiet', '--offline', '--manifest-path', path.join(fixture, 'Cargo.toml')], {
    encoding: 'utf8',
    env: { ...env, ...extraEnv }
  })
}

beforeAll(() => {
  fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-buildrs-'))
  targetDir = path.join(fixture, 'target')
  fs.writeFileSync(
    path.join(fixture, 'Cargo.toml'),
    '[package]\nname = "ubm-identity-fixture"\nversion = "0.0.0"\nedition = "2021"\n\n[features]\ndefault = ["alpha-one"]\nalpha-one = []\n\n[workspace]\n'
  )
  fs.writeFileSync(
    path.join(fixture, 'build.rs'),
    `include!(${JSON.stringify(helper)});\n\nfn main() {\n    ubm_build_identity::emit("jni");\n}\n`
  )
  fs.mkdirSync(path.join(fixture, 'src'))
  fs.writeFileSync(
    path.join(fixture, 'src', 'main.rs'),
    '#![deny(warnings)]\ninclude!(concat!(env!("OUT_DIR"), "/ubm_build_identity.rs"));\n\nfn main() {\n    println!("{}", ubm_build_identity_json("C-UBM.\\"quoted\\\\rev"));\n}\n'
  )
})

afterAll(() => {
  fs.rmSync(fixture, { recursive: true, force: true })
})

describe('executed helper (throwaway crate)', () => {
  test('unset variables produce an unsealed record with the frozen shape', () => {
    const result = runFixture({})
    expect(result.stderr).not.toMatch(/error|warning/)
    expect(result.status).toBe(0)
    const record = JSON.parse(result.stdout)
    expect(Object.keys(record)).toEqual([
      'schema',
      'binding',
      'contractRevision',
      'sourceDigest',
      'bindingSchema',
      'target',
      'profile',
      'features',
      'rustc'
    ])
    expect(record.schema).toBe('ubm-native-build-identity/1')
    expect(record.binding).toBe('jni')
    expect(record.contractRevision).toBe('C-UBM."quoted\\rev')
    expect(record.sourceDigest).toBe('unsealed')
    expect(record.bindingSchema).toBe('unsealed')
    expect(record.profile).toBe('debug')
    expect(record.features).toEqual(['alpha_one', 'default'])
    expect(record.rustc).toMatch(/^rustc \d+\.\d+\.\d+/)
    expect(typeof record.target).toBe('string')
    expect(record.target.length).toBeGreaterThan(0)
  }, 120000)

  test('sealed digests reach the binary and a changed value rebuilds it', () => {
    const first = runFixture({ UBM_BUILD_SOURCE_DIGEST: DIGEST_A, UBM_BUILD_BINDING_SCHEMA: DIGEST_B })
    expect(first.status).toBe(0)
    expect(JSON.parse(first.stdout)).toMatchObject({ sourceDigest: DIGEST_A, bindingSchema: DIGEST_B })
    const second = runFixture({ UBM_BUILD_SOURCE_DIGEST: DIGEST_B, UBM_BUILD_BINDING_SCHEMA: DIGEST_A })
    expect(second.status).toBe(0)
    expect(JSON.parse(second.stdout)).toMatchObject({ sourceDigest: DIGEST_B, bindingSchema: DIGEST_A })
  }, 120000)

  test('a malformed digest fails the build instead of being embedded', () => {
    const result = runFixture({ UBM_BUILD_SOURCE_DIGEST: 'not-a-digest' })
    expect(result.status).not.toBe(0)
    expect(result.stderr).toContain('UBM_BUILD_SOURCE_DIGEST must be 64 lowercase hex characters or unset')
  }, 120000)
})
