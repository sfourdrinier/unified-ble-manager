// __tests__/backends/desktop/desktop-ordered-real.test.js
// This suite exercises synthetic reclaim, which is deliberately excluded from
// the normal addon. An explicit UBM_NAPI_ADDON still selects a custom fixture.
process.env.UBM_NAPI_ADDON ??= require('node:path').resolve(__dirname, '../../../target/test-addons/reclaim/ubm.node')
const { openBackend, connectAndDiscover, hrmServices, subscribeOptions, HRM_SERVICE, HRM_MEASUREMENT } = require('../../helpers/desktop-rust-core-harness')

jest.setTimeout(30000)
beforeAll(async () => {
  const { backend, stage } = await openBackend('corebluetooth')
  try {
    if (typeof stage.orderedRecordingReclaimSupported !== 'function' ||
        stage.orderedRecordingReclaimSupported() !== true ||
        typeof stage.orderedRecordingReclaimWireVersion !== 'function' ||
        stage.orderedRecordingReclaimWireVersion() !== 2) {
      throw new Error('desktop ordered tests require a synthetic reclaim addon: node scripts/ci/build-napi-addon.js --reclaim-test-support --out target/test-addons/reclaim/ubm.node')
    }
  } finally {
    await backend.destroy()
  }
})
test('desktop adapter carries one native ordered FIFO through synthetic N-API', async () => {
  const { backend, stage } = await openBackend('corebluetooth')
  try {
    const services = hrmServices()
    services[0].characteristics.push({ ...services[0].characteristics[0], occurrence: 1 })
    const { database, snapshot } = await connectAndDiscover(backend, stage, services)
    const paths = snapshot.characteristics.filter(item => item.path.characteristicUuid === HRM_MEASUREMENT).map(item => item.path)
    const setup = await database.subscribeOrdered(paths.map((path, index) => ({ memberId: String.fromCharCode(97 + index), path, deliveryMode: 'prefer-notification' })), subscribeOptions())
    expect(setup.state).toBe('ready')
    expect(setup.sourceGeneration).toMatchObject({ kind: 'counter', value: expect.any(String) })
    for (const [occurrence, byte] of [[0, 1], [1, 2], [0, 3]]) {
      await stage.stageNotification({ peerId: 'peer-1', serviceUuid: HRM_SERVICE, serviceOccurrence: 0, characteristicUuid: HRM_MEASUREMENT, characteristicOccurrence: occurrence, value: Buffer.from([byte]) })
    }
    const iterator = setup.stream[Symbol.asyncIterator]()
    const values = [(await iterator.next()).value, (await iterator.next()).value, (await iterator.next()).value]
    expect(values.every(item => item.sourceGeneration.value === setup.sourceGeneration.value)).toBe(true)
    expect(values.every(item => typeof item.sourceMembership.member === 'string' && typeof item.sourceMembership.epoch === 'string')).toBe(true)
    expect(values.map(item => [item.memberId, [...item.value]])).toEqual([['a', [1]], ['b', [2]], ['a', [3]]])
    expect(await setup.stream.close()).toMatchObject({ group: { state: 'released' } })
    expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'closed' })
  } finally {
    await backend.destroy()
  }
})

test('desktop public ordered stream preserves repeated failure metadata and cleanup in order', async () => {
  const { backend, stage, harness } = await openBackend('corebluetooth')
  try {
    const { database, measurement } = await connectAndDiscover(backend, stage)
    const setup = await database.subscribeOrdered([{ memberId: 'a', path: measurement.path, deliveryMode: 'prefer-notification' }], subscribeOptions())
    expect(setup.state).toBe('ready')
    const open = harness.calls.findLast(([name]) => name === 'openOrderedGroup')
    const collectionId = open[1][0].collectionId
    for (const label of ['first', 'first', 'second']) stage.stageOrderedFailureObservation('peer-1', collectionId, label)
    const iterator = setup.stream[Symbol.asyncIterator]()
    const observations = [(await iterator.next()).value, (await iterator.next()).value, (await iterator.next()).value]
    expect(observations.map(item => item.kind)).toEqual(['failure-observation', 'failure-observation', 'failure-observation'])
    expect(observations.map(item => item.failure.orderedFailure.platformMetadataEntries[1].value.value)).toEqual(['first', 'first', 'second'])
    expect(observations[0].failure.orderedFailure.platformCleanup[0].code).toBe('first')
    expect(await setup.stream.close()).toMatchObject({ group: { state: 'released' } })
    expect((await iterator.next()).value.kind).toBe('terminal')
  } finally { await backend.destroy() }
})

