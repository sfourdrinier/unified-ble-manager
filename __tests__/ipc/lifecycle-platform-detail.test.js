const { decodeConnectionEventStreamItem } = require('../../src/electron/connection-event-codec')
const { parseDesktopRustCoreLifecyclePlatform } = require('../../src/backends/desktop/desktop-rust-core-binding')

const platform = {
  domain: 'bluez-mgmt',
  code: '8',
  safeMessage: 'LE link ended',
  metadata: { phase: 'connection.watch' }
}

test('native lifecycle platform JSON uses the existing typed validated platform mapper', () => {
  expect(
    parseDesktopRustCoreLifecyclePlatform(
      JSON.stringify({ ...platform, message: platform.safeMessage, safeMessage: undefined })
    )
  ).toEqual(platform)
  expect(() => parseDesktopRustCoreLifecyclePlatform('{')).toThrow()
  expect(parseDesktopRustCoreLifecyclePlatform(null)).toBeUndefined()
})

test('Electron lifecycle codec preserves the platform fact with exact connection identity and existing cause', () => {
  const attachment = {
    attachmentId: 'attachment',
    backendInstanceId: 'backend',
    backendGeneration: 'backend-generation',
    adapter: {
      adapterId: 'adapter',
      displayName: null,
      adapterGeneration: 'adapter-generation',
      limitations: [],
      state: {
        availability: 'available',
        authorization: 'granted',
        power: 'on',
        heard: null,
        backendGeneration: 'backend-generation',
        updatedAt: 0,
        safeReason: null
      }
    }
  }
  const value = {
    kind: 'connection-lifecycle',
    schemaVersion: 2,
    attachment,
    attachmentId: attachment.attachmentId,
    peerId: 'peer',
    connectionId: 'connection',
    connectionGeneration: 'generation',
    ownerLeaseId: 'lease',
    sequence: 2,
    backendIngressOrdinal: 3,
    previous: 'connected',
    current: 'lost',
    cause: 'peer-link-loss',
    platform
  }
  const decoded = decodeConnectionEventStreamItem({ kind: 'value', value })
  expect(decoded.value.platform).toEqual(platform)
  expect(decoded.value.connectionGeneration).toBe('generation')
  expect(decoded.value.cause).toBe('peer-link-loss')
  expect(() =>
    decodeConnectionEventStreamItem({ kind: 'value', value: { ...value, platform: { ...platform, code: 8 } } })
  ).toThrow()
})
