'use strict'

// One short physical session under Bun against the stock Polar H10 simulator.
// Connects only when the advertisement local name is exactly SIM Polar H10 0001.
// Any other sighting is recorded and left alone. This is a host receipt, not a
// CI gate: it needs that simulator in range and is not run by the package tests.
//
//   UBM_RADIO_PLATFORM=bluez UBM_RADIO_ADAPTER=hci0 bun scripts/ci/bun-desktop-h10-session.js
//   UBM_RADIO_PLATFORM=corebluetooth bun scripts/ci/bun-desktop-h10-session.js

const fs = require('node:fs')
const path = require('node:path')

const ROOT = path.join(__dirname, '..', '..')
const MINIMUM_BUN = [1, 4, 2]
const IDENTITY_SOURCE = path.join(ROOT, 'src', 'generated', 'native-build-identity.ts')
const SIM_NAME = 'SIM Polar H10 0001'
const SESSION_BUDGET_MS = 90000
const LEASE = 'bun-desktop-h10'

const HR_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const HR_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'
const BATTERY_SERVICE = '0000180f-0000-1000-8000-00805f9b34fb'
const BATTERY_LEVEL = '00002a19-0000-1000-8000-00805f9b34fb'
const DEVICE_INFO_SERVICE = '0000180a-0000-1000-8000-00805f9b34fb'
const MANUFACTURER_NAME = '00002a29-0000-1000-8000-00805f9b34fb'
const PMD_SERVICE = 'fb005c80-02e7-f387-1cad-8acd2d8df0c8'
const PMD_CONTROL = 'fb005c81-02e7-f387-1cad-8acd2d8df0c8'

let activeState = null
let releasing = false

function fail(message) {
  const error = new Error(message)
  error.ubmSessionFailure = true
  throw error
}

async function releaseAndExit(message) {
  console.error(`bun-desktop-h10: FAIL ${message}`)
  if (!releasing) {
    releasing = true
    if (activeState) {
      const errors = await cleanup(activeState)
      for (const error of errors) console.error(`bun-desktop-h10: cleanup ${error}`)
    }
  }
  process.exit(1)
}

function delay(ms) {
  return new Promise(resolve => {
    setTimeout(resolve, ms)
  })
}

function versionBelow(actual, minimum) {
  const parts = String(actual)
    .split('.')
    .map(part => Number.parseInt(part, 10))
  for (let index = 0; index < minimum.length; index += 1) {
    const left = parts[index] ?? 0
    const right = minimum[index]
    if (left < right) return true
    if (left > right) return false
  }
  return false
}

function assertBun() {
  const version = process.versions.bun
  if (typeof version !== 'string' || version.length === 0) {
    fail('this session runs under Bun, not Node')
  }
  if (versionBelow(version, MINIMUM_BUN)) {
    fail(`Bun ${version} is older than ${MINIMUM_BUN.join('.')}`)
  }
  if (typeof process.versions.napi !== 'string') {
    fail('Bun did not report a Node-API version')
  }
  return version
}

function hex(value) {
  return Buffer.from(value).toString('hex')
}

function text(value) {
  return Buffer.from(value).toString('utf8').replace(/\0+$/u, '')
}

function sameUuid(left, right) {
  return String(left).toLowerCase() === String(right).toLowerCase()
}

function selectorFor(paths, serviceUuid, characteristicUuid) {
  const match = paths.find(
    pathInfo => sameUuid(pathInfo.serviceUuid, serviceUuid) && sameUuid(pathInfo.characteristicUuid, characteristicUuid)
  )
  if (!match) {
    fail(`discovery has no ${serviceUuid} / ${characteristicUuid}`)
  }
  return {
    serviceUuid: match.serviceUuid,
    serviceOccurrence: match.serviceOccurrence,
    characteristicUuid: match.characteristicUuid,
    characteristicOccurrence: match.characteristicOccurrence
  }
}

