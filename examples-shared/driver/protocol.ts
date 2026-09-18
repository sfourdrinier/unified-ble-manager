// examples-shared/driver/protocol.ts
//
// Wire contract of the cross-host test driver, shared verbatim by every host
// (Metro, Vite, Node) and by the control server (Node type stripping). Keep
// this file free of imports and of non-erasable TypeScript syntax so every
// host loads it as is. Versioned and fail-closed: a peer that speaks another
// version, or names a host kind this version does not know, is refused.

export const TEST_DRIVER_PROTOCOL = 'ubm-test-driver/1'

/** Every runtime that can host the scenarios. A new host is a protocol change. */
export const HOST_KINDS = ['expo', 'web', 'tauri', 'electron', 'node'] as const
export type HostKind = (typeof HOST_KINDS)[number]

export type JsonValue = null | boolean | number | string | readonly JsonValue[] | JsonObject
export type JsonObject = { readonly [key: string]: JsonValue }

export type DriverError = {
  readonly code: string
  readonly message: string
  readonly detail: JsonValue
}

export type ScenarioEvent = {
  readonly scenario: string
  readonly seq: number
  readonly atMs: number
  /** The emitting runtime's label, `<host>/<platform>` (for example `expo/android`). */
  readonly host: string
  readonly kind: string
  readonly data: JsonObject
}

export type CommandPreset = {
  readonly label: string
  readonly args: JsonObject
}

export type CommandDescription = {
  readonly name: string
  readonly label: string
  readonly description: string
  readonly presets: readonly CommandPreset[]
  /** The command acquires a peer and takes a `device` argument (exact name, or a prefix ending in `*`). */
  readonly acceptsDevice: boolean
}

export type ScenarioDescription = {
  readonly id: string
  readonly title: string
  readonly description: string
  readonly commands: readonly CommandDescription[]
}

export type HelloMessage = {
  readonly type: 'hello'
  readonly protocol: string
  readonly host: HostKind
  /** Operating system: android | ios | macos | windows | linux | unknown. */
  readonly platform: string
  /** The radio stack the host adapter constructed (for example `node/corebluetooth`). */
  readonly backend: string
  readonly model: string
  readonly osVersion: string
  readonly appBuild: JsonObject
  readonly scenarios: readonly ScenarioDescription[]
}

export type SnapshotMessage = {
  readonly type: 'snapshot'
  readonly scenario: string
  readonly atMs: number
  readonly host: string
  readonly snapshot: JsonObject
}

export type EventMessage = {
  readonly type: 'event'
  readonly event: ScenarioEvent
}

export type ResultMessage = {
  readonly type: 'result'
  readonly id: string
  readonly scenario: string
  readonly command: string
  readonly atMs: number
  readonly result: JsonValue
}

export type ErrorMessage = {
  readonly type: 'error'
  readonly id: string | null
  readonly scenario: string | null
  readonly command: string | null
  readonly atMs: number
  readonly error: DriverError
}

export type NoticeMessage = {
  readonly type: 'notice'
  readonly atMs: number
  readonly code: string
  readonly message: string
  readonly detail: JsonValue
}

export type AppMessage = HelloMessage | SnapshotMessage | EventMessage | ResultMessage | ErrorMessage | NoticeMessage

export type WelcomeMessage = {
  readonly type: 'welcome'
  readonly protocol: string
  readonly hostId: string
}

export type CommandMessage = {
  readonly type: 'command'
  readonly id: string
  readonly scenario: string
  readonly command: string
  readonly args: JsonObject
}

export type ServerMessage = WelcomeMessage | CommandMessage

export type DecodeResult<Message> =
  | { readonly ok: true; readonly message: Message }
  | { readonly ok: false; readonly error: DriverError }

export function encodeMessage(message: AppMessage | ServerMessage): string {
  return JSON.stringify(message)
}

