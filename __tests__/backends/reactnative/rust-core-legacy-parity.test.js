// __tests__/backends/reactnative/rust-core-legacy-parity.test.js
//
// Legacy-vs-Rust capability parity for React Native (FIX-PLAN directive: no
// capability may regress; PARITY-INVENTORY.md §5–6). The legacy TypeScript
// providers are opened as a reference with the production handshake values
// they received on device; the Rust route must register every capability they
// registered, in the same state, and every registered capability must execute
// through its frozen wire op. Extras are listed explicitly and exercised.

const {
  DeterministicNativeControl,
  DeterministicReactNativeProtocolRuntime
} = require('../../../test-support/react-native/deterministic-legacy-native-protocol')
const {
  rustCoreHarness,
  environment,
  settle,
  scanOptions,
  subscribeOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER, defaultPeripheral } = require('../../../test-support/react-native/deterministic-rust-core-native')
const {
  createReactNativeAndroidBackendProvider: createLegacyAndroidProvider
} = require('../../../src/backends/reactnative/react-native-android-provider')
const {
  createReactNativeAppleLegacyBackendProvider
} = require('../../../src/backends/reactnative/react-native-apple-provider')
const {
  reactNativeAndroidDefaultAdapterId,
  reactNativeAppleDefaultAdapterId
} = require('../../../src/backends/reactnative/react-native-platform-identity')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { opaqueId } = require('../../../src/backend-contract/primitives')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

/** Opens a legacy reference backend with the handshake the production native module answered. */
async function legacyBackend(platform) {
  const control = new DeterministicNativeControl(platform === 'android')
  const handshake = control.handshake.bind(control)
  control.handshake = async request => ({
    ...(await handshake(request)),
    // UnifiedBleProtocolControlModule.java: phyAvailable = SDK >= 26, security
    // available, cancel-pairing never advertised. Apple's .mm advertised neither.
    ...(platform === 'android' ? { phyAvailable: true, securityCancelPairingAvailable: false } : {})
  })
  global.__unifiedBleNativeProtocolV2 = new DeterministicReactNativeProtocolRuntime(control, false)
  const provider =
    platform === 'android'
      ? createLegacyAndroidProvider({ control, now: () => 1 })
      : createReactNativeAppleLegacyBackendProvider({ control, now: () => 1 })
  return provider.create({
    selectedAdapterId:
      platform === 'android' ? reactNativeAndroidDefaultAdapterId() : reactNativeAppleDefaultAdapterId()
  })
}

async function rustManager(platform, overrides = {}, harnessOptions = {}) {
  const harness = rustCoreHarness({ platform, ...harnessOptions })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness, overrides))
  return { native: harness.native, manager, backend: manager.attachedBackend.backend }
}

function registrationStates(registry) {
  return Object.fromEntries(registry.registrations.map(registration => [registration.id, registration.state]))
}

async function failure(promise) {
  return promise.then(
    () => {
      throw new Error('expected a rejection')
    },
    error => error.normalized ?? error
  )
}

async function take(stream) {
  return stream[Symbol.asyncIterator]().next()
}

afterEach(() => {
  delete global.__unifiedBleNativeProtocolV2
})

/**
 * Deliberate 5.0 capability changes, not regressions: legacy React Native
 * reported `gatt:maximum-write-length` unavailable; 5.0 answers it from the
 * platform through the Rust owner (CHANGELOG, docs/MOBILE_RUST_WIRE.md).
 * Finding 217 likewise answers Apple `connection:effective-mtu` per link as
 * `maximumWriteValueLength(.withResponse) + 3` (owner modernize rule); the
 * legacy Apple reference route keeps reporting it unsupported.
 */
const FIVE_ZERO_STATES = Object.freeze({
  'gatt:maximum-write-length': 'limited',
  'connection:effective-mtu': 'limited'
})

