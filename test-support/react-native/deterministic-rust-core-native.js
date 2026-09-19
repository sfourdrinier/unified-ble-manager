// test-support/react-native/deterministic-rust-core-native.js
//
// A deterministic `UnifiedBleRustCore` native module (src/NativeUnifiedBleRustCore.ts)
// that speaks wire `ubm-mobile-wire/1` (docs/MOBILE_RUST_WIRE.md) over JSON
// text exactly as the Rust mobile owner does: the argument key sets are the
// owner's (`crates/ubm-mobile/src/session.rs`), every answer is envelope or
// drain JSON text, and rejections carry the structured failure JSON. Tests pass
// it to the REAL production binding (`createReactNativeRustCoreBinding`), so
// every byte crosses the production serializer.
//
// It is deterministic test infrastructure only (AGENTS.md): no radio, no
// timers of its own. Wakes are emitted on a microtask, like a native event.

const { Buffer: NodeBuffer } = require('node:buffer')
/**
 * The sealed identity the double answers with. Read from the TypeScript
 * sources under jest; a harness outside jest (the packed-package proof)
 * passes the packed `expectedIdentity` instead.
 */
function sourceExpectedIdentity() {
  return require('../../src/generated/native-build-identity').EXPECTED_NATIVE_BUILD_IDENTITY
}

const WIRE_REVISION = 'ubm-mobile-wire/1'
const SERVICE_UUID = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC_UUID = '00002a37-0000-1000-8000-00805f9b34fb'
const CCCD_UUID = '00002902-0000-1000-8000-00805f9b34fb'
const BATTERY_SERVICE_UUID = '0000180f-0000-1000-8000-00805f9b34fb'
const BATTERY_LEVEL_UUID = '00002a19-0000-1000-8000-00805f9b34fb'
const USER_DESCRIPTION_UUID = '00002901-0000-1000-8000-00805f9b34fb'
const DEFAULT_PEER = 'A0:9E:1A:00:00:01'

const ARG_SCHEMAS = Object.freeze({
  'adapter.state': [[], []],
  'counters.describe': [[], []],
  'peers.known': [[], []],
  'peers.connected': [[], []],
  'peers.restored': [[], []],
  'peers.claim-restored': [['maxPeers'], []],
  'session.dispose': [[], []],
  'session.reconcile': [[], []],
  'peers.bonded': [['operationId'], ['budgetMs']],
  'peers.resolve': [['reference'], []],
  'scan.start': [
    ['serviceUuids', 'duplicatePolicy', 'operationId'],
    ['deviceAddresses', 'platform', 'budgetMs']
  ],
  'scan.stop': [['operationId'], ['budgetMs']],
  'connection.connect': [
    ['peerId', 'lease', 'operationId'],
    ['budgetMs', 'intent', 'transport', 'preferredPhy']
  ],
  'connection.disconnect': [
    ['peerId', 'lease'],
    ['budgetMs', 'operationId']
  ],
  'connection.effective-mtu': [['peerId', 'lease'], []],
  'connection.request-mtu': [['peerId', 'lease', 'mtu', 'operationId'], ['budgetMs']],
  'connection.request-priority': [['peerId', 'lease', 'priority', 'operationId'], ['budgetMs']],
  'connection.request-phy': [
    ['peerId', 'lease', 'operationId'],
    ['tx', 'rx', 'budgetMs']
  ],
  'connection.rssi': [['peerId', 'lease', 'operationId'], ['budgetMs']],
  'connection.read-phy': [['peerId', 'lease', 'operationId'], ['budgetMs']],
  'connection.maximum-write-length': [['peerId', 'lease', 'mode', 'operationId'], ['budgetMs']],
  'security.state': [['peerId'], ['budgetMs', 'operationId']],
  'security.cancel-pairing': [['peerId'], ['budgetMs', 'operationId']],
  'security.pair': [['peerId', 'transport', 'operationId'], ['budgetMs']],
  'gatt.discover': [['peerId', 'lease', 'operationId'], ['budgetMs']],
  'gatt.read': [['peerId', 'selector', 'operationId'], ['budgetMs']],
  'gatt.read-descriptor': [['peerId', 'selector', 'operationId'], ['budgetMs']],
  'gatt.write': [['peerId', 'selector', 'valueB64', 'mode', 'operationId'], ['budgetMs']],
  'gatt.write-descriptor': [['peerId', 'selector', 'valueB64', 'mode', 'operationId'], ['budgetMs']],
  'gatt.subscribe': [
    ['peerId', 'selector', 'consumer', 'operationId'],
    ['deliveryMode', 'budgetMs']
  ],
  'gatt.unsubscribe': [['peerId', 'selector', 'consumer', 'operationId'], ['budgetMs']],
  'background.acquire': [
    ['kind', 'reason'],
    ['budgetMs', 'operationId']
  ],
  'background.release': [['leaseId'], ['budgetMs']],
  'background.update-notification': [
    ['leaseId', 'title'],
    ['body', 'budgetMs']
  ],
  'companion.associate': [[], ['name', 'serviceUuid', 'budgetMs', 'operationId']],
  'presence.observe': [['peerId'], ['budgetMs', 'operationId']],
  'presence.unobserve': [['peerId'], ['budgetMs', 'operationId']],
  'op.cancel': [['operationId'], []]
})

/** crates/ubm-mobile `ADMISSION_WINDOW`. */
const ADMISSION_WINDOW = 65536

// `connection.effective-mtu` is answered on Apple (finding 217): the Swift
// adapter reports `maximumWriteValueLength(.withResponse) + 3` per link.
// `connection.request-mtu` stays refused: CoreBluetooth has no request API.
const APPLE_UNSUPPORTED = new Set([
  'connection.request-mtu',
  'connection.request-priority',
  'connection.read-phy',
  'connection.request-phy',
  'companion.associate',
  'presence.observe',
  'presence.unobserve'
])

class WireFault extends Error {
  constructor(code, domain, operation, detail = null, commit = null, platform = null, retryability = null) {
    super(`${code}: ${operation}`)
    // As the owner: `platform` is the radio's own identity (finding 113), null otherwise.
    this.failure = { code, domain, operation, detail, platform }
    this.commit = commit
    this.retryability = retryability
  }
}

function invalid(path) {
  return new WireFault('argument.invalid', 'core', `ubm-mobile.wire.${path}`)
}

