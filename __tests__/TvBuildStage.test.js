'use strict'

// Finding 176: example-expo/scripts/build-tv.sh restage kept a stale
// unified-ble-manager copy (pnpm reuses the cached file: package), so the
// TV app failed closed with protocol.incompatible native-identity; the
// staged AppDelegate also defaulted to Metro 8081. The stage must always
// refresh the library copy and honour TV_METRO_PORT in every step, and a
// staged build whose identity differs from the repo must fail loudly.
//
// TV_STAGE_DIR redirects the stage to a temporary directory so these tests
// never touch the real example-expo/ios-tv tree.

const { execFileSync, spawn, spawnSync } = require('node:child_process')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const SCRIPT = path.join(__dirname, '..', 'example-expo', 'scripts', 'build-tv.sh')
const ROOT = path.join(__dirname, '..')
const HELD_PORT = 18099

function waitForListener(port) {
  const deadline = Date.now() + 10000
  while (Date.now() < deadline) {
    const probe = spawnSync('lsof', ['-nP', `-iTCP:${String(port)}`, '-sTCP:LISTEN', '-Fp'], { encoding: 'utf8' })
    if (probe.status === 0 && typeof probe.stdout === 'string' && probe.stdout.includes('p')) return true
    execFileSync('sleep', ['0.1'])
  }
  return false
}

function run(args, env) {
  try {
    const stdout = execFileSync('bash', [SCRIPT, ...args], {
      env: { ...process.env, ...env },
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe']
    })
    return { exit: 0, stdout }
  } catch (error) {
    return { exit: error.status ?? 1, stdout: error.stdout ?? '', stderr: error.stderr ?? '' }
  }
}

function stageDir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-tv-stage-'))
}

describe('build-tv.sh finding 176', () => {
  test('verify-identity passes for a staged copy equal to the repo and fails loudly otherwise', () => {
    const stage = stageDir()
    try {
      const stagedLib = path.join(stage, 'node_modules', 'unified-ble-manager', 'src', 'generated')
      fs.mkdirSync(stagedLib, { recursive: true })
      fs.copyFileSync(
        path.join(ROOT, 'src', 'generated', 'native-build-identity.ts'),
        path.join(stagedLib, 'native-build-identity.ts')
      )
      const version = JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8')).version
      fs.writeFileSync(
        path.join(stage, 'node_modules', 'unified-ble-manager', 'package.json'),
        JSON.stringify({ name: 'unified-ble-manager', version })
      )
      const fresh = run(['verify-identity'], { TV_STAGE_DIR: stage })
      expect(fresh.exit).toBe(0)

      fs.appendFileSync(path.join(stagedLib, 'native-build-identity.ts'), '// stale\n')
      const stale = run(['verify-identity'], { TV_STAGE_DIR: stage })
      expect(stale.exit).not.toBe(0)
      expect(stale.stderr).toMatch(/native-build-identity|stale/i)
    } finally {
      fs.rmSync(stage, { recursive: true, force: true })
    }
  })

  test('stage refreshes a stale staged library copy so the next install cannot reuse it', () => {
    const stage = stageDir()
    try {
      const first = run(['stage'], { TV_STAGE_DIR: stage })
      expect(first.exit).toBe(0)
      expect(fs.existsSync(path.join(stage, 'package.json'))).toBe(true)
      const staleLib = path.join(stage, 'node_modules', 'unified-ble-manager')
      fs.mkdirSync(staleLib, { recursive: true })
      fs.writeFileSync(path.join(staleLib, 'STALE-MARKER'), 'stale')
      const second = run(['stage'], { TV_STAGE_DIR: stage })
      expect(second.exit).toBe(0)
      expect(fs.existsSync(path.join(staleLib, 'STALE-MARKER'))).toBe(false)
    } finally {
      fs.rmSync(stage, { recursive: true, force: true })
    }
  })

  // Finding 241: the staged TV app is pointed at TV_LAN_HOST:TV_METRO_PORT, so
  // a port another project already serves hands it that project's bundle and
  // React Native throws on every native call without bound. `metro` must refuse
  // before it starts anything.
  test('metro refuses a port held from outside this repository', () => {
    const outside = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-not-this-repo-'))
    const holder = spawn(
      process.execPath,
      [
        '-e',
        "require('net').createServer().listen(Number(process.argv[1]), '127.0.0.1', () => console.log('ready'))",
        String(HELD_PORT)
      ],
      { cwd: outside, stdio: 'ignore' }
    )
    try {
      expect(waitForListener(HELD_PORT)).toBe(true)
      const result = run(['metro'], { TV_METRO_PORT: String(HELD_PORT) })
      expect(result.exit).not.toBe(0)
      expect(`${result.stdout}${result.stderr ?? ''}`).toContain('held by another project')
      expect(`${result.stdout}${result.stderr ?? ''}`).toContain(outside)
    } finally {
      holder.kill('SIGKILL')
      fs.rmSync(outside, { recursive: true, force: true })
    }
  })

  test('bundle-url honours TV_METRO_PORT even when an override is already present', () => {
    const stage = stageDir()
    try {
      const iosDir = path.join(stage, 'ios', 'TvApp')
      fs.mkdirSync(iosDir, { recursive: true })
      const delegate = path.join(iosDir, 'AppDelegate.swift')
      fs.writeFileSync(
        delegate,
        'func sourceURL() {\n    return RCTBundleURLProvider.sharedSettings().jsBundleURL(forBundleRoot: "index")\n}\n'
      )
      const first = run(['bundle-url'], {
        TV_STAGE_DIR: stage,
        TV_LAN_HOST: '192.168.68.116',
        TV_METRO_PORT: '8081'
      })
      expect(first.exit).toBe(0)
      expect(fs.readFileSync(delegate, 'utf8')).toContain('192.168.68.116:8081')
      const second = run(['bundle-url'], {
        TV_STAGE_DIR: stage,
        TV_LAN_HOST: '192.168.68.116',
        TV_METRO_PORT: '8091'
      })
      expect(second.exit).toBe(0)
      const text = fs.readFileSync(delegate, 'utf8')
      expect(text).toContain('192.168.68.116:8091')
      expect(text).not.toContain('192.168.68.116:8081')
    } finally {
      fs.rmSync(stage, { recursive: true, force: true })
    }
  })

  // Without a TTY, pnpm's "modules directory will be removed" prompt purged
  // node_modules and exited 0 without reinstalling the library, so the TV
  // app later failed to resolve unified-ble-manager/expo.
  test('install never waits on the modules-purge prompt and fails if the library is missing', () => {
    const script = fs.readFileSync(SCRIPT, 'utf8')
    const install = script.slice(script.indexOf('cmd_install() {'), script.indexOf('cmd_verify_identity() {'))
    expect(install).toContain('--config.confirm-modules-purge=false')
    expect(install).toContain('node_modules/unified-ble-manager/package.json')
  })
})
