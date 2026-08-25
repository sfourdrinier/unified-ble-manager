'use strict'

const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')
const { createIpcBootstrapRequest } = require('../src/ipc/protocol')

class FakeChannel {
  constructor() {
    this.onmessage = null
  }

  emit(message) {
    this.onmessage?.(message)
  }
}

describe('Tauri v2 IPC transport', () => {
  test('uses the scoped plugin command and one Channel for requests and streams', async () => {
    const invocations = []
    const channels = []
    const invoke = jest.fn(async (command, args) => {
      invocations.push({ command, args })
      return { kind: 'event.ack' }
    })
    class CapturedChannel extends FakeChannel {
      constructor() {
        super()
        channels.push(this)
      }
    }
    const { TAURI_BLE_PLUGIN_COMMAND, TauriBleIpcTransport } = require('../src/tauri/transport')
    const transport = new TauriBleIpcTransport({ invoke, Channel: CapturedChannel })
    const received = []
    const unsubscribe = transport.subscribe(event => received.push(event))

    const response = await transport.invoke(createIpcBootstrapRequest())
    channels[0].emit({
      eventId: 'event-1',
      streamId: 'adapter',
      rendererLease: { leaseId: 'lease-1', generation: 'generation-1' },
      item: { state: 'on' }
    })
    unsubscribe()
    channels[0].emit({
      eventId: 'event-2',
      streamId: 'adapter',
      rendererLease: { leaseId: 'lease-1', generation: 'generation-1' },
      item: { state: 'off' }
    })

    expect(TAURI_BLE_PLUGIN_COMMAND).toBe('plugin:unified-ble-manager|invoke')
    expect(response).toEqual({ kind: 'event.ack' })
    expect(channels).toHaveLength(1)
    expect(invocations).toEqual([
      {
        command: TAURI_BLE_PLUGIN_COMMAND,
        args: { request: createIpcBootstrapRequest(), eventChannel: channels[0] }
      }
    ])
    expect(received).toEqual([
      {
        eventId: 'event-1',
        streamId: 'adapter',
        rendererLease: { leaseId: 'lease-1', generation: 'generation-1' },
        item: { state: 'on' }
      }
    ])
  })

  test('acknowledgements use the same versioned plugin command without re-sending the channel', async () => {
    const invoke = jest.fn(async () => ({ kind: 'event.ack' }))
    const { TauriBleIpcTransport } = require('../src/tauri/transport')
    const transport = new TauriBleIpcTransport({ invoke, Channel: FakeChannel })
    const lease = { leaseId: 'lease-1', generation: 'generation-1' }

    await expect(transport.acknowledge(lease, 'event-1')).resolves.toEqual({ kind: 'event.ack' })
    expect(invoke).toHaveBeenCalledWith('plugin:unified-ble-manager|invoke', {
      request: { kind: 'event.ack', rendererLease: lease, eventId: 'event-1' }
    })
  })

  // Regression: Tauri deserializes every Channel command argument into a new
  // Rust Channel bound to the same JS callback id. Dropping any of them evals
  // `{ end: true, index }`, which unregisters the shared callback and kills the
  // event stream; their independent index counters also desynchronise the JS
  // side. The sink must therefore be bound exactly once, on the attach request.
  test('sends the event channel on the attach request only, never on later requests', async () => {
    const invocations = []
    const invoke = jest.fn(async (command, args) => {
      invocations.push(args)
      if (args.request.kind === 'event.ack') return { kind: 'event.ack' }
      return { kind: 'route', payload: {} }
    })
    const { TAURI_ATTACH_REQUEST_KIND, TauriBleIpcTransport } = require('../src/tauri/transport')
    const transport = new TauriBleIpcTransport({ invoke, Channel: FakeChannel })
    const lease = { leaseId: 'lease-1', generation: 'generation-1' }

    await transport.invoke(createIpcBootstrapRequest())
    await transport.invoke({ kind: 'route', envelope: { command: 'adapter.state' } })
    await transport.invoke({ kind: 'route', envelope: { command: 'scan.start' } })
    await transport.acknowledge(lease, 'event-1')
    await transport.invoke({ kind: 'release' })

    const carryingChannel = invocations.filter(args => 'eventChannel' in args)
    expect(carryingChannel).toHaveLength(1)
    expect(invocations[0]).toHaveProperty('eventChannel')
    expect(invocations[0].request.kind).toBe(TAURI_ATTACH_REQUEST_KIND)
  })

  test('re-attaching after a reload rebinds the sink so the stream can recover', async () => {
    const invocations = []
    const invoke = jest.fn(async (command, args) => {
      invocations.push(args)
      return { kind: 'route', payload: {} }
    })
    const { TAURI_ATTACH_REQUEST_KIND, TauriBleIpcTransport } = require('../src/tauri/transport')
    const transport = new TauriBleIpcTransport({ invoke, Channel: FakeChannel })

    await transport.invoke(createIpcBootstrapRequest())
    await transport.invoke({ kind: 'route', envelope: { command: 'adapter.state' } })
    await transport.invoke(createIpcBootstrapRequest())

    expect(invocations.filter(args => 'eventChannel' in args)).toHaveLength(2)
    expect(invocations[0].eventChannel).toBe(invocations[2].eventChannel)
  })

  test('preserves owned bytes through the actual JSON serialization boundary', async () => {
    const channels = []
    const invocations = []
    class CapturedChannel extends FakeChannel {
      constructor() {
        super()
        channels.push(this)
      }
    }
    const invoke = jest.fn(async (_command, args) => {
      const serializedArgs = JSON.parse(JSON.stringify(args))
      invocations.push(serializedArgs)
      return JSON.parse(
        JSON.stringify({
          kind: 'route',
          payload: { value: { $__unifiedBleBytesV2: [9, 8, 7] } }
        })
      )
    })
    const { TauriBleIpcTransport } = require('../src/tauri/transport')
    const transport = new TauriBleIpcTransport({ invoke, Channel: CapturedChannel })
    const received = []
    transport.subscribe(event => received.push(event))

    const response = await transport.invoke({
      kind: 'route',
      envelope: {
        binaryPayload: new Uint8Array([0, 127, 255]),
        payload: { nested: new Uint8Array([3, 4]) }
      }
    })
    channels[0].emit(
      JSON.parse(
        JSON.stringify({
          eventId: 'event-bytes',
          streamId: 'subscription-1',
          rendererLease: { leaseId: 'lease-1', generation: 'generation-1' },
          item: { kind: 'value', value: { value: { $__unifiedBleBytesV2: [5, 6] } } }
        })
      )
    )

    expect(invocations[0].request.envelope.binaryPayload).toEqual({
      $__unifiedBleBytesV2: [0, 127, 255]
    })
    expect(invocations[0].request.envelope.payload.nested).toEqual({
      $__unifiedBleBytesV2: [3, 4]
    })
    expect(response.payload.value).toEqual(new Uint8Array([9, 8, 7]))
    expect(received[0].item.value.value).toEqual(new Uint8Array([5, 6]))
  })

  test('does not reinterpret an already-decoded Uint8Array as a wire record', () => {
    const { decodeTauriWireValue } = require('../src/tauri/transport')
    const bytes = new Uint8Array([5, 6])
    const decoded = decodeTauriWireValue(bytes)

    expect(decoded).toEqual(bytes)
    expect(decoded).not.toBe(bytes)
    bytes[0] = 99
    expect(decoded[0]).toBe(5)
  })

  test('rejects malformed channel events before notifying listeners', () => {
    const channels = []
    class CapturedChannel extends FakeChannel {
      constructor() {
        super()
        channels.push(this)
      }
    }
    const { TauriBleIpcTransport } = require('../src/tauri/transport')
    const transport = new TauriBleIpcTransport({ invoke: jest.fn(), Channel: CapturedChannel })
    transport.subscribe(() => {
      throw new Error('listener must not receive malformed events')
    })

    expect(() => channels[0].emit({ kind: 'malformed' })).toThrow('protocol.malformed')
  })

  test('rejects malformed invoke responses instead of returning an unchecked protocol cast', async () => {
    const { TauriBleIpcTransport } = require('../src/tauri/transport')
    const transport = new TauriBleIpcTransport({
      invoke: jest.fn(async () => ({ kind: 'malformed' })),
      Channel: FakeChannel
    })

    await expect(transport.invoke(createIpcBootstrapRequest())).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed' }
    })
  })

  test('rejects unknown error codes and contradictory cleanup receipts', async () => {
    const { TauriBleIpcTransport } = require('../src/tauri/transport')
    const malformedErrorTransport = new TauriBleIpcTransport({
      invoke: jest.fn(async () => ({
        kind: 'failure',
        error: {
          code: 'made-up',
          domain: 'connection',
          operation: 'tauri.test',
          platform: null,
          retryability: 'never'
        }
      })),
      Channel: FakeChannel
    })
    await expect(malformedErrorTransport.invoke(createIpcBootstrapRequest())).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed' }
    })

    const contradictoryErrorTransport = new TauriBleIpcTransport({
      invoke: jest.fn(async () => ({
        kind: 'failure',
        error: {
          code: 'connection.failed',
          domain: 'connection',
          operation: 'tauri.test',
          platform: null,
          retryability: 'caller-decides'
        }
      })),
      Channel: FakeChannel
    })
    await expect(contradictoryErrorTransport.invoke(createIpcBootstrapRequest())).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed' }
    })

    const contradictoryCleanupTransport = new TauriBleIpcTransport({
      invoke: jest.fn(async () => ({
        kind: 'release',
        cleanup: {
          state: 'released',
          failures: [
            {
              resourceKind: 'connection',
              error: {
                code: 'connection.lost',
                domain: 'connection',
                operation: 'tauri.test',
                platform: null,
                retryability: 'never'
              }
            }
          ]
        }
      })),
      Channel: FakeChannel
    })
    await expect(contradictoryCleanupTransport.invoke({ kind: 'release' })).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed' }
    })
  })

  test('rejects non-serializable outbound values before JSON conversion', () => {
    const { encodeTauriWireValue } = require('../src/tauri/transport')

    expect(() => encodeTauriWireValue(new Date())).toThrow('protocol.malformed')
    expect(() => encodeTauriWireValue(1n)).toThrow('protocol.malformed')
    expect(() => encodeTauriWireValue(new Map())).toThrow('protocol.malformed')
  })

  test('rejects forbidden keys on encode and decode instead of mutating Object.prototype', () => {
    const { encodeTauriWireValue, decodeTauriWireValue } = require('../src/tauri/transport')
    const poisoned = JSON.parse('{"__proto__":{"polluted":true}}')
    const before = Object.prototype.polluted
    expect(() => encodeTauriWireValue(poisoned)).toThrow(/protocol\.malformed/)
    expect(() => decodeTauriWireValue(poisoned)).toThrow(/protocol\.malformed/)
    expect(Object.prototype.polluted).toBe(before)
  })

  test('round-trips nested route envelopes through encode then decode with null prototypes', () => {
    const { encodeTauriWireValue, decodeTauriWireValue } = require('../src/tauri/transport')
    const envelope = {
      command: 'connection.connect',
      payload: { peerId: 'peer-1', nested: { ok: true } }
    }
    const encoded = encodeTauriWireValue(envelope)
    expect(Object.getPrototypeOf(encoded)).toBe(null)
    expect(Object.getPrototypeOf(encoded.payload)).toBe(null)
    const decoded = decodeTauriWireValue(encoded)
    expect(Object.getPrototypeOf(decoded)).toBe(null)
    expect(decoded.command).toBe('connection.connect')
    expect(decoded.payload.peerId).toBe('peer-1')
    expect(decoded.payload.nested.ok).toBe(true)
  })

  test('publishes an explicit Tauri entrypoint without importing a radio backend', () => {
    const packageJson = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
    const entrypoint = fs.readFileSync(path.join(root, 'src', 'tauri.ts'), 'utf8')

    expect(packageJson.exports['./tauri']).toBeDefined()
    expect(entrypoint).not.toMatch(/node-(?:bluez|corebluetooth|winrt)/)
    expect(entrypoint).not.toMatch(/native\/electron/)
  })
})