export function decodeServerMessage(text: string): DecodeResult<ServerMessage> {
  const parsed = parseObject(text)
  if (!parsed.ok) return parsed
  const value = parsed.message
  if (value.type === 'welcome') {
    if (typeof value.protocol !== 'string' || typeof value.hostId !== 'string') {
      return invalid('welcome requires string protocol and hostId', value)
    }
    if (value.protocol !== TEST_DRIVER_PROTOCOL) return protocolMismatch(value.protocol)
    return { ok: true, message: { type: 'welcome', protocol: value.protocol, hostId: value.hostId } }
  }
  if (value.type === 'command') {
    const { id, scenario, command, args } = value
    if (typeof id !== 'string' || id.length === 0) return invalid('command requires a non-empty string id', value)
    if (typeof scenario !== 'string' || typeof command !== 'string') {
      return invalid('command requires string scenario and command', value)
    }
    const commandArgs = args === undefined ? {} : args
    if (!isJsonObject(commandArgs)) return invalid('command args must be a JSON object', value)
    return { ok: true, message: { type: 'command', id, scenario, command, args: commandArgs } }
  }
  return invalid(`unknown server message type ${JSON.stringify(value.type ?? null)}`, value)
}

export function decodeAppMessage(text: string): DecodeResult<AppMessage> {
  const parsed = parseObject(text)
  if (!parsed.ok) return parsed
  const value = parsed.message
  switch (value.type) {
    case 'hello': {
      const { protocol, host, platform, backend, model, osVersion, appBuild, scenarios } = value
      if (typeof protocol !== 'string') return invalid('hello requires a protocol', value)
      if (protocol !== TEST_DRIVER_PROTOCOL) return protocolMismatch(protocol)
      const hostKind = HOST_KINDS.find(kind => kind === host)
      if (hostKind === undefined) return invalid(`hello host must be one of ${HOST_KINDS.join(' | ')}`, value)
      if (typeof platform !== 'string' || typeof backend !== 'string' || typeof model !== 'string' || typeof osVersion !== 'string') {
        return invalid('hello requires string platform, backend, model and osVersion', value)
      }
      if (!isJsonObject(appBuild)) return invalid('hello appBuild must be an object', value)
      if (!Array.isArray(scenarios) || !scenarios.every(isScenarioDescription)) {
        return invalid('hello scenarios must be scenario descriptions', value)
      }
      return {
        ok: true,
        message: { type: 'hello', protocol, host: hostKind, platform, backend, model, osVersion, appBuild, scenarios: scenarios.filter(isScenarioDescription) }
      }
    }
    case 'snapshot': {
      const { scenario, atMs, host, snapshot } = value
      if (typeof scenario !== 'string' || typeof atMs !== 'number' || typeof host !== 'string') {
        return invalid('snapshot requires scenario, atMs and host', value)
      }
      if (!isJsonObject(snapshot)) return invalid('snapshot payload must be an object', value)
      return { ok: true, message: { type: 'snapshot', scenario, atMs, host, snapshot } }
    }
    case 'event': {
      const event = value.event
      if (!isScenarioEvent(event)) return invalid('event payload is not a scenario event', value)
      return { ok: true, message: { type: 'event', event } }
    }
    case 'result': {
      const { id, scenario, command, atMs } = value
      if (typeof id !== 'string' || typeof scenario !== 'string' || typeof command !== 'string' || typeof atMs !== 'number') {
        return invalid('result requires id, scenario, command and atMs', value)
      }
      if (!('result' in value)) return invalid('result requires a result field', value)
      return { ok: true, message: { type: 'result', id, scenario, command, atMs, result: toJsonValue(value.result) } }
    }
    case 'error': {
      const { id, scenario, command, atMs, error } = value
      if (!isNullableString(id) || !isNullableString(scenario) || !isNullableString(command) || typeof atMs !== 'number') {
        return invalid('error requires nullable id, scenario, command and numeric atMs', value)
      }
      if (!isRecord(error) || typeof error.code !== 'string' || typeof error.message !== 'string') {
        return invalid('error requires {code, message}', value)
      }
      return {
        ok: true,
        message: { type: 'error', id, scenario, command, atMs, error: { code: error.code, message: error.message, detail: toJsonValue(error.detail) } }
      }
    }
    case 'notice': {
      const { atMs, code, message } = value
      if (typeof atMs !== 'number' || typeof code !== 'string' || typeof message !== 'string') {
        return invalid('notice requires atMs, code and message', value)
      }
      return { ok: true, message: { type: 'notice', atMs, code, message, detail: toJsonValue(value.detail) } }
    }
    default:
      return invalid(`unknown app message type ${JSON.stringify(value.type ?? null)}`, value)
  }
}