async function pollValue(central, options, budgetMs, label) {
  const deadline = Date.now() + budgetMs
  while (Date.now() < deadline) {
    const poll = await central.pollNotification(options)
    if (poll.kind === 'value' && poll.value) return poll.value
    if (poll.kind === 'terminal' || poll.kind === 'invalidated' || poll.kind === 'closed') {
      fail(`${label} ended before a value: ${JSON.stringify({ kind: poll.kind, cause: poll.cause ?? null })}`)
    }
    await delay(50)
  }
  fail(`${label} produced no value within ${budgetMs}ms`)
}

async function cleanup(state) {
  const errors = []
  if (state.central && state.peerId) {
    for (const subscription of state.subscriptions) {
      try {
        await state.central.unsubscribe({
          peerId: state.peerId,
          selector: subscription.selector,
          consumer: subscription.consumer,
          timeoutMs: 5000
        })
      } catch (error) {
        errors.push(`unsubscribe ${subscription.consumer}: ${error.message}`)
      }
    }
    try {
      await state.central.disconnect({ peerId: state.peerId, lease: LEASE, timeoutMs: 10000 })
    } catch (error) {
      errors.push(`disconnect: ${error.message}`)
    }
  }
  if (state.central && state.scanId) {
    try {
      await state.central.stopScan(state.scanId, { timeoutMs: 5000 })
    } catch (error) {
      errors.push(`stopScan: ${error.message}`)
    }
  }
  if (state.central) {
    try {
      await state.central.close()
    } catch (error) {
      errors.push(`close: ${error.message}`)
    }
  }
  return errors
}

