// __tests__/tck/winrt-first-party-tck-registration.test.js

const { createWinRtFirstPartyTckRegistration } = require('../../src/testing')
const { runBackendTck } = require('../../src/tck')

const SERVICE_UUID = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC_UUID = '00002a37-0000-1000-8000-00805f9b34fb'

test('WinRT first-party factory executes provider and deterministic-boundary vertical standard scenarios', async () => {
  const registration = createWinRtFirstPartyTckRegistration({
    now: () => 20,
    nativePeerId: 'C0FFEE000001',
    createBoundary: () => new DeterministicWinRtBoundary()
  })

  const report = await runBackendTck(registration.factory, registration.featureSuites, {
    proofScope: 'deterministic',
    baseScenarioIds: registration.suites.flatMap(suite => suite.baseScenarioIds)
  })

  expect(registration.suites[0].suiteId).toBe('winrt-provider-contract-v2')
  expect(report.baseScenarioIds).toContain('scenario.scan-connect-discover-read-notify-destroy')
  expect(
    report.receipts.find(receipt => receipt.scenarioId === 'scenario.scan-connect-discover-read-notify-destroy')
  ).toMatchObject({
    error: null,
    facts: [expect.objectContaining({ id: 'vertical-slice-preserves-scan-and-cleans-up', holds: true })]
  })
  expect(registration.capabilityExclusions).toEqual(
    expect.arrayContaining([expect.objectContaining({ featureId: 'winrt:live-radio', state: 'unavailable' })])
  )
})

class DeterministicWinRtBoundary {
  constructor() {
    this.scanHandler = null
    this.scanToken = null
    this.scanHandlers = new Map()
    this.notificationHandlers = new Map()
    this.connected = new Set()
    this.connectionListeners = new Set()
    this.databaseListeners = new Set()
    this.scanTerminalListeners = new Set()
    this.adapterListeners = new Set()
    this.securityListeners = new Set()
    this.securityBonded = false
    this.suppressNextSecurityPair = false
    this.pendingSecurityPair = null
  }

  listAdapters() {
    return operation([
      {
        nativeAdapterId: 'winrt-tck-adapter',
        displayName: 'WinRT TCK adapter',
        state: this.adapterSnapshot(),
        deployment: 'unpackaged'
      }
    ])
  }
  selectAdapter() {
    return operation(undefined)
  }
  adapterSnapshot() {
    return { availability: 'available', authorization: 'granted', power: 'on', safeReason: null }
  }

  securityState(nativePeerId) {
    if (nativePeerId !== 'C0FFEE000001') return operation(Promise.reject(new Error('unknown security peer')))
    return operation(this.securityStateValue())
  }

  pair(nativePeerId) {
    if (nativePeerId !== 'C0FFEE000001') return operation(Promise.reject(new Error('unknown security peer')))
    if (this.suppressNextSecurityPair) {
      this.suppressNextSecurityPair = false
      let resolve
      const completion = new Promise(result => {
        resolve = result
      })
      this.pendingSecurityPair = resolve
      return {
        completion,
        cancel: async () => {
          this.pendingSecurityPair?.({ outcome: 'cancelled', state: null, reason: null })
          this.pendingSecurityPair = null
          return 'cancellation-requested'
        }
      }
    }
    if (this.securityBonded)
      return operation({ outcome: 'already-paired', state: this.securityStateValue(), reason: null })
    this.securityBonded = true
    this.emitSecurityState()
    return operation({ outcome: 'paired', state: this.securityStateValue(), reason: null })
  }

  cancelPairing() {
    return operation(undefined)
  }

  unpair(nativePeerId) {
    if (nativePeerId !== 'C0FFEE000001') return operation(Promise.reject(new Error('unknown security peer')))
    if (!this.securityBonded) return operation('already-unpaired')
    this.securityBonded = false
    this.emitSecurityState()
    return operation('unpaired')
  }

  onSecurityState(listener) {
    this.securityListeners.add(listener)
    return () => this.securityListeners.delete(listener)
  }

  prepareSecurityCancellation() {
    this.suppressNextSecurityPair = true
  }

