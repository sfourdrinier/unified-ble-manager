// test-support/web/in-memory-web-bluetooth-tck-boundary.js

const { opaqueId } = require('../../src/backend-contract/primitives')

const WEB_BLUETOOTH_TCK_SERVICE_UUID = '0000180d-0000-1000-8000-00805f9b34fb'
const WEB_BLUETOOTH_TCK_CHARACTERISTIC_UUID = '00002a37-0000-1000-8000-00805f9b34fb'
const WEB_BLUETOOTH_TCK_BATTERY_SERVICE_UUID = '0000180f-0000-1000-8000-00805f9b34fb'
const WEB_BLUETOOTH_TCK_BATTERY_LEVEL_UUID = '00002a19-0000-1000-8000-00805f9b34fb'
const WEB_BLUETOOTH_TCK_USER_DESCRIPTION_UUID = '00002901-0000-1000-8000-00805f9b34fb'
const DEFAULT_READ_VALUE = new Uint8Array([0, 72])
const DEFAULT_INITIAL_NOTIFICATION_VALUE = new Uint8Array([0, 73])

/**
 * A browser-global-free Web Bluetooth boundary for deterministic first-party
 * chooser scenarios. It intentionally provides no continuous-scan surface.
 */
class InMemoryWebBluetoothTckBoundary {
  constructor(options = {}) {
    this.implementationVersion = options.implementationVersion ?? 'in-memory-web-bluetooth-tck-1'
    this.browserEngine = options.browserEngine ?? 'in-memory-web-bluetooth'
    this.deviceId = options.deviceId ?? 'in-memory-web-bluetooth-device'
    this.expectedSelectedPeerId = opaqueId('web-device-1', 'peer', 'web-bluetooth')
    this.serviceUuid = options.serviceUuid ?? WEB_BLUETOOTH_TCK_SERVICE_UUID
    this.characteristicUuid = options.characteristicUuid ?? WEB_BLUETOOTH_TCK_CHARACTERISTIC_UUID
    this.readValue = copyBytes(options.expectedReadValue ?? DEFAULT_READ_VALUE, 'expectedReadValue')
    this.initialNotificationValue = copyBytes(
      options.expectedInitialNotificationValue ?? DEFAULT_INITIAL_NOTIFICATION_VALUE,
      'expectedInitialNotificationValue'
    )
    this.secureContext = options.secureContext ?? true
    this.userActivation = options.userActivation ?? true
    this.available = options.available ?? true
    this.clock = options.now ?? 20
    this.connected = false
    this.pendingChooser = null
    this.chooserRequests = 0
    this.lastChooserRequest = null
    this.connectCalls = 0
    this.disconnectCalls = 0
    this.notificationStarts = 0
    this.notificationStops = 0
    this.notificationEmissions = 0
    this.notificationDeliveries = 0
    this.timersScheduled = 0
    this.timersCleared = 0
    this.timersFired = 0
    this.nextTimer = 1
    this.lastPageLifecycleReason = null
    this.nextNotificationStopFailure = null
    this.nextDisconnectFailure = null
    this.disconnectListeners = new Set()
    this.pageLifecycleListeners = new Set()
    this.timers = new Map()
    this.characteristic = this.createCharacteristic({
      uuid: this.characteristicUuid,
      readValue: () => this.readValue,
      initialNotification: () => this.initialNotificationValue,
      descriptors: []
    })
    this.service = {
      uuid: this.serviceUuid,
      getCharacteristics: async () => [this.characteristic]
    }
    // The duplicate-UUID world (docs/UNIFIED_SEMANTICS.md §9), after the
    // chooser service in discovery order: a second service UUID that repeats,
    // a repeated characteristic UUID under one service, and a repeated
    // descriptor UUID under one characteristic.
    const batteryLevel = descriptors =>
      this.createCharacteristic({
        uuid: WEB_BLUETOOTH_TCK_BATTERY_LEVEL_UUID,
        readValue: () => new Uint8Array([0x50]),
        initialNotification: null,
        descriptors
      })
    const batteryServiceCharacteristics = [
      [
        batteryLevel([userDescription(new Uint8Array([0x61])), userDescription(new Uint8Array([0x62]))]),
        batteryLevel([])
      ],
      [batteryLevel([])]
    ]
    const batteryServices = batteryServiceCharacteristics.map(characteristics => ({
      uuid: WEB_BLUETOOTH_TCK_BATTERY_SERVICE_UUID,
      getCharacteristics: async () => characteristics
    }))
    this.services = [this.service, ...batteryServices]
    this.instances = []
    const serviceOccurrences = new Map()
    for (const [index, service] of this.services.entries()) {
      const characteristics = index === 0 ? [this.characteristic] : batteryServiceCharacteristics[index - 1]
      const serviceOccurrence = nextOccurrence(serviceOccurrences, service.uuid)
      const characteristicOccurrences = new Map()
      for (const characteristic of characteristics) {
        this.instances.push({
          serviceUuid: service.uuid,
          serviceOccurrence,
          characteristicUuid: characteristic.uuid,
          characteristicOccurrence: nextOccurrence(characteristicOccurrences, characteristic.uuid),
          characteristic
        })
      }
    }
    this.gatt = this.createGatt()
    this.device = {
      id: this.deviceId,
      gatt: this.gatt,
      addDisconnectListener: listener => {
        this.disconnectListeners.add(listener)
      },
      removeDisconnectListener: listener => {
        this.disconnectListeners.delete(listener)
      }
    }
  }

