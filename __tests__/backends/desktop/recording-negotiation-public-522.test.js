const {
  openBackend,
  observePeer,
  hrmServices,
  HRM_SERVICE,
  HRM_MEASUREMENT
} = require('../../helpers/desktop-rust-core-harness')

// Adapter capability snapshots are independently polled by the live desktop
// backend. They are not recording dispatch. These names are the complete
// recording-owner boundary whose absence proves no reservation/start escaped
// the public receipt-version guards.
const recordingOwnerCalls = calls =>
  calls.filter(([name]) =>
    [
      'reserveOrderedRecordingStart',
      'startOrderedRecording',
      'issueOrderedRecordingRecovery',
      'retireOrderedRecordingStart'
    ].includes(name)
  )

async function fixture() {
  const { backend, stage, harness } = await openBackend('corebluetooth')
  const host = await require('../../../src/desktop-process-host').createDesktopProcessHostFromBackend(backend, {
    now: () => performance.now()
  })
  const manager = await host.createInternalManager()
  const peer = await observePeer(backend, stage)
  await stage.stageServices('peer-1', hrmServices())
  stage.stageGattSnapshotIdentity('peer-1', 'negotiation-owner', '1', '1')
  const connection = await manager.connect(peer, { signal: null, deadline: null })
  const database = await require('../../../src/public/gatt').createPublicGattDatabase(
    await connection.discover({ signal: null, deadline: null })
  )
  const characteristic = database.service(HRM_SERVICE).characteristicsByUuid(HRM_MEASUREMENT)[0]
  return { backend, stage, host, manager, database, characteristic, harness }
}

test.each([undefined, 0, 1, 3])(
  'public desktop rejects receipt offer %s before reservation or recording dispatch',
  async version => {
    const { backend, stage, host, manager, database, harness } = await fixture()
    try {
      backend.central.orderedRecordingReceiptVersion = version === undefined ? undefined : () => version
      const callsStart = harness.calls.length
      expect(() => database.reserveOrderedRecordingStart()).toThrow()
      expect(recordingOwnerCalls(harness.calls.slice(callsStart))).toEqual([])
    } finally {
      await manager.destroy()
      await host.destroy()
    }
  },
  30000
)
test('desktop rechecks receipt offer after reservation and before native dispatch', async () => {
  const { backend, stage, host, manager, database, characteristic, harness } = await fixture()
  try {
    backend.central.orderedRecordingReceiptVersion = () => 2
    const handle = database.reserveOrderedRecordingStart()
    const callsStart = harness.calls.length
    backend.central.orderedRecordingReceiptVersion = () => 1
    await expect(
      handle.start([{ memberId: 'a', characteristic }], { recording: { id: 'skew', maxBytes: 1048576, maxRecords: 8 } })
    ).rejects.toMatchObject({ code: 'protocol.incompatible' })
    expect(recordingOwnerCalls(harness.calls.slice(callsStart))).toEqual([])
    await handle.retire()
  } finally {
    await manager.destroy()
    await host.destroy()
  }
})

test('native v2 offer permits public start, same-host stop, and offline stop', async () => {
  const fs = require('node:fs')
  const os = require('node:os')
  const path = require('node:path')
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-receipt-offer-'))
  const { backend, host, manager, database, characteristic } = await fixture()
  let closed = false
  try {
    expect(backend.central.orderedRecordingReceiptVersion()).toBe(2)
    const recordings = await host.continuation.recordings(directory)
    const handle = database.reserveOrderedRecordingStart()
    const result = await handle.start([{ memberId: 'heart', characteristic }], {
      recording: { id: 'offer-test', maxBytes: 1048576, maxRecords: 8 }
    })
    expect(result).toMatchObject({ kind: 'started', receipt: { state: 'active' } })
    let stop
    for (let i = 0; i < 200; i++) {
      stop = await recordings.stopOrdered('offer-test')
      if (stop.recording.state === 'finished' && stop.hostCleanup.state === 'released') break
      const page = await recordings.prepareOrdered('offer-test', { maxItems: 16, maxBytes: 4096 })
      if (page.token) await recordings.acknowledge('offer-test', page.token)
      await new Promise(resolve => setTimeout(resolve, 5))
    }
    expect(stop).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
    await handle.retire()
    await manager.destroy()
    await host.destroy()
    closed = true
    const addonPath =
      process.env.UBM_NAPI_ADDON ??
      path.join(__dirname, '../../../bindings/napi', `ubm_echo.${process.platform}-${process.arch}.node`)
    const binding = require('../../../src/desktop-core-addon').bindDesktopCore(
      { platform: 'corebluetooth', operationPrefix: 'direct-gatt' },
      { module: require(addonPath), path: addonPath, mode: 'source', sidecar: null }
    )
    const offline =
      await require('../../../src/backends/desktop/native-continuation-recording').openNativeContinuationRecordings(
        binding,
        directory
      )
    expect(offline.releaseIdle).toEqual(expect.any(Function))
    try {
      expect(await offline.stopOrdered('offer-test')).toMatchObject({ recording: { state: 'finished' } })
    } finally {
      await offline.releaseIdle()
    }
  } finally {
    if (!closed) {
      await manager.destroy()
      await host.destroy()
    }
    fs.rmSync(directory, { recursive: true, force: true })
  }
}, 30_000)
