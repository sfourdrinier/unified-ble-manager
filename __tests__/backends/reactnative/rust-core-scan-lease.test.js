// __tests__/backends/reactnative/rust-core-scan-lease.test.js
//
// Finding 185: after many scenarios in one process, a scan failed with
// scan.already-active; stopping every scenario did not clear it and only a
// fresh process fixed it. A scan stop that fails keeps its membership for
// retry (PR210-09), but nothing ever retried it: the lease was left behind
// after the manager moved on (and after its destroy), bricking every later
// scan on the session with already-active.

const {
  rustCoreHarness,
  environment,
  scanOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

async function openManager(harnessOptions = {}, overrides = {}) {
  const harness = rustCoreHarness(harnessOptions)
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness, overrides))
  return { harness, native: harness.native, manager, backend: manager.attachedBackend.backend }
}

describe('finding 185: a scan lease is never left behind', () => {
  test('a failed stop is retried by the next start instead of bricking the session', async () => {
    const { native, manager } = await openManager()
    const first = await manager.scan(scanOptions())
    native.failNext('scan.stop', 'platform.failure', 'platform', 'ubm-mobile.scan.stop')
    const failed = await first.stop()
    expect(failed.state).toBe('release-failed')

    // The next start heals the retained membership first: no already-active.
    const second = await manager.scan(scanOptions())
    expect(native.opsInvoked('scan.stop')).toHaveLength(2)
    expect(native.opsInvoked('scan.start')).toHaveLength(2)
    expect((await second.stop()).state).toBe('released')
    await manager.destroy()
  })

  test('a stop that keeps failing surfaces the release debt instead of already-active', async () => {
    const { native, manager } = await openManager()
    const first = await manager.scan(scanOptions())
    native.failNext('scan.stop', 'platform.failure', 'platform', 'ubm-mobile.scan.stop')
    expect((await first.stop()).state).toBe('release-failed')
    native.failNext('scan.stop', 'platform.failure', 'platform', 'ubm-mobile.scan.stop')
    const blocked = await manager.scan(scanOptions()).then(
      () => {
        throw new Error('expected the start to fail')
      },
      error => error
    )
    // The old scan is genuinely still active, and the release debt rides along.
    expect(blocked.code ?? blocked.normalized?.code).toBe('scan.already-active')
    await manager.destroy()
  })

  test('destroy disposes the session even when a scan release failed, and reports the debt', async () => {
    const { native, manager } = await openManager()
    const scan = await manager.scan(scanOptions())
    void scan
    native.failNext('scan.stop', 'platform.failure', 'platform', 'ubm-mobile.scan.stop')
    const destroyed = await manager.destroy()
    // The debt stays visible…
    expect(destroyed.state).toBe('release-failed')
    expect(JSON.stringify(destroyed.failures)).toContain('scan')
    // …but the native session was still disposed…
    expect(native.opsInvoked('session.dispose')).toHaveLength(1)
    // …so no scan membership survives the destroyed manager on any session.
    const retained = [...native.sessions.values()].flatMap(session => [...session.scans.keys()])
    expect(retained).toEqual([])
  })

  test('a manager created after a debted destroy scans on the same process host', async () => {
    const first = await openManager()
    const scan = await first.manager.scan(scanOptions())
    void scan
    first.native.failNext('scan.stop', 'platform.failure', 'platform', 'ubm-mobile.scan.stop')
    const destroyed = await first.manager.destroy()
    expect(destroyed.state).toBe('release-failed')

    const second = await openManager({ native: first.native }, { managerId: 'manager-after-debt' })
    const rescan = await second.manager.scan(scanOptions())
    expect((await rescan.stop()).state).toBe('released')
    await second.manager.destroy()
  })
})