  get expectedReadValue() {
    return new Uint8Array(this.readValue)
  }

  get expectedInitialNotificationValue() {
    return new Uint8Array(this.initialNotificationValue)
  }

  isSecureContext() {
    return this.secureContext
  }

  hasTransientUserActivation() {
    return this.userActivation
  }

  async bluetoothAvailable() {
    return this.available
  }

  async requestDevice(options) {
    if (this.pendingChooser !== null) {
      throw new Error('In-memory Web Bluetooth boundary permits exactly one outstanding chooser')
    }
    const request = copyChooserRequest(options)
    this.chooserRequests += 1
    this.lastChooserRequest = request
    return new Promise((resolve, reject) => {
      this.pendingChooser = { resolve, reject, request }
    })
  }

  now() {
    return this.clock
  }

  setTimer(callback, delayMilliseconds) {
    if (typeof callback !== 'function') {
      throw new Error('In-memory Web Bluetooth timer callback must be a function')
    }
    if (!Number.isFinite(delayMilliseconds) || delayMilliseconds < 0) {
      throw new Error('In-memory Web Bluetooth timer delay must be a non-negative finite number')
    }
    const handle = Object.freeze({ id: this.nextTimer })
    this.nextTimer += 1
    this.timers.set(handle, { callback, dueAt: this.clock + delayMilliseconds, order: handle.id })
    this.timersScheduled += 1
    return handle
  }

  clearTimer(handle) {
    if (this.timers.delete(handle)) {
      this.timersCleared += 1
    }
  }

  addPageLifecycleListener(listener) {
    this.pageLifecycleListeners.add(listener)
    return () => {
      this.pageLifecycleListeners.delete(listener)
    }
  }

  resolveChooser() {
    const pending = this.requirePendingChooser('resolveChooser')
    this.pendingChooser = null
    pending.resolve({ device: this.device, grantedServices: this.grantedServices(pending.request) })
  }

  /** As a browser grants: the device's services the request names, by filter or as optional. */
  grantedServices(request) {
    const requested = new Set([...request.filters.flatMap(filter => filter.services), ...request.optionalServices])
    const offered = [...new Set(this.services.map(service => service.uuid))]
    return offered.filter(uuid => requested.has(uuid))
  }

  rejectChooser(error = new Error('In-memory Web Bluetooth chooser rejected')) {
    if (!(error instanceof Error)) {
      throw new Error('In-memory Web Bluetooth chooser rejection must be an Error')
    }
    const pending = this.requirePendingChooser('rejectChooser')
    this.pendingChooser = null
    pending.reject(error)
  }

  emitNotification(input) {
    const characteristic = this.addressedCharacteristic(input)
    const value = copyBytes(input.value, 'notification value')
    this.deliverNotification(characteristic, value)
  }

  emitPageLifecycle(reason) {
    if (reason !== 'page-hidden' && reason !== 'page-unloaded') {
      throw new Error('In-memory Web Bluetooth page lifecycle reason is invalid')
    }
    this.lastPageLifecycleReason = reason
    for (const listener of [...this.pageLifecycleListeners]) {
      listener(reason)
    }
  }