function nestRecords(depth, leaf = { leaf: true }) {
  let value = leaf
  for (let remaining = depth; remaining > 1; remaining -= 1) {
    value = { nested: value }
  }
  return value
}

function assertProtocolMalformed(error) {
  expect(error).toMatchObject({
    normalized: { code: 'protocol.malformed', domain: 'ipc' }
  })
  expect(error).not.toBeInstanceOf(RangeError)
  expect(String(error)).not.toMatch(/Maximum call stack/i)
}

describe('Tauri wire codec budgets', () => {
  const {
    encodeTauriWireValue,
    decodeTauriWireValue,
    TauriBleIpcTransport,
    TAURI_WIRE_MAX_DEPTH,
    TAURI_WIRE_MAX_NODES,
    TAURI_WIRE_MAX_ARRAY_LENGTH,
    TAURI_WIRE_MAX_KEY_BYTES,
    TAURI_WIRE_MAX_TEXT_BYTES,
    TAURI_WIRE_MAX_BINARY_BYTES
  } = require('../src/tauri/transport')

  test('documents IPC-aligned traversal budgets', () => {
    expect(TAURI_WIRE_MAX_DEPTH).toBe(32)
    expect(TAURI_WIRE_MAX_NODES).toBe(16384)
    expect(TAURI_WIRE_MAX_ARRAY_LENGTH).toBe(65536)
    expect(TAURI_WIRE_MAX_KEY_BYTES).toBe(1024)
    expect(TAURI_WIRE_MAX_TEXT_BYTES).toBe(2 * 1024 * 1024)
    expect(TAURI_WIRE_MAX_BINARY_BYTES).toBe(2 * 1024 * 1024)
  })

  test('decodes the maximum inbound depth and rejects one extra nesting with protocol.malformed', () => {
    const atLimit = nestRecords(TAURI_WIRE_MAX_DEPTH)
    expect(decodeTauriWireValue(atLimit)).toEqual(atLimit)
    expect(encodeTauriWireValue(atLimit)).toEqual(atLimit)

    try {
      decodeTauriWireValue(nestRecords(TAURI_WIRE_MAX_DEPTH + 1))
      throw new Error('expected over-depth decode to fail')
    } catch (error) {
      assertProtocolMalformed(error)
    }
    try {
      encodeTauriWireValue(nestRecords(TAURI_WIRE_MAX_DEPTH + 1))
      throw new Error('expected over-depth encode to fail')
    } catch (error) {
      assertProtocolMalformed(error)
    }
  })

  test('rejects outbound object and array cycles with protocol.malformed', () => {
    const objectCycle = {}
    objectCycle.self = objectCycle
    const arrayCycle = []
    arrayCycle.push(arrayCycle)

    try {
      encodeTauriWireValue(objectCycle)
      throw new Error('expected cyclic object encode to fail')
    } catch (error) {
      assertProtocolMalformed(error)
    }
    try {
      encodeTauriWireValue(arrayCycle)
      throw new Error('expected cyclic array encode to fail')
    } catch (error) {
      assertProtocolMalformed(error)
    }
    try {
      decodeTauriWireValue(objectCycle)
      throw new Error('expected cyclic object decode to fail')
    } catch (error) {
      assertProtocolMalformed(error)
    }
  })

  test('rejects a shallow graph that exceeds the node budget', () => {
    const overBudget = {}
    for (let index = 0; index < TAURI_WIRE_MAX_NODES; index += 1) {
      overBudget[`k${index}`] = index
    }
    try {
      decodeTauriWireValue(overBudget)
      throw new Error('expected over-node decode to fail')
    } catch (error) {
      assertProtocolMalformed(error)
    }

    const atLimit = {}
    for (let index = 0; index < TAURI_WIRE_MAX_NODES - 1; index += 1) {
      atLimit[`k${index}`] = index
    }
    const decoded = decodeTauriWireValue(atLimit)
    expect(Object.keys(decoded)).toHaveLength(TAURI_WIRE_MAX_NODES - 1)
  })

  test('rejects aggregate tagged-byte payloads beyond the binary budget and accepts the exact limit', () => {
    const exact = { $__unifiedBleBytesV2: Array.from({ length: TAURI_WIRE_MAX_BINARY_BYTES }, () => 1) }
    const decodedExact = decodeTauriWireValue(exact)
    expect(decodedExact).toBeInstanceOf(Uint8Array)
    expect(decodedExact.byteLength).toBe(TAURI_WIRE_MAX_BINARY_BYTES)

    const overOne = { $__unifiedBleBytesV2: Array.from({ length: TAURI_WIRE_MAX_BINARY_BYTES + 1 }, () => 1) }
    try {
      decodeTauriWireValue(overOne)
      throw new Error('expected over-size tagged bytes to fail')
    } catch (error) {
      assertProtocolMalformed(error)
    }

    const manySmall = {
      a: { $__unifiedBleBytesV2: Array.from({ length: TAURI_WIRE_MAX_BINARY_BYTES / 2 }, () => 2) },
      b: { $__unifiedBleBytesV2: Array.from({ length: TAURI_WIRE_MAX_BINARY_BYTES / 2 }, () => 3) },
      c: { $__unifiedBleBytesV2: [4] }
    }
    try {
      decodeTauriWireValue(manySmall)
      throw new Error('expected cumulative tagged bytes to fail')
    } catch (error) {
      assertProtocolMalformed(error)
    }
  })

  test('rejects oversized strings, object keys, and arrays before schema validation', () => {
    try {
      decodeTauriWireValue({ text: 'x'.repeat(TAURI_WIRE_MAX_TEXT_BYTES + 1) })
      throw new Error('expected oversized string to fail')
    } catch (error) {
      assertProtocolMalformed(error)
    }
    try {
      decodeTauriWireValue({ ['k'.repeat(TAURI_WIRE_MAX_KEY_BYTES + 1)]: true })
      throw new Error('expected oversized key to fail')
    } catch (error) {
      assertProtocolMalformed(error)
    }
    try {
      decodeTauriWireValue(Array.from({ length: TAURI_WIRE_MAX_ARRAY_LENGTH + 1 }, () => 0))
      throw new Error('expected oversized array to fail')
    } catch (error) {
      assertProtocolMalformed(error)
    }
    expect(decodeTauriWireValue(Array.from({ length: 8 }, (_, index) => index))).toEqual([0, 1, 2, 3, 4, 5, 6, 7])
  })

  test('invoke responses, Channel events, and outbound routes share one budget authority', async () => {
    const channels = []
    class CapturedChannel extends FakeChannel {
      constructor() {
        super()
        channels.push(this)
      }
    }
    const overNested = nestRecords(TAURI_WIRE_MAX_DEPTH + 1, {
      kind: 'route',
      payload: {}
    })
    const invoke = jest.fn(async () => overNested)
    const transport = new TauriBleIpcTransport({ invoke, Channel: CapturedChannel })
    await expect(transport.invoke({ kind: 'route', envelope: { command: 'adapter.state' } })).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed', domain: 'ipc' }
    })

    const received = []
    transport.subscribe(event => received.push(event))
    expect(() => channels[0].emit(nestRecords(TAURI_WIRE_MAX_DEPTH + 1, { item: { ok: true } }))).toThrow(
      /protocol\.malformed/
    )
    expect(received).toEqual([])

    await expect(
      transport.invoke({ kind: 'route', envelope: nestRecords(TAURI_WIRE_MAX_DEPTH + 1, { command: 'adapter.state' }) })
    ).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed', domain: 'ipc' }
    })
  })

  test('package protocol fixtures still round-trip under the declared limits', () => {
    const request = createIpcBootstrapRequest()
    const encoded = encodeTauriWireValue(request)
    expect(decodeTauriWireValue(encoded)).toEqual(request)

    const notification = {
      kind: 'route',
      payload: {
        handle: 'subscription-1',
        value: { $__unifiedBleBytesV2: [1, 2, 3, 4] }
      }
    }
    expect(decodeTauriWireValue(encodeTauriWireValue(notification))).toEqual({
      kind: 'route',
      payload: {
        handle: 'subscription-1',
        value: new Uint8Array([1, 2, 3, 4])
      }
    })
  })
})

