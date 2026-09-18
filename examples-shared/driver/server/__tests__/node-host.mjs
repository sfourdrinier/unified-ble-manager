// A host stand-in that runs the real shared RemoteDriverChannel and scenario
// registry in Node, so hub tests exercise the exact code every host ships.

import { ScenarioController, ScenarioRegistry, args, defineCommand } from '../../scenario-core.ts'
import { RemoteDriverChannel } from '../../remote-channel.ts'

export class DemoScenario extends ScenarioController {
  id = 'demo'
  title = 'Demo'
  description = 'hub test scenario'
  commands = {
    start: defineCommand({
      label: 'Start',
      description: 'emits ticks',
      parse: raw => ({ ticks: args.number(raw, 'ticks', 3, { min: 0 }), intervalMs: args.number(raw, 'intervalMs', 5, { min: 1 }) }),
      run: async ({ ticks, intervalMs }) => {
        this.patch({ phase: 'running', ticks: 0 })
        for (let index = 1; index <= ticks; index += 1) {
          await new Promise(resolve => setTimeout(resolve, intervalMs))
          this.patch({ ticks: index })
          this.emit('tick', { n: index })
        }
        this.patch({ phase: 'done' })
        return { ticks }
      }
    }),
    unsupported: defineCommand({
      label: 'Unsupported',
      description: 'fails like an iOS when-available connect',
      parse: args.none,
      run: async () => {
        throw Object.assign(new Error('when-available is Android-only'), { code: 'capability.unsupported' })
      }
    }),
    hang: defineCommand({ label: 'Hang', description: 'never answers', parse: args.none, run: () => new Promise(() => {}) }),
    pick: defineCommand({
      label: 'Pick',
      description: 'acquires a peer like the BLE scenarios: echoes the device it was asked for',
      acceptsDevice: true,
      parse: raw => ({ device: args.optionalString(raw, 'device') }),
      run: async ({ device }) => {
        const peer = { id: `peer-${device ?? 'any'}`, name: device ?? 'Polar H10 0000' }
        this.patch({ device: peer.name })
        return { peer }
      }
    }),
    stop: defineCommand({ label: 'Stop', description: 'takes no arguments', parse: args.none, run: async () => ({ stopped: true }) })
  }

  constructor(runtime) {
    super(runtime, { phase: 'idle', ticks: 0 })
  }
}

export function nodeRuntime(label) {
  return {
    host: label,
    now: () => performance.now(),
    schedule(callback, delayMs) {
      const handle = setTimeout(callback, delayMs)
      return () => clearTimeout(handle)
    },
    log() {}
  }
}

export function nodeSocketFactory(url, handlers) {
  const socket = new WebSocket(url)
  socket.addEventListener('open', () => handlers.onOpen())
  socket.addEventListener('message', event => handlers.onMessage(event.data))
  socket.addEventListener('close', event => handlers.onClose(event.code, event.reason))
  socket.addEventListener('error', () => handlers.onError('socket error'))
  return { send: text => socket.send(text), close: () => socket.close() }
}

export function startNodeHost({ port, host = 'expo', platform, model = 'Test Host', reconnect }) {
  const runtime = nodeRuntime(`${host}/${platform}`)
  const registry = new ScenarioRegistry([new DemoScenario(runtime)])
  const channel = new RemoteDriverChannel({
    url: `ws://127.0.0.1:${port}/host`,
    noHostReason: 'unused',
    registry,
    runtime,
    identity: { host, platform, backend: `${host}/test`, model, osVersion: '1', appBuild: { test: true } },
    reconnect: reconnect ?? { initialMs: 50, maxMs: 200 },
    createSocket: nodeSocketFactory
  })
  channel.start()
  return { channel, registry }
}

export function waitFor(predicate, timeoutMs = 2_000) {
  return new Promise((resolve, reject) => {
    const started = Date.now()
    const poll = () => {
      const value = predicate()
      if (value) return resolve(value)
      if (Date.now() - started > timeoutMs) return reject(new Error('waitFor timed out'))
      setTimeout(poll, 10)
    }
    poll()
  })
}
