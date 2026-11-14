jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 34 },
  TurboModuleRegistry: { get: () => null },
  NativeModules: {}
}))

const { Buffer } = require('node:buffer')
const { createHash } = require('node:crypto')
const { createReactNativeApplicationHost } = require('../../../src/react-native-app-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const { createReactNativeRustCoreBinding } = require('../../../src/backends/reactnative/react-native-rust-core-binding')
const { encodeTerminalEvidence } = require('../../../src/core/terminal-evidence-codec')
const {
  DeterministicRustCoreNative,
  DEFAULT_PEER
} = require('../../../test-support/react-native/deterministic-rust-core-native')
const { settle } = require('../../../test-support/react-native/rust-core-harness')

const terminalEvidence = Object.freeze({ maximumCanonicalBytes: 1024, maximumDurableBytes: 4096 })

test('public RN subscription hydrates committed native evidence after Services Changed', async () => {
  const native = new DeterministicRustCoreNative({ platform: 'android' })
  const rustCore = createReactNativeRustCoreBinding({ platform: 'android', native })
  const host = await createReactNativeApplicationHost({
    rustCore,
    randomBytes: length => new Uint8Array(length).fill(1),
    terminalEvidence
  })
  try {
    const manager = await createPublicBleManager(host.manager, () => 1000)
    const peer = host.manager.attachedBackend.backend.connections.peerFromAddress({
      address: DEFAULT_PEER,
      addressType: 'public'
    })
    const connection = await manager.connect(peer)
    const database = await connection.discover()
    const subscription = await database.services[0].characteristics[0].subscribe()
    const session = [...native.sessions.values()][0]
    const consumer = [...session.consumers.keys()][0]
    const nativeLease = [...session.leases.values()][0]
    const [reservationId, reservation] = [...native.terminalEvidenceReservations.entries()][0]
    const originalError = {
      code: 'platform.failure',
      domain: 'platform',
      operation: 'gatt.subscribe',
      retryability: 'never',
      platform: null
    }
    const canonicalBytes = encodeTerminalEvidence(
      {
        kind: 'terminal',
        reason: 'service-changed',
        droppedItems: 0,
        droppedBytes: 0,
        replacedItems: 0,
        error: originalError
      },
      reservation.scope
    )
    const digest = createHash('sha256').update(canonicalBytes).digest('hex')
    const committed = JSON.parse(
      await native.invoke(
        session.id,
        'terminal-evidence.commit',
        JSON.stringify({ reservationId, canonicalBytesBase64: Buffer.from(canonicalBytes).toString('base64'), digest })
      )
    )
    expect(committed).toMatchObject({ ok: true, value: { kind: 'committed', digest } })
    const evidence = {
      producer: 'subscription',
      ...committed.value,
      scope: reservation.scope
    }
    native.push(session, {
      t: 'db-changed',
      peerId: DEFAULT_PEER,
      connectionGeneration: nativeLease.generation,
      databaseGeneration: nativeLease.databaseGeneration
    })
    native.push(session, {
      t: 'stream-end',
      consumer,
      reason: 'invalidated',
      droppedItems: 0,
      droppedBytes: 0,
      evidence
    })
    await settle(100)
    expect((await subscription.values[Symbol.asyncIterator]().next()).value).toMatchObject({
      kind: 'terminal',
      reason: 'service-changed',
      error: originalError
    })
    expect(native.opsInvoked('terminal-evidence.read')).toEqual([
      { ref: committed.value.ref, maximumBytes: terminalEvidence.maximumCanonicalBytes }
    ])
  } finally {
    await host.manager.destroy()
  }
})