describe.each([
  ['android', ['discovery:continuous-scan', 'security:cancel-pairing']],
  ['apple', ['discovery:continuous-scan']]
])('%s: the Rust route registers every legacy capability in the same state', (platform, extras) => {
  test('feature registry parity', async () => {
    const legacy = await legacyBackend(platform)
    const legacyStates = registrationStates(legacy.features)
    await legacy.destroy()
    const { manager, backend } = await rustManager(platform)
    const rustStates = registrationStates(backend.features)
    for (const [id, state] of Object.entries(legacyStates)) {
      expect({ id, state: rustStates[id] }).toEqual({ id, state: FIVE_ZERO_STATES[id] ?? state })
    }
    const added = Object.keys(rustStates).filter(id => !(id in legacyStates))
    expect(added.sort()).toEqual([...extras].sort())
    await manager.destroy()
  })

  test('backend method parity: every legacy connection, peer and security method exists', async () => {
    const legacy = await legacyBackend(platform)
    const legacyConnections = Object.keys(legacy.connections).filter(
      key => typeof legacy.connections[key] === 'function'
    )
    const legacyPeers =
      legacy.peers === undefined ? [] : ['resolve', 'known', 'connected', 'bonded', 'authorized', 'restored']
    const legacySecurity = legacy.security !== undefined
    await legacy.destroy()
    const { manager, backend } = await rustManager(platform)
    for (const method of legacyConnections) {
      if (method === 'writeWithoutResponseReadiness') continue // not registered by either route (unavailable boundary)
      expect({ method, present: typeof backend.connections[method] }).toEqual({ method, present: 'function' })
    }
    for (const method of legacyPeers) expect(typeof backend.peers[method]).toBe('function')
    expect(backend.security !== undefined).toBe(legacySecurity)
    await manager.destroy()
  })
})

// Physical run (Samsung, Expo Android): the manager reported
// `discovery:continuous-scan` unsupported, so `discovery.kind` was
// `system-chooser` and a driver that read the capability called choose(),
// while find() scanned fine. The Rust owner runs `scan.start` on both mobile
// platforms: the capability is reported where it is true (legacy React
// Native never registered it — a 4.x under-report, fixed in 5.0), and the
// chooser the RN route does not implement stays unsupported.
describe.each(['android', 'apple'])('%s: discovery capabilities are reported truthfully', platform => {
  test('continuous scan is supported, the system chooser is not', async () => {
    const { manager, backend } = await rustManager(platform)
    const states = registrationStates(backend.features)
    expect(states['discovery:continuous-scan']).toBe('limited')
    expect(manager.supports('discovery:continuous-scan')).toBe(true)
    expect(manager.supports('discovery:system-chooser')).toBe(false)
    await manager.destroy()
  })
})