  fireTimers() {
    for (;;) {
      const nextTimer = [...this.timers.entries()]
        .filter(([, timer]) => timer.dueAt <= this.clock)
        .sort(([, left], [, right]) => left.dueAt - right.dueAt || left.order - right.order)[0]
      if (nextTimer === undefined) {
        return
      }
      const [handle, timer] = nextTimer
      if (!this.timers.delete(handle)) {
        continue
      }
      this.timersFired += 1
      timer.callback()
    }
  }

  advanceTime(milliseconds) {
    if (!Number.isFinite(milliseconds) || milliseconds < 0) {
      throw new Error('In-memory Web Bluetooth clock advance must be a non-negative finite number')
    }
    this.clock += milliseconds
  }

  failNextNotificationStop(error = new Error('In-memory notification stop failed')) {
    if (!(error instanceof Error)) {
      throw new Error('In-memory notification stop failure must be an Error')
    }
    this.nextNotificationStopFailure = error
  }

  failNextDisconnect(error = new Error('In-memory GATT disconnect failed')) {
    if (!(error instanceof Error)) {
      throw new Error('In-memory GATT disconnect failure must be an Error')
    }
    this.nextDisconnectFailure = error
  }

  async flush() {
    for (let turn = 0; turn < 8; turn += 1) {
      await Promise.resolve()
    }
  }

  resourceSnapshot() {
    return Object.freeze({
      chooserRequests: this.chooserRequests,
      pendingChooser: this.pendingChooser !== null,
      lastChooserRequest: this.lastChooserRequest === null ? null : copyChooserRequest(this.lastChooserRequest),
      connected: this.connected,
      connectCalls: this.connectCalls,
      disconnectCalls: this.disconnectCalls,
      disconnectListeners: this.disconnectListeners.size,
      notificationListeners: this.notificationListenerCount(),
      notificationStarts: this.notificationStarts,
      notificationStops: this.notificationStops,
      notificationEmissions: this.notificationEmissions,
      notificationDeliveries: this.notificationDeliveries,
      pageLifecycleListeners: this.pageLifecycleListeners.size,
      lastPageLifecycleReason: this.lastPageLifecycleReason,
      activeTimers: this.timers.size,
      timersScheduled: this.timersScheduled,
      timersCleared: this.timersCleared,
      timersFired: this.timersFired
    })
  }

  createGatt() {
    const boundary = this
    return {
      get connected() {
        return boundary.connected
      },
      connect: async () => {
        this.connectCalls += 1
        this.connected = true
      },
      disconnect: () => {
        this.disconnectCalls += 1
        const failure = this.nextDisconnectFailure
        this.nextDisconnectFailure = null
        if (failure !== null) {
          throw failure
        }
        this.connected = false
        for (const listener of [...this.disconnectListeners]) {
          listener()
        }
      },
      getPrimaryServices: async () => [...this.services]
    }
  }

  createCharacteristic({ uuid, readValue, initialNotification, descriptors }) {
    const listeners = new Set()
    const characteristic = {
      uuid,
      properties: Object.freeze({
        read: true,
        write: false,
        writeWithoutResponse: false,
        notify: true,
        indicate: false
      }),
      listeners,
      getDescriptors: async () => descriptors,
      readValue: async () => new Uint8Array(readValue()),
      writeValueWithResponse: async () => {
        throw new Error('In-memory Web Bluetooth TCK characteristic does not support writes with response')
      },
      writeValueWithoutResponse: async () => {
        throw new Error('In-memory Web Bluetooth TCK characteristic does not support writes without response')
      },
      startNotifications: async () => {
        this.notificationStarts += 1
        if (initialNotification !== null) {
          this.deliverNotification(characteristic, initialNotification())
        }
      },
      stopNotifications: async () => {
        this.notificationStops += 1
        const failure = this.nextNotificationStopFailure
        this.nextNotificationStopFailure = null
        if (failure !== null) {
          throw failure
        }
      },
      addNotificationListener: listener => {
        listeners.add(listener)
      },
      removeNotificationListener: listener => {
        listeners.delete(listener)
      }
    }
    return characteristic
  }

  deliverNotification(characteristic, value) {
    this.notificationEmissions += 1
    for (const listener of [...characteristic.listeners]) {
      this.notificationDeliveries += 1
      listener(new Uint8Array(value))
    }
  }

  notificationListenerCount() {
    return this.instances.reduce((count, instance) => count + instance.characteristic.listeners.size, 0)
  }

  requirePendingChooser(operation) {
    if (this.pendingChooser === null) {
      throw new Error(`In-memory Web Bluetooth ${operation} requires an outstanding chooser`)
    }
    return this.pendingChooser
  }

