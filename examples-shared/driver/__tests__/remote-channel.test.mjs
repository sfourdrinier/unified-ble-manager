import { test } from 'node:test'
import assert from 'node:assert/strict'
import { TEST_DRIVER_PROTOCOL } from '../protocol.ts'
import { ScenarioController, ScenarioRegistry, args, defineCommand } from '../scenario-core.ts'
import { RemoteDriverChannel } from '../remote-channel.ts'
import { DRIVER_PORT, driverUrlFromQuery, driverUrlFromScriptUrl, explicitDriverUrl } from '../driver-url.ts'
import { createFakeRuntime } from './fake-runtime.mjs'

class EchoScenario extends ScenarioController {
  id = 'echo'
  title = 'Echo'
  description = 'returns its arguments'
  commands = {
    echo: defineCommand({ label: 'Echo', description: '', parse: raw => raw, run: async raw => raw }),
    bump: defineCommand({
      label: 'Bump',
      description: '',
      parse: args.none,
      run: async () => {
        this.patch({ count: this.snapshot().count + 1 })
        this.emit('bumped', { count: this.snapshot().count })
        return null
      }
    }),
    fail: defineCommand({
      label: 'Fail',
      description: '',
      parse: args.none,
      run: async () => {
        throw Object.assign(new Error('not today'), { code: 'capability.unsupported' })
      }
    })
  }

  constructor(runtime) {
    super(runtime, { count: 0 })
  }
}

function harness({ url = 'ws://mac:8795/host', outboxLimit } = {}) {
  const runtime = createFakeRuntime('expo/ios')
  const scenario = new EchoScenario(runtime)
  const registry = new ScenarioRegistry([scenario])
  const sockets = []
  const channel = new RemoteDriverChannel({
    url,
    noHostReason: 'bundle not served by Metro',
    registry,
    runtime,
    identity: { host: 'expo', platform: 'ios', backend: 'expo/ios', model: 'iPhone', osVersion: '26.0', appBuild: { ubmVersion: '5.0.0' } },
    reconnect: { initialMs: 1_000, maxMs: 4_000 },
    outboxLimit,
    createSocket(socketUrl, handlers) {
      const socket = {
        url: socketUrl,
        handlers,
        sent: [],
        closed: false,
        send(text) {
          this.sent.push(JSON.parse(text))
        },
        close() {
          this.closed = true
        }
      }
      sockets.push(socket)
      return socket
    }
  })
  return { runtime, scenario, registry, sockets, channel }
}

const settle = () => new Promise(resolve => setImmediate(resolve))

test('opening sends a versioned hello with every scenario, then the current snapshots', () => {
  const { channel, sockets } = harness()
  channel.start()
  assert.equal(channel.state().status, 'connecting')
  assert.equal(sockets[0].url, 'ws://mac:8795/host')
  sockets[0].handlers.onOpen()
  const [hello, snapshot] = sockets[0].sent
  assert.equal(hello.type, 'hello')
  assert.equal(hello.protocol, TEST_DRIVER_PROTOCOL)
  assert.deepEqual([hello.host, hello.platform, hello.backend], ['expo', 'ios', 'expo/ios'])
  assert.deepEqual(
    hello.scenarios.map(scenario => scenario.id),
    ['echo']
  )
  assert.deepEqual(snapshot, { type: 'snapshot', scenario: 'echo', atMs: 0, host: 'expo/ios', snapshot: { count: 0 } })
  assert.equal(channel.state().status, 'connected')
})

test('welcome records the assigned host id', () => {
  const { channel, sockets } = harness()
  channel.start()
  sockets[0].handlers.onOpen()
  sockets[0].handlers.onMessage(JSON.stringify({ type: 'welcome', protocol: TEST_DRIVER_PROTOCOL, hostId: 'expo-ios-iphone' }))
  assert.equal(channel.state().hostId, 'expo-ios-iphone')
})

test('a command goes through registry dispatch and answers with its id, streaming events on the way', async () => {
  const { channel, sockets } = harness()
  channel.start()
  const socket = sockets[0]
  socket.handlers.onOpen()
  socket.sent.length = 0
  socket.handlers.onMessage(JSON.stringify({ type: 'command', id: 'c1', scenario: 'echo', command: 'bump', args: {} }))
  await settle()
  const types = socket.sent.map(message => (message.type === 'event' ? `event:${message.event.kind}` : message.type))
  assert.deepEqual(types, ['event:command', 'snapshot', 'event:bumped', 'event:command-result', 'snapshot', 'result'])
  const result = socket.sent.at(-1)
  assert.deepEqual({ ...result, atMs: 0 }, { type: 'result', id: 'c1', scenario: 'echo', command: 'bump', atMs: 0, result: null })
  assert.equal(channel.state().receivedCommands, 1)
})

test('failures answer with the typed error code: command failure, unknown scenario, invalid frames', async () => {
  const { channel, sockets } = harness()
  channel.start()
  const socket = sockets[0]
  socket.handlers.onOpen()
  socket.handlers.onMessage(JSON.stringify({ type: 'command', id: 'c2', scenario: 'echo', command: 'fail', args: {} }))
  socket.handlers.onMessage(JSON.stringify({ type: 'command', id: 'c3', scenario: 'nope', command: 'x', args: {} }))
  socket.handlers.onMessage('{garbage')
  socket.handlers.onMessage(42)
  await settle()
  const errors = socket.sent.filter(message => message.type === 'error')
  const byKey = pairs => pairs.map(pair => pair.join(' ')).sort()
  assert.deepEqual(
    byKey(errors.map(message => [message.id, message.error.code])),
    byKey([
      ['c2', 'capability.unsupported'],
      ['c3', 'scenario.unknown'],
      [null, 'protocol.invalid-json'],
      [null, 'protocol.invalid-frame']
    ])
  )
  assert.equal(errors.length, 4)
})