function canonicalUuid(value) {
  if (typeof value !== 'string') throw invalid('uuid')
  const lower = value.toLowerCase()
  if (/^[0-9a-f]{4}$/.test(lower)) return `0000${lower}-0000-1000-8000-00805f9b34fb`
  if (/^[0-9a-f]{8}$/.test(lower)) return `${lower}-0000-1000-8000-00805f9b34fb`
  if (/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(lower)) return lower
  throw invalid('uuid')
}

function b64(bytes) {
  return NodeBuffer.from(bytes).toString('base64')
}

function unb64(text) {
  return new Uint8Array(NodeBuffer.from(text, 'base64'))
}

function sameInstance(selector, instance) {
  return (
    canonicalUuid(selector.serviceUuid) === canonicalUuid(instance.serviceUuid) &&
    selector.serviceOccurrence === instance.serviceOccurrence &&
    selector.characteristicOccurrence === instance.characteristicOccurrence
  )
}

function exact(args, required, optional, path = 'args') {
  if (typeof args !== 'object' || args === null || Array.isArray(args)) throw invalid(path)
  for (const key of required) if (!(key in args)) throw invalid(`${path}.${key}`)
  for (const key of Object.keys(args)) {
    if (!required.includes(key) && !optional.includes(key)) throw invalid(`${path}.${key}`)
  }
}

function defaultPeripheral(peerId = DEFAULT_PEER) {
  return {
    peerId,
    name: 'Polar H10 1234',
    rssi: -58,
    bonded: false,
    services: [
      {
        uuid: SERVICE_UUID,
        occurrence: 0,
        characteristics: [
          {
            uuid: CHARACTERISTIC_UUID,
            occurrence: 0,
            // READ | WRITE | WRITE_NO_RSP | NOTIFY
            properties: 0x0f,
            value: new Uint8Array([0x00, 0x48]),
            descriptors: [{ uuid: CCCD_UUID, occurrence: 0, value: new Uint8Array([0x00, 0x00]) }]
          }
        ]
      },
      // The duplicate-UUID world (docs/UNIFIED_SEMANTICS.md §9), after the
      // heart-rate service in discovery order: a second service UUID that
      // repeats, a repeated characteristic UUID under one service, and a
      // repeated descriptor UUID under one characteristic.
      {
        uuid: BATTERY_SERVICE_UUID,
        occurrence: 0,
        characteristics: [
          batteryLevel(
            0,
            [0x50],
            [
              { uuid: USER_DESCRIPTION_UUID, occurrence: 0, value: new Uint8Array([0x61]) },
              { uuid: USER_DESCRIPTION_UUID, occurrence: 1, value: new Uint8Array([0x62]) }
            ]
          ),
          batteryLevel(1, [0x51], [])
        ]
      },
      { uuid: BATTERY_SERVICE_UUID, occurrence: 1, characteristics: [batteryLevel(0, [0x52], [])] }
    ]
  }
}

function batteryLevel(occurrence, value, descriptors) {
  // READ | NOTIFY
  return { uuid: BATTERY_LEVEL_UUID, occurrence, properties: 0x0a, value: new Uint8Array(value), descriptors }
}

/**
 * The deterministic native module. `platform` selects the sealed identity
 * (`jni`/`uniffi`) and the platform rules the owner applies (Apple refuses
 * Android-only link controls, reports `unknown` delivery, and has no
 * companion association).
 */
class DeterministicRustCoreNative {
  constructor({
    platform = 'android',
    peripherals = [defaultPeripheral()],
    identity = null,
    expectedIdentity = null,
    drainResolution = 'microtask'
  } = {}) {
    /**
     * `'native-task'` resolves each drain in a host task of its own (Node
     * `setImmediate`), as a TurboModule promise resolves through the JS
     * call invoker: a task boundary that owes nothing to JS timers.
     */
    this.drainResolution = drainResolution
    const EXPECTED_NATIVE_BUILD_IDENTITY = expectedIdentity ?? sourceExpectedIdentity()
    this.expectedIdentity = EXPECTED_NATIVE_BUILD_IDENTITY
    this.platform = platform
    this.calls = []
    this.nextSessionId = 1
    this.sessions = new Map()
    this.wakeListeners = new Set()
    this.peripherals = new Map(peripherals.map(peripheral => [peripheral.peerId, peripheral]))
    this.initialBonds = new Map(peripherals.map(peripheral => [peripheral.peerId, peripheral.bonded]))
    this.restored = []
    // What the owner keeps for `session.reconcile` (104/105): the latest link
    // end and database change per peer, and the last security report.
    this.linkEnds = new Map()
    this.databaseChanges = new Map()
    this.securityReports = new Map()
    /** Set by `loseControl`: control records are counted, not queued. */
    this.controlLost = null
    /** Restored peer id → the session that claimed it; claims last for the process. */
    this.restorationClaims = new Map()
    /** Every `connection.connect` the owner admitted: `{ sessionId, peerId, preferredPhy }`. */
    this.connects = []
    /** Peer id → the ATT MTU the last `connection.request-mtu` negotiated on the current link. */
    this.negotiatedMtu = new Map()
    this.adapter = {
      availability: 'available',
      authorization: 'granted',
      power: 'on',
      safeReason: null,
      updatedAt: 0,
      // The owner's own legacy React Native generations (crates/ubm-mobile/src/identity.rs).
      backendGeneration: '1',
      adapterGeneration: '1'
    }
    this.binding = platform === 'android' ? 'jni' : 'uniffi'
    const sealed = EXPECTED_NATIVE_BUILD_IDENTITY.bindings[this.binding]
    this.identity = identity ?? {
      schema: EXPECTED_NATIVE_BUILD_IDENTITY.schema,
      binding: this.binding,
      contractRevision: EXPECTED_NATIVE_BUILD_IDENTITY.contractRevision,
      sourceDigest: sealed.sourceDigest,
      bindingSchema: sealed.bindingSchema,
      target: sealed.targets[0],
      profile: 'release',
      features: [],
      rustc: 'rustc 1.98.1 (deterministic)'
    }
    this.contractRevisionAnswer = EXPECTED_NATIVE_BUILD_IDENTITY.contractRevision
    this.wireRevisionAnswer = WIRE_REVISION
    this.faults = []
    // Foreground-service leases belong to the module instance (its background
    // scope), not to a session: they outlive `session.dispose` and end with
    // `invalidate()` (87/N8, docs/MOBILE_RUST_WIRE.md).
    this.backgroundLeases = new Set()
    this.presenceArmed = new Set()
    this.holds = new Map()
    this.liveOps = new Map()
    this.nextGeneration = 1
    this.nextBackground = 1
    this.pendingPair = null
    this.restorationAnswer = null
    /** Records `session.dispose` answers before releasing (release-failed injection). */
    this.disposeRecords = []
    /** Rewrites the `gatt.discover` value (malformed-discovery injection). */
    this.discoveryOverride = null
    /** What the scripted radio says characteristic reads are (Apple while notifying: `read-or-notification`). */
    this.readProvenance = 'read-response'
    this.randomSource = length => Uint8Array.from({ length }, (_, index) => (index * 37 + 11) & 0xff)
    this.onSessionWake = listener => {
      this.wakeListeners.add(listener)
      return { remove: () => this.wakeListeners.delete(listener) }
    }
  }

