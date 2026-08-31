import { BleError, type BleConnection, type BleManager, type BlePeer, type GattSubscription } from 'unified-ble-manager'
import { createWebBleManager } from 'unified-ble-manager/web'
import {
  BATTERY_LEVEL_CHARACTERISTIC,
  BATTERY_SERVICE,
  parseBatteryLevel
} from 'unified-ble-manager/profiles/battery-service'
import {
  HEART_RATE_MEASUREMENT_CHARACTERISTIC,
  HEART_RATE_SERVICE,
  parseHeartRateMeasurement
} from 'unified-ble-manager/profiles/heart-rate'
import './style.css'

const CHOOSER_TIMEOUT_MS = 60_000
const CONNECT_TIMEOUT_MS = 60_000
const DISCOVER_TIMEOUT_MS = 20_000
const GATT_TIMEOUT_MS = 10_000

const controls = {
  choose: element<HTMLButtonElement>('choose-connect'),
  authorized: element<HTMLButtonElement>('authorized-connect'),
  reconnect: element<HTMLButtonElement>('reconnect'),
  disconnect: element<HTMLButtonElement>('disconnect'),
  destroy: element<HTMLButtonElement>('destroy')
} as const

const fields = {
  support: element<HTMLElement>('support'),
  status: element<HTMLOutputElement>('status'),
  peer: element<HTMLElement>('peer'),
  battery: element<HTMLElement>('battery'),
  heartRate: element<HTMLElement>('heart-rate'),
  resources: element<HTMLElement>('resources'),
  events: element<HTMLOListElement>('events'),
  errorPanel: element<HTMLElement>('error-panel'),
  errorMessage: element<HTMLElement>('error-message'),
  errorCode: element<HTMLElement>('error-code'),
  errorDomain: element<HTMLElement>('error-domain'),
  errorOperation: element<HTMLElement>('error-operation'),
  errorBrowser: element<HTMLElement>('error-browser')
} as const

let manager: BleManager | null = null
let connection: BleConnection | null = null
let subscription: GattSubscription | null = null
let notificationTask: Promise<Error | null> | null = null
let selectedPeer: BlePeer | null = null
let busy = false
const webSupported = window.isSecureContext && 'bluetooth' in navigator

controls.choose.addEventListener('click', () => runAction('choose', chooseAndConnect))
controls.authorized.addEventListener('click', () => runAction('authorized', connectAuthorized))
controls.reconnect.addEventListener('click', () => runAction('reconnect', reconnect))
controls.disconnect.addEventListener('click', () => runAction('disconnect', disconnect))
controls.destroy.addEventListener('click', () => runAction('destroy', destroyManager))

window.addEventListener('pagehide', () => {
  void destroyManager().catch(error => console.error('[web-example.pagehide] Cleanup failed:', error))
})

renderSupport()
render()

async function chooseAndConnect(): Promise<void> {
  if (connection !== null) throw new Error('Disconnect before choosing another device.')
  const activeManager = await ensureManager()
  setStatus('Opening Chrome’s Bluetooth chooser…')
  selectedPeer = await activeManager.choose({
    filters: [{ serviceUuids: [HEART_RATE_SERVICE] }],
    optionalServices: [BATTERY_SERVICE],
    timeoutMs: CHOOSER_TIMEOUT_MS
  })
  appendEvent('chooser', `Selected ${selectedPeer.name ?? selectedPeer.id}`)
  await connectSelectedPeer('connect')
}

async function connectAuthorized(): Promise<void> {
  if (connection !== null) throw new Error('Disconnect before selecting another device.')
  const activeManager = await ensureManager()
  setStatus('Reading devices already authorized for this origin…')
  const peers = await activeManager.peers.authorized({ timeoutMs: GATT_TIMEOUT_MS })
  if (peers.length === 0) throw new Error('No authorized device is available. Use Choose and connect first.')
  if (peers.length > 1)
    throw new Error('More than one device is authorized. Use Choose and connect to select one explicitly.')
  selectedPeer = peers[0] ?? null
  appendEvent('authorized', `Resolved ${selectedPeer?.name ?? selectedPeer?.id ?? 'peer'}`)
  await connectSelectedPeer('authorized-connect')
}

