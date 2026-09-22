// __tests__/electron/packed-fixture-ipc-version.test.js
//
// The pack+install Electron boundary fixture
// (scripts/ci/electron-packed-boundary-fixture.js) fakes the main-process
// side of the bootstrap handshake with a hardcoded ipc-protocol version.
// When src/ipc/protocol.ts bumped 3 -> 4 (adapter-loss attachment rebind),
// the fixture stayed at 3, so the packed renderer's fail-closed bootstrap
// check refused it as `protocol.incompatible: version-accepted.ipc-protocol`
// — a gate that had been dark behind earlier smoke failures. Source-level
// suites kept passing because they negotiate through the real main router.
//
// This test replays the fixture's fake-host handshake against the real
// renderer client, so a stale fixture literal fails here instead of in a
// dark CI gate. It must not relax the version check: the client still
// refuses anything but IPC_PROTOCOL_VERSION.

const fs = require('fs')
const path = require('path')

const { ElectronRendererBleClient } = require('../../src/electron/renderer')
const { IPC_PROTOCOL_VERSION } = require('../../src/ipc/protocol')

const FIXTURE_PATH = path.resolve(__dirname, '../../scripts/ci/electron-packed-boundary-fixture.js')

function fixtureIpcProtocolLiteral() {
  const source = fs.readFileSync(FIXTURE_PATH, 'utf8')
  const matches = [...source.matchAll(/negotiated\('ipc-protocol',\s*(\d+)\)/g)]
  expect(matches.length).toBe(1)
  return Number(matches[0][1])
}

function negotiated(axis, value) {
  const selected = Object.freeze({ axis, value })
  const range = Object.freeze({ axis, minimum: selected, maximum: selected })
  return Object.freeze({ axis, selected, localRange: range, remoteRange: range })
}

// Same fake-host bootstrap shape as the fixture's createBootstrapResponse,
// but versioned by whatever literal the fixture currently carries.
function fixtureBootstrap(ipcVersion) {
  return {
    kind: 'bootstrap',
    bootstrap: {
      attachment: Object.freeze({ attachmentId: 'packed-attachment', backendGeneration: 'packed-backend-generation' }),
      attachmentId: 'packed-attachment',
      versions: Object.freeze({
        backendContract: negotiated('backend-contract', 1),
        capabilitySchema: negotiated('capability-schema', 1),
        eventSchema: negotiated('event-schema', 1),
        traceFormat: negotiated('trace-format', 1),
        ipcProtocol: negotiated('ipc-protocol', ipcVersion)
      }),
      capabilities: Object.freeze({
        schemaVersion: 2,
        backendGeneration: 'packed-backend-generation',
        descriptors: []
      }),
      renderer: Object.freeze({ clientId: 'packed-renderer', windowScope: 'window', sessionScope: 'session' }),
      rendererLease: Object.freeze({ leaseId: 'packed-lease', generation: 'packed-generation' })
    }
  }
}

test('packed fixture fake host offers the current IPC protocol version', () => {
  expect(fixtureIpcProtocolLiteral()).toBe(IPC_PROTOCOL_VERSION)
})

test('real renderer client accepts the fixture fake-host bootstrap', async () => {
  const requests = []
  const transport = {
    invoke: async request => {
      requests.push(request)
      if (request.kind === 'bootstrap') return fixtureBootstrap(fixtureIpcProtocolLiteral())
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      throw new Error(`unexpected renderer proxy request: ${request.kind}`)
    },
    subscribe: () => () => undefined,
    acknowledge: async () => undefined
  }
  const client = new ElectronRendererBleClient(transport)
  const bootstrap = await client.initialize()
  expect(bootstrap.versions.ipcProtocol.selected.value).toBe(IPC_PROTOCOL_VERSION)
  await client.destroy()
  expect(requests.map(request => request.kind)).toEqual(['bootstrap', 'release'])
})
