// __tests__/ipc/pending-stream-bounds.test.js

const { IpcBleManager, inspectIpcPendingStreamAccountingForTests } = require('../../src/ipc/manager')
const { BUILT_IN_FEATURE_IDS } = require('../../src/backend-contract/capabilities')

function negotiated(axis, value = axis === 'ipc-protocol' ? 4 : 1) {
  const selected = { axis, value }
  const range = { axis, minimum: selected, maximum: selected }
  return { axis, selected, localRange: range, remoteRange: range }
}

function bootstrapRecord() {
  const backendGeneration = 'backend-generation-1'
  return {
    attachment: {
      attachmentId: 'attachment-1',
      backendInstanceId: 'backend-1',
      backendGeneration,
      adapter: {
        adapterId: 'adapter-1',
        displayName: 'Bluetooth',
        state: {
          availability: 'available',
          authorization: 'granted',
          power: 'on',
          heard: null,
          backendGeneration,
          updatedAt: 1,
          safeReason: null
        },
        adapterGeneration: 'adapter-generation-1',
        limitations: []
      }
    },
    attachmentId: 'attachment-1',
    versions: {
      backendContract: negotiated('backend-contract'),
      capabilitySchema: negotiated('capability-schema'),
      eventSchema: negotiated('event-schema'),
      traceFormat: negotiated('trace-format'),
      ipcProtocol: negotiated('ipc-protocol')
    },
    capabilities: {
      schemaVersion: 2,
      backendGeneration,
      descriptors: Object.values(BUILT_IN_FEATURE_IDS).map(id => ({
        id,
        state: 'unsupported',
        selectedSchemaRange: negotiated('capability-schema').localRange,
        implementationOrigin: 'backend-native',
        tck: {
          suiteId: 'capability.catalog-v2',
          requiredScenarioIds: ['capability.truth-limits-evidence-and-binding'],
          contractRange: negotiated('capability-schema').localRange
        },
        evidence: {
          receiptId: `fixture-${id}`,
          evidenceLevel: 'blocked',
          implementationVersion: 'fixture',
          sourceDigest: `fixture-${id}`,
          scenarioIds: ['capability.truth-limits-evidence-and-binding'],
          limitations: [{ code: 'not-implemented', explanation: 'fixture', affectedGuarantee: 'support' }]
        },
        limitations: [{ code: 'not-implemented', explanation: 'fixture', affectedGuarantee: 'support' }],
        limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
      }))
    },
    renderer: { clientId: 'client-1', windowScope: 'window', sessionScope: 'session' },
    rendererLease: { leaseId: 'lease-1', generation: 'lease-generation-1' }
  }
}

async function createIpcHarness(options = {}) {
  const listeners = []
  const bootstrap = bootstrapRecord()
  const transport = {
    invoke: async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      return { kind: 'route', payload: { state: 'released', failureCount: 0 } }
    },
    subscribe(listener) {
      listeners.push(listener)
      return () => undefined
    },
    acknowledge: async () => ({ kind: 'event.ack' })
  }
  const ipc = await IpcBleManager.create(transport, options)
  return {
    ipc,
    bootstrap,
    emit(streamId, item, eventId = `event-${streamId}`) {
      listeners[0]({
        rendererLease: bootstrap.rendererLease,
        eventId,
        streamId,
        item
      })
    }
  }
}

async function flushPump() {
  for (let attempt = 0; attempt < 8; attempt += 1) {
    await new Promise(resolve => setImmediate(resolve))
  }
}

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function serializedBytes(value) {
  return Buffer.byteLength(JSON.stringify(value))
}

async function firstStreamItem(ipc, streamId) {
  const stream = ipc.registerStream(streamId, isRecord)
  const iterator = stream[Symbol.asyncIterator]()
  const item = await iterator.next()
  await iterator.return()
  return item
}

