jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 34 },
  TurboModuleRegistry: { get: () => null },
  NativeModules: {}
}))

const { Platform } = require('react-native')
const { createReactNativeApplicationHost } = require('../../../src/react-native-app-manager')
const { createExpoBleManagerWithEnvironment } = require('../../../src/expo')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const { createReactNativeRustCoreBinding } = require('../../../src/backends/reactnative/react-native-rust-core-binding')
const {
  DeterministicRustCoreNative,
  DEFAULT_PEER
} = require('../../../test-support/react-native/deterministic-rust-core-native')
const { scanOptions, settle } = require('../../../test-support/react-native/rust-core-harness')

const terminalEvidence = Object.freeze({ maximumCanonicalBytes: 128, maximumDurableBytes: 1024 })

function binding(platform) {
  const native = new DeterministicRustCoreNative({ platform })
  return { native, binding: createReactNativeRustCoreBinding({ platform, native }) }
}

test.each([
  ['android', 'android'],
  ['ios', 'apple']
])('React Native %s factory reserves, binds, and releases terminal evidence for a scan', async (os, platform) => {
  Platform.OS = os
  const fixture = binding(platform)
  const host = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
    randomBytes: length => new Uint8Array(length).fill(1),
    terminalEvidence
  })
  const session = await host.manager.scan(scanOptions())
  const configured = fixture.native.calls.findIndex(call => call[0] === 'configureTerminalEvidenceMaximumBytes')
  const opened = fixture.native.calls.findIndex(call => call[0] === 'openSession')
  expect(fixture.native.calls[configured]).toEqual(['configureTerminalEvidenceMaximumBytes', '1024'])
  expect(configured).toBeGreaterThanOrEqual(0)
  expect(configured).toBeLessThan(opened)
  const reserve = fixture.native.calls.find(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.reserve')
  const start = fixture.native.calls.find(call => call[0] === 'invoke' && call[2] === 'scan.start')
  const reservation = JSON.parse(reserve[3])
  const startArgs = JSON.parse(start[3])
  expect(reservation).toMatchObject({ producer: 'scan', maximumBytes: 128, scope: expect.any(Object) })
  expect(startArgs.terminalEvidenceScope).toEqual(reservation.scope)
  await session.stop()
  // A retained terminal belongs to late readers; physical stop cannot free
  // its source-native evidence before they observe it.
  expect(
    fixture.native.calls.find(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')
  ).toBeUndefined()
  const terminal = await session.observations[Symbol.asyncIterator]().next()
  expect(terminal.value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
  await settle()
  expect(
    fixture.native.calls.find(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')
  ).toBeDefined()
  await host.manager.destroy()
})

test('React Native public GATT subscription reserves a scoped terminal-evidence slot before native subscribe', async () => {
  Platform.OS = 'android'
  const fixture = binding('android')
  const host = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
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
    await database.services[0].characteristics[0].subscribe()
    const reserves = fixture.native.calls.filter(
      call => call[0] === 'invoke' && call[2] === 'terminal-evidence.reserve'
    )
    const subscribe = fixture.native.calls.find(call => call[0] === 'invoke' && call[2] === 'gatt.subscribe')
    expect(reserves).toHaveLength(1)
    const reservation = JSON.parse(reserves[0][3])
    expect(reservation).toMatchObject({ producer: 'subscription', maximumBytes: 128, scope: expect.any(Object) })
    expect(JSON.parse(subscribe[3]).terminalEvidenceScope).toEqual(reservation.scope)
  } finally {
    await host.manager.destroy()
  }
})

test('React Native public recorded GATT subscription reserves a scoped terminal-evidence slot before native subscribe', async () => {
  Platform.OS = 'android'
  const fixture = binding('android')
  const host = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
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
    await database.services[0].characteristics[0].subscribeRecorded({
      recording: { id: 'terminal-evidence-recorded', maxBytes: 1048576, maxRecords: 1 }
    })
    const reserve = fixture.native.calls.find(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.reserve')
    const subscribe = fixture.native.calls.find(call => call[0] === 'invoke' && call[2] === 'gatt.subscribe-recorded')
    const reservation = JSON.parse(reserve[3])
    expect(reservation).toMatchObject({ producer: 'subscription', maximumBytes: 128, scope: expect.any(Object) })
    expect(JSON.parse(subscribe[3]).terminalEvidenceScope).toEqual(reservation.scope)
  } finally {
    await host.manager.destroy()
  }
})

test('React Native public GATT subscription retains a failed terminal-evidence release for remove retry', async () => {
  Platform.OS = 'android'
  const fixture = binding('android')
  const host = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
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
    fixture.native.failNext('terminal-evidence.release', 'platform.failure', 'stream')
    await expect(subscription.remove()).resolves.toMatchObject({ state: 'release-failed' })
    const terminal = await subscription.values[Symbol.asyncIterator]().next()
    expect(terminal.value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
    await settle()
    await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
    expect(
      fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')
    ).toHaveLength(2)
    expect(fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'gatt.unsubscribe')).toHaveLength(1)
  } finally {
    await host.manager.destroy()
  }
})

