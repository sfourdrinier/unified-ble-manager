const { main, verifyRecording, verifyRecordingBaseline } = require('../scripts/native-protocol/test-continuation-radio')
const fs = require('node:fs')
const path = require('node:path')
const os = require('node:os')

const verification = () => ({ minimumSamples: 1, archive: jest.fn(async () => {}) })

test('simulator guidance distinguishes paired loopback evidence from unrestricted compatibility', () => {
  const guide = fs.readFileSync(path.join(__dirname, '../tool/h10-sim/README.md'), 'utf8')
  expect(guide).not.toContain('neither breaking any scenario')
  expect(guide).toContain('same-daemon, two-adapter')
  expect(guide).toContain('test-created bonds')
})

function harness({
  drop = { ok: true, state: { dropped: ['central-address'] } },
  close = { state: 'released', failures: [] }
} = {}) {
  const logs = []
  const central = {
    startScan: jest.fn(async () => ({ operationId: 'scan-1' })),
    stopScan: jest.fn(async () => 'stopped'),
    takeScanObservation: jest.fn(async () => ({
      advertisement: { localName: 'SIM Polar H10 0001', peerId: 'AA:BB:CC:DD:EE:FF' }
    })),
    continuationPrepareClaim: jest.fn(async () => JSON.stringify({ ok: true, value: { claimToken: 'claim-1' } })),
    close: jest.fn(async () => close)
  }
  let statusCalls = 0
  const controller = {
    execute: jest.fn(async () => ({ resubscribed: 1 })),
    status: jest.fn(async () => ({
      queuedData: ++statusCalls * 10,
      continuationOutcome: { event: 'continuation.completed' }
    })),
    claim: jest.fn(async () => ({
      disposed: true,
      disposeFailure: null,
      controlLost: 0,
      afterCutoffLoss: { items: 0, bytes: 0 },
      streamEnds: [],
      values: Array.from({ length: 20 }, (_, i) => ({
        consumer: `ubm-continuation-${i % 2}`,
        value: Uint8Array.of(0, 72)
      }))
    }))
  }
  const binding = { openProduction: jest.fn(async () => central) }
  const api = {
    loadDesktopCoreBinding: jest.fn(async () => binding),
    createNativeContinuationController: jest.fn(() => controller)
  }
  return {
    central,
    binding,
    controller,
    api,
    logs,
    options: {
      api,
      env: {
        UBM_NAPI_ADDON: '/tmp/test.node',
        UBM_RADIO_PLATFORM: 'corebluetooth',
        UBM_CONTINUATION_SECONDS: '30',
        UBM_SIM_CONTROL_PORT: '9000'
      },
      wait: async () => {},
      sendControl: async () => drop,
      log: message => logs.push(JSON.parse(message))
    }
  }
}

test('radio probe uses identity-checked one-central API and requires positive native data plus cleanup', async () => {
  const run = harness()
  await main(run.options)
  expect(run.api.loadDesktopCoreBinding).toHaveBeenCalledTimes(1)
  expect(run.api.createNativeContinuationController).toHaveBeenCalledWith(run.central)
  expect(run.central.close).toHaveBeenCalledTimes(1)
  expect(run.logs.at(-1).phase).toBe('passed')
})

test('BlueZ continuation probe forwards an explicit current daemon owner before opening the radio', async () => {
  const run = harness()
  run.options.env.UBM_RADIO_PLATFORM = 'bluez'
  run.options.env.UBM_BLUEZ_DAEMON_OWNER = ':1.812'
  await main(run.options)
  expect(run.binding.openProduction).toHaveBeenCalledWith({
    owner: 'native-continuation-radio-test',
    platform: 'bluez',
    adapterId: null,
    connectionPolicy: { mode: 'le-bearer', daemonUniqueOwner: ':1.812' }
  })
})