describe('Android: every registered capability executes through its wire op', () => {
  test('connection controls: RSSI, request MTU, effective MTU, priority, read/request PHY', async () => {
    const { native, manager, backend } = await rustManager('android')
    const connection = await manager.connect(
      backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' }),
      NO_OPTIONS
    )
    expect((await connection.readRssi(NO_OPTIONS)).rssi).toBe(-47)
    expect(await connection.requestMtu(247, NO_OPTIONS)).toMatchObject({ requestedMtu: 247, negotiatedMtu: 247 })
    expect(await connection.effectiveMtu()).toMatchObject({ attMtu: 247, payloadBytes: 244 })
    expect(await connection.requestPriority('high-throughput', NO_OPTIONS)).toMatchObject({ accepted: true })
    expect(await connection.readPhy(NO_OPTIONS)).toMatchObject({ txPhy: 'le-1m', rxPhy: 'le-1m' })
    expect(await connection.requestPhy({ tx: 'le-2m', rx: 'le-2m' }, NO_OPTIONS)).toMatchObject({
      accepted: true,
      observation: { txPhy: 'le-2m', rxPhy: 'le-2m' }
    })
    const lease = native.opsInvoked('connection.connect')[0].lease
    for (const op of [
      'connection.rssi',
      'connection.request-mtu',
      'connection.effective-mtu',
      'connection.request-priority',
      'connection.read-phy',
      'connection.request-phy'
    ]) {
      expect({ op, args: native.opsInvoked(op)[0] }).toMatchObject({ op, args: { peerId: DEFAULT_PEER, lease } })
    }
    expect((await failure(connection.requestMtu(22, NO_OPTIONS))).code).toBe('argument.invalid')
    await manager.destroy()
  })

  test('PHY below API 26 is reported unsupported and never reaches the owner', async () => {
    const { native, manager, backend } = await rustManager('android', { androidApiLevel: 25 })
    expect(backend.features.registrations.find(entry => entry.id === 'connection:phy').state).toBe('unsupported')
    const connection = await manager.connect(
      backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' }),
      NO_OPTIONS
    )
    expect((await failure(connection.readPhy(NO_OPTIONS))).code).toBe('capability.unsupported')
    expect(native.opsInvoked('connection.read-phy')).toHaveLength(0)
    await manager.destroy()
  })

  test('when-available connect, address targeting, address scan filter and platform scan options reach the owner', async () => {
    const { native, manager, backend } = await rustManager('android')
    const peerId = backend.connections.peerFromAddress({ address: 'a0-9e-1a-00-00-01', addressType: 'random' })
    const connection = await manager.connect(peerId, { ...NO_OPTIONS, intent: 'when-available' })
    expect(native.opsInvoked('connection.connect')[0]).toMatchObject({ peerId: DEFAULT_PEER, intent: 'when-available' })
    await connection.release()
    const scan = await manager.scan(
      scanOptions({
        filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null, deviceAddresses: [DEFAULT_PEER] },
        platform: { kind: 'android', mode: 'low-latency', callbackType: 'first-match', legacy: false }
      })
    )
    expect(native.opsInvoked('scan.start')[0]).toMatchObject({
      deviceAddresses: [DEFAULT_PEER],
      platform: { mode: 'low-latency', callbackType: 'first-match', legacy: false }
    })
    await scan.stop()
    await manager.destroy()
  })

  test('bonded-peer enumeration and bonded reference resolution', async () => {
    const peripheral = defaultPeripheral()
    peripheral.bonded = true
    const { manager, backend } = await rustManager('android', {}, { nativeOptions: { peripherals: [peripheral] } })
    const bonded = await backend.peers.bonded(NO_OPTIONS)
    expect(bonded).toHaveLength(1)
    expect(bonded[0]).toMatchObject({ source: 'system-bonded', reference: { scope: 'system' } })
    expect(bonded[0].reference.opaqueId).not.toContain(DEFAULT_PEER)
    const resolved = await backend.peers.resolve(bonded[0].reference, NO_OPTIONS)
    expect(resolved.peerId).toBe(bonded[0].peerId)
    const known = await backend.peers.known(NO_OPTIONS)
    expect(known[0].reference).toMatchObject({ scope: 'origin', opaqueId: DEFAULT_PEER })
    expect((await backend.peers.resolve(known[0].reference, NO_OPTIONS)).peerId).toBe(known[0].peerId)
    await manager.destroy()
  })

  test('security: state, pair, already-paired, cancel pairing, unpair and security events', async () => {
    const { native, manager, backend } = await rustManager('android')
    const security = manager.securityBackend()
    const peerId = String(backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' }))
    expect(await security.state(peerId, NO_OPTIONS)).toMatchObject({ bond: 'not-bonded' })
    const watch = security.watch(peerId)
    expect((await take(watch)).value.value.state.bond).toBe('not-bonded')
    const pairOptions = { ...NO_OPTIONS, transport: 'auto', protection: 'system-default', ceremony: 'system' }
    expect(await security.pair(peerId, pairOptions)).toMatchObject({ outcome: 'paired' })
    await settle(60)
    expect((await take(watch)).value.value.state.bond).toBe('bonded')
    expect(await security.pair(peerId, pairOptions)).toMatchObject({ outcome: 'already-paired' })
    native.peripherals.get(DEFAULT_PEER).bonded = false
    native.deferNextPair()
    const pending = security.pair(peerId, pairOptions)
    await settle()
    expect(await security.cancelPairing(peerId, NO_OPTIONS)).toEqual({ outcome: 'cancelled' })
    expect(await pending).toEqual({ outcome: 'cancelled' })
    expect(await security.unpair(peerId, NO_OPTIONS)).toEqual({ outcome: 'unsupported' })
    await watch.close()
    await manager.destroy()
  })

  test('an owner-ended scan (scan-end) ends the stream with the owner’s reason', async () => {
    const { native, manager } = await rustManager('android')
    const scan = await manager.scan(scanOptions())
    native.endScans('source-failed')
    await settle(60)
    expect((await take(scan.observations)).value).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
    expect((await scan.stop()).state).toBe('released')
    expect(native.opsInvoked('scan.stop')).toHaveLength(0)
    await manager.destroy()
  })

  test('adapter-state changes reach adapter watches', async () => {
    const { native, manager } = await rustManager('android')
    const watch = await manager.adapterStates()
    native.setAdapter({ power: 'off', safeReason: 'bluetooth off' })
    await settle(60)
    expect((await take(watch.values)).value.value).toMatchObject({ power: 'off', safeReason: 'bluetooth off' })
    await watch.stop()
    await manager.destroy()
  })

  test('an ingress drop is reported, never silent', async () => {
    const { native, manager, backend } = await rustManager('android')
    const events = backend.events()
    native.ingressDrop('control', 3)
    await settle(60)
    expect((await take(events)).value.value).toMatchObject({
      kind: 'diagnostic-warning',
      code: 'native-ingress-drop',
      detail: { class: 'control', count: 3 }
    })
    await manager.destroy()
  })

  test('several managers share the process owner through separate session leases', async () => {
    const harness = rustCoreHarness({ platform: 'android' })
    const first = await createReactNativeBleManagerWithEnvironment(environment(harness))
    const second = await createReactNativeBleManagerWithEnvironment(
      environment(harness, { clientId: 'client-b', managerId: 'manager-b' })
    )
    expect(harness.native.sessions.size).toBe(2)
    const peer = first.attachedBackend.backend.connections.peerFromAddress({
      address: DEFAULT_PEER,
      addressType: 'public'
    })
    const connection = await first.connect(peer, NO_OPTIONS)
    const scan = await second.scan(scanOptions())
    await connection.release()
    await scan.stop()
    await first.destroy()
    await second.destroy()
    expect(harness.native.calls.filter(call => call[0] === 'closeSession')).toHaveLength(2)
  })
})

