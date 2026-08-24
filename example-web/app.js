// example-web/app.js

import { createWebBleManager } from 'unified-ble-manager/web'
import {
  BATTERY_LEVEL_CHARACTERISTIC,
  BATTERY_SERVICE,
  parseBatteryLevel
} from 'unified-ble-manager/profiles/battery-service'
import {
  HEART_RATE_SERVICE,
  HEART_RATE_MEASUREMENT_CHARACTERISTIC,
  parseHeartRateMeasurement
} from 'unified-ble-manager/profiles/heart-rate'

const operationOptions = Object.freeze({})
const notificationOptions = Object.freeze({
  ...operationOptions,
  delivery: 'prefer-notification',
  stream: 'balanced'
})

const controls = Object.freeze({
  choose: document.getElementById('choose-connect'),
  reconnect: document.getElementById('reconnect'),
  disconnect: document.getElementById('disconnect'),
  destroy: document.getElementById('destroy')
})
const fields = Object.freeze({
  status: document.getElementById('status'),
  peer: document.getElementById('peer'),
  battery: document.getElementById('battery'),
  heartRate: document.getElementById('heart-rate'),
  resources: document.getElementById('resources'),
  events: document.getElementById('events')
})

let manager = null
let connection = null
let database = null
let subscription = null
let notificationTask = null
let selectedPeerId = null
let busy = false

controls.choose.addEventListener('click', () => runAction('choose-connect', chooseAndConnect))
controls.reconnect.addEventListener('click', () => runAction('reconnect', reconnect))
controls.disconnect.addEventListener('click', () => runAction('disconnect', disconnect))
controls.destroy.addEventListener('click', () => runAction('destroy', destroyManager))

window.addEventListener('pagehide', () => {
  void destroyManager().catch(error => {
    console.error('[web-example.pagehide] Unified BLE cleanup failed:', error)
  })
})

render()

async function runAction(name, action) {
  if (busy) return
  busy = true
  render()
  try {
    await action()
  } catch (error) {
    const failure = toError(error)
    console.error(`[web-example.${name}] Unified BLE operation failed:`, failure)
    setStatus(`${name} failed: ${failure.message}`, true)
  } finally {
    busy = false
    render()
  }
}

async function chooseAndConnect() {
  if (connection !== null) {
    throw new Error('Disconnect the current peer before choosing another device.')
  }
  await ensureManager()
  setStatus('Opening the Web Bluetooth chooser…')
  const selection = await manager.choose(
    {
      filters: [{ serviceUuids: [HEART_RATE_SERVICE], manufacturerData: [] }],
      acceptAllDevices: false,
      optionalServices: [HEART_RATE_SERVICE, BATTERY_SERVICE]
    },
    operationOptions
  )
  selectedPeerId = selection.id
  appendEvent('chooser', 'selected a Heart Rate Service peer')
  await connectSelectedPeer('connect')
}

async function reconnect() {
  if (selectedPeerId === null) throw new Error('Choose a device before reconnecting.')
  if (connection !== null) throw new Error('The selected peer is already connected.')
  await ensureManager()
  await connectSelectedPeer('reconnect')
}

async function connectSelectedPeer(operation) {
  if (manager === null || selectedPeerId === null) {
    throw new Error('The Web Bluetooth manager or selected peer is unavailable.')
  }
  setStatus(operation === 'reconnect' ? 'Reconnecting…' : 'Connecting…')
  const connected = await manager.connect(selectedPeerId, operationOptions)
  connection = connected
  appendEvent(operation, `connection generation ${String(connected.connectionGeneration)}`)

  const discovered = await connection.discover(operationOptions)
  database = discovered
  const snapshot = await database.snapshot()
  appendEvent('discover', `${String(snapshot.services.length)} services`)

  const batteryCharacteristic = database.characteristic(BATTERY_SERVICE, BATTERY_LEVEL_CHARACTERISTIC)
  const battery = parseBatteryLevel(await batteryCharacteristic.read(operationOptions))
  fields.battery.textContent = `${String(battery)}%`
  appendEvent('read', `Battery Level ${String(battery)}%`)

  const measurementCharacteristic = database.characteristic(
    HEART_RATE_SERVICE,
    HEART_RATE_MEASUREMENT_CHARACTERISTIC
  )
  subscription = await measurementCharacteristic.subscribe(notificationOptions)
  notificationTask = consumeHeartRateNotifications(subscription)
  appendEvent('notify', 'Heart Rate Measurement subscription active')
  setStatus('Connected, discovered, read, and subscribed.')
}

