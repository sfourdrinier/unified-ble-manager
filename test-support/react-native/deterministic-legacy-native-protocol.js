// test-support/react-native/deterministic-legacy-native-protocol.js
//
// Deterministic Native Protocol v2 control + JSI runtime for the LEGACY
// React Native TypeScript providers. Test reference only (FIX-PLAN decision
// 12): the production RN route is the Rust core; these doubles exist so the
// legacy-vs-Rust capability parity tests can open the legacy providers until
// Phase 4 deletes them.

const { decodeNativeProtocolRecord, encodeNativeProtocolRecord } = require('../../src/native-protocol/v2-codec')

const SERVICE_UUID = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC_UUID = '00002a37-0000-1000-8000-00805f9b34fb'
const DESCRIPTOR_UUID = '00002902-0000-1000-8000-00805f9b34fb'
const REACT_NATIVE_PEER_ID = 'C0FFEE000001'

function deterministicReactNativeTckBoundary(runtime) {
  return {
    emitAdvertisement: () => runtime.emitAdvertisement(),
    emitNotification: (address, bytes) => runtime.emitNotification(address, bytes),
    prepareSecurityCancellation: () => runtime.prepareSecurityCancellation()
  }
}

class DeterministicNativeControl {
  constructor(securityAvailable = false) {
    this.handshakes = []
    this.closedAttachments = []
    this.securityAvailable = securityAvailable
    this.priorityAccepted = true
    this.restorationJournalSeeded = false
    this.restorationConsumed = false
  }

  handshake(request) {
    this.handshakes.push(request)
    return Promise.resolve({
      nativeProtocol: 2,
      abi: 7,
      controlSurface: 2,
      backendContract: 1,
      capabilitySchema: 1,
      eventSchema: 1,
      traceFormat: 1,
      maximumControlRecordBytes: 65536,
      maximumBinaryPayloadBytes: 524288,
      securityAvailable: this.securityAvailable,
      securityCancelPairingAvailable: this.securityAvailable
    })
  }

  installExecutionRuntime() {
    return Promise.resolve()
  }

  cancelOperation() {
    return Promise.resolve({ state: 'alreadyTerminal' })
  }

  adoptRestoration(request) {
    if (!this.restorationJournalSeeded || this.restorationConsumed) {
      return Promise.resolve({
        receiptId: '',
        outcome: 'alreadyConsumed',
        boundClientId: request.clientId,
        adoptionEpoch: request.expectedEpoch,
        replayRecordCount: 0,
        records: []
      })
    }
    if (request.namespaceValue.endsWith('.rejected')) {
      return Promise.resolve({
        receiptId: '',
        outcome: 'namespaceMismatch',
        boundClientId: '',
        adoptionEpoch: request.expectedEpoch,
        replayRecordCount: 0,
        records: []
      })
    }
    this.restorationConsumed = true
    const handshake = this.handshakes[this.handshakes.length - 1]
    if (handshake === undefined) throw new Error('Deterministic control has no active attachment')
    return Promise.resolve({
      receiptId: 'first-party-registry-restoration-receipt',
      outcome: 'adopted',
      boundClientId: request.clientId,
      adoptionEpoch: request.expectedEpoch,
      replayRecordCount: 1,
      records: [
        {
          recordVersion: 1,
          namespaceValue: request.namespaceValue,
          attachmentId: handshake.attachmentId,
          backendInstanceId: handshake.backendInstanceId,
          backendGeneration: handshake.backendGeneration,
          adapterId: handshake.adapterId,
          adapterGeneration: handshake.adapterGeneration,
          ordinal: 1,
          adoptionEpoch: request.expectedEpoch,
          kind: 'adapter',
          peerId: null,
          connectionId: null,
          ownerLeaseId: null,
          connectionGeneration: null
        }
      ]
    })
  }

  seedRestorationJournal() {
    this.restorationJournalSeeded = true
    this.restorationConsumed = false
  }

  closeAttachment(attachment) {
    this.closedAttachments.push(attachment)
    return Promise.resolve()
  }

  activeAttachment() {
    const handshake = this.handshakes[this.handshakes.length - 1]
    if (handshake === undefined) throw new Error('Deterministic control has no active attachment')
    return record('attachment', [
      field(1, handshake.attachmentId),
      field(2, handshake.backendInstanceId),
      field(3, handshake.backendGeneration),
      field(4, handshake.adapterId),
      field(5, handshake.adapterGeneration)
    ])
  }
}

class DeterministicReactNativeProtocolRuntime {
  constructor(control, emitInitialSubscriptionNotification) {
    this.control = control
    this.listener = null
    this.buffers = new Map()
    this.nextBuffer = 1
    this.nextEvent = 1
    this.subscriptionId = null
    this.subscribeCorrelation = null
    this.connection = null
    this.descriptorValue = new Uint8Array([8, 7])
    this.emitInitialSubscriptionNotification = emitInitialSubscriptionNotification
    this.securityBondState = 'notBonded'
    this.pendingSecurityPair = null
    this.deferNextSecurityPair = false
  }