async function reconnect(): Promise<void> {
  if (selectedPeer === null) throw new Error('Choose or authorize a device first.')
  if (connection !== null) throw new Error('The selected device is already connected.')
  await connectSelectedPeer('reconnect')
}

async function connectSelectedPeer(operation: string): Promise<void> {
  if (manager === null || selectedPeer === null) throw new Error('The manager or selected peer is unavailable.')
  setStatus('Opening a bounded Bluetooth connection…')
  try {
    connection = await manager.connect(selectedPeer, { timeoutMs: CONNECT_TIMEOUT_MS })
  } catch (error) {
    const failure = toError(error)
    if (
      failure instanceof BleError &&
      failure.code === 'operation.timed-out' &&
      failure.operation === 'web-connection.connect'
    ) {
      appendEvent('timeout', 'Chrome’s native connect remains uncancellable; destroying its manager before retry')
      await destroyManager()
    }
    throw failure
  }
  appendEvent(operation, `Opened generation ${connection.connectionGeneration}`)

  setStatus('Discovering GATT services…')
  const database = await connection.discover({ timeoutMs: DISCOVER_TIMEOUT_MS })
  appendEvent('discover', `Found ${database.services.length} service(s)`)

  setStatus('Reading Battery Level…')
  const batteryCharacteristic = database.characteristic(BATTERY_SERVICE, BATTERY_LEVEL_CHARACTERISTIC)
  const battery = parseBatteryLevel(await batteryCharacteristic.read({ timeoutMs: GATT_TIMEOUT_MS }))
  fields.battery.textContent = `${battery}%`
  appendEvent('read', `Battery Level ${battery}%`)

  setStatus('Starting Heart Rate notifications…')
  const measurementCharacteristic = database.characteristic(HEART_RATE_SERVICE, HEART_RATE_MEASUREMENT_CHARACTERISTIC)
  subscription = await measurementCharacteristic.subscribe({
    timeoutMs: GATT_TIMEOUT_MS,
    delivery: 'prefer-notification',
    stream: 'balanced'
  })
  notificationTask = consumeNotifications(subscription)
  appendEvent('subscribe', `Active via ${subscription.effectiveDelivery}`)
  setStatus('Connected. Battery and live Heart Rate data are available.')
}

async function consumeNotifications(active: GattSubscription): Promise<Error | null> {
  try {
    for await (const event of active.values) {
      if (event.kind === 'value') {
        const measurement = parseHeartRateMeasurement(event.value.value)
        fields.heartRate.textContent = `${measurement.beatsPerMinute} BPM`
        appendEvent('notification', `${measurement.beatsPerMinute} BPM`)
      } else if (event.kind === 'overflow') {
        appendEvent('overflow', `${event.droppedItems} item(s) dropped`)
      } else {
        appendEvent('terminal', event.reason)
        return null
      }
    }
    return null
  } catch (error) {
    const failure = toError(error)
    appendEvent('notification-error', failure.message)
    return failure
  }
}

async function disconnect(): Promise<void> {
  const failures: Error[] = []
  if (subscription !== null) {
    try {
      assertReleased(await subscription.remove(), 'notification removal')
      subscription = null
      appendEvent('unsubscribe', 'Notification resource released')
    } catch (error) {
      failures.push(toError(error))
    }
  }
  if (notificationTask !== null) {
    const failure = await notificationTask
    notificationTask = null
    if (failure !== null) failures.push(failure)
  }
  if (connection !== null) {
    try {
      assertReleased(await connection.disconnect(), 'connection disconnect')
      connection = null
      appendEvent('disconnect', 'Connection resource released')
    } catch (error) {
      failures.push(toError(error))
    }
  }
  if (failures.length > 0) throw new AggregateError(failures, 'Bluetooth disconnect cleanup failed.')
  setStatus('Disconnected. Reconnect reuses this page’s selected peer.')
}