  securityStateValue() {
    return {
      bond: this.securityBonded ? 'bonded' : 'not-bonded',
      encryption: 'unsupported',
      authentication: 'unsupported',
      secureConnections: 'unsupported',
      pairingPossible: true
    }
  }

  emitSecurityState() {
    const record = { nativePeerId: 'C0FFEE000001', state: this.securityStateValue() }
    for (const listener of [...this.securityListeners]) listener(record)
  }
  startScan(scanToken, _services, handler) {
    this.scanToken = scanToken
    this.scanHandler = handler
    this.scanHandlers.set(scanToken, handler)
    return operation(undefined)
  }
  stopScan(scanToken) {
    if (this.scanToken !== scanToken) return operation(Promise.reject(new Error('scan token mismatch')))
    this.scanHandler = null
    this.scanToken = null
    this.emitScanTerminal({ scanToken, status: 'stopped', error: 'success' })
    return operation(undefined)
  }
  onScanTerminal(listener) {
    this.scanTerminalListeners.add(listener)
    return () => this.scanTerminalListeners.delete(listener)
  }
  connect(peerId, _connectionGeneration) {
    this.connected.add(peerId)
    return operation(undefined)
  }
  disconnect(peerId) {
    this.connected.delete(peerId)
    return operation(undefined)
  }
  discover() {
    return operation({
      cacheMode: 'uncached',
      services: [
        {
          uuid: SERVICE_UUID,
          occurrence: 0,
          characteristics: [
            {
              uuid: CHARACTERISTIC_UUID,
              occurrence: 0,
              readable: true,
              writableWithResponse: true,
              writableWithoutResponse: true,
              notifiable: true,
              indicatable: false,
              descriptors: []
            }
          ]
        }
      ]
    })
  }
  read(address) {
    return operation(new Uint8Array([address.serviceOccurrence, address.characteristicOccurrence]))
  }
  write() {
    return operation(undefined)
  }
  readDescriptor() {
    return operation(new Uint8Array([0]))
  }
  writeDescriptor() {
    return operation(undefined)
  }
  startNotify(address, _mode, handler) {
    this.notificationHandlers.set(addressKey(address), handler)
    return operation(undefined)
  }
  stopNotify(address) {
    this.notificationHandlers.delete(addressKey(address))
    return operation(undefined)
  }
  onConnectionLost(listener) {
    this.connectionListeners.add(listener)
    return () => this.connectionListeners.delete(listener)
  }
  onDatabaseChanged(listener) {
    this.databaseListeners.add(listener)
    return () => this.databaseListeners.delete(listener)
  }
  onAdapterState(listener) {
    this.adapterListeners.add(listener)
    return () => this.adapterListeners.delete(listener)
  }
  ingressTelemetry() {
    return {
      notificationQueueDrops: 0,
      advertisementQueueDrops: 0,
      notificationCloseDrops: 0,
      advertisementCloseDrops: 0
    }
  }
  destroy() {
    this.scanHandler = null
    this.scanToken = null
    this.scanHandlers.clear()
    this.notificationHandlers.clear()
    return operation(undefined)
  }
  emitAdvertisement() {
    if (this.scanToken === null) throw new Error('scan is not active')
    const handler = this.scanHandlers.get(this.scanToken)
    if (handler === undefined) throw new Error('scan handler is not registered')
    handler({
      scanToken: this.scanToken,
      nativePeerId: 'C0FFEE000001',
      localName: 'WinRT TCK peer',
      rssi: -40,
      serviceUuids: [SERVICE_UUID],
      connectable: true
    })
  }
  emitScanTerminal(record) {
    for (const listener of this.scanTerminalListeners) listener(record)
  }
  emitNotification(address, bytes) {
    const handler = this.notificationHandlers.get(addressKey(address))
    if (handler === undefined) throw new Error('notification is not active')
    handler(new Uint8Array(bytes))
  }
}

function operation(value) {
  return { completion: Promise.resolve(value), cancel: async () => 'already-terminal' }
}
function addressKey(address) {
  return [
    address.nativePeerId,
    address.serviceUuid,
    address.serviceOccurrence,
    address.characteristicUuid,
    address.characteristicOccurrence
  ].join('|')
}