test('updates produced while offline are buffered, flushed after the next hello, and overflow is reported', async () => {
  const { channel, sockets, scenario, runtime } = harness({ outboxLimit: 3 })
  channel.start()
  sockets[0].handlers.onOpen()
  sockets[0].handlers.onClose(1006, 'wifi dropped')
  assert.equal(channel.state().status, 'waiting-to-reconnect')
  assert.equal(channel.state().reconnectInMs, 1_000)
  for (let index = 0; index < 5; index += 1) await scenario.dispatch('echo', { index })
  runtime.advance(1_000)
  assert.equal(sockets.length, 2)
  sockets[1].handlers.onOpen()
  const sent = sockets[1].sent
  assert.equal(sent[0].type, 'hello')
  const notice = sent.find(message => message.type === 'notice')
  assert.equal(notice.code, 'driver.outbox-overflow')
  assert.ok(notice.detail.dropped > 0)
  const flushed = sent.slice(sent.indexOf(notice) + 1)
  // the 3 newest buffered messages, then the current snapshot of every scenario
  assert.equal(flushed.at(-1).type, 'snapshot')
  assert.equal(flushed.length, 4)
  assert.equal(channel.state().droppedWhileOffline, notice.detail.dropped)
})

test('reconnect backs off exponentially up to the cap and resets after a successful open', () => {
  const { channel, sockets, runtime } = harness()
  channel.start()
  sockets[0].handlers.onError('connection refused')
  sockets[0].handlers.onClose(1006, '')
  assert.equal(channel.state().lastError, 'connection refused')
  assert.equal(channel.state().reconnectInMs, 1_000)
  runtime.advance(1_000)
  sockets[1].handlers.onClose(1006, '')
  assert.equal(channel.state().reconnectInMs, 2_000)
  runtime.advance(2_000)
  sockets[2].handlers.onClose(1006, '')
  assert.equal(channel.state().reconnectInMs, 4_000)
  runtime.advance(4_000)
  sockets[3].handlers.onClose(1006, '')
  assert.equal(channel.state().reconnectInMs, 4_000)
  runtime.advance(4_000)
  sockets[4].handlers.onOpen()
  sockets[4].handlers.onClose(1000, 'server restart')
  assert.equal(channel.state().reconnectInMs, 1_000)
})

test('events from a replaced socket are ignored and stop() closes without reconnecting', () => {
  const { channel, sockets, runtime } = harness()
  channel.start()
  sockets[0].handlers.onClose(1006, '')
  runtime.advance(1_000)
  sockets[0].handlers.onOpen()
  assert.equal(channel.state().status, 'connecting')
  sockets[1].handlers.onOpen()
  channel.stop()
  assert.equal(sockets[1].closed, true)
  assert.equal(channel.state().status, 'stopped')
  sockets[1].handlers.onClose(1000, '')
  runtime.advance(10_000)
  assert.equal(sockets.length, 2)
})

test('without a driver url the channel reports no-host and opens nothing', () => {
  const { channel, sockets } = harness({ url: null })
  channel.start()
  assert.deepEqual(
    { status: channel.state().status, lastError: channel.state().lastError },
    { status: 'no-host', lastError: 'bundle not served by Metro' }
  )
  assert.equal(sockets.length, 0)
})

test('driver url is derived from the host that served the code, on the /host path', () => {
  assert.equal(DRIVER_PORT, 8795)
  assert.equal(
    driverUrlFromScriptUrl('http://192.168.1.20:8081/index.bundle?platform=ios&dev=true', DRIVER_PORT),
    'ws://192.168.1.20:8795/host'
  )
  assert.equal(driverUrlFromScriptUrl('http://localhost:8081/node_modules/expo/AppEntry.bundle', 9000), 'ws://localhost:9000/host')
  assert.equal(driverUrlFromScriptUrl('https://[fe80::1]:8081/x.bundle'), 'ws://[fe80::1]:8795/host')
  assert.equal(driverUrlFromScriptUrl('file:///var/containers/main.jsbundle', 8795), null)
  assert.equal(driverUrlFromScriptUrl(null, 8795), null)
})

test('an explicit driver url is used as given, "off" disables, a malformed one is refused with its reason', () => {
  assert.deepEqual(driverUrlFromQuery('?driver=ws://10.0.0.2:8795/host'), { url: 'ws://10.0.0.2:8795/host', reason: '?driver' })
  assert.equal(driverUrlFromQuery('?other=1'), null)
  assert.deepEqual(explicitDriverUrl('off', 'UBM_DRIVER_URL'), { url: null, reason: 'UBM_DRIVER_URL=off' })
  const refused = explicitDriverUrl('http://mac:8795/host', 'UBM_DRIVER_URL')
  assert.equal(refused.url, null)
  assert.match(refused.reason, /not a ws:\/\/ or wss:\/\/ URL/)
  assert.equal(explicitDriverUrl(undefined, 'X'), null)
})