test('React Native public recorded GATT subscription retains a failed terminal-evidence release for remove retry', async () => {
  Platform.OS = 'android'
  const fixture = binding('android')
  const host = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
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
    const subscription = await database.services[0].characteristics[0].subscribeRecorded({
      recording: { id: 'terminal-evidence-retry', maxBytes: 1048576, maxRecords: 1 }
    })
    fixture.native.failNext('terminal-evidence.release', 'platform.failure', 'stream')
    await expect(subscription.remove()).resolves.toMatchObject({ state: 'release-failed' })
    await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
    expect(
      fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')
    ).toHaveLength(2)
    expect(fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'gatt.unsubscribe')).toHaveLength(1)
  } finally {
    await host.manager.destroy()
  }
})

test('Expo environment factory preserves terminal evidence through its RN host', async () => {
  const fixture = binding('android')
  const manager = await createExpoBleManagerWithEnvironment({
    platform: 'android',
    now: () => 1,
    clientId: 'expo-client',
    managerId: 'expo-manager',
    hostSessionScope: 'expo-scope',
    rustCore: fixture.binding,
    androidApiLevel: 34,
    terminalEvidence
  })
  const session = await manager.scan()
  const reserve = fixture.native.calls.find(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.reserve')
  expect(fixture.native.calls.filter(call => call[0] === 'configureTerminalEvidenceMaximumBytes')).toHaveLength(1)
  expect(JSON.parse(reserve[3])).toMatchObject({ producer: 'scan', maximumBytes: 128 })
  await session.stop()
  await manager.destroy()
})

test('a second RN factory may verify the same native durable bound but cannot replace it', async () => {
  Platform.OS = 'android'
  const fixture = binding('android')
  const options = { rustCore: fixture.binding, randomBytes: length => new Uint8Array(length).fill(1), terminalEvidence }
  const first = await createReactNativeApplicationHost(options)
  await first.manager.destroy()
  const second = await createReactNativeApplicationHost(options)
  await second.manager.destroy()
  expect(fixture.native.calls.filter(call => call[0] === 'configureTerminalEvidenceMaximumBytes')).toHaveLength(1)
  await expect(
    createReactNativeApplicationHost({
      ...options,
      terminalEvidence: { maximumCanonicalBytes: 128, maximumDurableBytes: 2048 }
    })
  ).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
})

test.each([
  ['android', 'android'],
  ['ios', 'apple']
])('React Native %s attaches terminal evidence after a normal host was already installed', async (os, platform) => {
  Platform.OS = os
  const fixture = binding(platform)
  const normal = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
    randomBytes: length => new Uint8Array(length).fill(1)
  })
  await normal.manager.destroy()
  const terminal = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
    randomBytes: length => new Uint8Array(length).fill(1),
    terminalEvidence
  })
  expect(fixture.native.calls.filter(call => call[0] === 'configureTerminalEvidenceMaximumBytes')).toHaveLength(1)
  const session = await terminal.manager.scan(scanOptions())
  expect(fixture.native.calls.some(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.reserve')).toBe(true)
  await session.stop()
  await session.observations[Symbol.asyncIterator]().next()
  await terminal.manager.destroy()
})