describe('Apple: RSSI and derived MTU work; controls CoreBluetooth lacks are refused before the owner', () => {
  test('RSSI and effectiveMtu execute; request-MTU/PHY/priority are unsupported with no native call', async () => {
    const { native, manager, backend } = await rustManager('apple')
    const connection = await manager.connect(backend.peerIdForNativeId(DEFAULT_PEER), NO_OPTIONS)
    expect((await connection.readRssi(NO_OPTIONS)).rssi).toBe(-47)
    // Finding 217: the owner derives the ATT MTU per link as
    // `maximumWriteValueLength(.withResponse) + 3`; request-MTU stays refused.
    await expect(connection.effectiveMtu()).resolves.toMatchObject({ attMtu: 515, payloadBytes: 512 })
    for (const call of [
      () => connection.requestMtu(247, NO_OPTIONS),
      () => connection.readPhy(NO_OPTIONS),
      () => connection.requestPhy({ tx: 'le-2m' }, NO_OPTIONS),
      () => connection.requestPriority('balanced', NO_OPTIONS)
    ]) {
      expect((await failure(call())).code).toBe('capability.unsupported')
    }
    expect(native.opsInvoked('connection.effective-mtu')).toHaveLength(1)
    for (const op of ['connection.request-mtu', 'connection.read-phy', 'connection.request-priority']) {
      expect(native.opsInvoked(op)).toHaveLength(0)
    }
    expect(backend.connections.peerFromAddress).toBeUndefined()
    expect(
      (
        await failure(
          manager.connect(backend.peerIdForNativeId(DEFAULT_PEER), { ...NO_OPTIONS, intent: 'when-available' })
        )
      ).code
    ).toBe('capability.unsupported')
    expect((await failure(manager.scan(scanOptions({ platform: { kind: 'android', mode: 'balanced' } })))).code).toBe(
      'capability.unsupported'
    )
    expect(native.opsInvoked('scan.start')).toHaveLength(0)
    await manager.destroy()
  })
})

describe('Android scan platform options the legacy boundary refused (139, AN-1)', () => {
  test.each([
    ['phy', { phy: 'le-coded' }],
    ['reportDelayMs', { reportDelayMs: 500 }]
  ])('an Android scan %s is capability.unsupported with no owner call', async (_name, extra) => {
    const { native, manager } = await rustManager('android')
    const error = await failure(
      manager.scan(scanOptions({ platform: { kind: 'android', mode: 'balanced', ...extra } }))
    )
    expect(error.code).toBe('capability.unsupported')
    expect(error.domain).toBe('scan')
    expect(native.opsInvoked('scan.start')).toHaveLength(0)
    await manager.destroy()
  })
})

