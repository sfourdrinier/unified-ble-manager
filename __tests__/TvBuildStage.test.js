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
const { gitBashExecutableTemp } = require('./helpers/git-bash-temp')

const SCRIPT = path.join(__dirname, '..', 'example-expo', 'scripts', 'build-tv.sh')
const ROOT = path.join(__dirname, '..')
const HOSTS_SH = path.join(ROOT, 'examples-shared', 'driver', 'hosts.sh')
const ANDROID_TV_EMU_SH = path.join(__dirname, '..', 'example-expo', 'scripts', 'android-tv-emu.sh')
const GUARD = path.join(ROOT, 'examples-shared', 'dev', 'metro-port-guard.js')
const HELD_PORT = 18099

function sleepMs(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms)
}

// `lsof` does not exist on Windows (and `sleep` is not on its PATH either),
// so readiness is probed by connecting: refused means nothing listens yet.
// Same 10 s bound as before, same boolean answer, no helper binaries.
function isListening(port) {
  const probe = spawnSync(
    process.execPath,
    [
      '-e',
      "require('net').connect(Number(process.argv[1]), '127.0.0.1').on('connect', () => process.exit(0)).on('error', () => process.exit(1))",
      String(port)
    ],
    { stdio: 'ignore', timeout: 5000 }
  )
  return probe.status === 0
}

function waitForListener(port) {
  const deadline = Date.now() + 10000
  while (Date.now() < deadline) {
    if (isListening(port)) return true
    sleepMs(100)
  }
  return false
}

// `kill` is asynchronous on Windows: the process keeps its working-directory
// handle until it actually exits, so deleting that directory immediately
// races with EBUSY — and the EBUSY then masks the real assertion in the
// `finally`. Wait for the exit first (a fast no-op where it already landed);
// this synchronises teardown with the OS, it does not loosen any assertion.
function waitForExit(child) {
  const deadline = Date.now() + 10000
  while (Date.now() < deadline) {
    if (child.exitCode !== null || child.signalCode !== null) return
    sleepMs(50)
  }
}

// Portable executable discovery, done in Node instead of through
// `bash -lc 'command -v ...'`: on Windows that prints MSYS paths (/c/...)
// Node cannot resolve, and the first `python3` hit may be the 0-byte
// Microsoft Store stub. Scan PATH entries directly and skip zero-byte
// stubs; copy (never symlink — file symlinks need privilege on Windows).
function findOnPath(names) {
  const extensions = process.platform === 'win32' ? ['', '.exe', '.cmd', '.bat'] : ['']
  for (const entry of process.env.PATH.split(path.delimiter)) {
    if (entry === '') continue
    for (const name of names) {
      for (const extension of extensions) {
        const candidate = path.join(entry, name + extension)
        try {
          const stat = fs.statSync(candidate)
          if (stat.isFile() && stat.size > 0) return candidate
        } catch {
          // Missing or unreadable: keep scanning.
        }
      }
    }
  }
  return ''
}

function shimIntoDir(shim, source, name) {
  const target = path.join(shim, name ?? path.basename(source))
  if (process.platform === 'win32') {
    // File symlinks need privilege on Windows; copies run fine there.
    fs.copyFileSync(source, target)
    try {
      fs.chmodSync(target, 0o755)
    } catch {
      // Best-effort: chmod is a no-op on Windows, where the extension resolves.
    }
  } else {
    // Copies of platform-signed binaries are killed on exec; symlinking
    // keeps the original (and needs no privilege here).
    fs.symlinkSync(fs.realpathSync(source), target)
  }
  return target
}