test.each([undefined, '', 'org.bluez', ':1.812\n'])(
  'BlueZ probe rejects unattested owner %j before binding load',
  async owner => {
    const run = harness()
    run.options.env.UBM_RADIO_PLATFORM = 'bluez'
    if (owner !== undefined) run.options.env.UBM_BLUEZ_DAEMON_OWNER = owner
    await expect(main(run.options)).rejects.toThrow()
    expect(run.api.loadDesktopCoreBinding).not.toHaveBeenCalled()
    expect(run.binding.openProduction).not.toHaveBeenCalled()
  }
)

test.each([
  {
    options: { drop: { ok: false, error: 'faithful mode' } },
    message: '"error":"faithful mode"'
  },
  {
    options: { drop: { ok: true, state: { dropped: [] } } },
    message: 'simulator did not report a dropped client link'
  },
  {
    options: { close: { state: 'release-failed', failures: [{ resourceKind: 'link' }] } },
    message: 'central cleanup remained unresolved'
  }
])('radio probe refuses missing disruption or failed cleanup: %j', async ({ options, message }) => {
  const run = harness(options)
  await expect(main(run.options)).rejects.toThrow(message)
  expect(run.central.close).toHaveBeenCalledTimes(1)
  expect(run.logs.some(log => log.phase === 'passed')).toBe(false)
})

function recordingRecords(
  generations = [
    [1, 1],
    [2, 2]
  ],
  samplesPerGeneration = 1
) {
  const records = []
  generations.forEach(([connection, database], index) => {
    const consumer = `consumer-${index}`
    const metadata = {
      session: {
        peerId: 'peer',
        sessionId: '1',
        backendInstanceId: 'backend',
        sessionStartedAtUnixNs: '1',
        sessionEpoch: 'epoch-1'
      },
      consumer: {
        consumer,
        peerId: 'peer',
        connectionGeneration: String(connection),
        databaseGeneration: String(database),
        selector: {
          serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
          serviceOccurrence: 1,
          characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
          characteristicOccurrence: 1
        }
      }
    }
    const append = record => records.push({ ordinal: records.length + 1, metadata, record })
    append({ t: 'consumer-registration', consumer })
    for (let sample = 0; sample < samplesPerGeneration; sample++)
      append({ t: 'value', consumer, value: Uint8Array.of(0, 72) })
    if (index < generations.length - 1)
      append({ t: 'stream-end', consumer, reason: 'invalidated', droppedItems: 0, droppedBytes: 0 })
  })
  return records
}

function recordingHarness(records = recordingRecords()) {
  let acknowledged = false
  const store = {
    status: jest.fn(async () => ({
      lostRecords: 0,
      collectionFailure: null,
      runtimeFailure: null,
      records: acknowledged ? 0 : records.length
    })),
    stop: jest.fn(async () => ({ phase: 'stopped', accepting: false, radioRelease: 'not-requested' })),
    prepare: jest.fn(async () =>
      acknowledged
        ? { token: null, records: [], bytes: 0, more: false }
        : { token: 'prefix-1', records, bytes: 100, more: false }
    ),
    acknowledge: jest.fn(async () => {
      acknowledged = true
      return { acknowledged: true, token: 'prefix-1', records: records.length }
    }),
    clear: jest.fn(async () => ({ cleared: true, records: 0 }))
  }
  return store
}

test('registration-only native journal refuses disruption without consuming its prefix', async () => {
  const run = harness()
  run.options.env.UBM_RECORDING_DIRECTORY = '/tmp/owned-radio-baseline'
  const store = recordingHarness(recordingRecords([[1, 1]], 0))
  run.controller.recordings = jest.fn(async () => store)
  run.options.sendControl = jest.fn(async () => ({ ok: true, state: { dropped: ['central-address'] } }))
  await expect(main(run.options)).rejects.toThrow('no positive recorded HR before disruption')
  expect(run.options.sendControl).not.toHaveBeenCalled()
  expect(store.acknowledge).not.toHaveBeenCalled()
  expect(store.stop).not.toHaveBeenCalled()
  expect(store.clear).not.toHaveBeenCalled()
  expect(run.central.close).toHaveBeenCalledTimes(1)
})