describe('Apple state restoration adoption (legacy native journal semantics)', () => {
  const authority = Object.freeze({
    namespaceValue: 'ubm-ns:tck',
    adoptionEpoch: 'epoch-1',
    clientId: 'ubm-client:tck',
    hostSessionScope: 'ubm-host:tck'
  })

  function adoptionRequest(manager, overrides = {}) {
    const identity = manager.identity
    return {
      namespace: authority.namespaceValue,
      attachmentId: identity.attachment.attachmentId,
      expectedBackendInstanceId: identity.attachment.backendInstanceId,
      expectedEpoch: opaqueId(authority.adoptionEpoch, 'restoration-epoch', 'react-native-restoration'),
      expectedVersions: identity.versions,
      ...overrides
    }
  }

  async function restoringManager(overrides = {}) {
    const opened = await rustManager('apple', {
      clientId: authority.clientId,
      hostSessionScope: authority.hostSessionScope,
      restorationAuthority: authority,
      ...overrides
    })
    opened.native.seedRestored([{ peerId: 'C0FFEE00-0000-4000-8000-000000000001', connected: true }])
    return opened
  }

  test('a rejection is non-consuming; adoption replays the adapter and restored connections exactly once', async () => {
    const { manager } = await restoringManager()
    const rejected = await manager.adoptRestoration(adoptionRequest(manager, { namespace: 'ubm-ns:other' }))
    expect(rejected.outcome).toBe('namespace-mismatch')
    const epoch = await manager.adoptRestoration(
      adoptionRequest(manager, { expectedEpoch: opaqueId('epoch-0', 'restoration-epoch', 'react-native-restoration') })
    )
    expect(epoch.outcome).toBe('epoch-mismatch')
    const adopted = await manager.adoptRestoration(adoptionRequest(manager))
    expect(adopted.outcome).toBe('adopted')
    expect(adopted.replayedRecords.map(record => [record.ordinal, record.kind])).toEqual([
      [1, 'adapter'],
      [2, 'connection']
    ])
    expect(String(adopted.replayedRecords[1].peerId)).toBe('C0FFEE00-0000-4000-8000-000000000001')
    // origin/main ios/NativeProtocol/UnifiedBleProtocolAppleExecution.mm:1856-1863 (appendRestorationRecords):
    // the connection path of restored record `n` is restoration-connection-n / -owner-n / -generation-n.
    const replayed = JSON.stringify(adopted.replayedRecords[1].payload)
    for (const legacyId of ['restoration-connection-2', 'restoration-owner-2', 'restoration-generation-2']) {
      expect(replayed).toContain(`"${legacyId}"`)
    }
    expect((await manager.adoptRestoration(adoptionRequest(manager))).outcome).toBe('already-consumed')
    await manager.destroy()
  })

  test('PR210-52: restored peers are adopted once per process; a second manager replays the adapter only', async () => {
    const first = await restoringManager({ managerId: 'manager-a' })
    const harness = { native: first.native, binding: null, platform: 'apple' }
    const second = await rustManager(
      'apple',
      {
        clientId: authority.clientId,
        hostSessionScope: authority.hostSessionScope,
        restorationAuthority: authority,
        managerId: 'manager-b'
      },
      { native: harness.native }
    )
    const adopted = await second.manager.adoptRestoration(adoptionRequest(second.manager))
    expect(adopted.outcome).toBe('adopted')
    expect(adopted.replayedRecords.map(record => record.kind)).toEqual(['adapter', 'connection'])
    // Legacy consumed the OS restoration identifiers on the first adoption:
    // the other manager's adoption succeeds with the adapter record only.
    const late = await first.manager.adoptRestoration(adoptionRequest(first.manager))
    expect(late.outcome).toBe('adopted')
    expect(late.replayedRecords.map(record => record.kind)).toEqual(['adapter'])
    expect(first.native.opsInvoked('peers.claim-restored')).toEqual([{ maxPeers: 1023 }, { maxPeers: 1023 }])
    // Disposing the adopter does not hand the peer to a later manager.
    await second.manager.destroy()
    const third = await rustManager(
      'apple',
      {
        clientId: authority.clientId,
        hostSessionScope: authority.hostSessionScope,
        restorationAuthority: authority,
        managerId: 'manager-c'
      },
      { native: harness.native }
    )
    const after = await third.manager.adoptRestoration(adoptionRequest(third.manager))
    expect(after.replayedRecords.map(record => record.kind)).toEqual(['adapter'])
    await first.manager.destroy()
    await third.manager.destroy()
  })

  test('an unauthorized client and an unconfigured app are refused as platform.failure', async () => {
    const other = await restoringManager({ clientId: 'ubm-client:someone-else' })
    expect((await failure(other.manager.adoptRestoration(adoptionRequest(other.manager)))).code).toBe(
      'platform.failure'
    )
    expectConsoleErrorMatching(
      '[ReactNativeRestorationCoordinator.adopt] Native restoration adoption failed:',
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'platform.failure' }) })
    )
    await other.manager.destroy()
    const unconfigured = await rustManager('apple')
    expect((await failure(unconfigured.manager.adoptRestoration(adoptionRequest(unconfigured.manager)))).code).toBe(
      'platform.failure'
    )
    expectConsoleErrorMatching(
      '[ReactNativeRestorationCoordinator.adopt] Native restoration adoption failed:',
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'platform.failure' }) })
    )
    await unconfigured.manager.destroy()
  })

  test('Android has no restoration journal (legacy rule)', async () => {
    const { manager } = await rustManager('android')
    expect((await failure(manager.adoptRestoration(adoptionRequest(manager)))).code).toBe('capability.unsupported')
    await manager.destroy()
  })
})

describe('subscription delivery-mode parity', () => {
  test('Apple refuses a hard indication requirement on a notify+indicate characteristic before any effect', async () => {
    const peripheral = defaultPeripheral()
    peripheral.services[0].characteristics[0].properties = 0x18
    const { manager, backend } = await rustManager('apple', {}, { nativeOptions: { peripherals: [peripheral] } })
    const connection = await manager.connect(backend.peerIdForNativeId(DEFAULT_PEER), NO_OPTIONS)
    const database = await connection.discover(NO_OPTIONS)
    const path = (await database.snapshot()).characteristics[0].path
    expect(
      (await failure(database.subscribe(path, subscribeOptions({ deliveryMode: 'require-indication' })))).code
    ).toBe('capability.limited')
    await manager.destroy()
  })
})

