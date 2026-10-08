const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const { MANAGER_SCENARIO_IDS } = require('../scripts/ci/bun-packed-desktop-acceptance')

describe('packed Bun desktop acceptance', () => {
  const script = fs.readFileSync(path.join(root, 'scripts/ci/bun-packed-desktop-acceptance.js'), 'utf8')
  const ci = fs.readFileSync(path.join(root, '.github/workflows/ci.yml'), 'utf8')

  test('exports the six public manager scenario ids and does not pack on import', () => {
    expect(MANAGER_SCENARIO_IDS).toEqual([
      'manager.scan-connect-discover-read-notify-destroy',
      'manager.cancellation-deadline-and-late-completion',
      'manager.overflow-late-events-and-stream-settlement',
      'manager.generation-invalidation-reconnect-and-rediscovery',
      'manager.two-client-arbitration-and-retryable-cleanup',
      'manager.adapter-loss-and-zero-counter-settlement'
    ])
    expect(script).toContain('if (require.main === module)')
    expect(script).toContain("npm', ['pack'")
    expect(script).not.toContain('UBM_NAPI_ADDON=')
  })

  test('requires a sealed prebuild, both module formats, and a waker that fires', () => {
    const waker = script.indexOf('setEventWaker')
    const scan = script.indexOf('startScan')
    expect(waker).toBeGreaterThan(-1)
    expect(scan).toBeGreaterThan(waker)
    expect(script).toContain('delete process.env.UBM_NAPI_ADDON')
    expect(script).toContain("require.resolve('unified-ble-manager/package.json')")
    expect(script).toContain("loaded.mode, 'prebuilt'")
    expect(script).toContain("identity.profile, 'release'")
    expect(script).toContain("identity.binding, 'napi'")
    expect(script).toContain('src/generated/native-build-identity.ts')
    expect(script).toContain('wakes >= 1')
    expect(script).toContain('eventWakeFailures()')
    expect(script).toContain("require('unified-ble-manager/node/bluez')")
    expect(script).toContain("import('unified-ble-manager/node/bluez')")
    expect(script).toContain("import('unified-ble-manager/testing')")
    expect(script).toContain('runManagerScenarios')
  })

  test('joins the installed public host factory to the identity-checked native provider', () => {
    const route = fs.readFileSync(path.join(root, 'scripts/ci/packed-desktop-public-route.cjs'), 'utf8')
    expect(script).toContain('qualifyPublicRoute')
    expect(route).toContain('loadDesktopCoreBinding')
    expect(route).toContain('entry[host.factory]')
    expect(route).toContain('openProduction')
    expect(route).toContain('connection.controls.parameters()')
    expect(route).toContain('connection.controls.parameterEvents()')
    expect(route).toContain("stage.blockRadioOp('read')")
    expect(route).toContain('abort.abort()')
    expect(route).toContain('connection.release()')
    expect(route).toContain('for (const native of opened)')
    expect(route).toContain('native.resourceCounters()')
    expect(route).toContain('assert.equal(opens, 2)')
    expect(route).toContain("stage.failNextRadioOp('unsubscribe'")
    expect(route).toContain("stage.stageAdapterState('powered-off', true)")
    expect(route).toContain("overflowPolicy: 'error'")
    expect(route).toContain('stale database after rediscovery')
    expect(route).not.toContain('createDeterministicManagerScenarioFactory')
    expect(route).not.toContain("require('../../src/")
  })

  test('CI runs packed Node/Bun acceptance on all three desktop operating systems', () => {
    const job = ci.indexOf('bun-desktop-packed:')
    const contracts = ci.indexOf('\n  contracts:')
    const packedHost = 'node scripts/ci/packed-host-consumer-check.js'
    expect(job).toBeGreaterThan(-1)
    expect(contracts).toBeGreaterThan(job)
    expect(ci).toContain('os: [ubuntu-22.04, macos-latest, windows-latest]')
    expect(ci).toContain('timeout-minutes: 90')
    expect(ci).toContain("prepack: 'true'")
    expect(ci).toContain('node scripts/ci/bun-packed-desktop-acceptance.js')
    expect(ci).toContain('bun-v1.4.2')
    expect(ci.split(packedHost)).toHaveLength(2)
  })
})