function dirProvidesPython3(entry) {
  const names = process.platform === 'win32' ? ['python3', 'python3.exe'] : ['python3']
  return names.some(name => {
    try {
      fs.accessSync(path.join(entry, name), fs.constants.X_OK)
      return true
    } catch {
      return false
    }
  })
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
      // Protected names are never copied: the stage owns its node_modules
      // (tvos alias) and its ios/android trees (prebuilds). Vacuous where
      // the source lacks them, binding where it has them.
      expect(fs.existsSync(path.join(stage, 'node_modules'))).toBe(false)
      expect(fs.existsSync(path.join(stage, 'ios'))).toBe(false)
      expect(fs.existsSync(path.join(stage, 'android'))).toBe(false)
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
  // The stage owns the port, not the repo: a server started from the stage is
  // the TV's own and must be allowed, and a server started from elsewhere in
  // the repository (the bare example on the same default port) must not be.
  // Passing the repo root as the project root got both of these backwards.
  test('metro allows the port its own staged server holds', () => {
    const stage = stageDir()
    const holder = spawn(
      process.execPath,
      ['-e', "require('net').createServer().listen(Number(process.argv[1]), '127.0.0.1')", String(HELD_PORT + 1)],
      { cwd: stage, stdio: 'ignore' }
    )
    try {
      expect(waitForListener(HELD_PORT + 1)).toBe(true)
      const guard = spawnSync(process.execPath, [GUARD, String(HELD_PORT + 1), stage], { encoding: 'utf8' })
      if (process.platform === 'win32') {
        // No lsof on Windows: ownership is unreadable, so the guard reports
        // `undetermined` out loud and steps aside (exit 0) instead of
        // refusing — the documented honest outcome. The refusal logic itself
        // is pinned by MetroPortGuard.test.js with injected probes.
        expect(guard.status).toBe(0)
        expect(guard.stderr).toContain('cannot determine')
      } else {
        expect(guard.status).toBe(0)
        expect(guard.stdout).toContain('held by this project')
      }
    } finally {
      holder.kill('SIGKILL')
      waitForExit(holder)
      fs.rmSync(stage, { recursive: true, force: true })
    }
  })

  test('metro refuses a repository server that is not the stage', () => {
    const stage = stageDir()
    const holder = spawn(
      process.execPath,
      ['-e', "require('net').createServer().listen(Number(process.argv[1]), '127.0.0.1')", String(HELD_PORT + 2)],
      { cwd: path.join(ROOT, 'example'), stdio: 'ignore' }
    )
    try {
      expect(waitForListener(HELD_PORT + 2)).toBe(true)
      const guard = spawnSync(process.execPath, [GUARD, String(HELD_PORT + 2), stage], { encoding: 'utf8' })
      if (process.platform === 'win32') {
        // As above: unreadable ownership is reported, not refused.
        expect(guard.status).toBe(0)
        expect(guard.stderr).toContain('cannot determine')
      } else {
        expect(guard.status).not.toBe(0)
        expect(guard.stderr).toContain('held by another project')
      }
    } finally {
      holder.kill('SIGKILL')
      waitForExit(holder)
      fs.rmSync(stage, { recursive: true, force: true })
    }
  })

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
      if (process.platform === 'win32') {
        // Full `metro` would step past the unreadable guard into
        // `expo start`, so the verdict is asserted at the guard boundary
        // against the same default stage `metro` would use; the guard-first
        // wiring is pinned by 'metro consults the port guard before
        // starting expo' below.
        const guard = spawnSync(
          process.execPath,
          [GUARD, String(HELD_PORT), path.join(ROOT, 'example-expo', 'ios-tv')],
          { encoding: 'utf8' }
        )
        expect(guard.status).toBe(0)
        expect(guard.stderr).toContain('cannot determine')
      } else {
        const result = run(['metro'], { TV_METRO_PORT: String(HELD_PORT) })
        expect(result.exit).not.toBe(0)
        expect(`${result.stdout}${result.stderr ?? ''}`).toContain('held by another project')
        expect(`${result.stdout}${result.stderr ?? ''}`).toContain(outside)
      }
    } finally {
      holder.kill('SIGKILL')
      waitForExit(holder)
      fs.rmSync(outside, { recursive: true, force: true })
    }
  })

  test('metro consults the port guard before starting expo', () => {
    const script = fs.readFileSync(SCRIPT, 'utf8')
    const metro = script.slice(script.indexOf('cmd_metro() {'), script.indexOf('cmd_install_tv() {'))
    const guardAt = metro.indexOf('metro-port-guard.js')
    const startAt = metro.indexOf('expo start')
    expect(guardAt).toBeGreaterThan(-1)
    expect(startAt).toBeGreaterThan(-1)
    expect(guardAt).toBeLessThan(startAt)
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

  // Git Bash converts path-looking ARGUMENTS for a native Windows binary but
  // never rewrites a path embedded in the quoted `node -e` program string,
  // so `require('/c/...')` fails with MODULE_NOT_FOUND while the same path
  // as an argument is converted and resolves. Every node -e program must be
  // single-quoted (no shell interpolation) with paths passed as arguments.
  test('shell paths never hide inside a node -e program', () => {
    for (const file of [SCRIPT, HOSTS_SH, ANDROID_TV_EMU_SH]) {
      const text = fs.readFileSync(file, 'utf8')
      const offenders = text.split('\n').filter(line => /node\s+(-e|--eval)\s+"/.test(line))
      expect(offenders).toEqual([])
    }
  })

  // `python` is the canonical executable name on Windows while POSIX setups
  // only provide `python3`. The script must fall back instead of failing
  // when python3 is absent.
  test('bundle-url resolves python portably when python3 is absent', () => {
    const stage = stageDir()
    const shim = fs.mkdtempSync(path.join(gitBashExecutableTemp(), 'ubm-py-shim-'))
    try {
      const iosDir = path.join(stage, 'ios', 'TvApp')
      fs.mkdirSync(iosDir, { recursive: true })
      const delegate = path.join(iosDir, 'AppDelegate.swift')
      fs.writeFileSync(
        delegate,
        'func sourceURL() {\n    return RCTBundleURLProvider.sharedSettings().jsBundleURL(forBundleRoot: "index")\n}\n'
      )
      // A real interpreter to expose as `python` (on Windows there is no
      // python3 beyond the Store stub, so the real `python` is the source).
      const discovered = findOnPath(process.platform === 'win32' ? ['python'] : ['python3'])
      expect(discovered).not.toBe('')
      const resolved = p => {
        try {
          return fs.realpathSync(p)
        } catch {
          return p
        }
      }
      shimIntoDir(shim, discovered, process.platform === 'win32' ? 'python.exe' : 'python')
      // Hide every python3 on PATH (there can be more than one), re-shimming
      // any other tool the bundle-url path needs from an excluded directory.
      const excluded = new Set()
      const kept = process.env.PATH.split(path.delimiter).filter(entry => {
        if (entry === '') return false
        if (dirProvidesPython3(entry)) {
          excluded.add(resolved(entry))
          return false
        }
        return true
      })
      expect(excluded.size).toBeGreaterThan(0)
      for (const tool of ['bash', 'dirname', 'find', 'head', 'grep']) {
        const found = findOnPath([tool])
        if (found !== '' && excluded.has(resolved(path.dirname(found)))) {
          shimIntoDir(shim, found)
        }
      }
      const result = run(['bundle-url'], {
        TV_STAGE_DIR: stage,
        TV_LAN_HOST: '192.168.68.116',
        TV_METRO_PORT: '8091',
        PATH: `${shim}${path.delimiter}${kept.join(path.delimiter)}`
      })
      expect(result.exit).toBe(0)
      expect(fs.readFileSync(delegate, 'utf8')).toContain('192.168.68.116:8091')
    } finally {
      fs.rmSync(stage, { recursive: true, force: true })
      fs.rmSync(shim, { recursive: true, force: true })
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