test('positive HR baseline replays bounded native prefix without ACK, stop, or clear', async () => {
  const store = recordingHarness(recordingRecords([[1, 1]], 3))
  await expect(verifyRecordingBaseline(store, 'baseline')).resolves.toBe(3)
  expect(store.prepare).toHaveBeenCalledTimes(2)
  expect(store.prepare).toHaveBeenNthCalledWith(1, 'baseline', { maxItems: 256, maxBytes: 1048576 })
  expect(store.acknowledge).not.toHaveBeenCalled()
  expect(store.stop).not.toHaveBeenCalled()
  expect(store.clear).not.toHaveBeenCalled()
})

test.each(['zero', 'wrong-selector', 'changed-replay'])(
  'baseline refuses %s data without consuming journal',
  async mutation => {
    const records = recordingRecords([[1, 1]])
    if (mutation === 'zero') records[1].record.value = Uint8Array.of(0, 0)
    if (mutation === 'wrong-selector')
      records[1].metadata.consumer.selector.characteristicUuid = '00002a19-0000-1000-8000-00805f9b34fb'
    const store = recordingHarness(records)
    if (mutation === 'changed-replay')
      store.prepare.mockResolvedValueOnce({ token: 'different', records, bytes: 100, more: false })
    await expect(verifyRecordingBaseline(store, 'baseline')).rejects.toThrow()
    expect(store.acknowledge).not.toHaveBeenCalled()
    expect(store.clear).not.toHaveBeenCalled()
  }
)

test('held baseline preparation is bounded and late completion cannot authorize disruption', async () => {
  jest.useFakeTimers()
  try {
    let finish
    const store = recordingHarness()
    store.prepare.mockImplementationOnce(
      () =>
        new Promise(resolve => {
          finish = resolve
        })
    )
    const pending = verifyRecordingBaseline(store, 'baseline')
    const rejection = expect(pending).rejects.toThrow('native HR baseline observation timed out')
    await jest.advanceTimersByTimeAsync(10000)
    await rejection
    finish({ token: 'prefix-1', records: recordingRecords(), bytes: 100, more: false })
    await Promise.resolve()
    expect(store.prepare).toHaveBeenCalledTimes(1)
    expect(store.acknowledge).not.toHaveBeenCalled()
    expect(store.clear).not.toHaveBeenCalled()
  } finally {
    jest.useRealTimers()
  }
})

test('offline radio recording proof validates stable prefix before explicit acknowledgement', async () => {
  const store = recordingHarness()
  const options = verification()
  const result = await verifyRecording(store, 'radio-test', options)
  expect(result.samples).toHaveLength(2)
  expect(result.connectionGenerations).toBe(2)
  expect(result.databaseGenerations).toBe(2)
  expect(options.archive.mock.invocationCallOrder[0]).toBeLessThan(store.acknowledge.mock.invocationCallOrder[0])
  expect(store.prepare).toHaveBeenCalledTimes(3)
  expect(store.acknowledge).toHaveBeenCalledWith('radio-test', 'prefix-1')
  expect(store.clear).toHaveBeenCalledTimes(1)
})

test.each([
  {
    generations: [
      [1, 1],
      [1, 2],
      [2, 3]
    ],
    message: 'unexpected database generation churn'
  },
  {
    generations: [
      [1, 1],
      [2, 2],
      [3, 3]
    ],
    message: 'expected exactly two connection generations'
  }
])('one-outage recording qualification rejects extra generations: $message', async ({ generations, message }) => {
  const records = recordingRecords(generations)
  const store = recordingHarness(records)
  const options = verification()
  await expect(verifyRecording(store, 'radio-test', options)).rejects.toThrow(message)
  expect(options.archive).toHaveBeenCalledWith(records)
  expect(store.clear).not.toHaveBeenCalled()
})

