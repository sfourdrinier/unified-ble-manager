// examples-shared/driver/remote-channel.ts
//
// Outbound link from any host to the control server. Commands arrive as
// `{id, scenario, command, args}` and run through `ScenarioRegistry.dispatch`,
// the same call the host's own scenario buttons make.

import type { AppMessage, DriverError, HostKind, JsonObject } from './protocol.ts'
import { TEST_DRIVER_PROTOCOL, decodeServerMessage, describeError, encodeMessage } from './protocol.ts'
import type { ScenarioRegistry, ScenarioRuntime, ScenarioUpdate } from './scenario-core.ts'

export interface DriverSocket {
  send(text: string): void
  close(): void
}

export interface DriverSocketHandlers {
  onOpen(): void
  onMessage(data: unknown): void
  onClose(code: number, reason: string): void
  onError(message: string): void
}

export type DriverSocketFactory = (url: string, handlers: DriverSocketHandlers) => DriverSocket

export type RemoteDriverStatus = 'idle' | 'no-host' | 'connecting' | 'connected' | 'waiting-to-reconnect' | 'stopped'

export type RemoteDriverState = {
  readonly status: RemoteDriverStatus
  readonly url: string | null
  readonly hostId: string | null
  readonly lastError: string | null
  readonly reconnectInMs: number | null
  readonly receivedCommands: number
  readonly droppedWhileOffline: number
}

/** What the hello announces: the adapter's own facts, never guessed by the shared code. */
export interface RemoteDriverHost {
  readonly host: HostKind
  readonly platform: string
  readonly backend: string
  readonly model: string
  readonly osVersion: string
  readonly appBuild: JsonObject
}

export interface RemoteDriverOptions {
  readonly url: string | null
  readonly noHostReason: string
  readonly registry: ScenarioRegistry
  readonly runtime: ScenarioRuntime
  readonly identity: RemoteDriverHost
  readonly createSocket: DriverSocketFactory
  readonly reconnect?: { readonly initialMs: number; readonly maxMs: number }
  readonly outboxLimit?: number
}

const DEFAULT_RECONNECT = { initialMs: 1_000, maxMs: 10_000 }
const DEFAULT_OUTBOX_LIMIT = 2_000
const LOG_SCOPE = 'remote-driver'

export class RemoteDriverChannel {
  private readonly options: RemoteDriverOptions
  private readonly reconnect: { readonly initialMs: number; readonly maxMs: number }
  private readonly outboxLimit: number
  private readonly listeners = new Set<(state: RemoteDriverState) => void>()
  private current: RemoteDriverState
  private socket: DriverSocket | null = null
  private open = false
  private nextDelayMs: number
  private cancelReconnect: (() => void) | null = null
  private unsubscribeRegistry: (() => void) | null = null
  private outbox: string[] = []
  private droppedSinceLastFlush = 0

  constructor(options: RemoteDriverOptions) {
    this.options = options
    this.reconnect = options.reconnect ?? DEFAULT_RECONNECT
    this.outboxLimit = options.outboxLimit ?? DEFAULT_OUTBOX_LIMIT
    this.nextDelayMs = this.reconnect.initialMs
    this.current = {
      status: 'idle',
      url: options.url,
      hostId: null,
      lastError: null,
      reconnectInMs: null,
      receivedCommands: 0,
      droppedWhileOffline: 0
    }
  }

  state(): RemoteDriverState {
    return this.current
  }

  subscribe(listener: (state: RemoteDriverState) => void): () => void {
    this.listeners.add(listener)
    return () => {
      this.listeners.delete(listener)
    }
  }

  start(): void {
    if (this.current.status !== 'idle' && this.current.status !== 'stopped') return
    if (this.options.url === null) {
      this.setState({ status: 'no-host', lastError: this.options.noHostReason })
      this.options.runtime.log(LOG_SCOPE, 'remote driver disabled', { reason: this.options.noHostReason })
      return
    }
    this.unsubscribeRegistry = this.options.registry.subscribe(update => this.forward(update))
    this.connect(this.options.url)
  }

  stop(): void {
    this.cancelReconnect?.()
    this.cancelReconnect = null
    this.unsubscribeRegistry?.()
    this.unsubscribeRegistry = null
    const socket = this.socket
    this.socket = null
    this.open = false
    socket?.close()
    this.setState({ status: 'stopped', reconnectInMs: null })
  }