  retain(operationCorrelation, value) {
    const ownerToken = `first-party-registry-buffer-${this.nextBuffer}`
    this.nextBuffer += 1
    this.buffers.set(ownerToken, new Uint8Array(value))
    return {
      ownerToken,
      byteOffset: 0,
      byteLength: value.byteLength,
      ownership: 'nativeOwnedCopy',
      operationCorrelation
    }
  }

  copy(reference) {
    const value = this.buffers.get(reference.ownerToken)
    if (value === undefined) throw new Error(`Unknown deterministic buffer ${reference.ownerToken}`)
    return new Uint8Array(value)
  }

  release(reference) {
    return this.buffers.delete(reference.ownerToken)
  }

  setEventSink(listener) {
    this.listener = listener
    this.securityBondState = 'notBonded'
    this.pendingSecurityPair = null
    this.deferNextSecurityPair = false
    this.emitEvent('adapterState', [
      field(15, record('adapterStateSnapshot', [field(1, 'available'), field(2, 'granted'), field(3, 'on')]))
    ])
  }

  setFatalSink(listener) {
    this.fatalListener = listener
  }

  submit(bytes) {
    const command = decodeNativeProtocolRecord(bytes)
    const kind = requiredString(command, 3)
    if (kind === 'scanStart') return this.emitResult(command, 'scanStarted')
    if (kind === 'scanStop' || kind === 'disconnect') return this.emitResult(command, 'accepted')
    if (kind === 'unsubscribe') return this.emitResult(command, 'unsubscribed')
    if (kind === 'connect') {
      this.connection = requiredRecord(command, 10)
      return this.emitResult(command, 'connected', [field(11, this.connection)])
    }
    if (kind === 'discover') {
      const database = requiredRecord(command, 11)
      return this.emitResult(command, 'database', [field(4, database), field(12, databaseSnapshot(database))])
    }
    if (kind === 'read') {
      return this.emitResult(command, 'read', [
        field(6, binaryReferenceRecord(this.retain('first-party-registry-read', new Uint8Array([0, 1]))))
      ])
    }
    if (kind === 'readDescriptor') {
      const descriptorPath = requiredRecord(command, 5)
      return this.emitResult(command, 'descriptorRead', [
        field(15, descriptorPath),
        field(6, binaryReferenceRecord(this.retain('first-party-registry-descriptor-read', this.descriptorValue)))
      ])
    }
    if (kind === 'readRssi') return this.emitResult(command, 'rssi', [field(13, -47)])
    if (kind === 'requestMtu') return this.emitResult(command, 'mtu', [field(14, requiredNumber(command, 14))])
    if (kind === 'requestPriority')
      return this.emitResult(command, 'priority', [field(18, this.control.priorityAccepted)])
    if (kind === 'writeDescriptor') {
      const descriptorPath = requiredRecord(command, 5)
      const reference = binaryReferenceFromRecord(requiredRecord(command, 6))
      this.descriptorValue = this.copy(reference)
      if (!this.release(reference)) {
        throw new Error('The deterministic descriptor write input was not retained')
      }
      return this.emitResult(command, 'descriptorWrite', [field(15, descriptorPath)])
    }
    if (kind === 'subscribe') {
      this.subscriptionId = requiredString(command, 7)
      this.subscribeCorrelation = requiredRecord(command, 2)
      if (this.emitInitialSubscriptionNotification) {
        this.emitNotificationRecord(new Uint8Array([3, 4]))
      }
      return this.emitResult(command, 'subscribed', [
        field(5, requiredRecord(command, 4)),
        field(7, this.subscriptionId)
      ])
    }
    if (kind === 'securityState') {
      return this.emitResult(command, 'securityState', [
        field(16, requiredString(command, 15)),
        field(17, this.securityBondState)
      ])
    }
    if (kind === 'securityPair') {
      if (this.deferNextSecurityPair) {
        this.deferNextSecurityPair = false
        this.pendingSecurityPair = command
        return
      }
      this.securityBondState = 'bonded'
      this.emitEvent('securityStateChanged', [
        field(16, requiredString(command, 15)),
        field(17, this.securityBondState)
      ])
      return this.emitResult(command, 'securityPair', [
        field(16, requiredString(command, 15)),
        field(17, this.securityBondState)
      ])
    }
    if (kind === 'securityCancelPairing') {
      if (this.pendingSecurityPair !== null) {
        const pending = this.pendingSecurityPair
        this.pendingSecurityPair = null
        this.emitFailure(pending, 'cancelled', 'Android security pairing was cancelled')
      }
      return this.emitResult(command, 'accepted')
    }
    if (kind === 'destroy') return this.emitResult(command, 'destroyed')
    throw new Error(`Unsupported deterministic native command ${kind}`)
  }