  // -- the frozen TurboModule surface ------------------------------------------------

  async nativeBuildIdentity() {
    this.calls.push(['nativeBuildIdentity'])
    return JSON.stringify(this.identity)
  }

  async contractRevision() {
    this.calls.push(['contractRevision'])
    return this.contractRevisionAnswer
  }

  async wireRevision() {
    this.calls.push(['wireRevision'])
    return this.wireRevisionAnswer
  }

  async randomBytes(length) {
    this.calls.push(['randomBytes', length])
    if (!Number.isSafeInteger(length) || length < 1 || length > 1024) {
      throw this.rejection(invalid('random-bytes.length'))
    }
    return b64(this.randomSource(length))
  }

  async restorationIdentity(requestJson) {
    this.calls.push(['restorationIdentity', requestJson])
    const request = JSON.parse(requestJson)
    const answer = this.restorationAnswer
    // `{}` asks for the natively configured identity: `null` when none.
    if (Object.keys(request).length === 0) return JSON.stringify(answer)
    if (answer === null || answer.restorationId !== request.restorationId || answer.generation !== request.generation) {
      throw this.rejection(new WireFault('platform.failure', 'platform', 'restoration.identity', 'not configured'))
    }
    return JSON.stringify(answer)
  }

  async openSession(owner, expectedWireRevision) {
    this.calls.push(['openSession', owner, expectedWireRevision])
    const fault = this.takeFault('openSession')
    if (fault !== null) throw this.rejection(fault)
    if (expectedWireRevision !== WIRE_REVISION) {
      throw this.rejection(new WireFault('protocol.incompatible', 'core', 'ubm-mobile.session.open'))
    }
    const id = this.nextSessionId
    this.nextSessionId += 1
    // Each fixture starts from the configured bond table (the legacy
    // deterministic runtime reset it per attachment too).
    if (this.liveSessions().length === 0) {
      for (const [peerId, bonded] of this.initialBonds) this.peripherals.get(peerId).bonded = bonded
      this.pendingPair = null
    }
    this.sessions.set(String(id), {
      id: String(id),
      owner,
      outbox: [],
      armed: true,
      ordinal: 0,
      controlLostTotal: 0,
      disposed: false,
      closed: false,
      scans: new Map(),
      nextScan: 1,
      leases: new Map(),
      consumers: new Map(),
      endedConsumers: new Map(),
      // Finding 109: the highest admission received, and pre-admission
      // cancels above it.
      highestAdmission: 0,
      cancelledAhead: new Set()
    })
    if (this.admissionOverride !== undefined) {
      const override = this.admissionOverride(id)
      if (override !== undefined) return override
    }
    return JSON.stringify({
      sessionId: id,
      contractRevision: this.expectedIdentity.contractRevision,
      wireRevision: WIRE_REVISION,
      buildIdentity: this.identity
    })
  }

  async invoke(sessionId, op, argsJson) {
    this.calls.push(['invoke', sessionId, op, argsJson])
    const session = this.session(sessionId)
    let args
    try {
      args = JSON.parse(argsJson)
    } catch {
      return this.failure(op, invalid('args'))
    }
    const operationId = typeof args.operationId === 'string' ? args.operationId : null
    const admission = args.admission
    delete args.admission
    try {
      if (!(op in ARG_SCHEMAS)) throw invalid('op')
      if (admission !== undefined && (!Number.isSafeInteger(admission) || admission < 1)) {
        throw new WireFault(
          'argument.invalid',
          'core',
          `ubm-mobile.wire.args.${op}.admission`,
          null,
          this.commitFor(op, false)
        )
      }
      if (op !== 'op.cancel' && admission !== undefined) {
        // As the owner: an admission is consumed on arrival.
        if (admission <= session.highestAdmission) {
          throw new WireFault(
            'argument.invalid',
            'core',
            'ubm-mobile.wire.args.admission',
            null,
            this.commitFor(op, false)
          )
        }
        session.highestAdmission = admission
        const cancelled = session.cancelledAhead.delete(admission)
        for (const ahead of [...session.cancelledAhead]) if (ahead < admission) session.cancelledAhead.delete(ahead)
        if (cancelled) {
          throw new WireFault('operation.aborted', 'core', op, 'cancelled before admission', this.commitFor(op, false))
        }
      }
      const [required, optional] = ARG_SCHEMAS[op]
      exact(args, required, optional)
      const namesOperation = operationId !== null && op !== 'op.cancel' && op !== 'scan.stop'
      if (op === 'op.cancel' ? admission === undefined : namesOperation !== (admission !== undefined)) {
        throw new WireFault(
          'argument.invalid',
          'core',
          'ubm-mobile.wire.args.admission',
          null,
          this.commitFor(op, false)
        )
      }
      if (op === 'op.cancel') args.admission = admission
      const fault = this.takeFault(op)
      if (fault !== null) throw fault
      if (this.holds.has(op)) {
        const outcome = await this.held(op, operationId, session, admission)
        if (outcome.cancelled) {
          throw new WireFault('operation.aborted', 'core', op, 'cancelled by op.cancel', this.commitFor(op, true))
        }
        const value = outcome.value ?? (await this.run(session, op, args))
        return JSON.stringify({ ok: true, value })
      }
      const value = await this.run(session, op, args)
      return JSON.stringify({ ok: true, value })
    } catch (error) {
      if (error instanceof WireFault) return this.failure(op, error)
      throw error
    }
  }

  async drain(sessionId, maxItems, maxBytes) {
    this.calls.push(['drain', sessionId, maxItems, maxBytes])
    const session = this.session(sessionId)
    const records = session.outbox.splice(0, maxItems)
    if (session.outbox.length === 0) session.armed = true
    const text = JSON.stringify({
      more: session.outbox.length > 0,
      records,
      controlLost: session.controlLostTotal
    })
    if (this.drainResolution === 'native-task') await new Promise(resolve => setImmediate(resolve))
    return text
  }

