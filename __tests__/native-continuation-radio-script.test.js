const { main } = require('../scripts/native-protocol/test-continuation-radio')
const fs = require('node:fs')
const path = require('node:path')

test('simulator guidance distinguishes paired loopback evidence from unrestricted compatibility', () => {
  const guide = fs.readFileSync(path.join(__dirname, '../tool/h10-sim/README.md'), 'utf8')
  expect(guide).not.toContain('neither breaking any scenario')
  expect(guide).toContain('same-daemon, two-adapter')
  expect(guide).toContain('test-created bonds')
})

function harness({
  drop = { ok: true, state: { dropped: ['central-address'] } },
  close = { state: 'released', failures: [] }
} = {}) {
  const logs = []
  const central = {
    startScan: jest.fn(async () => ({ operationId: 'scan-1' })),
    stopScan: jest.fn(async () => 'stopped'),
    takeScanObservation: jest.fn(async () => ({
      advertisement: { localName: 'SIM Polar H10 0001', peerId: 'AA:BB:CC:DD:EE:FF' }
    })),
    continuationPrepareClaim: jest.fn(async () => JSON.stringify({ ok: true, value: { claimToken: 'claim-1' } })),
    close: jest.fn(async () => close)
  }
  let statusCalls = 0
  const controller = {
    execute: jest.fn(async () => ({ resubscribed: 1 })),
    status: jest.fn(async () => ({
      queuedData: ++statusCalls * 10,
      continuationOutcome: { event: 'continuation.completed' }
    })),
    claim: jest.fn(async () => ({
      disposed: true,
      disposeFailure: null,
      controlLost: 0,
      afterCutoffLoss: { items: 0, bytes: 0 },
      streamEnds: [],
      values: Array.from({ length: 20 }, (_, i) => ({
        consumer: `ubm-continuation-${i % 2}`,
        value: Uint8Array.of(0, 72)
      }))
    }))
  }
  const binding = { openProduction: jest.fn(async () => central) }
  const api = {
    loadDesktopCoreBinding: jest.fn(async () => binding),
    createNativeContinuationController: jest.fn(() => controller)
  }
  return {
    central,
    controller,
    api,
    logs,
    options: {
      api,
      env: {
        UBM_NAPI_ADDON: '/tmp/test.node',
        UBM_RADIO_PLATFORM: 'corebluetooth',
        UBM_CONTINUATION_SECONDS: '30',
        UBM_SIM_CONTROL_PORT: '9000'
      },
      wait: async () => {},
      sendControl: async () => drop,
      log: message => logs.push(JSON.parse(message))
    }
  }
}

test('radio probe uses identity-checked one-central API and requires positive native data plus cleanup', async () => {
  const run = harness()
  await main(run.options)
  expect(run.api.loadDesktopCoreBinding).toHaveBeenCalledTimes(1)
  expect(run.api.createNativeContinuationController).toHaveBeenCalledWith(run.central)
  expect(run.central.close).toHaveBeenCalledTimes(1)
  expect(run.logs.at(-1).phase).toBe('passed')
})

test.each([
  {
    options: { drop: { ok: false, error: 'faithful mode' } },
    message: '"error":"faithful mode"'
  },
  {
    options: { drop: { ok: true, state: { dropped: [] } } },
    message: 'simulator did not report a dropped client link'
  },
  {
    options: { close: { state: 'release-failed', failures: [{ resourceKind: 'link' }] } },
    message: 'central cleanup remained unresolved'
  }
])('radio probe refuses missing disruption or failed cleanup: %j', async ({ options, message }) => {
  const run = harness(options)
  await expect(main(run.options)).rejects.toThrow(message)
  expect(run.central.close).toHaveBeenCalledTimes(1)
  expect(run.logs.some(log => log.phase === 'passed')).toBe(false)
})
