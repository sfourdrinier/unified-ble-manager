'use strict'

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { spawnSync } = require('node:child_process')

const root = path.resolve(__dirname, '..')
let fixture

beforeAll(() => {
  fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-clean-artifact-verifier-'))
  for (const entry of [
    'src',
    'lib',
    'plugin/src',
    'plugin/build',
    'bin',
    'package.json',
    'app.plugin.js',
    'native/desktop-core/index.js',
    'native/load-node-api-addon.js',
    'scripts/ci/verify-package-artifacts.js',
    'scripts/ci/package-source-classification.js',
    'scripts/ci/forbidden-runtime-dependencies.js'
  ]) {
    const target = path.join(fixture, entry)
    fs.mkdirSync(path.dirname(target), { recursive: true })
    fs.cpSync(path.join(root, entry), target, { recursive: true })
  }
})

afterAll(() => fs.rmSync(fixture, { recursive: true, force: true }))

function verify() {
  const result = spawnSync(process.execPath, ['scripts/ci/verify-package-artifacts.js'], {
    cwd: fixture,
    encoding: 'utf8'
  })
  return { status: result.status, output: result.stdout + result.stderr }
}

test('clean source verification does not require the retired native/electron directory', () => {
  expect(fs.existsSync(path.join(fixture, 'native/electron'))).toBe(false)
  expect(verify()).toMatchObject({ status: 0 })
})

test('source verification still rejects a reintroduced retired C++ producer', () => {
  const retired = path.join(fixture, 'native/electron')
  fs.mkdirSync(retired)
  fs.writeFileSync(path.join(retired, 'retired.cpp'), 'int retired_producer = 1;\n')
  try {
    const result = verify()
    expect(result.status).toBe(1)
    expect(result.output).toContain('Retired desktop producer in source artifact')
  } finally {
    fs.rmSync(retired, { recursive: true })
  }
})

test('maintained native JavaScript loaders remain covered by forbidden-runtime validation', () => {
  const loader = path.join(fixture, 'native/desktop-core/index.js')
  const original = fs.readFileSync(loader, 'utf8')
  fs.appendFileSync(loader, '\nrequire("noble")\n')
  try {
    const result = verify()
    expect(result.status).toBe(1)
    expect(result.output).toContain('forbidden Noble runtime package noble: native/desktop-core/index.js')
  } finally {
    fs.writeFileSync(loader, original)
  }
})