test.each([
  [3, 3],
  [2, 2]
])('control-only extra registration (%s/%s) refuses qualification', async (connection, database) => {
  const records = recordingRecords()
  const third = recordingRecords([[connection, database]])[0]
  third.metadata.consumer.consumer = 'consumer-2'
  third.record.consumer = 'consumer-2'
  records.push({ ...third, ordinal: records.length + 1 })
  const store = recordingHarness(records)
  const options = verification()
  await expect(verifyRecording(store, 'radio-test', options)).rejects.toThrow('registered')
  expect(options.archive).toHaveBeenCalledWith(records)
  expect(store.clear).not.toHaveBeenCalled()
})

test.each(['value', 'stream-end'])('unregistered or mismatched %s cannot be acknowledged', async type => {
  for (const mismatch of [false, true]) {
    const records = recordingRecords()
    const entry = records.find(record => record.record.t === type)
    if (mismatch)
      entry.metadata = { ...entry.metadata, consumer: { ...entry.metadata.consumer, databaseGeneration: 'foreign' } }
    else entry.record.consumer = 'unregistered'
    const store = recordingHarness(records)
    await expect(verifyRecording(store, 'radio-test', verification())).rejects.toThrow('registration')
    expect(store.acknowledge).not.toHaveBeenCalled()
    expect(store.clear).not.toHaveBeenCalled()
  }
})

test('registered identity remains authoritative across archived and acknowledged page boundaries', async () => {
  const records = recordingRecords()
  const store = recordingHarness(records)
  const pages = [records.slice(0, 1), records.slice(1)]
  let page = 0
  store.prepare.mockImplementation(async () => ({
    token: page < pages.length ? `page-${page}` : null,
    records: pages[page] ?? [],
    bytes: page < pages.length ? 100 : 0,
    more: page === 0
  }))
  store.acknowledge.mockImplementation(async (_id, token) => ({
    acknowledged: true,
    token,
    records: pages[page++].length
  }))
  store.status.mockImplementation(async () => ({
    lostRecords: 0,
    collectionFailure: null,
    runtimeFailure: null,
    records: page === pages.length ? 0 : records.length
  }))
  const options = verification()
  expect((await verifyRecording(store, 'radio-test', options)).samples).toHaveLength(2)
  expect(options.archive).toHaveBeenCalledTimes(2)
  for (let index = 0; index < 2; index++) {
    expect(options.archive.mock.invocationCallOrder[index]).toBeLessThan(
      store.acknowledge.mock.invocationCallOrder[index]
    )
  }
})

test('fresh recording refuses an omitted ordinal between otherwise valid journal pages', async () => {
  const records = recordingRecords()
  const pages = [records.slice(0, 2), records.slice(2).map(entry => ({ ...entry, ordinal: entry.ordinal + 1 }))]
  const store = recordingHarness(records)
  let page = 0
  store.prepare.mockImplementation(async () => ({
    token: page < pages.length ? `page-${page}` : null,
    records: pages[page] ?? [],
    bytes: page < pages.length ? 100 : 0,
    more: page === 0
  }))
  store.acknowledge.mockImplementation(async (_id, token) => ({
    acknowledged: true,
    token,
    records: pages[page++].length
  }))
  store.status.mockImplementation(async () => ({
    lostRecords: 0,
    collectionFailure: null,
    runtimeFailure: null,
    records: page === pages.length ? 0 : records.length
  }))
  await expect(verifyRecording(store, 'radio-test', verification())).rejects.toThrow('ordinal')
  expect(store.acknowledge).toHaveBeenCalledTimes(1)
  expect(store.clear).not.toHaveBeenCalled()
})

test('one-outage volatile qualification rejects extra subscription generations', async () => {
  const run = harness()
  const backlog = await run.controller.claim()
  run.controller.claim.mockResolvedValue({
    ...backlog,
    values: backlog.values.map((sample, index) => ({
      ...sample,
      consumer: `consumer-${index % 3}`
    }))
  })
  await expect(main(run.options)).rejects.toThrow('expected exactly two subscription generations')
  expect(run.central.close).toHaveBeenCalledTimes(1)
  expect(run.logs.some(log => log.phase === 'passed')).toBe(false)
})

