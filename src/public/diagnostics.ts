import type { DiagnosticTraceDocument } from '../diagnostics/trace-format'

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
  readonly trace: DiagnosticTraceDocument
  readonly resourceCounters: BleResourceCounters
}

export interface BleDiagnostics {
  snapshot(): BleDiagnosticsSnapshot
  resourceCounters(): BleResourceCounters
  startTrace(): { stop(): Promise<DiagnosticTraceDocument> }
}

export function snapshotResourceCounters(value: Record<string, number>): BleResourceCounters {
  return Object.freeze({
    activeScanControllers: value.activeScanControllers ?? 0,
    scanConsumers: value.scanConsumers ?? 0,
    chooserSessions: value.chooserSessions ?? 0,
    connectionLeases: value.connectionLeases ?? 0,
    physicalLinks: value.physicalLinks ?? 0,
    databaseSnapshots: value.databaseSnapshots ?? 0,
    physicalCccdEnablements: value.physicalCccdEnablements ?? 0,
    subscriptionConsumers: value.subscriptionConsumers ?? 0,
    queuedOperations: value.queuedOperations ?? 0,
    dispatchedOperations: value.dispatchedOperations ?? 0,
    retainedByteBuffers: value.retainedByteBuffers ?? 0,
    restorationRecords: value.restorationRecords ?? 0,
    orphanedIpcOwners: value.orphanedIpcOwners ?? 0
  })
}

export function diagnosticsUnavailable(): BleDiagnostics {
  const unavailable = (): never => {
    throw new Error('Diagnostics are unavailable for this host')
  }
  return { snapshot: unavailable, resourceCounters: unavailable, startTrace: unavailable }
}