async function consumeHeartRateNotifications(activeSubscription) {
  try {
    for await (const item of activeSubscription.values) {
      if (item.kind === 'value') {
        const measurement = parseHeartRateMeasurement(item.value.value)
        fields.heartRate.textContent = `${String(measurement.beatsPerMinute)} BPM`
        appendEvent('notification', `${String(measurement.beatsPerMinute)} BPM`)
        continue
      }
      if (item.kind === 'overflow') {
        appendEvent('overflow', `${String(item.droppedItems)} notification item(s) dropped`)
        continue
      }
      appendEvent('terminal', item.reason)
      return null
    }
    return null
  } catch (error) {
    const failure = toError(error)
    console.error('[web-example.consumeHeartRateNotifications] Notification stream failed:', failure)
    return failure
  }
}

async function disconnect() {
  const failures = []
  await stopSubscription(failures)
  if (connection !== null) {
    try {
      assertReleased(await connection.disconnect(), 'connection disconnect')
      connection = null
      database = null
      appendEvent('disconnect', 'connection resources released')
    } catch (error) {
      failures.push(toError(error))
    }
  }
  if (failures.length > 0) throw new AggregateError(failures, 'Bluetooth disconnect cleanup failed.')
  setStatus('Disconnected. Reconnect uses the same authorized peer.')
}

async function stopSubscription(failures) {
  if (subscription === null) return
  try {
    assertReleased(await subscription.remove(), 'notification removal')
    subscription = null
    appendEvent('notification-stop', 'subscription resources released')
  } catch (error) {
    failures.push(toError(error))
    return
  }
  if (notificationTask !== null) {
    const notificationFailure = await notificationTask
    notificationTask = null
    if (notificationFailure !== null) failures.push(notificationFailure)
  }
}

async function destroyManager() {
  const failures = []
  try {
    await disconnect()
  } catch (error) {
    failures.push(toError(error))
  }
  if (manager !== null) {
    try {
      assertReleased(await manager.destroy(), 'manager destroy')
      fields.resources.textContent = JSON.stringify(manager.diagnostics.resourceCounters())
      manager = null
      selectedPeerId = null
      appendEvent('destroy', 'manager resources released')
    } catch (error) {
      failures.push(toError(error))
    }
  }
  if (failures.length > 0) throw new AggregateError(failures, 'Unified BLE manager cleanup failed.')
  setStatus('Manager destroyed. All owned resources were released.')
}

async function ensureManager() {
  if (manager !== null) return manager
  manager = await createWebBleManager()
  appendEvent('bootstrap', 'Web Bluetooth backend attached')
  return manager
}

function assertReleased(cleanup, operation) {
  if (cleanup.state !== 'released' || cleanup.failures.length !== 0) {
    throw new Error(`${operation} did not release every owned resource.`)
  }
}

function appendEvent(operation, detail) {
  const item = document.createElement('li')
  item.textContent = `${operation}: ${detail}`
  fields.events.append(item)
}

function setStatus(message, error = false) {
  fields.status.textContent = message
  fields.status.classList.toggle('error', error)
}

function render() {
  controls.choose.disabled = busy || connection !== null
  controls.reconnect.disabled = busy || manager === null || selectedPeerId === null || connection !== null
  controls.disconnect.disabled = busy || connection === null
  controls.destroy.disabled = busy || manager === null
  fields.peer.textContent = selectedPeerId ?? '—'
  if (manager !== null) fields.resources.textContent = JSON.stringify(manager.diagnostics.resourceCounters())
}

function toError(error) {
  return error instanceof Error ? error : new Error(String(error))
}
