import type { JsonObject, JsonValue } from './protocol.ts'
import { isJsonObject } from './protocol.ts'

export const PMD_RECORDING_SCHEMA = 'ubm-pmd-recording/1'
const RECORD_KINDS = new Set(['packet', 'control-command', 'control-response', 'generation', 'loss', 'error'])

export type PmdRecord = {
  readonly kind: 'packet' | 'control-command' | 'control-response' | 'generation' | 'loss' | 'error'
  readonly peerId: string
  readonly generation: string | null
  /** Host monotonic receipt time, not the device's sample clock. */
  readonly atMs: number
  /** Exact sensor timestamps belong here as decimal strings, never lossy numbers. */
  readonly data: JsonObject
}

/** Opt-in bounded capture. Snapshots contain only summary(), never the packet buffer.
 * Native/background retention is separate: this records only what this JS host observes. */
export class PmdRecorder {
  private readonly maxRecords: number
  private readonly maxBytes: number
  private records: JsonObject[] = []
  private metadata: JsonObject = {}
  private phase: 'empty' | 'recording' | 'stopped' | 'capacity-reached' = 'empty'
  private accepting = false
  private bytes = 0
  private omittedRecords = 0
  private startedAtMs: number | null = null
  private endedAtMs: number | null = null
  private readonly incompleteReasons = new Set<string>()

  constructor(limits: { readonly maxRecords?: number; readonly maxBytes?: number } = {}) {
    this.maxRecords = limits.maxRecords ?? 20_000
    this.maxBytes = limits.maxBytes ?? 8 * 1024 * 1024
    if (![this.maxRecords, this.maxBytes].every(value => Number.isSafeInteger(value) && value > 0)) {
      throw new Error('recording limits must be positive safe integers')
    }
  }

  start(metadata: JsonObject, startedAtMs: number): void {
    if (this.phase !== 'empty') throw new Error('clear the existing recording before starting another')
    assertTime(startedAtMs)
    const copy = copyObject(metadata)
    const bytes = utf8Bytes(JSON.stringify(copy))
    if (bytes > this.maxBytes) throw new Error('recording metadata exceeds byte capacity')
    this.metadata = copy
    this.bytes = bytes
    this.startedAtMs = startedAtMs
    this.phase = 'recording'
    this.accepting = true
  }

  append(record: PmdRecord): void {
    if (!this.accepting) return
    assertTime(record.atMs)
    const copy = copyObject(record)
    if (!isRecord(copy)) throw new Error('invalid recording record')
    if (record.kind === 'loss' || record.kind === 'error') this.incompleteReasons.add(record.kind)
    const bytes = utf8Bytes(JSON.stringify(copy))
    if (
      this.phase === 'capacity-reached' ||
      this.records.length >= this.maxRecords ||
      this.bytes + bytes > this.maxBytes
    ) {
      this.phase = 'capacity-reached'
      this.endedAtMs ??= record.atMs
      this.omittedRecords += 1
      this.incompleteReasons.add('capacity')
      return
    }
    this.records.push(copy)
    this.bytes += bytes
  }

  stop(endedAtMs: number): void {
    assertTime(endedAtMs)
    if (!this.accepting) return
    this.accepting = false
    this.endedAtMs = endedAtMs
    if (this.phase !== 'capacity-reached') this.phase = 'stopped'
  }

  clear(): void {
    if (this.accepting) throw new Error('stop recording before clearing it')
    this.records = []
    this.metadata = {}
    this.phase = 'empty'
    this.bytes = 0
    this.omittedRecords = 0
    this.startedAtMs = null
    this.endedAtMs = null
    this.incompleteReasons.clear()
  }

  summary(): JsonObject {
    return {
      phase: this.phase,
      accepting: this.accepting,
      records: this.records.length,
      bytes: this.bytes,
      omittedRecords: this.omittedRecords,
      startedAtMs: this.startedAtMs,
      endedAtMs: this.endedAtMs,
      incompleteReasons: [...this.incompleteReasons],
      maxRecords: this.maxRecords,
      maxBytes: this.maxBytes
    }
  }

  export(): JsonObject {
    if (this.phase !== 'stopped' && this.phase !== 'capacity-reached')
      throw new Error('recording must be stopped before export')
    return copyObject({
      schema: PMD_RECORDING_SCHEMA,
      metadata: this.metadata,
      summary: this.summary(),
      records: this.records
    })
  }
}

/** Validate the entire JSON boundary before treating a command result as a capture. */
export function isPmdRecording(value: unknown): value is JsonObject {
  if (!isJsonObject(value) || !isFiniteJson(value) || value.schema !== PMD_RECORDING_SCHEMA) return false
  if (!isJsonObject(value.metadata) || !isJsonObject(value.summary) || !Array.isArray(value.records)) return false
  const summary = value.summary
  return (
    (summary.phase === 'stopped' || summary.phase === 'capacity-reached') &&
    summary.records === value.records.length &&
    typeof summary.bytes === 'number' &&
    summary.bytes >= 0 &&
    typeof summary.omittedRecords === 'number' &&
    Number.isSafeInteger(summary.omittedRecords) &&
    summary.omittedRecords >= 0 &&
    typeof summary.startedAtMs === 'number' &&
    typeof summary.endedAtMs === 'number' &&
    Array.isArray(summary.incompleteReasons) &&
    summary.incompleteReasons.every(reason => typeof reason === 'string') &&
    value.records.every(isRecord)
  )
}

function isRecord(value: unknown): boolean {
  return (
    isJsonObject(value) &&
    typeof value.kind === 'string' &&
    RECORD_KINDS.has(value.kind) &&
    typeof value.peerId === 'string' &&
    (value.generation === null || typeof value.generation === 'string') &&
    typeof value.atMs === 'number' &&
    Number.isFinite(value.atMs) &&
    isJsonObject(value.data)
  )
}

function assertTime(value: number): void {
  if (!Number.isFinite(value)) throw new Error('recording timestamp must be finite')
}

function isFiniteJson(value: unknown, ancestors = new Set<object>()): value is JsonValue {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return true
  if (typeof value === 'number') return Number.isFinite(value)
  if (typeof value !== 'object' || ancestors.has(value)) return false
  if (
    !Array.isArray(value) &&
    Object.getPrototypeOf(value) !== Object.prototype &&
    Object.getPrototypeOf(value) !== null
  )
    return false
  ancestors.add(value)
  const valid = Object.values(value).every(child => isFiniteJson(child, ancestors))
  ancestors.delete(value)
  return valid
}

function copyObject(value: JsonObject): JsonObject {
  if (!isFiniteJson(value)) throw new Error('recording requires finite, acyclic JSON values')
  const copy: unknown = JSON.parse(JSON.stringify(value))
  if (!isJsonObject(copy)) throw new Error('recording requires a JSON object')
  return copy
}

// Avoid requiring TextEncoder in native JS hosts; iterate Unicode code points.
function utf8Bytes(value: string): number {
  let bytes = 0
  for (const char of value) {
    const point = char.codePointAt(0) ?? 0
    bytes += point <= 0x7f ? 1 : point <= 0x7ff ? 2 : point <= 0xffff ? 3 : 4
  }
  return bytes
}