  async closeSession(sessionId) {
    this.calls.push(['closeSession', sessionId])
    const session = this.sessions.get(sessionId)
    if (session === undefined || session.closed) return
    const fault = this.takeFault('closeSession')
    if (fault !== null) throw this.rejection(fault)
    if (!session.disposed) this.dispose(session)
    session.closed = true
  }

  // -- controller -------------------------------------------------------------------------

  /** The foreground-service leases this module instance holds. */
  heldBackgroundLeases() {
    return [...this.backgroundLeases]
  }

  /** Module invalidation (React reload/teardown): ends the background scope. */
  invalidate() {
    this.calls.push(['invalidate'])
    this.backgroundLeases.clear()
  }

  /** Queues one failure for the next call of `op` (an invoke op, 'openSession' or 'closeSession'). */
  failNext(op, code, domain = 'core', operation = `ubm-mobile.${op}`, detail = null, commit = null, platform = null) {
    this.faults.push({ op, fault: new WireFault(code, domain, operation, detail, commit, platform) })
  }

  /** Makes the next call of `op` wait until `release(op)` or an `op.cancel` for its id. */
  hold(op) {
    this.holds.set(op, [])
  }

  release(op, value) {
    const waiting = this.holds.get(op) ?? []
    this.holds.delete(op)
    for (const entry of waiting) {
      if (entry.operationId !== null) this.liveOps.delete(entry.operationId)
      entry.resolve({ cancelled: false, value })
    }
  }

  /** Native calls since `mark` (idle-cost measurement). */
  callsSince(mark) {
    return this.calls.slice(mark)
  }

  opsInvoked(op) {
    return this.calls.filter(call => call[0] === 'invoke' && call[2] === op).map(call => JSON.parse(call[3]))
  }

  emitAdvertisement(peerId = DEFAULT_PEER, overrides = {}) {
    const peripheral = this.peripherals.get(peerId)
    for (const session of this.liveSessions()) {
      if (session.scans.size === 0) continue
      this.push(session, {
        t: 'adv',
        peerId,
        localName: peripheral?.name ?? null,
        rssi: peripheral?.rssi ?? null,
        txPower: null,
        serviceUuids: [SERVICE_UUID],
        manufacturerData: [{ companyId: 107, payloadB64: b64([0x00, 0x80, 0xff]) }],
        serviceData: null,
        connectable: this.platform === 'android' ? true : null,
        solicitedServiceUuids: null,
        overflowServiceUuids: null,
        appearance: null,
        rawRecordB64: null,
        observedAtMs: 0,
        ...overrides
      })
    }
  }

  /**
   * Emits a value for every consumer on the characteristic, stamped with its
   * subscribe delivery. With an `instance` (`serviceUuid`, `serviceOccurrence`,
   * `characteristicOccurrence`) only consumers on that exact instance receive
   * it, as the owner routes by complete path.
   */
  emitNotification(bytes, peerId = DEFAULT_PEER, characteristicUuid = CHARACTERISTIC_UUID, instance = null) {
    let delivered = 0
    for (const session of this.liveSessions()) {
      for (const [consumer, entry] of session.consumers) {
        if (entry.peerId !== peerId || entry.selector.characteristicUuid !== canonicalUuid(characteristicUuid)) continue
        if (instance !== null && !sameInstance(entry.selector, instance)) continue
        this.push(session, { t: 'value', consumer, valueB64: b64(bytes), delivery: entry.delivery })
        delivered += 1
      }
    }
    if (delivered === 0) throw new Error('deterministic owner: no consumer for the notification')
  }

  /** The platform reports the link gone: `link` then `stream-end invalidated` per consumer (wire order). */
  dropLink(peerId = DEFAULT_PEER, reason = 'peer') {
    for (const session of this.liveSessions()) {
      for (const [, lease] of session.leases) {
        if (lease.peerId !== peerId || !lease.connected) continue
        lease.connected = false
        const end = {
          connectionGeneration: lease.generation,
          databaseGeneration: lease.databaseGeneration ?? null,
          reason
        }
        this.linkEnds.set(peerId, end)
        this.push(session, { t: 'link', peerId, ...end })
      }
      this.endConsumers(session, peerId)
    }
  }

  /** The owner ends every consumer on `peerId` (`stream-end invalidated`), remembering why. */
  endConsumers(session, peerId) {
    for (const [consumer, entry] of [...session.consumers]) {
      if (entry.peerId !== peerId) continue
      session.consumers.delete(consumer)
      const terminal = { reason: 'invalidated', droppedItems: 0, droppedBytes: 0 }
      session.endedConsumers.set(consumer, terminal)
      this.push(session, { t: 'stream-end', consumer, ...terminal })
    }
  }

  changeDatabase(peerId = DEFAULT_PEER) {
    for (const session of this.liveSessions()) {
      for (const [, lease] of session.leases) {
        if (lease.peerId !== peerId || !lease.connected) continue
        // As the owner: the record names the generation the change
        // invalidated; the database is undiscovered until the next discover.
        const change = { connectionGeneration: lease.generation, databaseGeneration: lease.databaseGeneration ?? null }
        lease.databaseGeneration = null
        if (change.databaseGeneration === null) continue
        this.databaseChanges.set(peerId, change)
        this.push(session, { t: 'db-changed', peerId, ...change })
      }
      this.endConsumers(session, peerId)
    }
  }

  /** The owner ends every consumer on `peerId` with `reason` while the link stays up. */
  endConsumer(peerId, reason, droppedItems = 0, droppedBytes = 0) {
    for (const session of this.liveSessions()) {
      for (const [consumer, entry] of [...session.consumers]) {
        if (entry.peerId !== peerId) continue
        session.consumers.delete(consumer)
        const terminal = { reason, droppedItems, droppedBytes }
        session.endedConsumers.set(consumer, terminal)
        this.push(session, { t: 'stream-end', consumer, ...terminal })
      }
    }
  }

  /** The owner holds the link again under a new connection generation (no record: the app never asked). */
  reconnect(peerId) {
    for (const session of this.liveSessions()) {
      for (const lease of session.leases.values()) {
        if (lease.peerId !== peerId || lease.connected) continue
        lease.connected = true
        lease.generation = `cg-${this.nextGeneration++}`
        lease.databaseGeneration = null
      }
    }
  }

