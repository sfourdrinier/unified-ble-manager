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

  test('CI runs the packed acceptance on ubuntu-22.04 before the contracts job', () => {
    const job = ci.indexOf('bun-desktop-packed:')
    const contracts = ci.indexOf('\n  contracts:')
    const packedHost = 'node scripts/ci/packed-host-consumer-check.js'
    expect(job).toBeGreaterThan(-1)
    expect(contracts).toBeGreaterThan(job)
    expect(ci).toContain('runs-on: ubuntu-22.04')
    expect(ci).toContain('timeout-minutes: 90')
    expect(ci).toContain("prepack: 'true'")
    expect(ci).toContain('node scripts/ci/bun-packed-desktop-acceptance.js')
    expect(ci).toContain('bun-v1.4.2')
    expect(ci.split(packedHost)).toHaveLength(2)
  })
})
