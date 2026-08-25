// src/public/diagnostics.ts

import type { DiagnosticTraceDocument } from '../diagnostics/trace-format'
import { contractError } from '../backend-contract/errors'

export interface BleDiagnosticTraceRecord {
  readonly ordinal: number
  readonly time: number
  readonly kind: 'operation' | 'resource' | 'stream' | 'attachment'
  readonly event: string
  readonly cause: string | null
  readonly correlation: string | null
  readonly redactedClient: boolean
  readonly redactedPeer: boolean
  readonly redactedPath: boolean
  readonly redactedPayload: boolean
}

export interface BleDiagnosticTraceDocument {
  readonly format: 'unified-ble-trace-v1'
  readonly truncated: boolean
  readonly records: readonly BleDiagnosticTraceRecord[]
}

export interface BleResourceCounters {
  readonly activeScanControllers: number
  readonly scanConsumers: number
  readonly chooserSessions: number
  readonly connectionLeases: number
  readonly physicalLinks: number
  readonly databaseSnapshots: number
  readonly physicalCccdEnablements: number
  readonly subscriptionConsumers: number
  readonly queuedOperations: number
  readonly dispatchedOperations: number
  readonly retainedByteBuffers: number
  readonly restorationRecords: number
  readonly orphanedIpcOwners: number
}

export interface BleDiagnosticsSnapshot {
  readonly trace: BleDiagnosticTraceDocument
  readonly resourceCounters: BleResourceCounters
}

export interface BleDiagnostics {
  snapshot(): BleDiagnosticsSnapshot
  resourceCounters(): BleResourceCounters
  startTrace(): { stop(): Promise<BleDiagnosticTraceDocument> }
}

export function snapshotPublicTraceDocument(value: DiagnosticTraceDocument): BleDiagnosticTraceDocument {
  return Object.freeze({
    format: value.format,
    truncated: value.truncated,
    records: Object.freeze(
      value.records.map(record =>
        Object.freeze({
          ordinal: Number(record.ordinal),
          time: Number(record.time),
          kind: record.kind,
          event: record.event,
          cause: record.cause,
          correlation: record.correlation,
          redactedClient: record.redactedClient,
          redactedPeer: record.redactedPeer,
          redactedPath: record.redactedPath,
          redactedPayload: record.redactedPayload
        })
      )
    )
  })
}

function requireCounter(value: Record<string, number>, key: keyof BleResourceCounters): number {
  const count = value[key]
  if (count === undefined) return 0
  if (!Number.isSafeInteger(count) || count < 0) {
    throw contractError('protocol.violation', 'core', `diagnostics.resource-counter.${key}`)
  }
  return count
}

export function snapshotResourceCounters(value: Record<string, number>): BleResourceCounters {
  return Object.freeze({
    activeScanControllers: requireCounter(value, 'activeScanControllers'),
    scanConsumers: requireCounter(value, 'scanConsumers'),
    chooserSessions: requireCounter(value, 'chooserSessions'),
    connectionLeases: requireCounter(value, 'connectionLeases'),
    physicalLinks: requireCounter(value, 'physicalLinks'),
    databaseSnapshots: requireCounter(value, 'databaseSnapshots'),
    physicalCccdEnablements: requireCounter(value, 'physicalCccdEnablements'),
    subscriptionConsumers: requireCounter(value, 'subscriptionConsumers'),
    queuedOperations: requireCounter(value, 'queuedOperations'),
    dispatchedOperations: requireCounter(value, 'dispatchedOperations'),
    retainedByteBuffers: requireCounter(value, 'retainedByteBuffers'),
    restorationRecords: requireCounter(value, 'restorationRecords'),
    orphanedIpcOwners: requireCounter(value, 'orphanedIpcOwners')
  })
}

export function diagnosticsUnavailable(): BleDiagnostics {
  const unavailable = (): never => {
    throw contractError('capability.unavailable', 'core', 'diagnostics.unavailable')
  }
  return { snapshot: unavailable, resourceCounters: unavailable, startTrace: unavailable }
}