  /** The platform's security report for `peerId` (a `security` record to every session). */
  reportSecurity(peerId, state) {
    this.securityReports.set(peerId, state)
    for (const session of this.liveSessions()) this.push(session, { t: 'security', peerId, state })
  }

  /**
   * Runs `action` with the control queue full: every control record it
   * causes is lost (counted), then one `ingress-drop{class:"control"}`
   * reports the loss. The owner's state still changes.
   */
  loseControl(action) {
    this.controlLost = 0
    try {
      action()
    } finally {
      const lost = this.controlLost
      this.controlLost = null
      // Every live session refused the same broadcast control records:
      // the cumulative counter moves per session, like the owner's.
      for (const session of this.liveSessions()) session.controlLostTotal += lost
      if (lost > 0) this.ingressDrop('control', lost)
    }
  }

  setAdapter(partial) {
    this.adapter = { ...this.adapter, ...partial }
    for (const session of this.liveSessions()) this.push(session, { t: 'adapter', state: this.adapter })
  }

  endScans(reason = 'source-failed') {
    for (const session of this.liveSessions()) {
      for (const membership of [...session.scans.keys()]) {
        session.scans.delete(membership)
        this.push(session, { t: 'scan-end', operationId: membership, reason })
      }
    }
  }

  ingressDrop(className, count) {
    for (const session of this.liveSessions()) this.push(session, { t: 'ingress-drop', class: className, count })
  }

  seedRestored(peers) {
    this.restored = peers.map(peer => ({ peerId: peer.peerId, name: peer.name ?? null, connected: peer.connected }))
    for (const session of this.liveSessions()) this.push(session, { t: 'restored', peers: this.restored })
  }

  /** The next `security.pair` waits for `security.cancel-pairing` (then fails `operation.aborted`). */
  deferNextPair() {
    this.pendingPair = 'armed'
  }

  // -- owner model --------------------------------------------------------------------------

  liveSessions() {
    return [...this.sessions.values()].filter(session => !session.closed && !session.disposed)
  }

  session(sessionId) {
    const session = this.sessions.get(sessionId)
    if (session === undefined) throw this.rejection(invalid('session'))
    if (session.closed) {
      throw this.rejection(new WireFault('lifecycle.destroyed', 'core', 'ubm-mobile.session'))
    }
    return session
  }

  rejection(fault) {
    const error = new Error(JSON.stringify(fault.failure))
    error.code = 'UnifiedBleRustCore'
    return error
  }

  failure(op, fault) {
    const write = op === 'gatt.write' || op === 'gatt.write-descriptor'
    const commit = write ? (fault.commit ?? 'not-dispatched') : null
    // As the owner: its own retryability on every failure envelope; a write
    // that may have committed is never retryable.
    const retryability =
      commit === 'uncertain'
        ? 'never'
        : (fault.retryability ??
          (fault.failure.code === 'operation.aborted' || fault.failure.code === 'operation.timed-out'
            ? 'caller-decides'
            : 'never'))
    return JSON.stringify({ ok: false, error: fault.failure, commit, retryability })
  }

  commitFor(op, dispatched) {
    return op === 'gatt.write' || op === 'gatt.write-descriptor' ? (dispatched ? 'uncertain' : 'not-dispatched') : null
  }

  takeFault(op) {
    const index = this.faults.findIndex(entry => entry.op === op)
    if (index < 0) return null
    return this.faults.splice(index, 1)[0].fault
  }

  held(op, operationId, session, admission) {
    return new Promise((resolve, reject) => {
      const entry = { resolve, reject, operationId, op, session, admission }
      this.holds.get(op).push(entry)
      if (operationId !== null) this.liveOps.set(operationId, entry)
    })
  }

  push(session, record) {
    if (this.controlLost !== null && record.t !== 'adv' && record.t !== 'value') {
      this.controlLost += 1
      return
    }
    session.ordinal += 1
    session.outbox.push({ ...record, ordinal: session.ordinal })
    if (session.armed) {
      session.armed = false
      queueMicrotask(() => {
        for (const listener of [...this.wakeListeners]) listener({ sessionId: session.id })
      })
    }
  }

  peripheral(peerId) {
    const peripheral = this.peripherals.get(peerId)
    if (peripheral === undefined) throw new WireFault('peer.not-found', 'connection', 'ubm-mobile.peer')
    return peripheral
  }

  lease(session, args) {
    const lease = session.leases.get(args.lease)
    if (lease === undefined || lease.peerId !== args.peerId) {
      throw new WireFault('ownership.denied', 'connection', 'ubm-mobile.lease')
    }
    if (!lease.connected) throw new WireFault('connection.stale', 'connection', 'ubm-mobile.lease')
    return lease
  }

  connectedLease(session, peerId) {
    for (const lease of session.leases.values()) if (lease.peerId === peerId && lease.connected) return lease
    throw new WireFault('connection.stale', 'connection', 'ubm-mobile.gatt')
  }

  characteristic(peerId, selector, descriptor) {
    exact(
      selector,
      descriptor
        ? [
            'serviceUuid',
            'serviceOccurrence',
            'characteristicUuid',
            'characteristicOccurrence',
            'descriptorUuid',
            'descriptorOccurrence'
          ]
        : ['serviceUuid', 'serviceOccurrence', 'characteristicUuid', 'characteristicOccurrence'],
      [],
      'args.selector'
    )
    for (const key of Object.keys(selector)) {
      if (key.endsWith('Occurrence') && !Number.isSafeInteger(selector[key])) throw invalid(`args.selector.${key}`)
    }
    const peripheral = this.peripheral(peerId)
    const service = peripheral.services.find(
      candidate =>
        candidate.uuid === canonicalUuid(selector.serviceUuid) && candidate.occurrence === selector.serviceOccurrence
    )
    const characteristic = service?.characteristics.find(
      candidate =>
        candidate.uuid === canonicalUuid(selector.characteristicUuid) &&
        candidate.occurrence === selector.characteristicOccurrence
    )
    if (characteristic === undefined) throw new WireFault('gatt.not-found', 'gatt', 'ubm-mobile.gatt')
    if (!descriptor) return characteristic
    const found = characteristic.descriptors.find(
      candidate =>
        candidate.uuid === canonicalUuid(selector.descriptorUuid) &&
        candidate.occurrence === selector.descriptorOccurrence
    )
    if (found === undefined) throw new WireFault('gatt.not-found', 'gatt', 'ubm-mobile.gatt')
    return found
  }