describe('PR210-71 ownership transfer: the legacy factory never exposed a grant or a borrower', () => {
  // Legacy `createReactNativeBleManagerWithEnvironment` built its manager with
  // `createBleManagerFromProvider`, which issues the attachment's one
  // ownership authority internally and never returns it. Grants are
  // WeakMap-authenticated by that authority and a second authority for the
  // same attachment is refused, so no application could transfer ownership
  // of, or register a borrower on, that manager. The Rust route answers the
  // same identities.
  const legacyManager = require('../../../src/manager/ble-manager')
  const { createDeterministicTestBackend } = require('../../../src/testing/deterministic/deterministic-test-backend')
  const { version, versionRange } = require('../../../src/backend-contract/primitives')

  function compatibility() {
    return {
      backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
      capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
      eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
      traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
    }
  }

  async function observed(manager) {
    const foreign = createDeterministicTestBackend()
    const foreignAttached = await legacyManager.attachBleBackend(foreign.backend, compatibility())
    const foreignAuthority = legacyManager.createManagerOwnershipAuthority(foreignAttached)
    const identities = {}
    const capture = async (name, run) => {
      try {
        await run()
        identities[name] = 'accepted'
      } catch (error) {
        identities[name] = `${error.normalized.code} ${error.normalized.operation}`
      }
    }
    await capture('borrower', async () => {
      // Legacy refuses the authority itself; the Rust route refuses the
      // borrower's admission. Either way no borrower exists.
      const authority = legacyManager.createManagerOwnershipAuthority(manager.attachedBackend)
      await legacyManager.createBleManager(
        {
          attachedBackend: manager.attachedBackend,
          clientId: opaqueId('client-b', 'client', 'deterministic:b'),
          managerId: opaqueId('manager-b', 'manager', 'deterministic:b'),
          ownerMode: 'borrowing'
        },
        authority,
        legacyManager.DEFAULT_BLE_MANAGER_OPTIONS
      )
    })
    await capture('forged grant', () => manager.transferOwnership({}))
    await capture('foreign grant', () =>
      manager.transferOwnership(
        new (require('../../../src/manager/manager-ownership-authority').OwnershipTransferGrant)()
      )
    )
    await capture('destination role', () => manager.becomeOwnershipTransferDestination({}))
    await capture('relinquish role', () => manager.relinquishOwnershipTransferSource({}))
    identities.acceptsTransfer = manager.acceptsOwnershipTransfer()
    identities.ownerMode = manager.ownerMode
    await foreignAuthority.attachedBackend.backend.destroy()
    return identities
  }

  test('the Rust manager answers every ownership request exactly as the legacy manager did', async () => {
    const fixture = createDeterministicTestBackend()
    const legacy = await legacyManager.createBleManagerFromBackend(
      fixture.backend,
      {
        coreCompatibility: compatibility(),
        manager: {
          clientId: opaqueId('client-a', 'client', 'deterministic:a'),
          managerId: opaqueId('manager-a', 'manager', 'deterministic:a'),
          ownerMode: 'owning'
        }
      },
      legacyManager.DEFAULT_BLE_MANAGER_OPTIONS
    )
    const expected = await observed(legacy)
    await legacy.destroy()
    expect(expected).toMatchObject({
      borrower: 'ownership.denied manager-ownership-authority.issuance',
      'forged grant': 'ownership.denied manager-ownership-authority.transfer-grant',
      acceptsTransfer: false,
      ownerMode: 'owning'
    })

    const { native, manager } = await rustManager('android')
    const rust = await observed(manager)
    expect(rust.borrower).toBe('ownership.denied manager-ownership-authority.borrower-without-owner')
    const { borrower: _legacyBorrower, ...legacyRest } = expected
    const { borrower: _rustBorrower, ...rustRest } = rust
    expect(rustRest).toEqual(legacyRest)
    // The refused borrower left the owner's session untouched.
    expect(native.liveSessions()).toHaveLength(1)
    await manager.destroy()
  })
})