test('Expo late-first terminal configuration reaches its RN host', async () => {
  const fixture = binding('android')
  const environment = {
    platform: 'android',
    now: () => 1,
    clientId: 'expo-client',
    managerId: 'expo-manager',
    hostSessionScope: 'expo-scope',
    rustCore: fixture.binding,
    androidApiLevel: 34
  }
  const normal = await createExpoBleManagerWithEnvironment(environment)
  await normal.destroy()
  const terminal = await createExpoBleManagerWithEnvironment({ ...environment, terminalEvidence })
  expect(fixture.native.calls.filter(call => call[0] === 'configureTerminalEvidenceMaximumBytes')).toHaveLength(1)
  await terminal.destroy()
})

test('a terminal-evidence release failure stays as retryable cleanup debt', async () => {
  Platform.OS = 'android'
  const fixture = binding('android')
  const host = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
    randomBytes: length => new Uint8Array(length).fill(1),
    terminalEvidence
  })
  const session = await host.manager.scan(scanOptions())
  fixture.native.failNext('terminal-evidence.release', 'platform.failure', 'stream')
  await session.stop()
  await session.observations[Symbol.asyncIterator]().next()
  await settle()
  expect(host.manager.backendDiagnosticWarnings()).toEqual(
    expect.arrayContaining([expect.objectContaining({ code: 'terminal-evidence-release-failed' })])
  )
  await host.manager.destroy()
  expect(
    fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')
  ).toHaveLength(2)
})

test('a reservation-release failure before scan tracking remains retryable through destroy', async () => {
  Platform.OS = 'android'
  const fixture = binding('android')
  const host = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
    randomBytes: length => new Uint8Array(length).fill(1),
    terminalEvidence
  })
  fixture.native.failNext('scan.start', 'platform.failure', 'scan')
  fixture.native.failNext('terminal-evidence.release', 'platform.failure', 'stream')

  const failure = await host.manager.scan(scanOptions()).then(
    () => {
      throw new Error('expected scan start to fail')
    },
    error => error
  )
  expect(failure).toBeInstanceOf(AggregateError)
  expect(String(failure.errors)).toContain('ubm-mobile.scan.start')
  expect(String(failure.errors)).toContain('ubm-mobile.terminal-evidence.release')
  expect(
    fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')
  ).toHaveLength(1)

  await expect(host.manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  expect(
    fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')
  ).toHaveLength(2)
})