  private connect(url: string): void {
    this.setState({ status: 'connecting', reconnectInMs: null })
    let socket: DriverSocket | null = null
    const isCurrent = (): boolean => socket !== null && this.socket === socket
    socket = this.options.createSocket(url, {
      onOpen: () => {
        if (isCurrent()) this.handleOpen()
        else this.options.runtime.log(LOG_SCOPE, 'ignored open from a replaced socket')
      },
      onMessage: data => {
        if (isCurrent()) this.handleMessage(data)
        else this.options.runtime.log(LOG_SCOPE, 'ignored message from a replaced socket')
      },
      onClose: (code, reason) => {
        if (isCurrent()) this.handleClose(code, reason)
      },
      onError: message => {
        if (!isCurrent()) return
        this.options.runtime.log(LOG_SCOPE, 'socket error', { message })
        this.setState({ lastError: message })
      }
    })
    this.socket = socket
  }

  private handleOpen(): void {
    this.open = true
    this.nextDelayMs = this.reconnect.initialMs
    const { identity, registry, runtime } = this.options
    this.sendNow({
      type: 'hello',
      protocol: TEST_DRIVER_PROTOCOL,
      host: identity.host,
      platform: identity.platform,
      backend: identity.backend,
      model: identity.model,
      osVersion: identity.osVersion,
      appBuild: identity.appBuild,
      scenarios: registry.describe()
    })
    if (this.droppedSinceLastFlush > 0) {
      this.sendNow({
        type: 'notice',
        atMs: runtime.now(),
        code: 'driver.outbox-overflow',
        message: `dropped ${this.droppedSinceLastFlush.toString()} oldest message(s) while the control server was unreachable`,
        detail: { dropped: this.droppedSinceLastFlush, outboxLimit: this.outboxLimit }
      })
      this.droppedSinceLastFlush = 0
    }
    const pending = this.outbox
    this.outbox = []
    for (const text of pending) this.socket?.send(text)
    for (const scenario of registry.list()) {
      this.sendNow({ type: 'snapshot', scenario: scenario.id, atMs: runtime.now(), host: runtime.host, snapshot: scenario.snapshot() })
    }
    this.setState({ status: 'connected', lastError: null })
  }

  private handleClose(code: number, reason: string): void {
    this.socket = null
    this.open = false
    const url = this.options.url
    if (url === null) return
    const delayMs = this.nextDelayMs
    this.nextDelayMs = Math.min(this.nextDelayMs * 2, this.reconnect.maxMs)
    this.options.runtime.log(LOG_SCOPE, 'socket closed', { code, reason, reconnectInMs: delayMs })
    this.setState({
      status: 'waiting-to-reconnect',
      reconnectInMs: delayMs,
      hostId: null,
      lastError: this.current.lastError ?? (reason.length > 0 ? `closed ${code.toString()}: ${reason}` : `closed ${code.toString()}`)
    })
    this.cancelReconnect = this.options.runtime.schedule(() => {
      this.cancelReconnect = null
      this.connect(url)
    }, delayMs)
  }

  private handleMessage(data: unknown): void {
    if (typeof data !== 'string') {
      this.reportProtocolError({ code: 'protocol.invalid-frame', message: `expected a text frame, received ${typeof data}`, detail: null })
      return
    }
    const decoded = decodeServerMessage(data)
    if (!decoded.ok) {
      this.reportProtocolError(decoded.error)
      return
    }
    const message = decoded.message
    if (message.type === 'welcome') {
      this.setState({ hostId: message.hostId })
      return
    }
    this.setState({ receivedCommands: this.current.receivedCommands + 1 })
    const { id, scenario, command, args } = message
    void this.options.registry.dispatch(scenario, command, args).then(
      result => this.send({ type: 'result', id, scenario, command, atMs: this.options.runtime.now(), result }),
      error => this.send({ type: 'error', id, scenario, command, atMs: this.options.runtime.now(), error: describeError(error) })
    )
  }

  private reportProtocolError(error: DriverError): void {
    this.options.runtime.log(LOG_SCOPE, 'protocol error', error)
    this.send({ type: 'error', id: null, scenario: null, command: null, atMs: this.options.runtime.now(), error })
  }

  private forward(update: ScenarioUpdate): void {
    this.send(update.type === 'event' ? { type: 'event', event: update.event } : update)
  }

  private send(message: AppMessage): void {
    const text = encodeMessage(message)
    if (this.open && this.socket !== null) {
      this.socket.send(text)
      return
    }
    this.outbox.push(text)
    if (this.outbox.length > this.outboxLimit) {
      const dropped = this.outbox.length - this.outboxLimit
      this.outbox = this.outbox.slice(dropped)
      this.droppedSinceLastFlush += dropped
      this.setState({ droppedWhileOffline: this.current.droppedWhileOffline + dropped })
    }
  }

  private sendNow(message: AppMessage): void {
    this.socket?.send(encodeMessage(message))
  }

  private setState(patch: Partial<RemoteDriverState>): void {
    this.current = { ...this.current, ...patch }
    for (const listener of this.listeners) listener(this.current)
  }
}