test('public GATT database exposes the ordered native stream', async () => {
  const { backend, stage, provider } = await openBackend('corebluetooth')
  const { attachBleBackend, createBleManager, createManagerOwnershipAuthority, DEFAULT_BLE_MANAGER_OPTIONS } = require('../../../src/manager/ble-manager')
  const { opaqueId } = require('../../../src/backend-contract/primitives')
  const { observePeer } = require('../../helpers/desktop-rust-core-harness')
  const attached = await attachBleBackend(backend, provider.descriptor.compatibility)
  const manager = await createBleManager({ attachedBackend: attached, clientId: opaqueId('ordered-client', 'client', 'test'), managerId: opaqueId('ordered-manager', 'manager', 'test'), ownerMode: 'owning' }, createManagerOwnershipAuthority(attached), DEFAULT_BLE_MANAGER_OPTIONS)
  try {
    const peer = await observePeer(backend, stage)
    const services = hrmServices()
    services[0].characteristics.push({ ...services[0].characteristics[0], occurrence: 1 })
    services[0].characteristics[0].properties.write = true
    await stage.stageServices('peer-1', services)
    stage.stageGattSnapshotIdentity('peer-1', 'synthetic-owner', '1', '1')
    const connection = await manager.connect(peer, { signal: null, deadline: null })
    const database = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
    const chars = database.service(HRM_SERVICE).characteristicsByUuid(HRM_MEASUREMENT)
    const beforeRecording = await stage.stagedRadioCalls()
    expect(() => database.reserveOrderedRecordingStart()).toThrow('capability.unsupported')
    expect(await stage.stagedRadioCalls()).toEqual(beforeRecording)
    const setup = await database.subscribeOrdered(chars.map((characteristic, index) => ({ memberId: String.fromCharCode(97 + index), characteristic })))
    expect(setup.state).toBe('ready')
    for (const [occurrence, byte] of [[0, 1], [1, 2], [0, 3]]) await stage.stageNotification({ peerId: 'peer-1', serviceUuid: HRM_SERVICE, serviceOccurrence: 0, characteristicUuid: HRM_MEASUREMENT, characteristicOccurrence: occurrence, value: Buffer.from([byte]) })
    const iterator = setup.stream[Symbol.asyncIterator]()
    const records = [(await iterator.next()).value, (await iterator.next()).value, (await iterator.next()).value]
    expect(records.map(item => [item.memberId, [...item.value]])).toEqual([['a', [1]], ['b', [2]], ['a', [3]]])
    const cancelled = setup.stream.reserveMarkerWrite()
    expect((await cancelled.cancel()).state).toBe('not-admitted')
    await cancelled.forget()
    const marker = setup.stream.reserveMarkerWrite()
    let markerStatus = await marker.start(chars[0], new Uint8Array([42]), 'with-response')
    for (let i = 0; i < 200 && markerStatus.state === 'pending'; i++) {
      await new Promise(resolve => setTimeout(resolve, 5))
      markerStatus = await marker.status()
    }
    expect(markerStatus.state).toBe('admitted')
    await expect(marker.forget()).rejects.toThrow()
    const markerItem = (await iterator.next()).value
    expect(markerItem).toMatchObject({ kind: 'marker', markerId: '0', ordinal: markerStatus.ordinal })
    expect(await marker.start(chars[0], new Uint8Array([42]), 'with-response')).toEqual(markerStatus)
    stage.stageSettleMarkerWrite(markerItem.collectionId, markerItem.markerId)
    expect((await marker.status()).write.state).toBe('settled')
    await marker.forget()
    await marker.forget()
    await stage.blockRadioOp('ordered-marker-before-admission')
    const pendingMarker = setup.stream.reserveMarkerWrite()
    const pending = await pendingMarker.start(chars[0], new Uint8Array([43]), 'with-response')
    expect(pending.state).toBe('pending')
    let canceled = await pendingMarker.cancel()
    await stage.unblockRadioOp('ordered-marker-before-admission')
    for (let i = 0; i < 200 && canceled.state === 'pending'; i++) {
      await new Promise(resolve => setTimeout(resolve, 5))
      canceled = await pendingMarker.status()
    }
    expect(canceled.state).toBe('not-admitted')
    expect((await pendingMarker.cancel()).state).toBe('not-admitted')
    await pendingMarker.forget()
    expect(await setup.stream.close()).toMatchObject({ group: { state: 'released' } })
    expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'closed' })
    await connection.release()
  } finally { await manager.destroy() }
})

test('desktop setup failure returns a retry owner and retained terminal', async () => {
  const { backend, stage } = await openBackend('corebluetooth')
  try {
    const { database, measurement } = await connectAndDiscover(backend, stage)
    await stage.failNextRadioOp('subscribe', 'synthetic enable refusal')
    const setup = await database.subscribeOrdered([{ memberId: 'a', path: measurement.path, deliveryMode: 'prefer-notification' }], subscribeOptions())
    expect(setup.state).toBe('failed')
    expect(setup.error.code).toBe('gatt.subscribe-failed')
    expect(setup.members).toHaveLength(1)
    const iterator = setup.stream[Symbol.asyncIterator]()
    const terminal = await iterator.next()
    expect(terminal.value).toMatchObject({ kind: 'terminal' })
    expect(await setup.close()).toMatchObject({ group: { state: 'released' } })
  } finally { await backend.destroy() }
})

