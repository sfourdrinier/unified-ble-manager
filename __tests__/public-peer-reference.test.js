const { encodePeerReference, decodePeerReference } = require('../src/public/peer-reference')
const { peerFromPublicObservation, snapshotBlePeer } = require('../src/public/ble-manager')
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
    expect(() => encodePeerReference({ ...reference, extra: true })).toThrow('peer.reference-invalid')
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

  test('carries a trusted reference through compact scan normalization without aliasing it', () => {
    const mutableReference = { ...reference }
    const normalized = normalizeScanObservation({
      peerId: 'peer-1',
      peerReference: mutableReference,
      localName: 'Known peer',
      rssi: -40,
      serviceUuids: [],
      manufacturerData: [],
      serviceData: []
    })

    mutableReference.opaqueId = 'changed'
    expect(normalized.peerReference).toEqual(reference)
    expect(Object.isFrozen(normalized.peerReference)).toBe(true)

    const peer = peerFromPublicObservation({
      peerId: 'peer-1',
      peerReference: reference,
      localName: 'Known peer',
      rssi: -40,
      txPowerLevel: null,
      serviceUuids: [],
      manufacturerData: [],
      serviceData: []
    })
    expect(peer).toMatchObject({
      reference,
      sources: ['scan-observed'],
      lastAdvertisement: { peerReference: reference }
    })
    expect(snapshotBlePeer({ id: 'plain', name: null, rssi: null })).toMatchObject({
      reference: null,
      sources: [],
      lastAdvertisement: null
    })
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

  test('orders independent peers by source priority before their stable identity', () => {
    const restoredReference = { ...reference, opaqueId: 'restored-peer' }
    const merged = mergePeerDirectoryRecords([
      {
        reference: restoredReference,
        peer: { id: 'restored-peer', name: null, rssi: null },
        source: 'restored',
        state: { reachability: 'unknown', connection: 'disconnected', bond: 'unknown', lastSeenAtMonotonicMs: null }
      },
      {
        reference,
        peer: { id: 'connected-peer', name: null, rssi: null },
        source: 'system-connected',
        state: { reachability: 'reachable', connection: 'connected', bond: 'unknown', lastSeenAtMonotonicMs: null }
      }
    ])

    expect(merged.map(peer => peer.reference.opaqueId)).toEqual(['peer-opaque-1', 'restored-peer'])
  })

  test('does not expose mutable input state after merging', () => {
    const state = {
      reachability: 'reachable',
      connection: 'connected',
      bond: 'bonded',
      lastSeenAtMonotonicMs: 8
    }
    const merged = mergePeerDirectoryRecords([
      { reference, peer: { id: 'peer-1', name: null, rssi: -40 }, source: 'system-connected', state, clockScope: 'clock-1' }
    ])

    state.connection = 'disconnected'
    state.lastSeenAtMonotonicMs = 1

    expect(merged[0].state).toEqual({
      reachability: 'reachable',
      connection: 'connected',
      bond: 'bonded',
      lastSeenAtMonotonicMs: 8
    })
  })

  test('deduplicates references without delimiter collisions', () => {
    const collidingReference = { ...reference, backendId: 'unified-ble:test|peer', opaqueId: 'opaque' }
    const distinctReference = { ...reference, backendId: 'unified-ble:test', opaqueId: 'peer|opaque' }
    const merged = mergePeerDirectoryRecords([
      {
        reference: collidingReference,
        peer: { id: 'one', name: null, rssi: null },
        source: 'app-reference',
        state: { reachability: 'unknown', connection: 'unknown', bond: 'unknown', lastSeenAtMonotonicMs: null }
      },
      {
        reference: distinctReference,
        peer: { id: 'two', name: null, rssi: null },
        source: 'app-reference',
        state: { reachability: 'unknown', connection: 'unknown', bond: 'unknown', lastSeenAtMonotonicMs: null }
      }
    ])

    expect(merged).toHaveLength(2)
  })
})
