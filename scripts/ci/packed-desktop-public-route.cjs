'use strict'

// A fresh installed public host factory, production desktop provider, sealed
// addon and real Rust dispatch. Only the selected radio is explicitly synthetic.
// This fixture proves runtime integration, never physical-radio qualification.
const assert = require('node:assert/strict')
const HOSTS = Object.freeze({
  linux: {
    platform: 'bluez',
    prefix: 'bluez',
    entry: 'unified-ble-manager/node/bluez',
    factory: 'createBluezBleManager'
  },
  darwin: {
    platform: 'corebluetooth',
    prefix: 'direct-gatt',
    entry: 'unified-ble-manager/node/corebluetooth',
    factory: 'createCoreBluetoothBleManager'
  },
  win32: {
    platform: 'winrt',
    prefix: 'winrt',
    entry: 'unified-ble-manager/node/winrt',
    factory: 'createWinRtBleManager'
  }
})
const SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC = '00002a37-0000-1000-8000-00805f9b34fb'
const PEER = 'packed-native-peer'

function bounded(promise, label) {
  let timer
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label} did not settle`)), 8000)
    })
  ]).finally(() => clearTimeout(timer))
}
async function nextValue(iterator) {
  for (;;) {
    const item = await bounded(iterator.next(), 'native public stream')
    assert.equal(item.done, false, 'stream ended before its required observation')
    if (item.value.kind === 'value') return item.value.value
    assert.notEqual(item.value.kind, 'terminal', 'stream terminal before value')
  }
}
async function nextTerminal(iterator, reason) {
  for (let index = 0; index < 256; index += 1) {
    const item = await bounded(iterator.next(), `${reason} public terminal`)
    assert.equal(item.done, false, 'stream ended without its terminal notice')
    if (item.value.kind === 'terminal') {
      assert.equal(item.value.reason, reason)
      return item.value
    }
  }
  throw new Error(`stream did not report ${reason}`)
}
function target() {
  return {
    peerId: PEER,
    serviceUuid: SERVICE,
    serviceOccurrence: 0,
    characteristicUuid: CHARACTERISTIC,
    characteristicOccurrence: 0
  }
}
function requireReleased(cleanup) {
  assert.equal(cleanup.state, 'released', JSON.stringify(cleanup))
}

async function qualifyPublicRoute({ moduleKind = 'cjs' } = {}) {
  const host = HOSTS[process.platform]
  assert.ok(host, `unsupported qualification host ${process.platform}`)
  const entry = moduleKind === 'esm' ? await import(host.entry) : require(host.entry)
  const binding = await entry.loadDesktopCoreBinding({ platform: host.platform, operationPrefix: host.prefix })
  assert.equal(binding.diagnostics.addonMode, 'prebuilt')
  let stage
  let opens = 0
  const opened = []
  const calls = []
  const injected = {
    diagnostics: binding.diagnostics,
    capabilityStates: binding.capabilityStates,
    openSynthetic: binding.openSynthetic,
    listAdapters: async () => [
      { index: 0, label: 'qualification-native-adapter', error: null, displayName: null, default: true }
    ],
    openProduction: async options => {
      opens += 1
      assert.equal(options.platform, host.platform)
      stage = await binding.openSynthetic(options.owner, { platform: host.platform })
      opened.push(stage)
      if (host.platform === 'corebluetooth') await stage.stageAdapterState('powered-on', true)
      return new Proxy(stage, {
        get(native, name) {
          const method = Reflect.get(native, name)
          if (typeof method !== 'function') return method
          return (...args) => {
            calls.push(String(name))
            return Reflect.apply(method, native, args)
          }
        }
      })
    }
  }
  // This is the installed consumer's real public factory and default provider.
  // Explicit binding injection changes only radio creation inside the native
  // boundary; no deterministic TypeScript manager or fallback is installed.
  const manager = await entry[host.factory]({ binding: injected, owner: `packed-${moduleKind}` })
  let scan, connection, shared, subscription, observations, notifications, parameters
  try {
    // The real provider probes the listed adapter, closes that temporary owner,
    // then opens the selected manager owner. Both traverse the actual addon.
    assert.equal(opens, 2)
    assert.equal((await opened[0].resourceCounters()).liveConnections, 0)
    scan = await manager.scan()
    observations = scan.observations[Symbol.asyncIterator]()
    const pendingPeer = nextValue(observations)
    await stage.stageAdvertisement({ peerId: PEER, localName: 'Packed native', serviceUuids: [SERVICE] })
    const peer = (await pendingPeer).peer
    await observations.return()
    requireReleased(await scan.stop())
    connection = await manager.connect(peer)
    await stage.stageServices(PEER, [
      {
        uuid: SERVICE,
        occurrence: 0,
        primary: false,
        includedServices: [],
        characteristics: [
          {
            uuid: CHARACTERISTIC,
            occurrence: 0,
            properties: { read: true, write: true, writeWithoutResponse: true, notify: true, indicate: false },
            descriptors: []
          }
        ]
      }
    ])
    await stage.stageCharacteristicValue(target(), new Uint8Array([42]))
    let database = await connection.discover()
    assert.equal(database.services[0].primary, false)
    let characteristic = database.characteristic('180d', '2a37')
    assert.deepEqual([...(await characteristic.read())], [42])
    subscription = await characteristic.subscribe()
    notifications = subscription.values[Symbol.asyncIterator]()
    const pendingValue = nextValue(notifications)
    await stage.stageNotification({ ...target(), value: new Uint8Array([7, 8]) })
    assert.deepEqual([...(await pendingValue).value], [7, 8])
    requireReleased(await subscription.remove())

    await stage.blockRadioOp('read')
    const abort = new AbortController()
    const cancelled = characteristic.read({ signal: abort.signal, timeoutMs: 5000 })
    abort.abort()
    await assert.rejects(bounded(cancelled, 'cancelled public read'), { code: 'operation.aborted' })
    await assert.rejects(bounded(characteristic.read({ timeoutMs: 25 }), 'deadline public read'), {
      code: 'operation.timed-out'
    })
    await stage.unblockRadioOp('read')
    assert.deepEqual(
      [...(await characteristic.read({ timeoutMs: 5000 }))],
      [42],
      'late completion must not corrupt the next native operation'
    )

    if (host.platform === 'winrt') {
      await stage.stageConnectionParameters(PEER, 30000, 2, 4000000, false)
      assert.equal((await connection.controls.parameters()).intervalMs, 30)
      parameters = connection.controls.parameterEvents()[Symbol.asyncIterator]()
      assert.equal((await bounded(parameters.next(), 'initial native parameters')).value.intervalMs, 30)
      await stage.stageConnectionParameters(PEER, 60000, 3, 5000000, true)
      assert.equal((await bounded(parameters.next(), 'native parameter callback')).value.intervalMs, 60)
      assert.ok(
        calls.filter(name => name === 'connectionParameters').length >= 2,
        'read and watch must reach real NAPI forwarding'
      )
      await parameters.return()
    }

    // A full public queue must report its native/provider loss and end;
    // late native values may not reopen the terminated consumer.
    subscription = await characteristic.subscribe({
      stream: {
        preset: 'custom',
        budget: {
          itemCapacity: 1,
          byteCapacity: 4096,
          reservedControlCapacity: 1,
          overflowPolicy: 'error'
        }
      }
    })
    notifications = subscription.values[Symbol.asyncIterator]()
    for (let value = 0; value < 64; value += 1) {
      await stage.stageNotification({ ...target(), value: new Uint8Array([value]) })
    }
    const overflow = await nextTerminal(notifications, 'overflow')
    assert.ok(
      overflow.droppedItems > 0 || overflow.error?.code === 'stream.overflow',
      'overflow must retain loss evidence'
    )
    await stage.stageNotification({ ...target(), value: new Uint8Array([99]) })
    assert.equal((await bounded(notifications.next(), 'closed overflowing consumer')).done, true)
    requireReleased(await subscription.remove())

    // Independent logical owners share one native dial. Their children and
    // failed cleanup stay owned by the exact connection which acquired them.
    shared = await manager.connect(peer)
    assert.equal(calls.filter(name => name === 'connect').length, 1)
    const stale = characteristic
    // Logical leases own independent snapshots. Refresh the owner of the
    // handle under test, then obtain the surviving owner's own database.
    await connection.rediscoverGatt({ reason: 'manual' })
    await assert.rejects(stale.read(), { code: 'gatt.stale-handle' }, 'stale database after rediscovery')
    database = await shared.discover()
    characteristic = database.characteristic('180d', '2a37')
    subscription = await characteristic.subscribe()
    await stage.failNextRadioOp('unsubscribe', 'packed native cleanup refusal')
    const refused = await subscription.remove()
    assert.equal(refused.state, 'release-failed')
    assert.ok(refused.failures.length > 0)
    requireReleased(await subscription.remove())
    requireReleased(await connection.release())
    assert.equal(calls.filter(name => name === 'disconnect').length, 0, 'a shared owner must preserve the live link')
    assert.deepEqual([...(await characteristic.read())], [42])
    requireReleased(await shared.release())
    shared = undefined
    assert.equal(calls.filter(name => name === 'disconnect').length, 1)
    await assert.rejects(
      characteristic.read(),
      error => error.code === 'gatt.stale-handle' || error.code === 'connection.stale'
    )

    // Reconnect and discovery create a usable new generation; ending the
    // adapter source settles a native-dispatched read with its actual cause.
    connection = await manager.connect(peer)
    database = await connection.discover()
    characteristic = database.characteristic('180d', '2a37')
    assert.deepEqual([...(await characteristic.read())], [42])
    await stage.blockRadioOp('read')
    const lost = characteristic.read({ timeoutMs: 5000 })
    const lostResult = assert.rejects(bounded(lost, 'adapter-loss public read'), { code: 'operation.reset' })
    await bounded(
      (async () => {
        const admissionDeadline = Date.now() + 4000
        while ((await stage.resourceCounters()).nativeGattAdmissions === 0) {
          assert.ok(Date.now() < admissionDeadline, 'adapter-loss read never entered native GATT admission')
          await new Promise(resolve => setTimeout(resolve, 1))
        }
      })(),
      'adapter-loss native read admission'
    )
    await stage.stageAdapterState('powered-off', true)
    await lostResult
    await stage.unblockRadioOp('read')
    for (const method of ['createTicket', 'startScan', 'connect', 'discover', 'read', 'subscribe', 'unsubscribe']) {
      assert.ok(calls.includes(method), `public provider never reached native ${method}`)
    }
    requireReleased(await connection.release())
    assert.ok(calls.includes('disconnect'))
    requireReleased(await manager.destroy())
    assert.ok(calls.includes('close'))
    for (const native of opened) {
      const nativeResources = await native.resourceCounters()
      for (const field of [
        'nativeGattAdmissions',
        'acquiredGattTransports',
        'pendingGattAcquisitions',
        'routedSubscriptions',
        'pendingDisables',
        'retainedEnablements',
        'liveOperations',
        'liveConnections',
        'liveConsumers'
      ]) {
        assert.equal(nativeResources[field], 0, `native resource remains after destroy: ${field}`)
      }
      assert.equal(nativeResources.scanOwned, false)
    }
    return {
      platform: host.platform,
      moduleKind,
      evidence: 'packed-public-provider-native-synthetic',
      nativeCalls: calls.length,
      scenarios: [
        'scan-connect-discover-read-notify',
        'cancel-deadline-late-completion',
        'overflow-late-events',
        'generation-invalidation-reconnect',
        'two-owner-cleanup-retry',
        'adapter-loss-native-zero-counters'
      ]
    }
  } finally {
    if (stage) await stage.unblockRadioOp('read')
    await parameters?.return?.()
    await notifications?.return?.()
    await observations?.return?.()
    if (subscription) requireReleased(await subscription.remove())
    if (shared) requireReleased(await shared.release())
    if (connection) requireReleased(await connection.release())
    if (scan) requireReleased(await scan.stop())
    requireReleased(await manager.destroy())
  }
}

module.exports = { qualifyPublicRoute }