describe('attachment identity is the legacy React Native one, never a desktop host name', () => {
  /** A backend's attachment with its per-process instance ordinal abstracted. */
  function named(attachment) {
    const instance = String(attachment.backendInstanceId)
    return {
      backendInstance: instance.replace(/-\d+$/, '-<n>'),
      attachmentId: String(attachment.attachmentId).replace(instance, '<instance>'),
      backendGeneration: String(attachment.backendGeneration),
      adapterId: String(attachment.adapter.adapterId),
      adapterGeneration: String(attachment.adapter.adapterGeneration),
      displayName: attachment.adapter.displayName,
      stateGeneration: String(attachment.adapter.state.backendGeneration)
    }
  }

  test.each(['android', 'apple'])('%s: every attachment name has the legacy format', async platform => {
    const legacy = await legacyBackend(platform)
    const expected = named(legacy.identity.attachment)
    await legacy.destroy()
    expect(expected).toEqual({
      backendInstance: `react-native-${platform}-backend-<n>`,
      attachmentId: '<instance>:1:1',
      backendGeneration: '1',
      adapterId: platform === 'android' ? 'android-default-adapter' : 'apple-corebluetooth-default-adapter',
      adapterGeneration: '1',
      displayName: platform === 'android' ? 'Android default BLE adapter' : 'Apple CoreBluetooth central adapter',
      stateGeneration: '1'
    })

    const { manager, backend } = await rustManager(platform)
    expect(named(backend.identity.attachment)).toEqual(expected)
    const state = await backend.adapter.currentState()
    expect(String(state.backendGeneration)).toBe('1')
    expect(JSON.stringify(backend.identity).toLowerCase()).not.toContain('desktop')
    await manager.destroy()
  })

  test('each backend is its own instance, numbered per process as legacy numbered them', async () => {
    const first = await rustManager('android')
    const second = await rustManager('android')
    const ordinal = backend => Number(/-(\d+)$/.exec(String(backend.identity.attachment.backendInstanceId))[1])
    expect(ordinal(second.backend)).toBeGreaterThan(ordinal(first.backend))
    await first.manager.destroy()
    await second.manager.destroy()
  })
})

describe('resource names have the legacy React Native formats', () => {
  // origin/main src/backends/corebluetooth/corebluetooth-backend.ts:615-618,915-921,1437 and
  // corebluetooth-gatt-operations.ts:80-82,207 — the shared direct-GATT core both legacy React
  // Native providers ran on; every counter is the backend's own and starts at 1.
  test.each(['android', 'apple'])('%s: scan, peer, connection, database and subscription', async platform => {
    const { native, manager, backend } = await rustManager(platform)
    const scan = await manager.scan(scanOptions())
    expect(String(scan.scanSessionId)).toBe('corebluetooth-scan-session-1')
    expect(String(scan.leaseId)).toBe('corebluetooth-scan-lease-1')
    native.emitAdvertisement()
    const observation = (await take(scan.observations)).value.value
    const peerId = observation.device.id
    expect(String(peerId)).toBe('corebluetooth-peer-1-1')
    expect((await scan.stop()).state).toBe('released')
    expect(backend.identity.registeredBackendId).toContain(platform)
    const connection = await manager.connect(peerId, NO_OPTIONS)
    expect(String(connection.connectionId)).toBe('corebluetooth-connection-1')
    expect(String(connection.ownerLeaseId)).toBe('corebluetooth-connection-lease-1')
    expect(String(connection.connectionGeneration)).toBe('corebluetooth-connection-generation-1')
    const database = await connection.discover(NO_OPTIONS)
    expect(String(database.path.databaseId)).toBe('corebluetooth-database-1')
    expect(String(database.path.databaseGeneration)).toBe('corebluetooth-database-generation-1')
    const path = (await database.snapshot()).characteristics[0].path
    const subscription = await database.subscribe(path, subscribeOptions())
    expect(String(subscription.subscriptionId)).toBe('corebluetooth-subscription-1')
    expect((await subscription.remove()).state).toBe('released')

    // A rediscovery and a reconnect advance the backend's own counters, as legacy did.
    const rediscovered = await connection.discover(NO_OPTIONS)
    expect(String(rediscovered.path.databaseId)).toBe('corebluetooth-database-2')
    expect(String(rediscovered.path.databaseGeneration)).toBe('corebluetooth-database-generation-2')
    expect((await connection.disconnect()).state).toBe('released')
    const again = await manager.connect(peerId, NO_OPTIONS)
    expect(String(again.connectionId)).toBe('corebluetooth-connection-2')
    expect(String(again.connectionGeneration)).toBe('corebluetooth-connection-generation-2')
    expect(native.opsInvoked('connection.connect')).toHaveLength(2)
    await manager.destroy()
  })
})