test('offline recording proof never acknowledges changing replay or silently ignores stored loss', async () => {
  const store = recordingHarness()
  store.prepare.mockResolvedValueOnce({ token: 'first', records: [], bytes: 0, more: false })
  await expect(verifyRecording(store, 'radio-test', verification())).rejects.toThrow('replay changed')
  expect(store.acknowledge).not.toHaveBeenCalled()
  expect(store.clear).not.toHaveBeenCalled()
  const lost = recordingHarness()
  lost.status.mockResolvedValue({ lostRecords: 1, collectionFailure: null, runtimeFailure: null })
  await expect(verifyRecording(lost, 'radio-test', verification())).rejects.toThrow('recording lost records')
  expect(lost.acknowledge).not.toHaveBeenCalled()
})

test('durable radio mode reopens storage only after radio cleanup and preserves the claim identity', async () => {
  const run = harness()
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-radio-journal-test-'))
  run.options.env.UBM_RECORDING_DIRECTORY = directory
  const store = recordingHarness()
  const records = recordingRecords(
    [
      [1, 1],
      [2, 2]
    ],
    10
  )
  let acknowledged = false
  store.prepare.mockImplementation(async () =>
    acknowledged
      ? { token: null, records: [], bytes: 0, more: false }
      : { token: 'prefix-1', records, bytes: 1000, more: false }
  )
  store.acknowledge.mockImplementation(async () => {
    acknowledged = true
    return { acknowledged: true, token: 'prefix-1', records: records.length }
  })
  store.status.mockImplementation(async () => ({
    lostRecords: 0,
    collectionFailure: null,
    runtimeFailure: null,
    records: acknowledged ? 0 : records.length
  }))
  let liveStatus = 0
  run.controller.recordings = jest.fn(async () => ({
    status: async () => ({ records: ++liveStatus * 10 }),
    prepare: store.prepare
  }))
  run.controller.claim.mockImplementation(async () => ({
    disposed: true,
    disposeFailure: null,
    controlLost: 0,
    afterCutoffLoss: { items: 0, bytes: 0 },
    streamEnds: [],
    values: [],
    recording: { id: run.controller.execute.mock.calls[0][0].recording.id }
  }))
  run.api.openNativeContinuationRecordings = jest.fn(async () => {
    expect(run.central.close).toHaveBeenCalledTimes(1)
    return store
  })
  await main(run.options)
  expect(run.controller.recordings).toHaveBeenCalledWith(directory)
  expect(run.api.openNativeContinuationRecordings).toHaveBeenCalledTimes(1)
  expect(run.logs.at(-1)).toEqual(
    expect.objectContaining({
      phase: 'passed',
      sampleCount: 20,
      connectionGenerations: 2,
      recording: expect.objectContaining({ offlineAfterRadioClose: true, acknowledged: true, cleared: true })
    })
  )
  const archive = run.logs.at(-1).recording.archivePath
  expect(fs.readFileSync(archive, 'utf8').trim().split('\n')).toHaveLength(records.length)
  fs.unlinkSync(archive)
  fs.rmdirSync(directory)
})

test('insufficient recording retains flushed evidence and refuses clear; failed archive refuses acknowledgement', async () => {
  const short = recordingHarness()
  const options = { ...verification(), minimumSamples: 100 }
  await expect(verifyRecording(short, 'radio-test', options)).rejects.toThrow('too few recorded HR samples')
  expect(options.archive).toHaveBeenCalledTimes(1)
  expect(short.clear).not.toHaveBeenCalled()
  const failedArchive = recordingHarness()
  await expect(
    verifyRecording(failedArchive, 'radio-test', {
      minimumSamples: 1,
      archive: async () => {
        throw new Error('disk full')
      }
    })
  ).rejects.toThrow('disk full')
  expect(failedArchive.acknowledge).not.toHaveBeenCalled()
  expect(failedArchive.clear).not.toHaveBeenCalled()
})