  addressedCharacteristic(input) {
    if (input === null || typeof input !== 'object') {
      throw new Error('In-memory Web Bluetooth notification input must be an object')
    }
    const instance = this.instances.find(
      candidate =>
        candidate.serviceUuid === input.serviceUuid &&
        candidate.serviceOccurrence === input.serviceOccurrence &&
        candidate.characteristicUuid === input.characteristicUuid &&
        candidate.characteristicOccurrence === input.characteristicOccurrence
    )
    if (instance === undefined) {
      throw new Error('In-memory Web Bluetooth notification input does not address a deterministic characteristic')
    }
    return instance.characteristic
  }
}

function nextOccurrence(occurrences, uuid) {
  const occurrence = occurrences.get(uuid) ?? 0
  occurrences.set(uuid, occurrence + 1)
  return occurrence
}

function userDescription(value) {
  return {
    uuid: WEB_BLUETOOTH_TCK_USER_DESCRIPTION_UUID,
    readValue: async () => new Uint8Array(value),
    writeValue: async () => {
      throw new Error('In-memory Web Bluetooth TCK descriptors are read-only')
    }
  }
}

function copyBytes(value, label) {
  if (!(value instanceof Uint8Array)) {
    throw new Error(`In-memory Web Bluetooth ${label} must be Uint8Array`)
  }
  return new Uint8Array(value)
}

function copyChooserRequest(options) {
  if (options === null || typeof options !== 'object') {
    throw new Error('In-memory Web Bluetooth chooser options must be an object')
  }
  if (!Array.isArray(options.filters) || !Array.isArray(options.optionalServices)) {
    throw new Error('In-memory Web Bluetooth chooser options must include filter and optional-service arrays')
  }
  if (typeof options.acceptAllDevices !== 'boolean') {
    throw new Error('In-memory Web Bluetooth chooser acceptAllDevices must be a boolean')
  }
  return Object.freeze({
    filters: Object.freeze(
      options.filters.map(filter => {
        if (
          filter === null ||
          typeof filter !== 'object' ||
          !Array.isArray(filter.services) ||
          !Array.isArray(filter.manufacturerData)
        ) {
          throw new Error('In-memory Web Bluetooth chooser filter must include service and manufacturer arrays')
        }
        if (filter.namePrefix !== null && typeof filter.namePrefix !== 'string') {
          throw new Error('In-memory Web Bluetooth chooser filter namePrefix must be a string or null')
        }
        if (filter.services.some(service => typeof service !== 'string' || service.length === 0)) {
          throw new Error('In-memory Web Bluetooth chooser filter services must be non-empty strings')
        }
        return Object.freeze({
          services: Object.freeze([...filter.services]),
          manufacturerData: Object.freeze(
            filter.manufacturerData.map(manufacturer => {
              if (
                manufacturer === null ||
                typeof manufacturer !== 'object' ||
                !Number.isSafeInteger(manufacturer.companyIdentifier) ||
                manufacturer.companyIdentifier < 0 ||
                manufacturer.companyIdentifier > 0xffff ||
                (manufacturer.dataPrefix !== null && !(manufacturer.dataPrefix instanceof Uint8Array))
              ) {
                throw new Error('In-memory Web Bluetooth manufacturer criterion is invalid')
              }
              return Object.freeze({
                companyIdentifier: manufacturer.companyIdentifier,
                dataPrefix: manufacturer.dataPrefix === null ? null : new Uint8Array(manufacturer.dataPrefix)
              })
            })
          ),
          namePrefix: filter.namePrefix
        })
      })
    ),
    acceptAllDevices: options.acceptAllDevices,
    optionalServices: Object.freeze(
      options.optionalServices.map(service => {
        if (typeof service !== 'string' || service.length === 0) {
          throw new Error('In-memory Web Bluetooth optional services must be non-empty strings')
        }
        return service
      })
    )
  })
}

module.exports = {
  WEB_BLUETOOTH_TCK_BATTERY_SERVICE_UUID,
  DEFAULT_INITIAL_NOTIFICATION_VALUE,
  DEFAULT_READ_VALUE,
  InMemoryWebBluetoothTckBoundary,
  WEB_BLUETOOTH_TCK_CHARACTERISTIC_UUID,
  WEB_BLUETOOTH_TCK_SERVICE_UUID
}