describe('IPC pre-registration stream buffering', () => {
  test('active terminal-only source loss retains its counters', async () => {
    const { ipc, emit } = await createIpcHarness()
    const iterator = ipc.registerStream('active-terminal-loss', isRecord)[Symbol.asyncIterator]()
    emit(
      'active-terminal-loss',
      { kind: 'terminal', reason: 'overflow', droppedItems: 3, droppedBytes: 30, replacedItems: 2 },
      'active-terminal-loss-event'
    )
    await flushPump()
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'overflow', droppedItems: 3, droppedBytes: 30, replacedItems: 2 }
    })
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'overflow', droppedItems: 3, droppedBytes: 30, replacedItems: 2 }
    })
    await ipc.destroy()
  })
  test('terminal-only upstream loss remains visible', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit(
      'terminal-loss',
      { kind: 'terminal', reason: 'source-failed', droppedItems: 3, droppedBytes: 30, replacedItems: 2 },
      'terminal-loss-event'
    )
    await flushPump()
    const iterator = ipc.registerStream('terminal-loss', isRecord)[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'overflow', droppedItems: 3, droppedBytes: 30, replacedItems: 2 }
    })
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', droppedItems: 3, droppedBytes: 30, replacedItems: 2 }
    })
    await ipc.destroy()
  })

  test('upstream cumulative 3 then 5 plus independent pending 2 reports 7', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit(
      'staged-loss',
      { kind: 'overflow', policy: 'drop-oldest', droppedItems: 3, droppedBytes: 30, replacedItems: 1 },
      'staged-loss-3'
    )
    emit(
      'staged-loss',
      { kind: 'overflow', policy: 'drop-oldest', droppedItems: 5, droppedBytes: 50, replacedItems: 4 },
      'staged-loss-5'
    )
    for (let index = 0; index < 130; index += 1) {
      emit('staged-loss', { kind: 'value', value: { index } }, `staged-value-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    emit(
      'staged-loss',
      { kind: 'terminal', reason: 'closed', droppedItems: 5, droppedBytes: 50, replacedItems: 4 },
      'staged-terminal'
    )
    await flushPump()
    const iterator = ipc.registerStream('staged-loss', isRecord)[Symbol.asyncIterator]()
    let terminal
    for (;;) {
      const next = await iterator.next()
      if (next.value?.kind === 'terminal') {
        terminal = next.value
        break
      }
    }
    expect(terminal).toMatchObject({ droppedItems: 7, replacedItems: 4 })
    expect(terminal.droppedBytes).toBe(
      50 +
        serializedBytes({ kind: 'value', value: { index: 0 } }) +
        serializedBytes({ kind: 'value', value: { index: 1 } })
    )
    await ipc.destroy()
  })

  test('upstream totals survive pre-registration control eviction without a terminal', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit(
      'open-loss',
      { kind: 'overflow', policy: 'drop-oldest', droppedItems: 3, droppedBytes: 30, replacedItems: 1 },
      'open-loss-3'
    )
    emit(
      'open-loss',
      { kind: 'overflow', policy: 'drop-oldest', droppedItems: 5, droppedBytes: 50, replacedItems: 4 },
      'open-loss-5'
    )
    for (let index = 0; index < 130; index += 1) {
      emit('open-loss', { kind: 'value', value: { index } }, `open-value-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    await flushPump()
    const iterator = ipc.registerStream('open-loss', isRecord)[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      value: {
        kind: 'overflow',
        droppedItems: 7,
        replacedItems: 4
      }
    })
    await iterator.return()
    await ipc.destroy()
  })

  test.each([
    { values: 128, expectedDroppedItems: 5, expectedLocalBytes: 0 },
    {
      values: 129,
      expectedDroppedItems: 6,
      expectedLocalBytes: serializedBytes({ kind: 'value', value: { index: 0 } })
    }
  ])(
    'reports displaced upstream control after $values values without double-counting',
    async ({ values, expectedDroppedItems, expectedLocalBytes }) => {
      const { ipc, emit } = await createIpcHarness()
      emit(
        'control-only-loss',
        { kind: 'overflow', policy: 'drop-oldest', droppedItems: 5, droppedBytes: 50, replacedItems: 4 },
        'control-only-loss-overflow'
      )
      for (let index = 0; index < values; index += 1) {
        emit('control-only-loss', { kind: 'value', value: { index } }, `control-only-loss-${index}`)
        await new Promise(resolve => setImmediate(resolve))
      }
      await flushPump()
      const iterator = ipc.registerStream('control-only-loss', isRecord)[Symbol.asyncIterator]()
      await expect(iterator.next()).resolves.toMatchObject({
        value: {
          kind: 'overflow',
          droppedItems: expectedDroppedItems,
          droppedBytes: 50 + expectedLocalBytes,
          replacedItems: 4
        }
      })
      await iterator.return()
      await ipc.destroy()
    }
  )

  test('reports replacements-only loss after its control is displaced', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit(
      'replacements-only',
      { kind: 'overflow', policy: 'drop-oldest', droppedItems: 0, droppedBytes: 0, replacedItems: 4 },
      'replacements-only-overflow'
    )
    for (let index = 0; index < 128; index += 1) {
      emit('replacements-only', { kind: 'value', value: { index } }, `replacements-only-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    await flushPump()
    const iterator = ipc.registerStream('replacements-only', isRecord)[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'overflow', droppedItems: 0, droppedBytes: 0, replacedItems: 4 }
    })
    await iterator.return()
    await ipc.destroy()
  })

  test.each([
    { values: 128, droppedItems: 0 },
    { values: 129, droppedItems: 1 }
  ])('preserves latest policy after a control is displaced by $values values', async ({ values, droppedItems }) => {
    const { ipc, emit } = await createIpcHarness()
    emit(
      'latest-control-loss',
      { kind: 'overflow', policy: 'latest', droppedItems: 0, droppedBytes: 0, replacedItems: 4 },
      'latest-control-overflow'
    )
    for (let index = 0; index < values; index += 1) {
      emit('latest-control-loss', { kind: 'value', value: { index } }, `latest-control-value-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    await flushPump()
    const iterator = ipc.registerStream('latest-control-loss', isRecord)[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'overflow', policy: 'latest', droppedItems, replacedItems: 4 }
    })
    await iterator.return()
    await ipc.destroy()
  })

  test('uses local drop-oldest policy when a displaced latest control reported zero upstream loss', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit(
      'zero-upstream-latest',
      { kind: 'overflow', policy: 'latest', droppedItems: 0, droppedBytes: 0, replacedItems: 0 },
      'zero-upstream-latest-overflow'
    )
    for (let index = 0; index < 129; index += 1) {
      emit('zero-upstream-latest', { kind: 'value', value: { index } }, `zero-upstream-latest-value-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    await flushPump()
    const iterator = ipc.registerStream('zero-upstream-latest', isRecord)[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'overflow', policy: 'drop-oldest', droppedItems: 1, replacedItems: 0 }
    })
    await iterator.return()
    await ipc.destroy()
  })

  test('reports upstream loss when the byte budget displaces only its control', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit(
      'byte-control-only',
      { kind: 'overflow', policy: 'drop-oldest', droppedItems: 5, droppedBytes: 50, replacedItems: 4 },
      'byte-control-overflow'
    )
    const payload = 'x'.repeat(65_420)
    emit('byte-control-only', { kind: 'value', value: { payload } }, 'byte-control-value')
    await flushPump()
    const iterator = ipc.registerStream('byte-control-only', isRecord)[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'overflow', droppedItems: 5, droppedBytes: 50, replacedItems: 4 }
    })
    await iterator.return()
    await ipc.destroy()
  })

  test('eviction retains upstream and buffered value losses', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit(
      'evicted-source-loss',
      { kind: 'overflow', policy: 'drop-oldest', droppedItems: 5, droppedBytes: 50, replacedItems: 4 },
      'evicted-overflow'
    )
    emit('evicted-source-loss', { kind: 'value', value: { index: 0 } }, 'evicted-value')
    await flushPump()
    for (let index = 0; index < 256; index += 1) {
      emit(`other-${index}`, { kind: 'value', value: { index } }, `other-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    const iterator = ipc.registerStream('evicted-source-loss', isRecord)[Symbol.asyncIterator]()
    const terminal = await iterator.next()
    expect(terminal.value).toMatchObject({ kind: 'terminal', reason: 'overflow', droppedItems: 6, replacedItems: 4 })
    expect(terminal.value.droppedBytes).toBe(50 + serializedBytes({ kind: 'value', value: { index: 0 } }))
    await ipc.destroy()
  })
  test('forwards a structured source failure after emitted peers without inventing drops', async () => {
    const { ipc, emit } = await createIpcHarness()
    const stream = ipc.registerStream('scan-error', isRecord)
    const iterator = stream[Symbol.asyncIterator]()
    const error = {
      code: 'platform.transport',
      domain: 'stream',
      operation: 'tauri.event-send',
      platform: {
        domain: 'btleplug',
        code: 'native-error',
        safeMessage: 'native channel closed',
        metadata: {}
      },
      retryability: 'never'
    }

    emit('scan-error', { kind: 'value', value: { peer: 'peer-1' } }, 'scan-peer')
    emit('scan-error', { kind: 'terminal', reason: 'source-failed', error }, 'scan-terminal')
    await flushPump()

    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { peer: 'peer-1' } }
    })
    await expect(iterator.next()).resolves.toMatchObject({
      value: {
        kind: 'terminal',
        reason: 'source-failed',
        droppedItems: 0,
        droppedBytes: 0,
        error
      }
    })
    await iterator.return()
    await ipc.destroy()
  })

  test('delivers legitimate early events in order after registerStream', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit('early-1', { kind: 'value', value: { seq: 1 } }, 'early-a')
    emit('early-1', { kind: 'value', value: { seq: 2 } }, 'early-b')
    await flushPump()
    const stream = ipc.registerStream('early-1', isRecord)
    const iterator = stream[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'value', value: { seq: 1 } } })
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'value', value: { seq: 2 } } })
    await iterator.return()
    const accounting = inspectIpcPendingStreamAccountingForTests(ipc)
    expect(accounting).toEqual({
      pendingIdCount: 0,
      pendingItemCount: 0,
      pendingByteCount: 0,
      tombstoneCount: 0,
      activeStreamHandles: ['early-1']
    })
    await ipc.destroy()
  })

  test.each(['closed', 'source-failed'])('retains early values before a %s terminal', async reason => {
    const { ipc, emit } = await createIpcHarness()
    emit('early-terminal', { kind: 'value', value: { seq: 1 } }, 'early-terminal-a')
    emit('early-terminal', { kind: 'value', value: { seq: 2 } }, 'early-terminal-b')
    emit('early-terminal', { kind: 'terminal', reason }, 'early-terminal-end')
    await flushPump()
    const iterator = ipc.registerStream('early-terminal', isRecord)[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'value', value: { seq: 1 } } })
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'value', value: { seq: 2 } } })
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason, droppedItems: 0, droppedBytes: 0 }
    })
    await iterator.return()
    await ipc.destroy()
  })

  test('oversized early item accounts for values evicted before its overflow terminal', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit('oversized-early', { kind: 'value', value: { seq: 1 } }, 'oversized-a')
    emit('oversized-early', { kind: 'value', value: { bytes: 'x'.repeat(70 * 1024) } }, 'oversized-b')
    await flushPump()
    const iterator = ipc.registerStream('oversized-early', isRecord)[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'overflow', droppedItems: 2 }
    })
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'overflow' } })
    await iterator.return()
    await ipc.destroy()
  })

  test('oversized upstream overflow control counts only displaced values as local loss', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit('oversized-control', { kind: 'value', value: { seq: 1 } }, 'control-value')
    emit(
      'oversized-control',
      {
        kind: 'overflow',
        policy: 'drop-oldest',
        droppedItems: 3,
        droppedBytes: 30,
        replacedItems: 2,
        padding: 'x'.repeat(70 * 1024)
      },
      'control-overflow'
    )
    await flushPump()
    const iterator = ipc.registerStream('oversized-control', isRecord)[Symbol.asyncIterator]()
    const overflow = await iterator.next()
    expect(overflow.value).toMatchObject({ kind: 'overflow', droppedItems: 4, replacedItems: 2 })
    const terminal = await iterator.next()
    expect(terminal.value).toMatchObject({ kind: 'terminal', reason: 'overflow', droppedItems: 4, replacedItems: 2 })
    expect(terminal.value.droppedBytes).toBe(30 + serializedBytes({ kind: 'value', value: { seq: 1 } }))
    await ipc.destroy()
  })

  test.each([
    ['value', { kind: 'value', value: 42 }, 'ipc-manager.stream-value'],
    ['control', { kind: 'overflow', policy: 'invalid', droppedItems: 1, droppedBytes: 1 }, 'ipc-manager.event']
  ])('malformed buffered %s fails the registered child with its exact diagnostic', async (_label, item, operation) => {
    const { ipc, emit } = await createIpcHarness()
    emit('early-malformed', item, `early-malformed-${_label}`)
    await flushPump()
    const onTerminal = jest.fn()
    const stream = ipc.registerStream('early-malformed', isRecord, undefined, undefined, onTerminal)
    await expect(stream[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed', error: { code: 'protocol.malformed', operation } }
    })
    expect(onTerminal).toHaveBeenCalledTimes(1)
    expect(inspectIpcPendingStreamAccountingForTests(ipc).activeStreamHandles).not.toContain('early-malformed')
    await ipc.destroy()
  })

  test('reserved terminal capacity preserves a near-budget value and quarantines late data until registration', async () => {
    const { ipc, emit } = await createIpcHarness()
    const payload = 'x'.repeat(63 * 1024)
    emit('near-budget', { kind: 'value', value: { payload } }, 'budget-value')
    emit('near-budget', { kind: 'terminal', reason: 'closed' }, 'budget-terminal')
    emit('near-budget', { kind: 'value', value: { seq: 99 } }, 'budget-late')
    await flushPump()
    const iterator = ipc.registerStream('near-budget', isRecord)[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'value', value: { payload } } })
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'closed', droppedItems: 0, droppedBytes: 0 }
    })
    await expect(iterator.next()).resolves.toMatchObject({ done: true })
    await ipc.destroy()
  })

  test('aggregate value budget leaves one reserved terminal control slot per pending stream', async () => {
    const { ipc, emit } = await createIpcHarness()
    for (let streamIndex = 0; streamIndex < 4; streamIndex += 1) {
      for (let valueIndex = 0; valueIndex < 128; valueIndex += 1) {
        emit(
          `full-${streamIndex}`,
          { kind: 'value', value: { streamIndex, valueIndex } },
          `full-${streamIndex}-${valueIndex}`
        )
        await new Promise(resolve => setImmediate(resolve))
      }
    }
    expect(inspectIpcPendingStreamAccountingForTests(ipc).pendingItemCount).toBe(512)
    emit('full-0', { kind: 'terminal', reason: 'closed' }, 'full-0-terminal')
    await flushPump()
    expect(inspectIpcPendingStreamAccountingForTests(ipc)).toMatchObject({
      pendingIdCount: 4,
      pendingItemCount: 513
    })
    const iterator = ipc.registerStream('full-0', isRecord)[Symbol.asyncIterator]()
    for (let valueIndex = 0; valueIndex < 128; valueIndex += 1) {
      await expect(iterator.next()).resolves.toMatchObject({
        value: { kind: 'value', value: { streamIndex: 0, valueIndex } }
      })
    }
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'closed', droppedItems: 0, droppedBytes: 0 }
    })
    await iterator.return()
    await ipc.destroy()
  })

  test('unique unknown stream IDs remain globally bounded', async () => {
    const { ipc, emit } = await createIpcHarness()
    const count = 400
    for (let index = 0; index < count; index += 1) {
      emit(`unknown-${index}`, { kind: 'value', value: { index } }, `unknown-event-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    const accounting = inspectIpcPendingStreamAccountingForTests(ipc)
    expect(accounting.pendingIdCount).toBeLessThanOrEqual(256)
    expect(accounting.pendingItemCount).toBeLessThanOrEqual(512)
    expect(accounting.pendingByteCount).toBeLessThanOrEqual(2 * 1024 * 1024)
    expect(accounting.pendingIdCount + accounting.tombstoneCount).toBeGreaterThan(0)

    let retained = 0
    for (let index = 0; index < count; index += 1) {
      const item = await firstStreamItem(ipc, `unknown-${index}`)
      if (item.value?.kind === 'value' && item.value.value?.index === index) retained += 1
    }
    await ipc.destroy()
    expect(retained).toBeLessThanOrEqual(256)
  })

  test('aggregate eviction is fail-visible when the evicted ID later registers', async () => {
    const { ipc, emit } = await createIpcHarness()
    for (let index = 0; index < 257; index += 1) {
      emit(`quota-${index}`, { kind: 'value', value: { index } }, `quota-event-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    const evicted = await firstStreamItem(ipc, 'quota-0')
    expect(evicted.value?.kind).toBe('terminal')
    expect(['overflow', 'source-failed']).toContain(evicted.value?.reason)
    expect(evicted.value?.droppedItems).toBeGreaterThanOrEqual(1)
    expect(evicted.value?.droppedBytes).toBeGreaterThan(0)
    const kept = await firstStreamItem(ipc, 'quota-256')
    await expect(kept).toMatchObject({ value: { kind: 'value', value: { index: 256 } } })
    await ipc.destroy()
  })

  test('terminal-only unknown IDs cannot accumulate forever', async () => {
    const { ipc, emit } = await createIpcHarness()
    const count = 400
    for (let index = 0; index < count; index += 1) {
      emit(`terminal-${index}`, { kind: 'terminal', reason: 'closed' }, `terminal-event-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    const accounting = inspectIpcPendingStreamAccountingForTests(ipc)
    expect(accounting.pendingIdCount).toBeLessThanOrEqual(256)
    let originalTerminals = 0
    for (let index = 0; index < count; index += 1) {
      const item = await firstStreamItem(ipc, `terminal-${index}`)
      if (item.value?.kind === 'terminal' && item.value.reason === 'closed') originalTerminals += 1
    }
    await ipc.destroy()
    expect(originalTerminals).toBeLessThanOrEqual(256)
  })

  test('pending unknown IDs expire after the injected age bound', async () => {
    let now = 0
    const { ipc, emit } = await createIpcHarness({ now: () => now })
    emit('stale-1', { kind: 'value', value: { seq: 1 } }, 'stale-event')
    await flushPump()
    expect(inspectIpcPendingStreamAccountingForTests(ipc).pendingIdCount).toBe(1)
    now = 5_001
    const item = await firstStreamItem(ipc, 'stale-1')
    expect(item.value?.kind).toBe('terminal')
    expect(['overflow', 'source-failed']).toContain(item.value?.reason)
    await ipc.destroy()
  })

  test('destroy returns pending accounting to zero', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit('pending-1', { kind: 'value', value: { seq: 1 } }, 'destroy-a')
    emit('pending-2', { kind: 'value', value: { seq: 2 } }, 'destroy-b')
    await flushPump()
    expect(inspectIpcPendingStreamAccountingForTests(ipc).pendingIdCount).toBe(2)
    await ipc.destroy()
    expect(inspectIpcPendingStreamAccountingForTests(ipc)).toEqual({
      pendingIdCount: 0,
      pendingItemCount: 0,
      pendingByteCount: 0,
      tombstoneCount: 0,
      activeStreamHandles: []
    })
  })

  test('tombstone registration does not retain the sink in the active map', async () => {
    let now = 0
    const { ipc, emit } = await createIpcHarness({ now: () => now })
    emit('tomb-1', { kind: 'value', value: { seq: 1 } }, 'tomb-event')
    await flushPump()
    now = 5_001
    const stream = ipc.registerStream('tomb-1', isRecord)
    const accounting = inspectIpcPendingStreamAccountingForTests(ipc)
    expect(accounting.activeStreamHandles).not.toContain('tomb-1')
    expect(accounting.tombstoneCount).toBe(0)
    const iterator = stream[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed' }
    })
    emit('tomb-1', { kind: 'value', value: { seq: 2 } }, 'tomb-late')
    await flushPump()
    await expect(iterator.next()).resolves.toMatchObject({ done: true })
    await iterator.return()
    await ipc.destroy()
  })

  test('repeated tombstone registration keeps active pending item and byte counts bounded', async () => {
    let now = 0
    const { ipc, emit } = await createIpcHarness({ now: () => now })
    const count = 400
    for (let index = 0; index < count; index += 1) {
      emit(`tomb-repeat-${index}`, { kind: 'value', value: { index } }, `tomb-repeat-event-${index}`)
      await new Promise(resolve => setImmediate(resolve))
    }
    now = 5_001
    for (let index = 0; index < count; index += 1) {
      const stream = ipc.registerStream(`tomb-repeat-${index}`, isRecord)
      const iterator = stream[Symbol.asyncIterator]()
      await iterator.return()
    }
    const accounting = inspectIpcPendingStreamAccountingForTests(ipc)
    expect(accounting.pendingIdCount).toBeLessThanOrEqual(256)
    expect(accounting.pendingItemCount).toBeLessThanOrEqual(512)
    expect(accounting.pendingByteCount).toBeLessThanOrEqual(2 * 1024 * 1024)
    expect(accounting.tombstoneCount).toBeLessThanOrEqual(256)
    expect(accounting.activeStreamHandles.length).toBeLessThanOrEqual(256)
    await ipc.destroy()
  })

  test('tombstone owner cleanup runs once and retains release failure for destroy', async () => {
    let now = 0
    const { ipc, emit } = await createIpcHarness({ now: () => now })
    emit('tomb-owner', { kind: 'value', value: { seq: 1 } }, 'tomb-owner-event')
    await flushPump()
    now = 5_001
    const ownerError = new Error('tombstone-owner-cleanup-failed')
    const onTerminal = jest.fn(() => {
      throw ownerError
    })
    const stream = ipc.registerStream('tomb-owner', isRecord, undefined, undefined, onTerminal)
    expect(onTerminal).toHaveBeenCalledTimes(1)
    emit('tomb-owner', { kind: 'value', value: { seq: 2 } }, 'tomb-owner-late')
    await flushPump()
    expect(onTerminal).toHaveBeenCalledTimes(1)
    expect(inspectIpcPendingStreamAccountingForTests(ipc).activeStreamHandles).not.toContain('tomb-owner')
    await stream[Symbol.asyncIterator]().return()
    await expect(ipc.destroy()).rejects.toMatchObject({
      errors: expect.arrayContaining([ownerError])
    })
  })

  test('destroy after tombstone returns all IPC stream accounting to zero', async () => {
    let now = 0
    const { ipc, emit } = await createIpcHarness({ now: () => now })
    emit('tomb-destroy', { kind: 'value', value: { seq: 1 } }, 'tomb-destroy-event')
    await flushPump()
    now = 5_001
    const stream = ipc.registerStream('tomb-destroy', isRecord)
    await stream[Symbol.asyncIterator]().return()
    await ipc.destroy()
    expect(inspectIpcPendingStreamAccountingForTests(ipc)).toEqual({
      pendingIdCount: 0,
      pendingItemCount: 0,
      pendingByteCount: 0,
      tombstoneCount: 0,
      activeStreamHandles: []
    })
  })

  test('non-evicted early events remain ordered and lossless', async () => {
    const { ipc, emit } = await createIpcHarness()
    emit('kept-1', { kind: 'value', value: { seq: 1 } }, 'kept-a')
    emit('kept-1', { kind: 'value', value: { seq: 2 } }, 'kept-b')
    emit('kept-1', { kind: 'value', value: { seq: 3 } }, 'kept-c')
    await flushPump()
    const stream = ipc.registerStream('kept-1', isRecord)
    const iterator = stream[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'value', value: { seq: 1 } } })
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'value', value: { seq: 2 } } })
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'value', value: { seq: 3 } } })
    await iterator.return()
    expect(inspectIpcPendingStreamAccountingForTests(ipc).activeStreamHandles).toEqual(['kept-1'])
    ipc.closeStream('kept-1')
    await ipc.destroy()
  })
})
