'use strict'

// Cross-boundary contract: the TypeScript scan-query normalizer owns the
// canonical JSON and digest, and the Tauri Rust decoder
// (native/tauri/src/scan_plan.rs) must reproduce them byte-identically.
// The shared corpus in ./fixtures/scan-query-digests.json is generated from
// the TS normalizer by scripts/generate-scan-query-digest-corpus.js; this
// suite pins the fixture to the live normalizer (drift check) and pins the
// TS half of the wire contract the Rust corpus test consumes.
//
// Why this exists: every filtered Tauri scan failed on hardware with
// protocol.malformed at tauri.scan-query ("normalized scan query digest is
// invalid") while unfiltered scans worked. The Rust decoder recomputed the
// digest over a canonical form that drops null fields, but the TS
// canonical form keeps nulls for services/names/manufacturerData/
// serviceData/rssi (only null peers/addresses are dropped). The pre-existing
// Rust unit test only covered a services-only clause and computed its digest
// with the same divergent code, so nothing cross-checked the two sides.

const corpus = require('./fixtures/scan-query-digests.json')
const { canonicalScanQueryJson } = require('../src/backend-contract/scan-query')
const { decodeIpcScanQuery, encodeIpcScanQuery } = require('../src/ipc/scan-planning')
const { normalizeScanQuery } = require('../src/public/scan-query')
const { decodeTauriWireValue, encodeTauriWireValue } = require('../src/tauri/transport')

function fromFixtureInput(value) {
  if (Array.isArray(value)) return value.map(fromFixtureInput)
  if (value !== null && typeof value === 'object') {
    if (Object.prototype.hasOwnProperty.call(value, '$bytes')) return new Uint8Array(value.$bytes)
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, fromFixtureInput(item)]))
  }
  return value
}

function recompute(entry) {
  const normalized = normalizeScanQuery(fromFixtureInput(entry.input))
  const canonical = canonicalScanQueryJson({ anyOf: normalized.anyOf, exclude: normalized.exclude })
  const wire = JSON.parse(JSON.stringify(encodeTauriWireValue(encodeIpcScanQuery(normalized))))
  return { normalized, canonical, wire }
}

function entryById(id) {
  const entry = corpus.entries.find(candidate => candidate.id === id)
  if (entry === undefined) throw new Error(`scan-query digest corpus is missing entry ${id}`)
  return entry
}

/** Mirrors production: Tauri JSON transport materializes bytes as Uint8Array. */
function decodeTauriQuery(wire) {
  return decodeIpcScanQuery(decodeTauriWireValue(JSON.parse(JSON.stringify(wire))), 'tauri.scan.query')
}

describe('Tauri scan-query digest corpus (TS contract owner)', () => {
  test('corpus fixture matches the live TS normalizer, canonical JSON, digest and wire', () => {
    expect(corpus.entries.length).toBeGreaterThan(0)
    for (const entry of corpus.entries) {
      const { normalized, canonical, wire } = recompute(entry)
      expect({ id: entry.id, canonical }).toEqual({ id: entry.id, canonical: entry.canonical })
      expect({ id: entry.id, digest: normalized.digest }).toEqual({ id: entry.id, digest: entry.digest })
      expect({ id: entry.id, wire }).toEqual({ id: entry.id, wire: entry.wire })
    }
  })

  test('physical driver queries keep the name filter on the Tauri wire with a stable digest', () => {
    // Shapes from examples-shared/driver/scenarios/ble-scenario.ts (scanPeer:
    // services.any heart-rate + exact name, or name prefix). This is the
    // public find()/scan() path: normalizeScanQuery, then the Tauri IPC
    // encoder, then the Rust decoder.
    for (const id of ['exact-name-heart-rate', 'prefix-name-heart-rate']) {
      const entry = entryById(id)
      const { normalized, wire } = recompute(entry)
      expect(wire.anyOf).toHaveLength(1)
      expect(wire.anyOf[0].names).not.toBeNull()
      expect(wire.digest).toBe(normalized.digest)
      expect(decodeTauriQuery(wire).digest).toBe(normalized.digest)
    }
  })

  test('radio addresses survive the IPC encoding with a stable digest', () => {
    const entry = entryById('radio-addresses')
    const { normalized, wire } = recompute(entry)
    expect(normalized.anyOf[0].addresses).toEqual(['AA:BB:CC:DD:EE:00', 'AA:BB:CC:DD:EE:FF'])
    expect(wire.anyOf[0].addresses).toEqual(['AA:BB:CC:DD:EE:00', 'AA:BB:CC:DD:EE:FF'])
    expect(decodeTauriQuery(wire).digest).toBe(normalized.digest)
  })

  test('one-sided RSSI bounds and prefix-less manufacturer patterns survive the IPC round trip', () => {
    for (const id of ['rssi-minimum-only', 'rssi-maximum-only', 'manufacturer-company-only']) {
      const entry = entryById(id)
      const { normalized, wire } = recompute(entry)
      expect(decodeTauriQuery(wire).digest).toBe(normalized.digest)
    }
  })
})
