'use strict'

// F9: every consumer refreshes what it needs itself before it builds or
// launches — hosts.sh up (desktop N-API for electron/node), build-tv.sh
// build (Apple RustCore), and the phone Expo iOS/Android builds — with an
// explicit UBM_NATIVE_REFRESH=off check-only opt-out. A refresh failure
// aborts the consumer; nothing continues on a stale artifact.

const { spawnSync } = require('node:child_process')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const ROOT = path.join(__dirname, '..')
const HOSTS_SH = path.join(ROOT, 'examples-shared', 'driver', 'hosts.sh')
const BUILD_TV_SH = path.join(ROOT, 'example-expo', 'scripts', 'build-tv.sh')
const ENSURE_JS = path.join(ROOT, 'scripts', 'native', 'ensure-native.js')

function stubPnpm(exitCode) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-stub-pnpm-'))
  const log = path.join(dir, 'calls.log')
  if (process.platform === 'win32') {
    // Windows Node (CreateProcess) cannot execute the extensionless sh
    // stub; PATHEXT resolves bare `pnpm` to this companion and runs it via
    // cmd, the same mechanism that runs the real pnpm shim. CRLF: cmd
    // batch files must use carriage returns.
    fs.writeFileSync(
      path.join(dir, 'pnpm.cmd'),
      `@echo off\r\necho %*>>${JSON.stringify(log)}\r\nexit /b ${exitCode}\r\n`
    )
  } else {
    fs.writeFileSync(path.join(dir, 'pnpm'), `#!/bin/sh\necho "$@" >> ${JSON.stringify(log)}\nexit ${exitCode}\n`, {
      mode: 0o755
    })
  }
  return { dir, log }
}

function run(file, args, env) {
  const result = spawnSync(file, args, { env: { ...process.env, ...env }, encoding: 'utf8' })
  return { exit: result.status ?? 1, stdout: result.stdout ?? '', stderr: result.stderr ?? '' }
}

describe('consumer refresh guards', () => {
  test('hosts.sh up refreshes desktop N-API for electron/node, never for tauri', () => {
    const script = fs.readFileSync(HOSTS_SH, 'utf8')
    expect(script.match(/ensure-native\.js/g)).toHaveLength(1)
    expect(script).toMatch(
      /if \[ "\$host" = electron \] \|\| \[ "\$host" = node \]; then\n[\s\S]*?ensure-native\.js" desktop/
    )
  })

  test('hosts.sh up aborts when the desktop refresh fails', () => {
    // The driver hosts may be live on this machine: stub pgrep so the
    // stray-process precondition is hermetic, and use a fresh STATE dir so
    // no real PID file is read. The run aborts at the refresh guard, before
    // any host is touched.
    const stub = stubPnpm(1)
    fs.writeFileSync(path.join(stub.dir, 'pgrep'), '#!/bin/sh\nexit 1\n', { mode: 0o755 })
    const state = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-hosts-state-'))
    const outcome = run('bash', [HOSTS_SH, 'up', 'electron'], {
      PATH: `${stub.dir}${path.delimiter}${process.env.PATH}`,
      UBM_DRIVER_HOSTS_STATE: state,
      UBM_HOSTS_DIRECT: '1'
    })
    expect(outcome.exit).not.toBe(0)
    expect(`${outcome.stdout}\n${outcome.stderr}`).toMatch(/stale|refresh/i)
    expect(fs.readFileSync(stub.log, 'utf8')).toMatch(/native:refresh --only desktop/)
  })

  test('build-tv.sh build refreshes Apple RustCore and aborts on failure', () => {
    const script = fs.readFileSync(BUILD_TV_SH, 'utf8')
    const build = script.slice(script.indexOf('cmd_build() {'), script.indexOf('cmd_metro() {'))
    expect(build).toContain('ensure-native.js')
    expect(build).toContain('apple')
    const stub = stubPnpm(1)
    const stage = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-tv-guard-'))
    const outcome = run('bash', [BUILD_TV_SH, 'build'], {
      PATH: `${stub.dir}${path.delimiter}${process.env.PATH}`,
      TV_STAGE_DIR: stage,
      DEVELOPMENT_TEAM: 'TESTTEAM'
    })
    expect(outcome.exit).not.toBe(0)
    expect(`${outcome.stdout}\n${outcome.stderr}`).toMatch(/stale|refresh/i)
  })

  test('phone Expo builds refresh what they consume (ios: apple, android: android)', () => {
    const pkg = JSON.parse(fs.readFileSync(path.join(ROOT, 'example-expo', 'package.json'), 'utf8'))
    expect(pkg.scripts.ios).toMatch(/ensure-native\.js.*apple/)
    expect(pkg.scripts.android).toMatch(/ensure-native\.js.*android/)
  })

  test('UBM_NATIVE_REFRESH=off switches ensure-native to check-only', () => {
    const stub = stubPnpm(0)
    const outcome = run(process.execPath, [ENSURE_JS, 'desktop'], {
      PATH: `${stub.dir}${path.delimiter}${process.env.PATH}`,
      UBM_NATIVE_REFRESH: 'off'
    })
    expect(outcome.exit).toBe(0)
    expect(fs.readFileSync(stub.log, 'utf8')).toMatch(/native:status --only desktop/)
  })

  test('ensure-native fails loudly when the refresh fails', () => {
    const stub = stubPnpm(3)
    const outcome = run(process.execPath, [ENSURE_JS, 'android'], {
      PATH: `${stub.dir}${path.delimiter}${process.env.PATH}`
    })
    expect(outcome.exit).not.toBe(0)
    expect(outcome.stderr).toMatch(/refusing to continue on a stale artifact/)
    expect(fs.readFileSync(stub.log, 'utf8')).toMatch(/native:refresh --only android/)
  })

  test('ensure-native check-only fails loudly when the status check fails', () => {
    const stub = stubPnpm(1)
    const outcome = run(process.execPath, [ENSURE_JS, 'apple'], {
      PATH: `${stub.dir}${path.delimiter}${process.env.PATH}`,
      UBM_NATIVE_REFRESH: 'off'
    })
    expect(outcome.exit).not.toBe(0)
    expect(outcome.stderr).toMatch(/UBM_NATIVE_REFRESH=off/)
  })
})