  emitAdvertisement() {
    this.emitEvent('advertisement', [
      field(
        12,
        record('advertisement', [
          field(1, REACT_NATIVE_PEER_ID),
          field(2, 20),
          field(3, 1),
          field(4, 'first-party-registry-scan'),
          field(5, 'First-party registry peer'),
          field(6, -47),
          field(10, [SERVICE_UUID]),
          field(17, ['native:android-scan-result'])
        ])
      )
    ])
  }

  emitNotification(_address, bytes) {
    this.emitNotificationRecord(bytes)
  }

  /**
   * Emits a notification the way the native binding does: the subscribe's
   * operationCorrelation on the event, and the payload retained under that
   * correlation's nonce. Inventing a correlation here would model a record the
   * native codec refuses to deliver (issue #168).
   */
  emitNotificationRecord(bytes) {
    if (this.subscriptionId === null || this.subscribeCorrelation === null) {
      throw new Error('Deterministic runtime has no active subscription')
    }
    this.emitEvent('notification', [
      field(10, this.subscribeCorrelation),
      field(11, this.subscriptionId),
      field(13, binaryReferenceRecord(this.retain(requiredString(this.subscribeCorrelation, 3), bytes)))
    ])
  }

  prepareSecurityCancellation() {
    this.deferNextSecurityPair = true
  }

  emitResult(command, kind, additions = []) {
    this.emit(
      record('result', [
        field(1, 1),
        field(2, kind),
        field(3, record('terminal', [field(1, requiredRecord(command, 2)), field(2, 'succeeded')])),
        ...additions
      ])
    )
  }

  emitFailure(command, code, safeMessage) {
    this.emit(
      record('result', [
        field(1, 1),
        field(2, 'cancelled'),
        field(3, record('terminal', [field(1, requiredRecord(command, 2)), field(2, 'failed'), field(3, code)])),
        field(
          10,
          record('error', [
            field(1, code),
            field(2, 'android'),
            field(3, requiredString(command, 3)),
            field(4, 'notRetryable'),
            field(7, safeMessage)
          ])
        )
      ])
    )
  }

  emitEvent(kind, additions) {
    this.nextEvent += 1
    this.emit(
      record('event', [
        field(1, 1),
        field(2, `first-party-registry-event-${this.nextEvent}`),
        field(3, kind),
        field(4, this.control.activeAttachment()),
        field(5, this.nextEvent),
        field(6, 20),
        ...additions
      ])
    )
  }

  emit(value) {
    if (this.listener === null) throw new Error('Deterministic runtime event sink is not installed')
    this.listener(encodeNativeProtocolRecord(value))
  }
}

function databaseSnapshot(database) {
  const service = record('servicePath', [field(1, database), field(2, SERVICE_UUID), field(3, '0')])
  const characteristic = record('characteristicPath', [field(1, service), field(2, CHARACTERISTIC_UUID), field(3, '0')])
  const descriptor = record('descriptorPath', [field(1, characteristic), field(2, DESCRIPTOR_UUID), field(3, '0')])
  return record('databaseSnapshot', [
    field(1, database),
    field(2, [service]),
    field(3, [
      record('characteristicSnapshot', [
        field(1, characteristic),
        field(2, true),
        field(3, true),
        field(4, true),
        field(5, true)
      ])
    ]),
    field(4, [descriptor])
  ])
}

function binaryReferenceRecord(reference) {
  return record('binaryReference', [
    field(1, reference.ownerToken),
    field(2, reference.byteOffset),
    field(3, reference.byteLength),
    field(4, reference.ownership),
    field(5, reference.operationCorrelation)
  ])
}

function binaryReferenceFromRecord(value) {
  return {
    ownerToken: requiredString(value, 1),
    byteOffset: requiredNumber(value, 2),
    byteLength: requiredNumber(value, 3),
    ownership: requiredString(value, 4),
    operationCorrelation: requiredString(value, 5)
  }
}

function record(kind, fields) {
  return { kind, fields }
}

function field(id, value) {
  return { id, value }
}

function requiredRecord(value, id) {
  const fieldValue = requiredField(value, id)
  if (typeof fieldValue !== 'object' || fieldValue === null || Array.isArray(fieldValue)) {
    throw new Error(`Deterministic native field ${id} is not a record`)
  }
  return fieldValue
}

function requiredString(value, id) {
  const fieldValue = requiredField(value, id)
  if (typeof fieldValue !== 'string') throw new Error(`Deterministic native field ${id} is not a string`)
  return fieldValue
}

function requiredNumber(value, id) {
  const fieldValue = requiredField(value, id)
  if (typeof fieldValue !== 'number') throw new Error(`Deterministic native field ${id} is not a number`)
  return fieldValue
}

function requiredField(value, id) {
  const fieldValue = value.fields.find(candidate => candidate.id === id)
  if (fieldValue === undefined) throw new Error(`Deterministic native field ${id} is missing`)
  return fieldValue.value
}

module.exports = {
  DeterministicNativeControl,
  DeterministicReactNativeProtocolRuntime,
  deterministicReactNativeTckBoundary,
  REACT_NATIVE_PEER_ID
}