test.each(['terminal-evidence.reserve', 'scan.start'])(
  'destroy cancels a held %s acquisition without disposing its session',
  async held => {
    Platform.OS = 'android'
    const fixture = binding('android')
    const host = await createReactNativeApplicationHost({
      rustCore: fixture.binding,
      randomBytes: length => new Uint8Array(length).fill(1),
      terminalEvidence
    })
    fixture.native.hold(held)
    const starting = host.manager.scan(scanOptions())
    await settle()
    const destroying = host.manager.destroy()
    await expect(starting).rejects.toMatchObject({ normalized: { code: 'operation.cancelled-by-destroy' } })
    await expect(destroying).resolves.toMatchObject({ state: 'release-failed' })
    expect(fixture.native.calls.some(call => call[0] === 'invoke' && call[2] === 'session.dispose')).toBe(false)
    if (held === 'scan.start')
      expect(fixture.native.calls.some(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')).toBe(
        false
      )
    fixture.native.release(held)
    await settle(60)
    await expect(host.manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(
      fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')
    ).toHaveLength(1)
  }
)

test.each([
  ['terminal-evidence.reserve', 'ordinary'],
  ['terminal-evidence.reserve', 'recorded'],
  ['gatt.subscribe', 'ordinary'],
  ['gatt.subscribe-recorded', 'recorded']
])(
  'destroy fences a held %s %s subscription acquisition and compensates its late native result',
  async (held, kind) => {
    Platform.OS = 'android'
    const fixture = binding('android')
    const host = await createReactNativeApplicationHost({
      rustCore: fixture.binding,
      randomBytes: length => new Uint8Array(length).fill(1),
      terminalEvidence
    })
    const manager = await createPublicBleManager(host.manager, () => 1000)
    const peer = host.manager.attachedBackend.backend.connections.peerFromAddress({
      address: DEFAULT_PEER,
      addressType: 'public'
    })
    const connection = await manager.connect(peer)
    const database = await connection.discover()
    fixture.native.hold(held)
    const starting =
      kind === 'ordinary'
        ? database.services[0].characteristics[0].subscribe()
        : database.services[0].characteristics[0].subscribeRecorded({
            recording: { id: `held-${held}`, maxBytes: 1048576, maxRecords: 1 }
          })
    await settle()
    const destroying = host.manager.destroy()
    await expect(starting).rejects.toMatchObject({ code: 'operation.cancelled-by-destroy' })
    await expect(destroying).resolves.toMatchObject({ state: 'release-failed' })
    expect(fixture.native.calls.some(call => call[0] === 'invoke' && call[2] === 'session.dispose')).toBe(false)
    fixture.native.release(held)
    await settle(60)
    if (held !== 'terminal-evidence.reserve')
      expect(fixture.native.calls.some(call => call[0] === 'invoke' && call[2] === 'gatt.unsubscribe')).toBe(true)
    await expect(host.manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(
      fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')
    ).toHaveLength(1)
  }
)

test.each([
  ['gatt.subscribe', 'ordinary'],
  ['gatt.subscribe-recorded', 'recorded']
])('destroy retries failed late %s %s subscription compensation', async (held, kind) => {
  Platform.OS = 'android'
  const fixture = binding('android')
  const host = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
    randomBytes: length => new Uint8Array(length).fill(1),
    terminalEvidence
  })
  const manager = await createPublicBleManager(host.manager, () => 1000)
  const peer = host.manager.attachedBackend.backend.connections.peerFromAddress({
    address: DEFAULT_PEER,
    addressType: 'public'
  })
  const connection = await manager.connect(peer)
  const database = await connection.discover()
  fixture.native.hold(held)
  const starting =
    kind === 'ordinary'
      ? database.services[0].characteristics[0].subscribe()
      : database.services[0].characteristics[0].subscribeRecorded({
          recording: { id: `retry-${held}`, maxBytes: 1048576, maxRecords: 1 }
        })
  await settle()
  const destroying = host.manager.destroy()
  await expect(starting).rejects.toMatchObject({ code: 'operation.cancelled-by-destroy' })
  await expect(destroying).resolves.toMatchObject({ state: 'release-failed' })
  fixture.native.failNext('gatt.unsubscribe', 'platform.failure', 'gatt')
  fixture.native.release(held)
  await settle(60)
  await expect(host.manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  expect(fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'gatt.unsubscribe')).toHaveLength(2)
  expect(
    fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')
  ).toHaveLength(1)
})

test('a cancelled late scan whose stop fails remains visible for destroy retry', async () => {
  Platform.OS = 'android'
  const fixture = binding('android')
  const host = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
    randomBytes: length => new Uint8Array(length).fill(1),
    terminalEvidence
  })
  fixture.native.hold('scan.start')
  const starting = host.manager.scan(scanOptions())
  await settle()
  const destroying = host.manager.destroy()
  await expect(starting).rejects.toMatchObject({ normalized: { code: 'operation.cancelled-by-destroy' } })
  await expect(destroying).resolves.toMatchObject({ state: 'release-failed' })
  fixture.native.failNext('scan.stop', 'platform.failure', 'scan')
  fixture.native.release('scan.start')
  await settle(60)
  expect(host.manager.backendDiagnosticWarnings()).toEqual(
    expect.arrayContaining([expect.objectContaining({ code: 'cancelled-scan-stop-failed' })])
  )
  await expect(host.manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  expect(fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'scan.stop')).toHaveLength(2)
  expect(
    fixture.native.calls.filter(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')
  ).toHaveLength(1)
})

test('destroy reports evidence debt until a late reader drains the terminal', async () => {
  Platform.OS = 'android'
  const fixture = binding('android')
  const host = await createReactNativeApplicationHost({
    rustCore: fixture.binding,
    randomBytes: length => new Uint8Array(length).fill(1),
    terminalEvidence
  })
  const session = await host.manager.scan(scanOptions())
  await expect(host.manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
  expect(fixture.native.calls.some(call => call[0] === 'invoke' && call[2] === 'terminal-evidence.release')).toBe(false)
  await session.observations[Symbol.asyncIterator]().next()
  await settle()
  await expect(host.manager.destroy()).resolves.toMatchObject({ state: 'released' })
})
