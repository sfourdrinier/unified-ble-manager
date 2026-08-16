'use strict'

const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')

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
    const { TAURI_BLE_PLUGIN_COMMAND, TauriBleIpcTransport } = require('../src/tauri')
    const transport = new TauriBleIpcTransport({ invoke, Channel: CapturedChannel })
    const received = []
    const unsubscribe = transport.subscribe(event => received.push(event))

    const response = await transport.invoke({ kind: 'bootstrap' })
    channels[0].emit({ eventId: 'event-1', streamId: 'adapter', rendererLease: {}, item: { state: 'on' } })
    unsubscribe()
    channels[0].emit({ eventId: 'event-2', streamId: 'adapter', rendererLease: {}, item: { state: 'off' } })

    expect(TAURI_BLE_PLUGIN_COMMAND).toBe('plugin:unified-ble-manager|invoke')
    expect(response).toEqual({ kind: 'event.ack' })
    expect(channels).toHaveLength(1)
    expect(invocations).toEqual([
      {
        command: TAURI_BLE_PLUGIN_COMMAND,
        args: { request: { kind: 'bootstrap' }, eventChannel: channels[0] }
      }
    ])
    expect(received).toEqual([
      { eventId: 'event-1', streamId: 'adapter', rendererLease: {}, item: { state: 'on' } }
    ])
  })

  test('acknowledgements use the same versioned plugin command and channel', async () => {
    const invoke = jest.fn(async () => ({ kind: 'event.ack' }))
    const { TauriBleIpcTransport } = require('../src/tauri')
    const transport = new TauriBleIpcTransport({ invoke, Channel: FakeChannel })
    const lease = { leaseId: 'lease-1', generation: 'generation-1' }

    await expect(transport.acknowledge(lease, 'event-1')).resolves.toEqual({ kind: 'event.ack' })
    expect(invoke).toHaveBeenCalledWith('plugin:unified-ble-manager|invoke', {
      request: { kind: 'event.ack', rendererLease: lease, eventId: 'event-1' },
      eventChannel: expect.any(FakeChannel)
    })
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
          payload: { value: { $__unifiedBleBytesV1: [9, 8, 7] } }
        })
      )
    })
    const { TauriBleIpcTransport } = require('../src/tauri')
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
          rendererLease: {},
          item: { kind: 'value', value: { value: { $__unifiedBleBytesV1: [5, 6] } } }
        })
      )
    )

    expect(invocations[0].request.envelope.binaryPayload).toEqual({
      $__unifiedBleBytesV1: [0, 127, 255]
    })
    expect(invocations[0].request.envelope.payload.nested).toEqual({
      $__unifiedBleBytesV1: [3, 4]
    })
    expect(response.payload.value).toEqual(new Uint8Array([9, 8, 7]))
    expect(received[0].item.value.value).toEqual(new Uint8Array([5, 6]))
  })

  test('publishes an explicit Tauri entrypoint without importing a radio backend', () => {
    const packageJson = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
    const entrypoint = fs.readFileSync(path.join(root, 'src', 'tauri.ts'), 'utf8')

    expect(packageJson.exports['./tauri']).toBeDefined()
    expect(entrypoint).not.toMatch(/node-(?:bluez|corebluetooth|winrt)/)
    expect(entrypoint).not.toMatch(/native\/electron/)
  })
})
