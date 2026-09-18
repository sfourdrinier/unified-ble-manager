// examples-shared/driver/server/hub.mjs
//
// The control server of the cross-host test driver: hosts connect on /host,
// CLIs on /control.
// Every message in either direction becomes a record that is streamed to all
// control clients and to the serve process's log, so nothing happens off the
// record.

import { createServer } from 'node:http'
import { once } from 'node:events'
import { acceptWebSocket } from './websocket.mjs'
import { matchesTarget } from './targets.mjs'
import { TEST_DRIVER_PROTOCOL, decodeAppMessage, encodeMessage } from '../protocol.ts'

export const DEFAULT_DRIVER_PORT = 8795
const CLOSE_PROTOCOL_ERROR = 4400
const CLOSE_HELLO_TIMEOUT = 4408

export function createHub({
  port = DEFAULT_DRIVER_PORT,
  host = '0.0.0.0',
  onRecord = () => {},
  helloTimeoutMs = 10_000,
  heartbeatMs = 20_000
} = {}) {
  const hosts = new Map()
  const controls = new Set()
  const pending = new Map()
  let nextCommand = 1

  const server = createServer((request, response) => {
    if (request.method === 'GET' && request.url === '/hosts') {
      response.writeHead(200, { 'content-type': 'application/json' })
      response.end(JSON.stringify(listHosts()))
      return
    }
    response.writeHead(404, { 'content-type': 'text/plain' })
    response.end('host driver: WebSocket /host (apps) and /control (CLI); GET /hosts\n')
  })

  server.on('upgrade', (request, socket, head) => {
    const path = new URL(request.url ?? '/', 'http://driver').pathname
    if (path !== '/host' && path !== '/control') {
      record({ direction: 'hub', event: 'upgrade-refused', remote: `${request.socket.remoteAddress}:${request.socket.remotePort}`, detail: { path, expected: ['/host', '/control'] } })
      socket.end('HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n')
      return
    }
    const connection = acceptWebSocket(request, socket, head)
    if (connection === null) return
    const remote = `${request.socket.remoteAddress}:${request.socket.remotePort}`
    connection.on('error', error => record({ direction: 'hub', event: 'socket-error', remote, detail: { message: error.message } }))
    connection.on('protocol-error', detail => record({ direction: 'hub', event: 'websocket-protocol-error', remote, detail }))
    if (path === '/host') acceptHost(connection, remote)
    else acceptControl(connection)
  })

  function record(entry) {
    const full = { t: new Date().toISOString(), ...entry }
    onRecord(full)
    const text = JSON.stringify({ type: 'record', record: full })
    for (const control of controls) {
      if (control.open) control.send(text)
    }
    return full
  }

  function who(host) {
    return { hostId: host.hostId, host: host.hello.host, platform: host.hello.platform }
  }

  function summary(host) {
    return {
      hostId: host.hostId,
      host: host.hello.host,
      platform: host.hello.platform,
      backend: host.hello.backend,
      model: host.hello.model,
      osVersion: host.hello.osVersion,
      appBuild: host.hello.appBuild,
      remote: host.remote,
      connectedAt: host.connectedAt,
      scenarios: host.hello.scenarios
    }
  }

  function listHosts() {
    return [...hosts.values()].map(summary)
  }

  function assignHostId(hello) {
    const base = `${hello.host}-${hello.platform}-${hello.model}`.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '')
    let candidate = base
    for (let suffix = 2; hosts.has(candidate); suffix += 1) candidate = `${base}-${suffix}`
    return candidate
  }

  function acceptHost(connection, remote) {
    let host = null
    const helloTimer = setTimeout(() => {
      record({ direction: 'hub', event: 'host-rejected', remote, detail: { code: 'driver.hello-timeout' } })
      connection.close(CLOSE_HELLO_TIMEOUT, 'driver.hello-timeout')
    }, helloTimeoutMs)

    connection.on('message', text => {
      const decoded = decodeAppMessage(text)
      if (host === null) {
        if (!decoded.ok || decoded.message.type !== 'hello') {
          clearTimeout(helloTimer)
          const error = decoded.ok ? { code: 'protocol.hello-required', message: `first message was ${decoded.message.type}` } : decoded.error
          record({ direction: 'hub', event: 'host-rejected', remote, detail: error })
          connection.close(CLOSE_PROTOCOL_ERROR, error.code)
          return
        }
        clearTimeout(helloTimer)
        const hello = decoded.message
        host = { hostId: assignHostId(hello), hello, connection, remote, connectedAt: new Date().toISOString(), snapshots: new Map(), awaitingPong: false }
        hosts.set(host.hostId, host)
        connection.send(encodeMessage({ type: 'welcome', protocol: TEST_DRIVER_PROTOCOL, hostId: host.hostId }))
        record({ direction: 'hub', event: 'host-connected', ...who(host), detail: summary(host) })
        return
      }
      if (!decoded.ok) {
        record({ direction: 'from-host', ...who(host), invalid: decoded.error, raw: text.slice(0, 2_000) })
        return
      }
      const message = decoded.message
      if (message.type === 'snapshot') host.snapshots.set(message.scenario, { atMs: message.atMs, snapshot: message.snapshot })
      if ((message.type === 'result' || message.type === 'error') && message.id !== null) pending.delete(message.id)
      record({ direction: 'from-host', ...who(host), message })
    })

    connection.on('pong', () => {
      if (host !== null) host.awaitingPong = false
    })

    connection.on('close', ({ code, reason }) => {
      clearTimeout(helloTimer)
      if (host === null) return
      if (hosts.get(host.hostId) === host) hosts.delete(host.hostId)
      record({ direction: 'hub', event: 'host-disconnected', ...who(host), detail: { code, reason } })
      for (const [id, entry] of pending) {
        if (entry.host !== host) continue
        pending.delete(id)
        record({
          direction: 'hub',
          ...who(host),
          message: {
            type: 'error',
            id,
            scenario: entry.scenario,
            command: entry.command,
            atMs: null,
            error: { code: 'driver.host-disconnected', message: `host disconnected (${code} ${reason}) before answering`, detail: null }
          }
        })
      }
    })
  }

  function resolveTarget(target) {
    const all = [...hosts.values()]
    if (target === 'all') return all
    const exact = hosts.get(target)
    if (exact !== undefined) return [exact]
    return all.filter(host => matchesTarget(summary(host), target))
  }

  function acceptControl(connection) {
    controls.add(connection)
    connection.on('close', () => controls.delete(connection))
    connection.on('message', text => {
      let request
      try {
        request = JSON.parse(text)
      } catch (error) {
        connection.send(JSON.stringify({ type: 'control-error', requestId: null, error: { code: 'protocol.invalid-json', message: error.message } }))
        return
      }
      const reply = message => connection.send(JSON.stringify({ requestId: request.requestId ?? null, ...message }))
      const controlError = (code, message) => reply({ type: 'control-error', error: { code, message } })
      switch (request.type) {
        case 'list':
          reply({ type: 'hosts', hosts: listHosts() })
          return
        case 'snapshots': {
          const targets = resolveTarget(request.target ?? 'all')
          reply({
            type: 'snapshots',
            hosts: targets.map(host => ({ ...who(host), snapshots: Object.fromEntries(host.snapshots) }))
          })
          return
        }
        case 'run': {
          const { target, scenario, command } = request
          const args = request.args ?? {}
          if (typeof target !== 'string' || typeof scenario !== 'string' || typeof command !== 'string') {
            controlError('control.invalid-request', 'run requires string target, scenario and command')
            return
          }
          if (typeof args !== 'object' || args === null || Array.isArray(args)) {
            controlError('control.invalid-request', 'run args must be a JSON object')
            return
          }
          const targets = resolveTarget(target)
          if (targets.length === 0) {
            controlError('driver.no-matching-host', `no connected host matches "${target}"; connected: ${[...hosts.keys()].join(', ') || 'none'}`)
            return
          }
          const commands = targets.map(host => {
            const id = `c${nextCommand++}-${host.hostId}`
            pending.set(id, { host, scenario, command })
            const message = { type: 'command', id, scenario, command, args }
            host.connection.send(encodeMessage(message))
            record({ direction: 'to-host', ...who(host), message })
            return { id, ...who(host) }
          })
          reply({ type: 'dispatched', commands })
          return
        }
        default:
          controlError('control.unknown-request', `unknown control request type ${JSON.stringify(request.type ?? null)}`)
      }
    })
  }

  const heartbeat = setInterval(() => {
    for (const host of hosts.values()) {
      if (host.awaitingPong) {
        record({ direction: 'hub', event: 'heartbeat-timeout', ...who(host), detail: { heartbeatMs } })
        host.connection.terminate(1006, 'driver.heartbeat-timeout')
        continue
      }
      host.awaitingPong = true
      host.connection.ping()
    }
  }, heartbeatMs)
  heartbeat.unref()

  return {
    async listen() {
      server.listen(port, host)
      await once(server, 'listening')
      const address = server.address()
      return { port: address.port, host }
    },
    async close() {
      clearInterval(heartbeat)
      for (const host of hosts.values()) host.connection.close(1001, 'driver shutting down')
      for (const control of controls) control.close(1001, 'driver shutting down')
      server.closeAllConnections()
      await new Promise(resolve => server.close(() => resolve()))
    },
    hosts: listHosts
  }
}
