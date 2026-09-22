// examples-shared/driver/server/client.mjs
//
// Control-side client for a running `cli.mjs serve`.

import { once } from 'node:events'

export const DEFAULT_CONTROL_URL = 'ws://127.0.0.1:8795/control'

export class DriverCommandError extends Error {
  constructor(error) {
    super(error.message)
    this.name = 'DriverCommandError'
    this.code = error.code
  }
}

export async function connectControl(url = DEFAULT_CONTROL_URL) {
  const socket = new WebSocket(url)
  const failed = once(socket, 'error').then(() => {
    throw new DriverCommandError({ code: 'driver.unreachable', message: `cannot reach the test driver at ${url}; start it with: node examples-shared/driver/server/cli.mjs serve` })
  })
  await Promise.race([once(socket, 'open'), failed])
  const waiting = new Map()
  const recordListeners = new Set()
  let nextRequest = 1
  let closed = false

  socket.addEventListener('message', event => {
    const message = JSON.parse(event.data)
    if (message.type === 'record') {
      for (const listener of recordListeners) listener(message.record)
      return
    }
    const entry = waiting.get(message.requestId)
    if (entry === undefined) {
      process.stderr.write(`[ubm-driver] unsolicited control reply: ${event.data}\n`)
      return
    }
    waiting.delete(message.requestId)
    if (message.type === 'control-error') entry.reject(new DriverCommandError(message.error))
    else entry.resolve(message)
  })
  socket.addEventListener('close', event => {
    closed = true
    for (const entry of waiting.values()) {
      entry.reject(new DriverCommandError({ code: 'driver.control-closed', message: `control connection closed (${event.code} ${event.reason})` }))
    }
    waiting.clear()
  })

  return {
    request(message) {
      if (closed) return Promise.reject(new DriverCommandError({ code: 'driver.control-closed', message: 'control connection is closed' }))
      const requestId = `r${nextRequest++}`
      return new Promise((resolve, reject) => {
        waiting.set(requestId, { resolve, reject })
        socket.send(JSON.stringify({ ...message, requestId }))
      })
    },
    onRecord(listener) {
      recordListeners.add(listener)
      return () => recordListeners.delete(listener)
    },
    async close() {
      if (closed) return
      socket.close()
      await once(socket, 'close')
    }
  }
}

/**
 * Dispatches one command to every host matching `target` and resolves with
 * each host's own answer; a host that does not answer within `timeoutMs`
 * yields `driver.command-timeout` rather than disappearing from the result.
 */
export async function runOnHosts(client, { target, scenario, command, args = {}, timeoutMs = 60_000 }) {
  const answers = new Map()
  const buffered = []
  const stop = client.onRecord(record => {
    const message = record.message
    if (message === undefined || (message.type !== 'result' && message.type !== 'error') || message.id === null) return
    buffered.push({ record, message })
    const entry = answers.get(message.id)
    if (entry !== undefined) entry.settle(record, message)
  })
  try {
    const startedAt = Date.now()
    const { commands } = await client.request({ type: 'run', target, scenario, command, args })
    const outcomes = commands.map(
      dispatched =>
        new Promise(resolve => {
          const timer = setTimeout(() => {
            resolve({ ...dispatched, ok: false, error: { code: 'driver.command-timeout', message: `no answer within ${timeoutMs} ms`, detail: null }, durationMs: Date.now() - startedAt })
          }, timeoutMs)
          const settle = (_record, message) => {
            clearTimeout(timer)
            resolve(
              message.type === 'result'
                ? { ...dispatched, ok: true, result: message.result, durationMs: Date.now() - startedAt }
                : { ...dispatched, ok: false, error: message.error, durationMs: Date.now() - startedAt }
            )
          }
          answers.set(dispatched.id, { settle })
          const early = buffered.find(entry => entry.message.id === dispatched.id)
          if (early !== undefined) settle(early.record, early.message)
        })
    )
    return await Promise.all(outcomes)
  } finally {
    stop()
  }
}
