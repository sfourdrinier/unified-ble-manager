'use strict'

// The public IPC adapter exposes diagnostics on every production manager.
// Lightweight route fixtures use this complete inert implementation instead
// of omitting a required manager boundary.
const counters = Object.freeze({
  activeScanControllers: 0,
  scanConsumers: 0,
  chooserSessions: 0,
  connectionLeases: 0,
  physicalLinks: 0,
  databaseSnapshots: 0,
  physicalCccdEnablements: 0,
  subscriptionConsumers: 0,
  queuedOperations: 0,
  dispatchedOperations: 0,
  retainedByteBuffers: 0,
  restorationRecords: 0,
  orphanedIpcOwners: 0
})
const trace = Object.freeze({ format: 'ubm-diagnostic-trace/1', truncated: false, records: Object.freeze([]) })
const snapshot = Object.freeze({ trace, resourceCounters: counters })
const warnings = Object.freeze([])
const diagnostics = Object.freeze({
  snapshot: () => snapshot,
  resourceCounters: () => counters,
  startTrace: () => ({ stop: async () => trace }),
  warnings: () => warnings
})

function emptyIpcDiagnostics() {
  return diagnostics
}

module.exports = { emptyIpcDiagnostics }