async function destroyManager(): Promise<void> {
  const failures: Error[] = []
  try {
    await disconnect()
  } catch (error) {
    failures.push(toError(error))
  }
  if (manager !== null) {
    const current = manager
    try {
      assertReleased(await current.destroy(), 'manager destroy')
      fields.resources.textContent = JSON.stringify(current.diagnostics.resourceCounters(), null, 2)
      appendEvent('destroy', 'Manager and every owned resource released')
      manager = null
      selectedPeer = null
    } catch (error) {
      failures.push(toError(error))
    }
  }
  if (failures.length > 0) throw new AggregateError(failures, 'Manager cleanup failed.')
  setStatus('Manager destroyed. A fresh chooser action can start a new session.')
}

async function ensureManager(): Promise<BleManager> {
  if (manager !== null) return manager
  manager = await createWebBleManager()
  appendEvent('manager', 'Web Bluetooth backend attached')
  return manager
}

async function runAction(name: string, action: () => Promise<void>): Promise<void> {
  if (busy) return
  busy = true
  clearError()
  render()
  try {
    await action()
  } catch (error) {
    const failure = toError(error)
    console.error(`[web-example.${name}]`, failure)
    showError(failure)
    setStatus(`${name} did not complete. Review the diagnostic detail below.`)
  } finally {
    busy = false
    render()
  }
}

function showError(failure: Error): void {
  fields.errorPanel.hidden = false
  fields.errorMessage.textContent = failure.message
  if (failure instanceof BleError) {
    fields.errorCode.textContent = failure.code
    fields.errorDomain.textContent = failure.domain
    fields.errorOperation.textContent = failure.operation
    const browserName = failure.platform?.metadata.browserErrorName
    fields.errorBrowser.textContent = typeof browserName === 'string' ? browserName : (failure.platform?.code ?? '—')
    appendEvent('error', `${failure.code} · ${failure.operation}`)
  } else {
    fields.errorCode.textContent = failure.name
    fields.errorDomain.textContent = 'application'
    fields.errorOperation.textContent = 'example action'
    fields.errorBrowser.textContent = '—'
    appendEvent('error', failure.message)
  }
}

function clearError(): void {
  fields.errorPanel.hidden = true
}

function assertReleased(
  cleanup: { readonly state: string; readonly failures: readonly unknown[] },
  operation: string
): void {
  if (cleanup.state !== 'released' || cleanup.failures.length !== 0) {
    throw new Error(`${operation} did not release every owned resource.`)
  }
}

function appendEvent(operation: string, detail: string): void {
  const item = document.createElement('li')
  const time = document.createElement('time')
  time.dateTime = new Date().toISOString()
  time.textContent = new Date().toLocaleTimeString()
  const copy = document.createElement('span')
  copy.textContent = `${operation} · ${detail}`
  item.append(time, copy)
  fields.events.prepend(item)
}

function setStatus(message: string): void {
  fields.status.textContent = message
}

function renderSupport(): void {
  fields.support.textContent = webSupported ? 'Web Bluetooth ready' : 'Web Bluetooth unavailable'
  fields.support.classList.toggle('unavailable', !webSupported)
}

function render(): void {
  controls.choose.disabled = !webSupported || busy || connection !== null
  controls.authorized.disabled = !webSupported || busy || connection !== null
  controls.reconnect.disabled = busy || manager === null || selectedPeer === null || connection !== null
  controls.disconnect.disabled = busy || connection === null
  controls.destroy.disabled = busy || manager === null
  fields.peer.textContent = selectedPeer?.name ?? selectedPeer?.id ?? '—'
  if (manager !== null) fields.resources.textContent = JSON.stringify(manager.diagnostics.resourceCounters(), null, 2)
}

function toError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error))
}

function element<T extends HTMLElement>(id: string): T {
  const found = document.getElementById(id)
  if (found === null) throw new Error(`Missing #${id}`)
  return found as T
}
