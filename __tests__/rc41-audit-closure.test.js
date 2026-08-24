const { createEphemeralHostIdentity, normalizeBleManagerCreateOptions } = require('../src/public/host-identity')
const { resolveStreamPreset } = require('../src/public/stream-presets')
const { snapshotResourceCounters, diagnosticsUnavailable } = require('../src/public/diagnostics')
const { mergePeerDirectoryRecords } = require('../src/public/peer-directory')
const { BackendContractError } = require('../src/backend-contract/errors')

function expectCode(run, code) {
  try {
    run()
    throw new Error(`expected ${code}`)
  } catch (error) {
    expect(error).toBeInstanceOf(BackendContractError)
    expect(error.normalized.code).toBe(code)
  }
}

describe('rc.4.1 fail-closed audit closure', () => {
  test('P2-04 rejects injected randomBytes that are the wrong length or type', () => {
    expectCode(
      () => createEphemeralHostIdentity({ randomBytes: () => new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]) }),
      'argument.invalid'
    )
    expectCode(() => createEphemeralHostIdentity({ randomBytes: () => 'not-bytes' }), 'argument.invalid')
    const identity = createEphemeralHostIdentity({
      randomBytes: length => new Uint8Array(length).fill(9)
    })
    expect(identity.attachmentNonce).toHaveLength(32)
    expect(identity.operationNonce).toHaveLength(16)
  })

  test('P2-01 rejects non-record create options before Object.keys', () => {
    expectCode(() => normalizeBleManagerCreateOptions(null), 'argument.invalid')
    expectCode(() => normalizeBleManagerCreateOptions([]), 'argument.invalid')
    expectCode(() => normalizeBleManagerCreateOptions({ diagnostics: null }), 'argument.invalid')
  })

  test('P2-05 maps unknown and incomplete custom stream presets to argument.invalid', () => {
    expectCode(() => resolveStreamPreset({ preset: 'custom' }), 'argument.invalid')
    expectCode(() => resolveStreamPreset({ preset: 'not-a-preset' }), 'argument.invalid')
  })

  test('P2-03 rejects malformed counters and uses a public capability error when diagnostics are unavailable', () => {
    expectCode(
      () => snapshotResourceCounters({ activeScanControllers: Number.NaN }),
      'protocol.violation'
    )
    expectCode(() => snapshotResourceCounters({ scanConsumers: -1 }), 'protocol.violation')
    expectCode(() => snapshotResourceCounters({ connectionLeases: 1.5 }), 'protocol.violation')
    const diagnostics = diagnosticsUnavailable()
    expectCode(() => diagnostics.snapshot(), 'capability.unavailable')
  })

  test('P2-06 uses code-unit order and rejects non-finite RSSI', () => {
    const base = {
      source: 'scan-observed',
      state: { reachability: 'unknown', connection: 'unknown', bond: 'unknown', lastSeenAtMonotonicMs: null }
    }
    const upper = {
      ...base,
      reference: { version: 1, backendId: 'unified-ble:test', scope: 'application', opaqueId: 'A-peer' },
      peer: { id: 'A-peer', name: null, rssi: -40 }
    }
    const lower = {
      ...base,
      reference: { version: 1, backendId: 'unified-ble:test', scope: 'application', opaqueId: 'a-peer' },
      peer: { id: 'a-peer', name: null, rssi: -50 }
    }
    expect(mergePeerDirectoryRecords([lower, upper]).map(peer => peer.id)).toEqual(['A-peer', 'a-peer'])
    expectCode(
      () => mergePeerDirectoryRecords([{ ...upper, peer: { ...upper.peer, rssi: Number.NaN } }]),
      'peer.reference-invalid'
    )
    expectCode(
      () => mergePeerDirectoryRecords([{ ...upper, peer: { ...upper.peer, rssi: Number.POSITIVE_INFINITY } }]),
      'peer.reference-invalid'
    )
  })
})