describe('adapter loss advances the generations and rebuilds the attachment as legacy did', () => {
  // origin/main corebluetooth-backend.ts handleAdapterState :1146 and advanceGeneration :1282.
  async function observeLoss(backend, lose) {
    const events = backend.events()
    const watch = await backend.adapter.watchState()
    const iterator = events[Symbol.asyncIterator]()
    const transitions = watch.transitions[Symbol.asyncIterator]()
    await lose()
    const kinds = []
    for (;;) {
      const item = await Promise.race([iterator.next(), settle(200).then(() => null)])
      if (item === null) break
      if (item.value.kind !== 'value') break
      kinds.push(item.value.value.kind)
      if (item.value.value.kind === 'backend-restarted') break
    }
    const snapshots = []
    for (;;) {
      const item = await Promise.race([transitions.next(), settle(50).then(() => null)])
      if (item === null || item.value.kind !== 'value') break
      snapshots.push([item.value.value.power, String(item.value.value.backendGeneration)])
    }
    return { kinds, snapshots, attachment: named(backend.identity.attachment) }
  }

  function named(attachment) {
    const instance = String(attachment.backendInstanceId)
    return {
      attachmentId: String(attachment.attachmentId).replace(instance, '<instance>'),
      backendGeneration: String(attachment.backendGeneration),
      adapterGeneration: String(attachment.adapter.adapterGeneration)
    }
  }

  test.each(['android', 'apple'])('%s', async platform => {
    const legacy = await legacyBackend(platform)
    const runtime = global.__unifiedBleNativeProtocolV2
    const expected = await observeLoss(legacy, async () => {
      runtime.emitEvent('adapterState', [
        {
          id: 15,
          value: {
            kind: 'adapterStateSnapshot',
            fields: [
              { id: 1, value: 'available' },
              { id: 2, value: 'granted' },
              { id: 3, value: 'off' }
            ]
          }
        }
      ])
    })
    await legacy.destroy()
    expect(expected.attachment).toEqual({
      attachmentId: '<instance>:2:2',
      backendGeneration: '2',
      adapterGeneration: '2'
    })

    const { native, manager, backend } = await rustManager(platform)
    // The owner's order (crates/ubm-mobile/tests/adapter_loss.rs): the change
    // under the old generations, then the advance as its own record.
    const observed = await observeLoss(backend, async () => {
      native.setAdapter({ power: 'off' })
      native.setAdapter({ backendGeneration: '2', adapterGeneration: '2' })
    })
    expect(observed).toEqual(expected)
    await manager.destroy()
  })
})

describe('public operation correlations are the legacy core ones (`operation-{n}`)', () => {
  // origin/main src/core/unified-ble-core.ts:179 minted every coordinator-run
  // operation's correlation as `operation-{n}` from one per-manager counter;
  // write receipts and connection-control results return it to the app
  // (`PortableOperationTerminalRecord.correlation`). The owner's wire
  // operation ids stay internal.
  const legacyManager = require('../../../src/manager/ble-manager')
  const { version, versionRange } = require('../../../src/backend-contract/primitives')

  function compatibility() {
    return {
      backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
      capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
      eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
      traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
    }
  }

  async function correlations(manager, peerId) {
    const connection = await manager.connect(peerId, NO_OPTIONS)
    const first = await connection.readRssi(NO_OPTIONS)
    const database = await connection.discover(NO_OPTIONS)
    const snapshot = await database.snapshot()
    await database.read(snapshot.characteristics[0].path, NO_OPTIONS)
    const receipt = await database.writeDescriptor(snapshot.descriptors[0].path, new Uint8Array([1, 0]), {
      ...NO_OPTIONS,
      mode: 'with-response'
    })
    const second = await connection.readRssi(NO_OPTIONS)
    return [first.terminal.correlation, receipt.terminal.correlation, second.terminal.correlation].map(String)
  }

  test('android and apple number them as the legacy manager did', async () => {
    const backend = await legacyBackend('android')
    const legacy = await legacyManager.createBleManagerFromBackend(
      backend,
      {
        coreCompatibility: compatibility(),
        manager: {
          clientId: opaqueId('client-a', 'client', 'legacy:a'),
          managerId: opaqueId('manager-a', 'manager', 'legacy:a'),
          ownerMode: 'owning'
        }
      },
      legacyManager.DEFAULT_BLE_MANAGER_OPTIONS
    )
    const expected = await correlations(
      legacy,
      backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
    )
    await legacy.destroy()
    expect(expected.every(value => /^operation-\d+$/.test(value))).toBe(true)

    for (const platform of ['android', 'apple']) {
      const { native, manager, backend: rust } = await rustManager(platform)
      const scan = await manager.scan(scanOptions())
      native.emitAdvertisement()
      const peerId = (await take(scan.observations)).value.value.device.id
      await scan.stop()
      expect(await correlations(manager, peerId)).toEqual(expected)
      expect(rust.identity.registeredBackendId).toContain(platform)
      await manager.destroy()
    }
  })
})