test('aborted ordered setup retains its cleanup owner', async () => {
  const { backend, stage } = await openBackend('corebluetooth')
  try {
    const { database, measurement } = await connectAndDiscover(backend, stage)
    await stage.blockRadioOp('subscribe')
    const controller = new AbortController()
    const pending = database.subscribeOrdered([{ memberId: 'a', path: measurement.path, deliveryMode: 'prefer-notification' }], subscribeOptions({ signal: controller.signal }))
    const deadline = Date.now() + 5000
    while (!(await stage.stagedRadioCalls()).includes('set_notifications')) {
      if (Date.now() >= deadline) throw new Error('notification enable did not start')
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    await stage.stageNotification({ peerId: 'peer-1', serviceUuid: HRM_SERVICE, serviceOccurrence: 0, characteristicUuid: HRM_MEASUREMENT, characteristicOccurrence: 0, value: Buffer.from([9]) })
    controller.abort()
    await stage.unblockRadioOp('subscribe')
    const setup = await pending
    expect(setup.state).toBe('failed')
    expect(setup.error.code).toBe('operation.aborted')
    expect(typeof setup.close).toBe('function')
    const items = [...setup.observations]
    for await (const item of setup.stream) items.push(item)
    expect(items.filter(item => item.kind === 'value').map(item => [...item.value])).toEqual([[9]])
    expect(items.at(-1)).toMatchObject({ kind: 'terminal' })
    expect(await setup.close()).toMatchObject({ group: { state: 'released' } })
  } finally { await stage.unblockRadioOp('subscribe'); await backend.destroy() }
})

test('discard reports unread native notifications after source seal', async () => {
  const { backend, stage } = await openBackend('corebluetooth')
  try {
    const { database, measurement } = await connectAndDiscover(backend, stage)
    const setup = await database.subscribeOrdered([{ memberId: 'a', path: measurement.path, deliveryMode: 'prefer-notification' }], subscribeOptions())
    expect(setup.state).toBe('ready')
    for (const byte of [1, 2]) await stage.stageNotification({ peerId: 'peer-1', serviceUuid: HRM_SERVICE, serviceOccurrence: 0, characteristicUuid: HRM_MEASUREMENT, characteristicOccurrence: 0, value: Buffer.from([byte]) })
    let receipt = await setup.stream.discard()
    for (let attempt = 0; attempt < 100 && receipt.group.state !== 'released'; attempt += 1) {
      await new Promise(resolve => setTimeout(resolve, 5))
      receipt = await setup.stream.close()
    }
    expect(receipt).toMatchObject({ group: { state: 'released' }, discarded: { values: '2', bytes: '2', controls: '1', terminal: { kind: 'terminal', reason: 'closed' } } })
    expect(await setup.stream.close()).toEqual(receipt)
  } finally { await backend.destroy() }
})

test('native retained malformed delivery is explicitly discarded with exact evidence', async () => {
  const { backend, stage } = await openBackend('corebluetooth')
  try {
    const { database, measurement } = await connectAndDiscover(backend, stage)
    const setup = await database.subscribeOrdered([{ memberId: 'a', path: measurement.path, deliveryMode: 'prefer-notification' }], subscribeOptions())
    expect(setup.state).toBe('ready')
    const original = backend.central.pollOrderedGroup.bind(backend.central)
    backend.central.pollOrderedGroup = async options => {
      const result = await original(options)
      if (result.deliveryId != null && JSON.parse(result.record).kind === 'value') return { ...result, record: '{malformed' }
      return result
    }
    await stage.stageNotification({ peerId: 'peer-1', serviceUuid: HRM_SERVICE, serviceOccurrence: 0, characteristicUuid: HRM_MEASUREMENT, characteristicOccurrence: 0, value: Buffer.from([1, 2, 3]) })
    const iterator = setup.stream[Symbol.asyncIterator]()
    await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
    let receipt = await setup.stream.discard()
    for (let attempt = 0; attempt < 100 && receipt.group.state !== 'released'; attempt += 1) {
      await new Promise(resolve => setTimeout(resolve, 5))
      receipt = await setup.stream.discard()
    }
    expect(receipt).toMatchObject({ group: { state: 'released' }, discarded: { values: '1', bytes: '3', controls: '1', terminal: { reason: 'closed' }, trustedValues: [{ memberId: 'a', ordinal: expect.any(String), sourceMembership: { member: expect.any(String), epoch: expect.any(String) }, bytes: '3' }] } })
  } finally { await backend.destroy() }
})

test('discard retains malformed native loss control evidence', async () => {
  const { backend, stage } = await openBackend('corebluetooth')
  try {
    const { database, measurement } = await connectAndDiscover(backend, stage)
    const setup = await database.subscribeOrdered([{ memberId: 'a', path: measurement.path, deliveryMode: 'prefer-notification' }], subscribeOptions())
    expect(setup.state).toBe('ready')
    const original = backend.central.pollOrderedGroup.bind(backend.central)
    backend.central.pollOrderedGroup = async options => {
      const result = await original(options)
      if (result.deliveryId != null && JSON.parse(result.record).kind === 'loss') return { ...result, record: '{malformed' }
      return result
    }
    stage.stageOrderedSourceLoss('peer-1', '7')
    const iterator = setup.stream[Symbol.asyncIterator]()
    await expect(iterator.next()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
    let receipt = await setup.stream.discard()
    for (let attempt = 0; attempt < 100 && receipt.group.state !== 'released'; attempt += 1) {
      await new Promise(resolve => setTimeout(resolve, 5))
      receipt = await setup.stream.discard()
    }
    expect(receipt).toMatchObject({ group: { state: 'released' }, discarded: { values: '0', controls: '2', losses: [{ reason: 'source-loss', lostSourceRecords: '7' }], terminal: { reason: 'closed' } } })
  } finally { await backend.destroy() }
})

test('discard retains malformed native terminal evidence', async () => {
  const { backend, stage } = await openBackend('corebluetooth')
  try {
    const { database, measurement } = await connectAndDiscover(backend, stage)
    const setup = await database.subscribeOrdered([{ memberId: 'a', path: measurement.path, deliveryMode: 'prefer-notification' }], subscribeOptions())
    expect(setup.state).toBe('ready')
    const original = backend.central.pollOrderedGroup.bind(backend.central)
    backend.central.pollOrderedGroup = async options => {
      const result = await original(options)
      if (result.deliveryId != null && JSON.parse(result.record).kind === 'terminal') return { ...result, record: '{malformed' }
      return result
    }
    let receipt = await setup.stream.discard()
    for (let attempt = 0; attempt < 100 && receipt.group.state !== 'released'; attempt += 1) {
      await new Promise(resolve => setTimeout(resolve, 5))
      receipt = await setup.stream.discard()
    }
    expect(receipt).toMatchObject({ group: { state: 'released' }, discarded: { values: '0', controls: '1', terminal: { kind: 'terminal', reason: 'closed' } } })
  } finally { await backend.destroy() }
})

test('stale native ordered wire is refused before group reservation or radio work', async () => {
  const { backend, stage, harness } = await openBackend('corebluetooth')
  try {
    const { database, measurement } = await connectAndDiscover(backend, stage)
    const callsBefore = await stage.stagedRadioCalls()
    const callsStart = harness.calls.length
    backend.central.orderedGroupWireVersion = () => 4
    await expect(database.subscribeOrdered([{ memberId: 'a', path: measurement.path, deliveryMode: 'prefer-notification' }], subscribeOptions())).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
    expect(harness.calls.slice(callsStart).some(([name]) => name === 'reserveOrderedGroup')).toBe(false)
    expect(await stage.stagedRadioCalls()).toEqual(callsBefore)
  } finally { await backend.destroy() }
})

test('legacy metadata offer is refused before group reservation or radio work', async () => {
  const { backend, stage, harness } = await openBackend('corebluetooth')
  try {
    const { database, measurement } = await connectAndDiscover(backend, stage)
    const callsBefore = await stage.stagedRadioCalls()
    const callsStart = harness.calls.length
    backend.central.orderedMetadataVersion = () => 1
    await expect(database.subscribeOrdered([{ memberId: 'a', path: measurement.path, deliveryMode: 'prefer-notification' }], subscribeOptions())).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
    expect(harness.calls.slice(callsStart).some(([name]) => name === 'reserveOrderedGroup')).toBe(false)
    expect(await stage.stagedRadioCalls()).toEqual(callsBefore)
  } finally { await backend.destroy() }
})

test('public GATT starts a supervised native ordered recording with replayable receipt', async () => {
  const fs = require('node:fs')
  const os = require('node:os')
  const path = require('node:path')
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-public-recording-'))
  const { backend, stage, provider } = await openBackend('corebluetooth')
  const { observePeer } = require('../../helpers/desktop-rust-core-harness')
  const host = await require('../../../src/desktop-process-host').createDesktopProcessHostFromBackend(backend, { now: () => performance.now() })
  const manager = await host.createInternalManager()
  try {
    expect(JSON.parse(await stage.continuationConfigureRecordingDirectory(directory)).ok).toBe(true)
    const peer = await observePeer(backend, stage)
    const services = hrmServices()
    services[0].characteristics[0].properties.write = true
    await stage.stageServices('peer-1', services)
    stage.stageGattSnapshotIdentity('peer-1', 'synthetic-recording-owner', '1', '1')
    const connection = await manager.connect(peer, { signal: null, deadline: null })
    const database = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
    const characteristic = database.service(HRM_SERVICE).characteristicsByUuid(HRM_MEASUREMENT)[0]
    const handle = database.reserveOrderedRecordingStart()
    const members = [{ memberId: 'heart', characteristic }]
    const options = { recording: { id: 'public-recording', maxBytes: 1048576, maxRecords: 1 } }
    const first = await handle.start(members, options)
    expect(first).toMatchObject({ kind: 'started', receipt: { state: 'active' } })
    expect(await handle.start(members, options)).toEqual(first)
    expect(await handle.status()).toEqual({ state: 'result', result: first })
    const foreignManager = await host.createInternalManager()
    const foreignConnection = await foreignManager.connect(peer, { signal: null, deadline: null })
    const foreignDatabase = await require('../../../src/public/gatt').createPublicGattDatabase(await foreignConnection.discover({ signal: null, deadline: null }))
    expect(() => foreignDatabase.reserveOrderedRecordingMarker('public-recording')).toThrow('ownership.denied')
    await foreignConnection.release()
    await foreignManager.destroy()
    const controller = require('../../../src/core/continuation-recording').createNativeContinuationRecordingController(stage.continuationRecordingStore(), 'ubm-desktop')
    let page
    const recorderMarker = handle.reserveMarkerWrite()
    let markerStatus
    try {
      markerStatus = await recorderMarker.start(characteristic, new Uint8Array([7]), 'with-response')
    } catch (error) {
      throw new Error(`recorder marker after foreign release: ${JSON.stringify({ error, calls: await stage.stagedRadioCalls() })}`, { cause: error })
    }
    for (let i = 0; i < 200 && markerStatus.state === 'pending'; i++) {
      await new Promise(resolve => setTimeout(resolve, 5))
      markerStatus = await recorderMarker.status()
    }
    if (markerStatus.state !== 'admitted') throw new Error(`recorder marker: ${JSON.stringify(markerStatus)}`)
    for (let i = 0; i < 200; i++) {
      page = await controller.prepareOrdered('public-recording', { maxItems: 16, maxBytes: 4096 })
      if (page.records.length) break
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    expect(page.records[0].record).toMatchObject({ kind: 'marker', markerId: '0' })
    await controller.acknowledge('public-recording', page.token)
    stage.stageSettleMarkerWrite(first.receipt.collectionId, '0')
    expect((await recorderMarker.status()).write.state).toBe('settled')
    await recorderMarker.forget()
    await stage.stageNotification({ peerId: 'peer-1', serviceUuid: HRM_SERVICE, serviceOccurrence: 0, characteristicUuid: HRM_MEASUREMENT, characteristicOccurrence: 0, value: Buffer.from([42]) })
    await stage.stageNotification({ peerId: 'peer-1', serviceUuid: HRM_SERVICE, serviceOccurrence: 0, characteristicUuid: HRM_MEASUREMENT, characteristicOccurrence: 0, value: Buffer.from([43]) })
    for (let i = 0; i < 200; i++) {
      page = await controller.prepareOrdered('public-recording', { maxItems: 16, maxBytes: 4096 })
      if (page.records.length) break
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    expect(page.records[0].record).toMatchObject({ kind: 'value', member: '0' })
    await controller.acknowledge('public-recording', page.token)
    for (let i = 0; i < 200; i++) {
      page = await controller.prepareOrdered('public-recording', { maxItems: 16, maxBytes: 4096 })
      if (page.records.length) break
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    expect(page.records[0].record).toMatchObject({ kind: 'value', member: '0' })
    await controller.acknowledge('public-recording', page.token)
    expect((await controller.contextOrdered('public-recording')).members[0].publicMemberId).toBe('heart')
    const supervised = await host.continuation.recordings(directory)
    let stop
    for (let i = 0; i < 200; i++) {
      stop = await supervised.stopOrdered('public-recording')
      if (stop.recording.state === 'finished') break
      page = await controller.prepareOrdered('public-recording', { maxItems: 16, maxBytes: 4096 })
      if (page.token) await controller.acknowledge('public-recording', page.token)
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    expect(stop.recording.state).toBe('finished')
    expect(stop.hostCleanup.state).toBe('released')
    expect(await handle.start(members, options)).toEqual(first)
    expect(() => handle.recoveryHandle()).toThrow('ownership.denied')
    await handle.retire()
    await handle.retire()
    expect(await handle.status()).toEqual({ state: 'retired' })
    const failedHandle = database.reserveOrderedRecordingStart()
    await stage.failNextRadioOp('subscribe', 'synthetic enable refusal')
    const failed = await failedHandle.start(members, { recording: { id: 'setup-failed', maxBytes: 1048576, maxRecords: 8 } })
    expect(failed).toMatchObject({ kind: 'started', receipt: { state: 'setup-failed' } })
    expect(await failedHandle.status()).toEqual({ state: 'result', result: failed })
    for (let i = 0; i < 200; i++) {
      const cleanup = await controller.stopOrdered('setup-failed')
      const tail = await controller.prepareOrdered('setup-failed', { maxItems: 16, maxBytes: 4096 })
      if (tail.token) await controller.acknowledge('setup-failed', tail.token)
      if (cleanup.recording.state === 'finished') break
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    await failedHandle.retire()
    const canceledHandle = database.reserveOrderedRecordingStart()
    const priorCalls = (await stage.stagedRadioCalls()).filter(call => call === 'set_notifications').length
    await stage.blockRadioOp('subscribe')
    const abort = new AbortController()
    const pending = canceledHandle.start(members, { recording: { id: 'cancel-start', maxBytes: 1048576, maxRecords: 8 }, signal: abort.signal })
    const cutoff = Date.now() + 5000
    while ((await stage.stagedRadioCalls()).filter(call => call === 'set_notifications').length <= priorCalls) {
      if (Date.now() >= cutoff) throw new Error('ordered recording enable did not start')
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    abort.abort()
    await stage.unblockRadioOp('subscribe')
    await pending.catch(() => undefined)
    let recovered
    for (let i = 0; i < 200; i++) {
      recovered = await canceledHandle.status()
      if (recovered.state !== 'pending') break
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    expect(recovered.state).toBe('result')
    expect(recovered.result.kind).toBe('started')
    for (let i = 0; i < 200; i++) {
      const cleanup = await controller.stopOrdered('cancel-start')
      const tail = await controller.prepareOrdered('cancel-start', { maxItems: 16, maxBytes: 4096 })
      if (tail.token) await controller.acknowledge('cancel-start', tail.token)
      if (cleanup.recording.state === 'finished') break
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    await canceledHandle.retire()
    await connection.release()
  } finally { await manager.destroy(); await host.destroy(); fs.rmSync(directory, { recursive: true, force: true }) }
})

test('ordered recorder survives originating manager destruction and reports retryable lease release failure', async () => {
  const fs = require('node:fs')
  const os = require('node:os')
  const path = require('node:path')
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-recording-owner-'))
  const { backend, stage, provider, harness } = await openBackend('corebluetooth')
  const { observePeer } = require('../../helpers/desktop-rust-core-harness')
  const host = await require('../../../src/desktop-process-host').createDesktopProcessHostFromBackend(backend, { now: () => performance.now() })
  const manager = await host.createInternalManager()
  try {
    expect(JSON.parse(await stage.continuationConfigureRecordingDirectory(directory)).ok).toBe(true)
    const peer = await observePeer(backend, stage)
    const services = hrmServices()
    services[0].characteristics[0].properties.write = true
    await stage.stageServices('peer-1', services)
    stage.stageGattSnapshotIdentity('peer-1', 'replacement-owner', '1', '1')
    const connection = await manager.connect(peer, { signal: null, deadline: null })
    const database = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
    const characteristic = database.service(HRM_SERVICE).characteristicsByUuid(HRM_MEASUREMENT)[0]
    const start = database.reserveOrderedRecordingStart()
    const startReceipt = await start.start([{ memberId:'heart', characteristic }], { recording:{ id:'lease-test', maxBytes:1048576, maxRecords:8 } })
    expect(startReceipt.kind).toBe('started')
    const handoff = start.recoveryHandle()
    const beforeOldReclaim = await stage.stagedRadioCalls()
    await expect(stage.orderedRecordingReclaim('next', { wireVersion: 1, recordingId: 'lease-test', capability: 'invalid', lease: 'invalid', callerKey: 'old' })).rejects.toThrow('protocol.incompatible')
    expect(await stage.stagedRadioCalls()).toEqual(beforeOldReclaim)
    await start.retire()
    await manager.destroy()
    const replacement = await host.createInternalManager()
    const replacementConnection = await replacement.connect(peer, { signal: null, deadline: null })
    const replacementDatabase = await require('../../../src/public/gatt').createPublicGattDatabase(await replacementConnection.discover({ signal: null, deadline: null }))
    expect(() => replacementDatabase.reserveOrderedRecordingMarker('lease-test')).toThrow('ownership.denied')
    expect(() => replacementDatabase.reserveOrderedRecordingReclaim({ kind: 'ordered-recording-recovery' })).toThrow('ownership.denied')
    const reclaim = replacementDatabase.reserveOrderedRecordingReclaim(handoff)
    harness.dropReclaimResponse = 'next'
    await expect(reclaim.start()).rejects.toThrow()
    harness.dropReclaimResponse = 'reserve'
    await expect(reclaim.start()).rejects.toThrow()
    harness.dropReclaimResponse = 'start'
    await expect(reclaim.start()).rejects.toThrow()
    let reclaimStatus = await reclaim.start()
    for (let i=0;i<200 && reclaimStatus.state === 'pending';i++) { await new Promise(resolve=>setTimeout(resolve,5)); reclaimStatus = await reclaim.status() }
    if (reclaimStatus.state !== 'ready') throw new Error(`reclaim did not become ready: ${JSON.stringify(reclaimStatus)}`)
    harness.dropReclaimResponse = 'commit'
    await expect(reclaim.commit()).rejects.toThrow()
    expect((await reclaim.status()).mappingCommitted).toBe(true)
    harness.dropReclaimResponse = 'release'
    await expect(reclaim.release()).rejects.toThrow()
    reclaimStatus = await reclaim.status()
    for (let i=0;i<200 && reclaimStatus.state === 'releasing';i++) { await new Promise(resolve=>setTimeout(resolve,5)); reclaimStatus = await reclaim.status() }
    expect(reclaimStatus.state).toBe('released')
    harness.dropReclaimResponse = 'forget-retirement'
    await expect(reclaim.retire()).rejects.toThrow()
    expect((await reclaim.status()).state).toBe('retiring')
    expect(() => replacementDatabase.reserveOrderedRecordingReclaim(handoff)).toThrow('lifecycle.invalid-state')
    await reclaim.retire()
    const replacementCharacteristic = replacementDatabase.service(HRM_SERVICE).characteristicsByUuid(HRM_MEASUREMENT)[0]
    const recoveredMarker = replacementDatabase.reserveOrderedRecordingMarker('lease-test')
    let recoveredMarkerStatus = await recoveredMarker.start(replacementCharacteristic, new Uint8Array([9]), 'with-response')
    for (let i=0;i<200 && recoveredMarkerStatus.state === 'pending';i++) { await new Promise(resolve=>setTimeout(resolve,5)); recoveredMarkerStatus = await recoveredMarker.status() }
    expect(recoveredMarkerStatus).toMatchObject({state:'admitted'})
    const otherPeer = await observePeer(backend, stage, { peerId: 'other-peer' })
    await stage.stageServices('other-peer', services)
    stage.stageGattSnapshotIdentity('other-peer', 'other-owner', '2', '1')
    const otherConnection = await replacement.connect(otherPeer, { signal: null, deadline: null })
    const otherDatabase = await require('../../../src/public/gatt').createPublicGattDatabase(await otherConnection.discover({ signal: null, deadline: null }))
    const beforeWrongPeer = await stage.stagedRadioCalls()
    expect(() => otherDatabase.reserveOrderedRecordingReclaim(handoff)).toThrow('ownership.denied')
    expect(await stage.stagedRadioCalls()).toEqual(beforeWrongPeer)
    await otherConnection.release()
    await stage.stageNotification({peerId:'peer-1',serviceUuid:HRM_SERVICE,serviceOccurrence:0,characteristicUuid:HRM_MEASUREMENT,characteristicOccurrence:0,value:Buffer.from([88])})
    const recordings = require('../../../src/core/continuation-recording').createNativeContinuationRecordingController(stage.continuationRecordingStore(), 'ubm-desktop')
    let page
    for (let i=0;i<200;i++) { page=await recordings.prepareOrdered('lease-test',{maxItems:16,maxBytes:4096}); if(page.records.length) break; await new Promise(resolve=>setTimeout(resolve,5)) }
    if (!page.records.length) throw new Error(`missing after destroy: ${JSON.stringify({page,status:await recordings.status('lease-test'),calls:await stage.stagedRadioCalls()})}`)
    expect(page.records[0].record).toMatchObject({kind:'marker',markerId:'0'})
    let observedValue = page.records.some(item => item.record.kind === 'value' && item.record.member === '0')
    await recordings.acknowledge('lease-test', page.token)
    stage.stageSettleMarkerWrite(startReceipt.receipt.collectionId, '0')
    expect((await recoveredMarker.status()).write.state).toBe('settled')
    await recoveredMarker.forget()
    for (let i=0;i<200 && !observedValue;i++) {
      page=await recordings.prepareOrdered('lease-test',{maxItems:16,maxBytes:4096})
      observedValue = page.records.some(item => item.record.kind === 'value' && item.record.member === '0')
      if(page.token) await recordings.acknowledge('lease-test', page.token)
      if(!observedValue) await new Promise(resolve=>setTimeout(resolve,5))
    }
    expect(observedValue).toBe(true)
    const finalReclaim = replacementDatabase.reserveOrderedRecordingReclaim(handoff)
    let finalStatus = await finalReclaim.start()
    for (let i=0;i<200 && finalStatus.state === 'pending';i++) { await new Promise(resolve=>setTimeout(resolve,5)); finalStatus = await finalReclaim.status() }
    expect(finalStatus.state).toBe('ready')
    expect((await finalReclaim.commit()).mappingCommitted).toBe(true)
    finalStatus = await finalReclaim.release()
    for (let i=0;i<200 && finalStatus.state === 'releasing';i++) { await new Promise(resolve=>setTimeout(resolve,5)); finalStatus = await finalReclaim.status() }
    expect(finalStatus.state).toBe('released')
    harness.dropReclaimResponse = 'forget-retirement'
    await expect(finalReclaim.retire()).rejects.toThrow()
    expect((await host.destroy()).state).toBe('release-failed')
    await replacementConnection.release()
    await stage.failNextRadioOp('disconnect', 'synthetic recorder lease release failure')
    let result
    for (let i=0;i<200;i++) {
      result = await recordings.stopOrdered('lease-test')
      if (result.hostCleanup.state === 'release-failed') break
      page = await recordings.prepareOrdered('lease-test',{maxItems:16,maxBytes:4096})
      if (page.token) await recordings.acknowledge('lease-test',page.token)
      await new Promise(resolve=>setTimeout(resolve,5))
    }
    expect(result.hostCleanup.state).toBe('release-failed')
    for (let i=0;i<200;i++) {
      result = await recordings.stopOrdered('lease-test')
      if (result.recording.state === 'finished' && result.hostCleanup.state === 'released') break
      await new Promise(resolve=>setTimeout(resolve,5))
    }
    expect(result).toMatchObject({recording:{state:'finished'},hostCleanup:{state:'released'}})
    await finalReclaim.retire()
    await start.retire()
    await replacement.destroy()
    expect((await host.destroy()).state).toBe('released')
  } finally { await manager.destroy(); const cleanup = await host.destroy(); if (cleanup.state === 'released') fs.rmSync(directory,{recursive:true,force:true,maxRetries:5,retryDelay:20}) }
})

test('process shutdown fences an in-flight recorder start and retains its receipt', async () => {
  const fs = require('node:fs')
  const os = require('node:os')
  const path = require('node:path')
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-start-shutdown-'))
  const { backend, stage } = await openBackend('corebluetooth')
  const { observePeer } = require('../../helpers/desktop-rust-core-harness')
  const host = await require('../../../src/desktop-process-host').createDesktopProcessHostFromBackend(backend, { now: () => performance.now() })
  const manager = await host.createInternalManager()
  try {
    const recordings = await host.continuation.recordings(directory)
    const peer = await observePeer(backend, stage)
    await stage.stageServices('peer-1', hrmServices())
    const connection = await manager.connect(peer, { signal: null, deadline: null })
    const database = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
    const characteristic = database.service(HRM_SERVICE).characteristicsByUuid(HRM_MEASUREMENT)[0]
    const start = database.reserveOrderedRecordingStart()
    await stage.blockRadioOp('ordered-open')
    const pending = start.start([{memberId:'heart',characteristic}], { recording:{ id:'inflight', maxBytes:1048576, maxRecords:8 } })
    const cutoff = Date.now() + 5000
    while (!(await stage.stagedRadioCalls()).includes('ordered_open')) {
      if (Date.now() >= cutoff) throw new Error('ordered source open did not start')
      await new Promise(resolve=>setTimeout(resolve,5))
    }
    expect((await host.destroy()).state).toBe('release-failed')
    await stage.unblockRadioOp('ordered-open')
    await pending.catch(() => undefined)
    let status
    for (let i=0;i<200;i++) { status=await start.status(); if(status.state !== 'pending') break; await new Promise(resolve=>setTimeout(resolve,5)) }
    expect(status.state).toBe('result')
    let stopped
    for (let i=0;i<200;i++) {
      stopped=await recordings.stopOrdered('inflight')
      const page=await recordings.prepareOrdered('inflight',{maxItems:16,maxBytes:4096})
      if(page.token) await recordings.acknowledge('inflight',page.token)
      if(stopped.recording.state === 'finished' && stopped.hostCleanup.state === 'released') break
      await new Promise(resolve=>setTimeout(resolve,5))
    }
    expect(stopped).toMatchObject({recording:{state:'finished'},hostCleanup:{state:'released'}})
    await start.retire()
    expect((await host.destroy()).state).toBe('released')
  } finally {
    await stage.unblockRadioOp('ordered-open')
    await manager.destroy()
    const cleanup = await host.destroy()
    if (cleanup.state === 'released') fs.rmSync(directory,{recursive:true,force:true,maxRetries:5,retryDelay:20})
  }
})

test('replacement recorder marker rejects a changed native GATT graph before writing', async () => {
  const fs = require('node:fs')
  const os = require('node:os')
  const path = require('node:path')
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-stale-marker-'))
  const { backend, stage } = await openBackend('corebluetooth')
  const { observePeer } = require('../../helpers/desktop-rust-core-harness')
  const host = await require('../../../src/desktop-process-host').createDesktopProcessHostFromBackend(backend, { now: () => performance.now() })
  const manager = await host.createInternalManager()
  let replacement
  try {
    const recordings = await host.continuation.recordings(directory)
    const peer = await observePeer(backend, stage)
    const services = hrmServices()
    services[0].characteristics[0].properties.write = true
    await stage.stageServices('peer-1', services)
    stage.stageGattSnapshotIdentity('peer-1', 'stale-owner', '1', '1')
    const connection = await manager.connect(peer, { signal:null, deadline:null })
    const database = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover({ signal:null, deadline:null }))
    const characteristic = database.service(HRM_SERVICE).characteristicsByUuid(HRM_MEASUREMENT)[0]
    const start = database.reserveOrderedRecordingStart()
    expect((await start.start([{memberId:'heart',characteristic}], {recording:{id:'stale-graph',maxBytes:1048576,maxRecords:8}})).kind).toBe('started')
    const handoff = start.recoveryHandle()
    await manager.destroy()
    stage.stageGattSnapshotIdentity('peer-1', 'stale-owner', '1', '2')
    await stage.stageServicesChanged('peer-1')
    replacement = await host.createInternalManager()
    const replacementConnection = await replacement.connect(peer, { signal:null, deadline:null })
    const replacementDatabase = await require('../../../src/public/gatt').createPublicGattDatabase(await replacementConnection.discover({ signal:null, deadline:null }))
    const reclaim = replacementDatabase.reserveOrderedRecordingReclaim(handoff)
    const before = await stage.stagedRadioCalls()
    let status = await reclaim.start()
    for (let i=0;i<200 && status.state === 'pending';i++) { await new Promise(resolve=>setTimeout(resolve,5)); status=await reclaim.status() }
    if (status.state === 'failed') {
      expect(status.error.code).toBe('gatt.stale-handle')
    } else {
      expect(status.state).toBe('not-reserved')
      expect(['lifecycle.invalid-state', 'ownership.denied']).toContain(status.error.code)
    }
    await reclaim.retire()
    expect((await stage.stagedRadioCalls()).filter(call=>call === 'ordered_marker_start')).toEqual(before.filter(call=>call === 'ordered_marker_start'))
    for (let i=0;i<200;i++) {
      const stopped=await recordings.stopOrdered('stale-graph')
      const page=await recordings.prepareOrdered('stale-graph',{maxItems:16,maxBytes:4096})
      if(page.token) await recordings.acknowledge('stale-graph',page.token)
      if(stopped.recording.state === 'finished' && stopped.hostCleanup.state === 'released') break
      await new Promise(resolve=>setTimeout(resolve,5))
    }
    await start.retire()
    await replacementConnection.release()
    await replacement.destroy()
    expect((await host.destroy()).state).toBe('released')
  } finally {
    await manager.destroy()
    if (replacement) await replacement.destroy()
    const cleanup=await host.destroy()
    if(cleanup.state === 'released') fs.rmSync(directory,{recursive:true,force:true,maxRetries:5,retryDelay:20})
  }
})

test('lost reclaim reservation response remains recoverable after host shutdown begins', async () => {
  const fs = require('node:fs')
  const os = require('node:os')
  const path = require('node:path')
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-reclaim-close-'))
  const { backend, stage, harness } = await openBackend('corebluetooth')
  const { observePeer } = require('../../helpers/desktop-rust-core-harness')
  const host = await require('../../../src/desktop-process-host').createDesktopProcessHostFromBackend(backend, { now: () => performance.now() })
  const manager = await host.createInternalManager()
  try {
    await host.continuation.recordings(directory)
    const peer = await observePeer(backend, stage)
    const services = hrmServices()
    services[0].characteristics[0].properties.write = true
    await stage.stageServices('peer-1', services)
    stage.stageGattSnapshotIdentity('peer-1', 'close-reclaim', '1', '1')
    const connection = await manager.connect(peer, { signal: null, deadline: null })
    const database = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
    const characteristic = database.service(HRM_SERVICE).characteristicsByUuid(HRM_MEASUREMENT)[0]
    const start = database.reserveOrderedRecordingStart()
    expect((await start.start([{ memberId: 'heart', characteristic }], { recording: { id: 'close-reclaim', maxBytes: 1048576, maxRecords: 8 } })).kind).toBe('started')
    const handoff = start.recoveryHandle()
    await start.retire()
    const replacement = await host.createInternalManager()
    const replacementConnection = await replacement.connect(peer, { signal: null, deadline: null })
    const replacementDatabase = await require('../../../src/public/gatt').createPublicGattDatabase(await replacementConnection.discover({ signal: null, deadline: null }))
    const reclaim = replacementDatabase.reserveOrderedRecordingReclaim(handoff)
    harness.dropReclaimResponse = 'next'
    await expect(reclaim.start()).rejects.toThrow()
    expect((await host.destroy()).state).toBe('release-failed')
    expect((await reclaim.cancel()).state).toBe('failed')
    await reclaim.retire()
    const raw = require('../../../src/core/continuation-recording').createNativeContinuationRecordingController(stage.continuationRecordingStore(), 'ubm-desktop')
    let result
    for (let i = 0; i < 200; i++) {
      result = await raw.stopOrdered('close-reclaim')
      const page = await raw.prepareOrdered('close-reclaim', { maxItems: 16, maxBytes: 4096 })
      if (page.token) await raw.acknowledge('close-reclaim', page.token)
      if (result.recording.state === 'finished' && result.hostCleanup.state === 'released') break
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    expect(result).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
    await replacement.destroy()
    expect((await host.destroy()).state).toBe('released')
  } finally {
    await manager.destroy()
    const cleanup = await host.destroy()
    if (cleanup.state === 'released') fs.rmSync(directory, { recursive: true, force: true, maxRetries: 5, retryDelay: 20 })
  }
})