  peerRecord(peripheral, source, connected) {
    return {
      peerId: peripheral.peerId,
      name: peripheral.name,
      rssi: peripheral.rssi,
      source,
      reachability: connected ? 'reachable' : 'unknown',
      connection: connected ? 'connected' : 'unknown',
      bond: peripheral.bonded ? 'bonded' : 'unknown',
      lastSeenAtMonotonicMs: null
    }
  }

  restoredRecord(peer) {
    return {
      peerId: peer.peerId,
      name: peer.name,
      rssi: null,
      source: 'restored',
      reachability: peer.connected ? 'reachable' : 'unknown',
      connection: peer.connected ? 'connected' : 'unknown',
      bond: 'unknown',
      lastSeenAtMonotonicMs: null
    }
  }

  reconcile(session) {
    const links = []
    for (const lease of session.leases.values()) {
      if (!lease.connected) continue
      const change = this.databaseChanges.get(lease.peerId)
      links.push({
        peerId: lease.peerId,
        connectionGeneration: lease.generation,
        state: 'connected',
        reason: null,
        databaseGeneration: lease.databaseGeneration ?? null,
        databaseChange:
          change !== undefined && change.connectionGeneration === lease.generation ? change.databaseGeneration : null,
        databaseState: (lease.databaseGeneration ?? null) === null ? 'undiscovered' : 'current'
      })
    }
    for (const [peerId, end] of this.linkEnds) {
      links.push({ peerId, ...end, state: 'ended', databaseChange: null, databaseState: null })
    }
    const subscriptions = [
      ...[...session.consumers.keys()].map(consumer => ({ consumer, state: 'live' })),
      ...[...session.endedConsumers].map(([consumer, terminal]) => ({ consumer, state: 'ended', ...terminal }))
    ].sort((a, b) => (a.consumer < b.consumer ? -1 : 1))
    return {
      adapter: this.adapter,
      links,
      subscriptions,
      security: [...this.securityReports].map(([peerId, state]) => ({ peerId, state })),
      restored: this.restored,
      scan: [...session.scans.keys()][0] ?? null
    }
  }

  security(peripheral) {
    return {
      // Android's public bond API reports bond state only (legacy Android rule).
      bond: peripheral.bonded ? 'bonded' : 'not-bonded',
      encryption: 'unsupported',
      authentication: 'unsupported',
      secureConnections: 'unsupported',
      pairingPossible: null
    }
  }

  /** Counters of the resources `sessions` hold (one session, or every live one for the process). */
  resourceCounters(sessions) {
    let scans = 0
    let leases = 0
    let links = 0
    let databases = 0
    let consumers = 0
    const cccds = new Set()
    let retained = 0
    for (const session of sessions) {
      scans += session.scans.size
      leases += session.leases.size
      for (const lease of session.leases.values()) {
        if (lease.connected) links += 1
        if (lease.databaseGeneration !== undefined) databases += 1
      }
      consumers += session.consumers.size
      for (const entry of session.consumers.values()) cccds.add(`${entry.peerId}|${entry.selector.characteristicUuid}`)
      for (const record of session.outbox) {
        if (record.t === 'adv' || record.t === 'value') retained += JSON.stringify(record).length
      }
    }
    const members = new Set(sessions)
    const held = [...this.holds.values()].flat().filter(entry => members.has(entry.session)).length
    return {
      counters: {
        activeScanControllers: scans > 0 ? 1 : 0,
        scanConsumers: scans,
        chooserSessions: 0,
        connectionLeases: leases,
        physicalLinks: links,
        databaseSnapshots: databases,
        physicalCccdEnablements: cccds.size,
        subscriptionConsumers: consumers,
        queuedOperations: 0,
        dispatchedOperations: held,
        retainedByteBuffers: retained,
        restorationRecords: 0,
        orphanedIpcOwners: 0
      },
      held
    }
  }

  /** `counters.describe` for `session`: its own resources, then the process owner under `process`. */
  counters(session) {
    const own = this.resourceCounters([session])
    const owner = this.resourceCounters(this.liveSessions())
    const claimed = [...this.restorationClaims.values()].filter(sessionId => sessionId === session.id).length
    return {
      counters: { ...own.counters, restorationRecords: claimed },
      native: { pendingRadioRequests: own.held, liveOps: own.held },
      process: {
        counters: { ...owner.counters, restorationRecords: this.restored.length },
        native: {
          pendingRadioRequests: owner.held,
          lateRadioCompletions: 0,
          ingressDrops: { advertisement: 0, notification: 0, control: 0 },
          liveOps: owner.held
        }
      }
    }
  }

  dispose(session) {
    // The owner ends every operation still in flight on the disposed session
    // (and only that session's).
    for (const [op, waiting] of [...this.holds]) {
      this.holds.set(
        op,
        waiting.filter(entry => entry.session !== session)
      )
      for (const entry of waiting.filter(candidate => candidate.session === session)) {
        if (entry.operationId !== null) this.liveOps.delete(entry.operationId)
        entry.resolve({ cancelled: true, value: undefined })
      }
    }
    session.scans.clear()
    session.leases.clear()
    session.consumers.clear()
    session.endedConsumers.clear()
    session.outbox.length = 0
    session.disposed = true
  }