/**
 * Projects any runtime value into JSON without losing what it was: bytes
 * become lowercase hex, bigints decimal strings, non-finite numbers their
 * name, errors a {@link DriverError}. `undefined` object fields are omitted.
 */
export function toJsonValue(value: unknown, depth = 0): JsonValue {
  if (value === null || value === undefined) return null
  if (typeof value === 'boolean' || typeof value === 'string') return value
  if (typeof value === 'number') return Number.isFinite(value) ? value : String(value)
  if (typeof value === 'bigint') return value.toString()
  if (typeof value === 'function' || typeof value === 'symbol') return `[${typeof value}]`
  if (depth > 8) return '[depth-limit]'
  if (value instanceof Uint8Array) return bytesToHex(value)
  if (value instanceof Error) return describeError(value)
  if (Array.isArray(value)) return value.map(item => toJsonValue(item, depth + 1))
  if (typeof value === 'object') {
    const out: Record<string, JsonValue> = {}
    for (const [key, field] of Object.entries(value)) {
      if (field !== undefined) out[key] = toJsonValue(field, depth + 1)
    }
    return out
  }
  return String(value)
}

const ERROR_DETAIL_KEYS = ['domain', 'operation', 'retryability', 'commit', 'platform', 'limitations', 'recovery'] as const

/** Keeps the typed code of a BleError (or any coded error) and its structured detail. */
export function describeError(error: unknown): DriverError {
  if (!(error instanceof Error)) return { code: 'non-error-thrown', message: String(error), detail: null }
  const code = 'code' in error && typeof error.code === 'string' ? error.code : error.name
  const detail: Record<string, JsonValue> = {}
  for (const key of ERROR_DETAIL_KEYS) {
    if (key in error) detail[key] = toJsonValue(Reflect.get(error, key), 1)
  }
  if (error instanceof AggregateError) detail.errors = error.errors.map(inner => describeError(inner))
  if (error.cause !== undefined) detail.cause = toJsonValue(error.cause, 1)
  return { code, message: error.message, detail: Object.keys(detail).length === 0 ? null : detail }
}

export function bytesToHex(bytes: Readonly<Uint8Array>): string {
  let hex = ''
  for (const byte of bytes) hex += byte.toString(16).padStart(2, '0')
  return hex
}

export function isJsonObject(value: unknown): value is JsonObject {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function parseObject(text: string): DecodeResult<{ readonly [key: string]: unknown }> {
  let parsed: unknown
  try {
    parsed = JSON.parse(text)
  } catch (error) {
    return { ok: false, error: { code: 'protocol.invalid-json', message: error instanceof Error ? error.message : String(error), detail: null } }
  }
  if (!isRecord(parsed)) return { ok: false, error: { code: 'protocol.invalid-message', message: 'message is not a JSON object', detail: null } }
  return { ok: true, message: parsed }
}

function isRecord(value: unknown): value is { readonly [key: string]: unknown } {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function invalid(message: string, value: { readonly [key: string]: unknown }): { ok: false; error: DriverError } {
  return { ok: false, error: { code: 'protocol.invalid-message', message, detail: toJsonValue(value) } }
}

function protocolMismatch(received: string): { ok: false; error: DriverError } {
  return {
    ok: false,
    error: {
      code: 'protocol.version-mismatch',
      message: `expected ${TEST_DRIVER_PROTOCOL}, received ${received}`,
      detail: null
    }
  }
}

function isNullableString(value: unknown): value is string | null {
  return value === null || typeof value === 'string'
}

function isScenarioEvent(value: unknown): value is ScenarioEvent {
  return (
    isRecord(value) &&
    typeof value.scenario === 'string' &&
    typeof value.seq === 'number' &&
    typeof value.atMs === 'number' &&
    typeof value.host === 'string' &&
    typeof value.kind === 'string' &&
    isJsonObject(value.data)
  )
}

function isScenarioDescription(value: unknown): value is ScenarioDescription {
  return (
    isRecord(value) &&
    typeof value.id === 'string' &&
    typeof value.title === 'string' &&
    typeof value.description === 'string' &&
    Array.isArray(value.commands) &&
    value.commands.every(
      command =>
        isRecord(command) &&
        typeof command.name === 'string' &&
        typeof command.label === 'string' &&
        typeof command.description === 'string' &&
        Array.isArray(command.presets) &&
        typeof command.acceptsDevice === 'boolean'
    )
  )
}
