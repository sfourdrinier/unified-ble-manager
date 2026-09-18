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

describe.each([
  ['android', ['security:cancel-pairing']],
  ['apple', []]
])('%s: the Rust route registers every legacy capability in the same state', (platform, extras) => {
  test('feature registry parity', async () => {
    const legacy = await legacyBackend(platform)
    const legacyStates = registrationStates(legacy.features)
    await legacy.destroy()
    const { manager, backend } = await rustManager(platform)
    const rustStates = registrationStates(backend.features)
    for (const [id, state] of Object.entries(legacyStates)) {
      expect({ id, state: rustStates[id] }).toEqual({ id, state })
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

describe('Apple: RSSI works; controls CoreBluetooth lacks are refused before the owner', () => {
  test('RSSI executes; MTU/PHY/priority are unsupported with no native call', async () => {
    const { native, manager, backend } = await rustManager('apple')
    const connection = await manager.connect(backend.peerIdForNativeId(DEFAULT_PEER), NO_OPTIONS)
    expect((await connection.readRssi(NO_OPTIONS)).rssi).toBe(-47)
    for (const call of [
      () => connection.requestMtu(247, NO_OPTIONS),
      () => connection.effectiveMtu(),
      () => connection.readPhy(NO_OPTIONS),
      () => connection.requestPhy({ tx: 'le-2m' }, NO_OPTIONS),
      () => connection.requestPriority('balanced', NO_OPTIONS)
    ]) {
      expect((await failure(call())).code).toBe('capability.unsupported')
    }
    for (const op of [
      'connection.request-mtu',
      'connection.effective-mtu',
      'connection.read-phy',
      'connection.request-priority'
    ]) {
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