async function main() {
  const watchdog = setTimeout(() => {
    void releaseAndExit(`session exceeded ${SESSION_BUDGET_MS}ms`)
  }, SESSION_BUDGET_MS)
  const bun = assertBun()
  const platform = process.env.UBM_RADIO_PLATFORM
  if (!['corebluetooth', 'bluez', 'winrt'].includes(platform)) {
    fail('UBM_RADIO_PLATFORM must be corebluetooth, bluez, or winrt')
  }
  const adapterId = process.env.UBM_RADIO_ADAPTER || null
  if (platform === 'bluez' && !adapterId) {
    fail('UBM_RADIO_ADAPTER is required on bluez so the scan stays off the peripheral adapter')
  }

  const { loadDesktopCore } = require(path.join(ROOT, 'native', 'desktop-core', 'index.js'))
  let loaded
  try {
    loaded = loadDesktopCore()
  } catch (error) {
    fail(`${error.code ?? 'load-failed'}: ${error.message}`)
  }
  const identity = JSON.parse(loaded.module.nativeBuildIdentity())
  const expected = fs.readFileSync(IDENTITY_SOURCE, 'utf8')
  if (identity.binding !== 'napi') fail(`addon is not an napi build: ${JSON.stringify(identity)}`)
  const allowedProfile = loaded.mode === 'source' ? ['release', 'debug'] : ['release']
  if (!allowedProfile.includes(identity.profile)) {
    fail(`addon profile ${identity.profile} is not valid for ${loaded.mode} mode`)
  }
  if (!expected.includes(identity.sourceDigest) || !expected.includes(identity.bindingSchema)) {
    fail('addon identity does not match src/generated/native-build-identity.ts')
  }

  const UbmCentral = loaded.module.UbmCentral
  const adapters = (await UbmCentral.listAdapters()).map(entry => ({
    index: entry.index,
    label: entry.label ?? null,
    displayName: entry.displayName ?? null,
    default: entry.default === true,
    error: entry.error ?? null
  }))
  const state = { central: null, scanId: null, peerId: null, subscriptions: [] }
  activeState = state
  let wakes = 0
  try {
    state.central = await UbmCentral.open({
      owner: 'bun-desktop-h10',
      platform,
      ...(adapterId ? { adapterId } : {})
    })
    state.central.setEventWaker(() => {
      wakes += 1
    })
    if (state.central.eventWakeFailures() !== 0) {
      fail(`event waker installed with ${state.central.eventWakeFailures()} failures`)
    }

    const scan = await state.central.startScan({
      owner: 'bun-desktop-h10',
      serviceUuids: [HR_SERVICE],
      localNamePrefix: 'SIM Polar H10',
      duplicatePolicy: 'all',
      timeoutMs: 20000
    })
    state.scanId = scan.operationId
    const ignored = []
    let peer = null
    const deadline = Date.now() + 20000
    while (Date.now() < deadline && !peer) {
      const observation = await state.central.takeScanObservation()
      const advertisement = observation?.advertisement
      if (!advertisement) {
        await delay(50)
        continue
      }
      if (advertisement.localName === SIM_NAME) {
        peer = advertisement
        break
      }
      const sighting = {
        localName: advertisement.localName ?? null,
        address: advertisement.address ?? null
      }
      const known = ignored.some(entry => entry.localName === sighting.localName && entry.address === sighting.address)
      if (!known && ignored.length < 20) ignored.push(sighting)
    }
    const stopped = await state.central.stopScan(state.scanId, { timeoutMs: 10000 })
    state.scanId = null
    if (!peer) {
      fail(`named simulator ${SIM_NAME} was not observed; ignored ${JSON.stringify(ignored)}; scan ${stopped}`)
    }

    state.peerId = peer.peerId
    const connection = await state.central.connect({
      peerId: state.peerId,
      lease: LEASE,
      timeoutMs: 20000
    })
    const discovery = await state.central.discover({
      peerId: state.peerId,
      lease: LEASE,
      timeoutMs: 20000
    })
    const paths = await state.central.discoveredPaths(state.peerId)
    const batterySelector = selectorFor(paths, BATTERY_SERVICE, BATTERY_LEVEL)
    const manufacturerSelector = selectorFor(paths, DEVICE_INFO_SERVICE, MANUFACTURER_NAME)
    const controlSelector = selectorFor(paths, PMD_SERVICE, PMD_CONTROL)
    const heartSelector = selectorFor(paths, HR_SERVICE, HR_MEASUREMENT)

    const battery = await state.central.read({
      peerId: state.peerId,
      lease: LEASE,
      selector: batterySelector,
      timeoutMs: 10000
    })
    if (battery.value.length !== 1 || battery.value[0] !== 90) {
      fail(`battery level was ${hex(battery.value)}, expected 5a`)
    }
    const manufacturer = await state.central.read({
      peerId: state.peerId,
      lease: LEASE,
      selector: manufacturerSelector,
      timeoutMs: 10000
    })
    if (text(manufacturer.value) !== 'Polar Electro Oy') {
      fail(`manufacturer was ${JSON.stringify(text(manufacturer.value))}`)
    }

    const controlConsumer = 'bun-pmd-settings'
    await state.central.subscribe({
      peerId: state.peerId,
      lease: LEASE,
      selector: controlSelector,
      consumer: controlConsumer,
      deliveryMode: 'indication',
      timeoutMs: 10000
    })
    state.subscriptions.push({ selector: controlSelector, consumer: controlConsumer })
    await state.central.write({
      peerId: state.peerId,
      lease: LEASE,
      selector: controlSelector,
      value: Buffer.from([0x01, 0x00]),
      mode: 'with-response',
      timeoutMs: 10000
    })
    const indication = await pollValue(
      state.central,
      { peerId: state.peerId, selector: controlSelector, consumer: controlConsumer },
      5000,
      'pmd get-settings indication'
    )
    if (indication.length < 4 || indication[0] !== 0xf0 || indication[1] !== 0x01 || indication[3] !== 0x00) {
      fail(`pmd get-settings indication was ${hex(indication)}`)
    }
    await state.central.unsubscribe({
      peerId: state.peerId,
      selector: controlSelector,
      consumer: controlConsumer,
      timeoutMs: 5000
    })
    state.subscriptions.pop()

    const heartConsumer = 'bun-heart-rate'
    const subscribed = await state.central.subscribe({
      peerId: state.peerId,
      lease: LEASE,
      selector: heartSelector,
      consumer: heartConsumer,
      deliveryMode: 'notification',
      timeoutMs: 10000
    })
    state.subscriptions.push({ selector: heartSelector, consumer: heartConsumer })
    const heart = await pollValue(
      state.central,
      { peerId: state.peerId, selector: heartSelector, consumer: heartConsumer },
      5000,
      'heart-rate notification'
    )
    // Stock bpm is 72 as a uint8. RR-interval bytes are optional; the live
    // simulator includes them even when rr jitter is zero.
    const flags = heart[0]
    const bpm = heart[1]
    if (heart.length < 2 || (flags & 0x01) !== 0 || bpm !== 72) {
      fail(`heart-rate notification was ${hex(heart)}, expected uint8 72 bpm`)
    }
    if ((flags & 0x10) !== 0 && (heart.length - 2) % 2 !== 0) {
      fail(`heart-rate notification was ${hex(heart)}, RR flag set with a short payload`)
    }
    await state.central.unsubscribe({
      peerId: state.peerId,
      selector: heartSelector,
      consumer: heartConsumer,
      timeoutMs: 5000
    })
    state.subscriptions.pop()

    await delay(200)
    const eventWakeFailures = state.central.eventWakeFailures()
    if (wakes < 1) fail(`setEventWaker was not invoked (wakes ${wakes})`)
    if (eventWakeFailures !== 0) fail(`event waker recorded ${eventWakeFailures} failures`)

    let released
    let disconnectError = null
    try {
      released = await state.central.disconnect({
        peerId: state.peerId,
        lease: LEASE,
        timeoutMs: 10000
      })
    } catch (error) {
      disconnectError = error && error.message ? error.message : String(error)
    }
    state.peerId = null
    let closed
    let closeError = null
    try {
      closed = await state.central.close()
    } catch (error) {
      closeError = error && error.message ? error.message : String(error)
    }
    state.central = null
    const disconnectState = released && released.state ? released.state : null
    const closeState = closed && closed.state ? closed.state : null
    const ok = disconnectState === 'released' && closeState === 'released'

    clearTimeout(watchdog)
    process.stdout.write(
      `${JSON.stringify({
        ok,
        bun,
        napi: process.versions.napi,
        node: process.versions.node,
        modules: process.versions.modules,
        target: identity.target,
        profile: identity.profile,
        mode: loaded.mode,
        platform,
        adapterId,
        adapters,
        peer: {
          peerId: peer.peerId,
          address: peer.address ?? null,
          localName: peer.localName,
          rssi: peer.rssi ?? null
        },
        ignored,
        connectionGeneration: connection.connectionGeneration ?? null,
        pathsRegistered: discovery.pathsRegistered,
        battery: { level: battery.value[0], provenance: battery.provenance },
        manufacturer: text(manufacturer.value),
        write: {
          characteristic: 'pmd-control-point',
          mode: 'with-response',
          command: '0100',
          indication: hex(indication)
        },
        notification: {
          characteristic: 'heart-rate-measurement',
          delivery: subscribed.delivery,
          flags: heart[0],
          bpm: heart[1],
          hex: hex(heart)
        },
        wakes,
        eventWakeFailures,
        disconnect: disconnectState,
        disconnectError,
        close: closeState,
        closeError,
        scanStop: stopped
      })}\n`
    )
    if (!ok) {
      fail(
        `session evidence is above; disconnect ${disconnectState ?? disconnectError}; close ${closeState ?? closeError}`
      )
    }
  } catch (error) {
    const detail = error && error.message ? error.message : String(error)
    let extra = ''
    if (!releasing) {
      releasing = true
      const cleanupErrors = await cleanup(state)
      if (cleanupErrors.length > 0) extra = `; cleanup: ${cleanupErrors.join('; ')}`
    }
    console.error(`bun-desktop-h10: FAIL ${detail}${extra}`)
    process.exit(1)
  }
}

main().catch(error => {
  void releaseAndExit(error && error.message ? error.message : String(error))
})
