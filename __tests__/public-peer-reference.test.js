const { encodePeerReference, decodePeerReference } = require('../src/public/peer-reference')
const { mergePeerDirectoryRecords } = require('../src/public/peer-directory')
const {
  normalizeScanQuery,
  normalizeScanObservation,
  observationMatchesScanQuery
} = require('../src/public/scan-query')

describe('scoped PeerReference v1', () => {
  const reference = { version: 1, backendId: 'unified-ble:test', scope: 'application', opaqueId: 'peer-opaque-1' }

  test('round-trips deterministically and rejects malformed/future versions', () => {
    const encoded = encodePeerReference(reference)
    expect(encoded).toBe(
      '{"backendId":"unified-ble:test","opaqueId":"peer-opaque-1","scope":"application","version":1}'
    )
    expect(decodePeerReference(encoded)).toEqual(reference)
    expect(() => decodePeerReference('{"version":2,"backendId":"x","scope":"system","opaqueId":"y"}')).toThrow(
      'peer.reference-version-unsupported'
    )
    expect(() => decodePeerReference('{"version":1,"backendId":"","scope":"system","opaqueId":"y"}')).toThrow(
      'peer.reference-invalid'
    )
  })

  test('adds peers as the only PR5 ScanQuery clause extension', () => {
    const query = normalizeScanQuery({ anyOf: [{ peers: [reference] }] })
    const observation = normalizeScanObservation({
      localName: null,
      rssi: null,
      connectable: null,
      serviceUuids: [],
      manufacturerData: [],
      serviceData: [],
      peerReference: reference
    })
    expect(observationMatchesScanQuery(query, observation)).toBe(true)
    expect(() => normalizeScanQuery({ anyOf: [{ unknownFutureField: true }] })).toThrow()
  })

  test('merges duplicate scoped records conservatively and orders stronger sources first', () => {
    const peer = { id: 'peer-1', name: null, rssi: -60 }
    const merged = mergePeerDirectoryRecords([
      {
        reference,
        peer,
        source: 'backend-cache',
        state: { reachability: 'unknown', connection: 'disconnected', bond: 'unknown', lastSeenAtMonotonicMs: 4 },
        clockScope: 'backend-1'
      },
      {
        reference,
        peer: { ...peer, name: 'Known peer', rssi: -40 },
        source: 'system-connected',
        state: { reachability: 'reachable', connection: 'connected', bond: 'bonded', lastSeenAtMonotonicMs: 8 },
        clockScope: 'backend-1'
      }
    ])
    expect(merged).toHaveLength(1)
    expect(merged[0]).toMatchObject({ name: 'Known peer', rssi: -40, sources: ['system-connected', 'backend-cache'] })
    expect(merged[0].state).toMatchObject({
      reachability: 'reachable',
      connection: 'connected',
      bond: 'bonded',
      lastSeenAtMonotonicMs: 8
    })
  })
})