  async run(session, op, args) {
    const apple = this.platform === 'apple'
    if (apple && APPLE_UNSUPPORTED.has(op)) {
      throw new WireFault('capability.unsupported', 'capability', op, 'CoreBluetooth has no such control')
    }
    switch (op) {
      case 'adapter.state':
        return this.adapter
      case 'counters.describe':
        return this.counters(session)
      case 'peers.known':
        return [...this.peripherals.values()].map(peripheral => this.peerRecord(peripheral, 'scan-observed', false))
      case 'peers.connected': {
        const connected = new Set(
          [...session.leases.values()].filter(lease => lease.connected).map(lease => lease.peerId)
        )
        return [...connected].map(peerId => this.peerRecord(this.peripheral(peerId), 'system-connected', true))
      }
      case 'peers.bonded':
        if (apple) throw new WireFault('capability.unsupported', 'capability', op)
        return [...this.peripherals.values()]
          .filter(peripheral => peripheral.bonded)
          .map(peripheral => this.peerRecord(peripheral, 'system-bonded', false))
      case 'peers.restored':
        return this.restored.map(peer => this.restoredRecord(peer))
      case 'peers.claim-restored': {
        if (!Number.isSafeInteger(args.maxPeers) || args.maxPeers < 0) throw invalid('args.maxPeers')
        const unclaimed = this.restored.filter(peer => !this.restorationClaims.has(peer.peerId))
        if (unclaimed.length > args.maxPeers) {
          throw new WireFault(
            'bytes.too-large',
            'restoration',
            op,
            'more unclaimed restored peers than the journal can hold'
          )
        }
        for (const peer of unclaimed) this.restorationClaims.set(peer.peerId, session.id)
        return { peers: unclaimed.map(peer => this.restoredRecord(peer)) }
      }
      case 'peers.resolve': {
        exact(args.reference, ['opaqueId'], ['version', 'backendId', 'scope'], 'args.reference')
        const peripheral = this.peripherals.get(args.reference.opaqueId)
        return peripheral === undefined ? null : this.peerRecord(peripheral, 'app-reference', false)
      }
      case 'scan.start': {
        if (args.duplicatePolicy !== 'all') throw new WireFault('capability.unsupported', 'capability', op)
        // One live scan per session, like the real owner (finding 185):
        // a membership kept for retry after a failed stop still occupies
        // the session until it is released.
        if (session.scans.size > 0) throw new WireFault('scan.already-active', 'scan', op)
        args.serviceUuids.forEach(canonicalUuid)
        if (apple && ((args.deviceAddresses ?? []).length > 0 || args.platform !== undefined)) {
          throw new WireFault('capability.unsupported', 'capability', op)
        }
        if (args.platform !== undefined) exact(args.platform, [], ['mode', 'callbackType', 'legacy'], 'args.platform')
        const membership = `s${session.id}-scan-${session.nextScan++}`
        session.scans.set(membership, args)
        return { operationId: membership }
      }
      case 'scan.stop':
        if (!session.scans.delete(args.operationId)) {
          throw new WireFault('lifecycle.invalid-state', 'scan', op, 'scan-not-active')
        }
        return { state: 'released', failures: [] }
      case 'connection.connect': {
        const peripheral = this.peripheral(args.peerId)
        if (args.intent === 'when-available' && apple) throw new WireFault('capability.unsupported', 'capability', op)
        const preferredPhy = [...new Set(args.preferredPhy ?? [])]
        if (preferredPhy.some(phy => !['le-1m', 'le-2m', 'le-coded'].includes(phy))) throw invalid('args.preferredPhy')
        if (preferredPhy.length > 0) {
          const refused = detail => new WireFault('capability.unsupported', 'capability', `${op}.preferred-phy`, detail)
          if (apple) throw refused('CoreBluetooth has no LE PHY control')
          if (args.intent === 'when-available') throw refused('Android does not apply a connect PHY with autoConnect')
          const linked = this.liveSessions().some(other =>
            [...other.leases.values()].some(lease => lease.peerId === peripheral.peerId && lease.connected)
          )
          if (linked) throw refused('the link is already established')
        }
        this.connects.push({ sessionId: session.id, peerId: peripheral.peerId, preferredPhy })
        this.negotiatedMtu.delete(peripheral.peerId)
        const generation = `cg-${this.nextGeneration++}`
        session.leases.set(args.lease, { peerId: peripheral.peerId, generation, connected: true })
        return { peerKey: `peer-${peripheral.peerId}`, connectionGeneration: generation }
      }
      case 'connection.disconnect': {
        const lease = session.leases.get(args.lease)
        if (lease === undefined || lease.peerId !== args.peerId) {
          throw new WireFault('ownership.denied', 'connection', op)
        }
        session.leases.delete(args.lease)
        for (const [consumer, entry] of [...session.consumers]) {
          if (entry.peerId === args.peerId) session.consumers.delete(consumer)
        }
        return { state: 'released', failures: [] }
      }
      case 'connection.rssi':
        this.lease(session, args)
        return { rssi: -47 }
      case 'connection.effective-mtu':
        this.lease(session, args)
        // Apple derives the ATT MTU per link as
        // `maximumWriteValueLength(.withResponse) + 3` (frozen wire rule,
        // ios/UnifiedBleRustRadioAdapter.swift); Android reports no MTU
        // until `onMtuChanged` (native `readEffectiveMtu`).
        if (apple) return { mtu: 512 + 3 }
        return { mtu: this.negotiatedMtu.get(args.peerId) ?? null }
      case 'connection.request-mtu': {
        this.lease(session, args)
        const mtu = Math.min(args.mtu, 247)
        this.negotiatedMtu.set(args.peerId, mtu)
        return { mtu }
      }
      case 'connection.maximum-write-length': {
        this.lease(session, args)
        if (args.mode !== 'with-response' && args.mode !== 'without-response') throw invalid('args.mode')
        // As the native adapters answer `ReadWriteLimits`: CoreBluetooth's
        // `maximumWriteValueLength(for:)`; Android 512 with response (the
        // stack's long write) and one ATT payload of the negotiated MTU, or
        // of the ATT default 23 before any exchange, without.
        if (apple) return { maximumWriteLength: args.mode === 'with-response' ? 512 : 182 }
        const mtu = this.negotiatedMtu.get(args.peerId) ?? 23
        return { maximumWriteLength: args.mode === 'with-response' ? 512 : mtu - 3 }
      }
      case 'connection.request-priority':
        this.lease(session, args)
        return { accepted: true }
      case 'connection.read-phy':
        this.lease(session, args)
        return { tx: 'le-1m', rx: 'le-1m' }
      case 'connection.request-phy':
        this.lease(session, args)
        return { accepted: true, observation: { tx: args.tx ?? 'le-1m', rx: args.rx ?? 'le-1m' } }
      case 'security.state':
        if (apple) throw new WireFault('capability.unsupported', 'capability', op)
        return this.security(this.peripheral(args.peerId))
      case 'security.pair': {
        if (apple) throw new WireFault('capability.unsupported', 'capability', op)
        const peripheral = this.peripheral(args.peerId)
        if (args.transport === 'auto' && peripheral.bonded) {
          return { outcome: 'already-paired', state: this.security(peripheral) }
        }
        if (this.pendingPair === 'armed') {
          await new Promise(resolve => {
            this.pendingPair = resolve
          })
          throw new WireFault('operation.aborted', 'core', op, 'pairing cancelled')
        }
        peripheral.bonded = true
        this.securityReports.set(peripheral.peerId, this.security(peripheral))
        this.push(session, { t: 'security', peerId: peripheral.peerId, state: this.security(peripheral) })
        return { outcome: 'paired', state: this.security(peripheral) }
      }
      case 'security.cancel-pairing':
        if (apple) throw new WireFault('capability.unsupported', 'capability', op)
        if (typeof this.pendingPair === 'function') {
          const resolve = this.pendingPair
          this.pendingPair = null
          resolve()
        }
        return { state: 'requested' }
      case 'gatt.discover': {
        const lease = this.lease(session, args)
        const peripheral = this.peripheral(args.peerId)
        lease.databaseGeneration = lease.databaseGeneration ?? `db-${this.nextGeneration++}`
        const discovery = {
          connectionGeneration: lease.generation,
          databaseGeneration: lease.databaseGeneration,
          services: peripheral.services.map(service => ({
            uuid: service.uuid,
            occurrence: service.occurrence,
            characteristics: service.characteristics.map(characteristic => ({
              uuid: characteristic.uuid,
              occurrence: characteristic.occurrence,
              properties: characteristic.properties,
              descriptors: characteristic.descriptors.map(descriptor => ({
                uuid: descriptor.uuid,
                occurrence: descriptor.occurrence
              }))
            }))
          }))
        }
        return this.discoveryOverride === null ? discovery : this.discoveryOverride(discovery)
      }
      case 'gatt.read': {
        this.connectedLease(session, args.peerId)
        const attribute = this.characteristic(args.peerId, args.selector, false)
        return { valueB64: b64(attribute.value), provenance: this.readProvenance }
      }
      case 'gatt.read-descriptor': {
        this.connectedLease(session, args.peerId)
        const attribute = this.characteristic(args.peerId, args.selector, true)
        return { valueB64: b64(attribute.value) }
      }
      case 'gatt.write':
      case 'gatt.write-descriptor': {
        const descriptor = op === 'gatt.write-descriptor'
        if (args.mode !== 'with-response' && args.mode !== 'without-response') throw invalid('args.mode')
        if (descriptor && args.mode === 'without-response') {
          throw new WireFault('capability.unsupported', 'capability', op, null, 'not-dispatched')
        }
        this.connectedLease(session, args.peerId)
        const attribute = this.characteristic(args.peerId, args.selector, descriptor)
        attribute.value = unb64(args.valueB64)
        return { commitState: args.mode === 'with-response' ? 'confirmed' : 'unknown' }
      }
      case 'gatt.subscribe': {
        this.connectedLease(session, args.peerId)
        const characteristic = this.characteristic(args.peerId, args.selector, false)
        const notify = (characteristic.properties & 0x08) !== 0
        const indicate = (characteristic.properties & 0x10) !== 0
        const mode = args.deliveryMode
        if ((mode === 'require-notification' && !notify) || (mode === 'require-indication' && !indicate)) {
          throw new WireFault('gatt.property-not-supported', 'gatt', op)
        }
        if (!notify && !indicate) throw new WireFault('gatt.property-not-supported', 'gatt', op)
        if (apple && mode === 'require-indication' && notify) {
          // CoreBluetooth enables notifications when both are offered (FIX-PLAN decision C).
          throw new WireFault('capability.limited', 'capability', 'gatt.subscribe.delivery')
        }
        const delivery = apple
          ? 'unknown'
          : mode === 'require-indication' || (mode === 'prefer-indication' && indicate) || !notify
            ? 'indication'
            : 'notification'
        session.consumers.set(args.consumer, {
          peerId: args.peerId,
          selector: { ...args.selector, characteristicUuid: canonicalUuid(args.selector.characteristicUuid) },
          delivery
        })
        return { consumer: args.consumer, delivery }
      }
      case 'gatt.unsubscribe': {
        if (session.endedConsumers.delete(args.consumer)) return { state: 'released', physicalDisabled: false }
        if (!session.consumers.delete(args.consumer)) throw new WireFault('gatt.not-found', 'gatt', op)
        const remaining = [...session.consumers.values()].some(entry => entry.peerId === args.peerId)
        return { state: 'released', physicalDisabled: !remaining }
      }
      case 'background.acquire': {
        if (apple) throw new WireFault('capability.unsupported', 'capability', op)
        const leaseId = `fgs-lease-${this.nextBackground++}`
        this.backgroundLeases.add(leaseId)
        return { leaseId }
      }
      case 'background.release':
        if (!this.backgroundLeases.delete(args.leaseId)) throw new WireFault('ownership.denied', 'core', op)
        return { state: 'released', failures: [] }
      case 'background.update-notification':
        if (!this.backgroundLeases.has(args.leaseId)) throw new WireFault('ownership.denied', 'core', op)
        return { state: 'updated' }
      case 'companion.associate':
        return { source: 'associated', associationId: 7, peerId: DEFAULT_PEER, displayName: args.name ?? null }
      case 'presence.observe': {
        if (apple) throw new WireFault('capability.unsupported', 'capability', op)
        this.presenceArmed.add(args.peerId)
        return { state: 'observing' }
      }
      case 'presence.unobserve': {
        if (apple) throw new WireFault('capability.unsupported', 'capability', op)
        this.presenceArmed.delete(args.peerId)
        return { state: 'idle' }
      }
      case 'op.cancel': {
        const live = this.liveOps.get(args.operationId)
        if (live !== undefined && live.session === session && live.admission === args.admission) {
          this.liveOps.delete(args.operationId)
          const list = this.holds.get(live.op) ?? []
          this.holds.set(
            live.op,
            list.filter(entry => entry !== live)
          )
          live.resolve({ cancelled: true, value: undefined })
          return { state: 'cancellation-requested' }
        }
        if (live !== undefined) throw invalid('args.op.cancel.admission')
        if (args.admission <= session.highestAdmission) return { state: 'already-terminal' }
        if (args.admission - session.highestAdmission > ADMISSION_WINDOW) throw invalid('args.op.cancel.admission')
        session.cancelledAhead.add(args.admission)
        return { state: 'cancellation-requested' }
      }
      case 'session.reconcile':
        return this.reconcile(session)
      case 'session.dispose': {
        const override = this.disposeRecords.shift()
        if (override !== undefined) return override
        this.dispose(session)
        return { state: 'released', failures: [] }
      }
      default:
        throw invalid('op')
    }
  }
}

module.exports = {
  DeterministicRustCoreNative,
  WireFault,
  defaultPeripheral,
  SERVICE_UUID,
  CHARACTERISTIC_UUID,
  CCCD_UUID,
  DEFAULT_PEER,
  WIRE_REVISION
}
