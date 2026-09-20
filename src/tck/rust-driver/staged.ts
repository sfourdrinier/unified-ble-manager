// src/tck/rust-driver/staged.ts
//
// U7 staged-transition slice: scripted SYNTHETIC-radio programs (no BLE
// hardware exists) plus pinned frozen-rule expectations, per frozen base
// TCK scenario. Each program runs step-by-step against the REAL staged
// transition core (the napi session's `StagedDriver`, which owns a real
// `ubm-core` Central); the parity suite asserts each observation matches
// its frozen-rule pin below (SINGLE-COLUMN comparison: one executor, the
// napi staged core — there is no second staged executor, so these pins
// are not mechanical ref-vs-Rust output).
//
// Read-echo caveat: `gatt.read`/`gatt.read-descriptor` `bytes` and every
// `read.taken` observation are DRIVER-SIDE ECHO of the scripted `value`
// (retained verbatim by the driver; see `StagedDriver` read handling).
// Read bytes never transit the core, so byte equality here is
// script-echo equality — including the post-invalidation pins, which
// prove the echo survives while the core path itself goes stale.
// Notifications are genuine: `sub.notify` deliver-take runs through the
// core stream, and `sub.take` bytes are core observations.
//
// Pin provenance (read-only sources; nothing here is edited):
// - Every expectation transcribes a frozen C-UBM.0.1.2-DRAFT rule as
//   implemented by the untouched `ubm-core` (the independent oracle).
//   Wires were captured from real staged runs in this slice and verified
//   against the frozen tables before pinning; a mismatch fails the suite,
//   never the pin. Each program records its own capture source in its
//   `provenance` field.
// - Kernel op ids (`central-op-N`) are session-scoped serialization labels,
//   not contract observations: both columns normalize them to first-seen
//   `@opN` tokens (`normalizeStagedLines`), preserving identity relations
//   (which step admitted which op, which effect belongs to which op) while
//   abstracting the counter.
// - Programs that genuinely need real radio (adapter enumeration, peer
//   handshakes, adapter state, trace sinks) are NOT scripted here: those
//   scenarios stay open with sharpened `enforcedBy` probes
//   (`STAY_OPEN_STAGED_PROBES`), never silently closed, never faked.

import type { RustNativeAddon } from './rust-driver'
import { RustBackendDriver } from './rust-driver'
import { RUST_PARITY_REVISION } from './corpus'

/** Frozen staged batch bound surfaced in every accounting observation. */
export const STAGED_BATCH_CAP = 64

/** Pinned staged capability rows (six central rows, all limited). */
export const STAGED_CAPABILITY_ROWS =
  'central.scan=limited;central.connect=limited;central.discover=limited;' +
  'central.read=limited;central.write=limited;central.subscribe=limited'

/** Shared synthetic fixtures (deterministic, hardware-free). */
export const STAGED_SVC = '12345678-1234-5678-1234-56789abcdef0'
export const STAGED_CHR = '12345678-1234-5678-1234-56789abcdef1'
export const STAGED_CCCD = '00002902-0000-1000-8000-00805f9b34fb'

/** Recursively stringifies a value with sorted object keys. */
export function stableStringify(value: unknown): string {
  if (value === null || typeof value !== 'object') {
    return JSON.stringify(value) ?? 'null'
  }
  if (Array.isArray(value)) {
    return `[${value.map(entry => stableStringify(entry)).join(',')}]`
  }
  const record = value as Record<string, unknown>
  const keys = Object.keys(record).sort()
  const fields = keys.map(key => `${JSON.stringify(key)}:${stableStringify(record[key])}`)
  return `{${fields.join(',')}}`
}

/**
 * Normalizes session-scoped kernel op labels (`central-op-N`) to
 * first-seen `@opN` tokens across a whole observation run, then
 * stable-stringifies each line. Identity relations survive (the same op
 * maps to the same token on every line); the counter does not leak into
 * the comparison.
 */
export function normalizeStagedLines(lines: readonly string[]): string[] {
  const seen: string[] = []
  return lines.map(line => {
    const mapped = line.replace(/central-op-\d+/gu, match => {
      const index = seen.indexOf(match)
      if (index !== -1) {
        return `@op${index}`
      }
      seen.push(match)
      return `@op${seen.length - 1}`
    })
    return stableStringify(JSON.parse(mapped))
  })
}

/** One scripted staged program with its pinned normalized expectations. */
export interface StagedProgram {
  readonly scenarioId: string
  /** Step lines sent to `stagedStep` in order (JSON objects). */
  readonly steps: readonly string[]
  /** Normalized expected observations, one per step, in order. */
  readonly expected: readonly string[]
  /**
   * Where this program's pins came from: the frozen rule transcribed plus
   * the staged run they were captured from. Single-column pins are only
   * as honest as their provenance, so every program records its own.
   */
  readonly provenance: string
}

function step(line: string): string {
  return line
}

function norm(object: unknown): string {
  return stableStringify(object)
}

const CAP_PROJECT = step('{"step":"cap.project"}')
const CAP_PROJECTED = norm({ step: 'cap.project', ok: true, rows: 6, staged: 0, effects: '' })

const CAP_ROWS = step('{"step":"cap.rows"}')
const CAP_ROWS_SEEN = norm({
  step: 'cap.rows',
  ok: true,
  rows: STAGED_CAPABILITY_ROWS,
  count: 6,
  staged: 0,
  effects: ''
})

function capCheck(id: string): string {
  return step(`{"step":"cap.check","id":"${id}"}`)
}

/** Staged program closing `capability.truth-limits-evidence-and-binding`. */
export const PROGRAM_CAPABILITY: StagedProgram = {
  scenarioId: 'capability.truth-limits-evidence-and-binding',
  provenance:
    'Pins transcribe the capability projection table (six limited rows; unknown ids reject ' +
    'capability.unavailable); captured from a staged run of this program and checked against the frozen table.',
  steps: [CAP_PROJECT, CAP_ROWS, capCheck('central.scan'), capCheck('central.teleport')],
  expected: [
    CAP_PROJECTED,
    CAP_ROWS_SEEN,
    norm({
      step: 'cap.check',
      ok: true,
      admission: 'proceed-with-limitation',
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'cap.check',
      ok: false,
      error: 'capability.unavailable|capability|staged-cap-check|capability.unknown'
    })
  ]
}

/** Staged program closing `scan.owner-join-authority-and-signature`. */
export const PROGRAM_SCAN_OWNER: StagedProgram = {
  scenarioId: 'scan.owner-join-authority-and-signature',
  provenance:
    'Pins transcribe the scan arbitration rule (a second owner fails scan.already-active; ' +
    'stop settles through stopping to stopped); captured from a staged run of this program.',
  steps: [
    step('{"step":"scan.start","op":"scan0","owner":"owner-a"}'),
    step('{"step":"scan.platform","op":"scan0","event":"platform-started"}'),
    step('{"step":"scan.start","op":"scan1","owner":"owner-b"}'),
    step('{"step":"peer.advertise","peer":"p","domain":"platform-guid","value":"peer-1"}'),
    step('{"step":"scan.platform","op":"scan0","event":"stop"}'),
    step('{"step":"scan.platform","op":"scan0","event":"platform-stopped"}')
  ],
  expected: [
    norm({
      step: 'scan.start',
      ok: true,
      op_id: '@op0',
      owner: 'owner-a',
      staged: 1,
      effects: 'central.scan-start#@op0:scan.start'
    }),
    norm({
      step: 'scan.platform',
      ok: true,
      event: 'platform-started',
      state: 'active',
      staged: 0,
      effects: 'central.scan-settled#@op0:scan.platform-started'
    }),
    norm({
      step: 'scan.start',
      ok: false,
      error: 'scan.already-active|core|staged-scan-start|scan.arbitration'
    }),
    norm({
      step: 'peer.advertise',
      ok: true,
      peer_key: 'platform-guid:peer-1',
      domain: 'platform-guid',
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'scan.platform',
      ok: true,
      event: 'stop',
      state: 'stopping',
      staged: 0,
      effects: 'central.scan-settled#@op0:scan.stop'
    }),
    norm({
      step: 'scan.platform',
      ok: true,
      event: 'platform-stopped',
      state: 'stopped',
      staged: 5,
      effects: 'central.scan-settled#@op0:scan.platform-event'
    })
  ]
}

/** Staged program closing `scan.fairness-abort-deadline-and-final-cleanup`. */
export const PROGRAM_SCAN_FAIRNESS: StagedProgram = {
  scenarioId: 'scan.fairness-abort-deadline-and-final-cleanup',
  provenance:
    'Pins transcribe cancel-before-dispatch receipts (commit not-dispatched) and expiry-sweep ' +
    'settlement; captured from a staged run of this program. The sweep pin includes the ' +
    'scan.timeout session transition: a sweep that settles a Starting scan op fails the ' +
    'session (Starting->Failed) so no ownerless live session survives.',
  steps: [
    step('{"step":"scan.start","op":"s0","owner":"owner-a","timeout_ms":5000,"now":100}'),
    step('{"step":"op.cancel","op":"s0"}'),
    step('{"step":"scan.platform","op":"s0","event":"stop"}'),
    step('{"step":"scan.platform","op":"s0","event":"platform-stopped"}'),
    step('{"step":"scan.start","op":"s1","owner":"owner-b"}'),
    step('{"step":"op.expire-sweep","now":99999}')
  ],
  expected: [
    norm({
      step: 'scan.start',
      ok: true,
      op_id: '@op0',
      owner: 'owner-a',
      staged: 1,
      effects: 'central.scan-start#@op0:scan.start'
    }),
    norm({
      step: 'op.cancel',
      ok: true,
      settle: 'settled',
      terminal: 'aborted',
      cause: 'operation.aborted',
      reached_radio: false,
      commit: 'not-dispatched',
      suppressed: 0,
      staged: 3,
      effects: ''
    }),
    norm({
      step: 'scan.platform',
      ok: true,
      event: 'stop',
      state: 'stopping',
      staged: 0,
      effects: 'central.scan-settled#@op0:scan.stop'
    }),
    norm({
      step: 'scan.platform',
      ok: true,
      event: 'platform-stopped',
      state: 'stopped',
      staged: 0,
      effects: 'central.scan-settled#@op0:scan.platform-event'
    }),
    norm({
      step: 'scan.start',
      ok: true,
      op_id: '@op1',
      owner: 'owner-b',
      staged: 1,
      effects: 'central.scan-start#@op1:scan.start'
    }),
    norm({
      step: 'op.expire-sweep',
      ok: true,
      settled: 1,
      truncated: false,
      staged: 2,
      effects: 'central.scan-settled#@op1:scan.timeout'
    })
  ]
}

const ADVERTISE_P = step('{"step":"peer.advertise","peer":"p","domain":"platform-guid","value":"peer-1"}')
const ADVERTISE_P_SEEN = norm({
  step: 'peer.advertise',
  ok: true,
  peer_key: 'platform-guid:peer-1',
  domain: 'platform-guid',
  staged: 0,
  effects: ''
})

function discoverDb(owner: string): string {
  return step(
    `{"step":"gatt.discover","peer":"p","owner":"${owner}","services":[{` +
      `"uuid":"${STAGED_SVC}","occurrence":0,"characteristics":[{` +
      `"uuid":"${STAGED_CHR}","occurrence":0,"properties":"read+write+notify",` +
      `"descriptors":[{"uuid":"${STAGED_CCCD}","occurrence":0}]}]}]}`
  )
}

/** Staged program closing `connection.lease-joins-borrowing-transfer-and-revocation`. */
export const PROGRAM_CONNECTION_LEASE: StagedProgram = {
  scenarioId: 'connection.lease-joins-borrowing-transfer-and-revocation',
  provenance:
    'Pins transcribe lease join/borrow/transfer/release counts (released flips only on the last ' +
    'release) and the loss terminal; captured from a staged run of this program.',
  steps: [
    step('{"step":"link.sharing","supported":true}'),
    ADVERTISE_P,
    step('{"step":"link.connect","peer":"p","lease":"lease-a","op":"conn0"}'),
    step('{"step":"link.established","peer":"p","op":"conn0"}'),
    step('{"step":"link.borrow","peer":"p","lease":"lease-b","op":"borrow0"}'),
    step('{"step":"link.transfer","peer":"p","source":"lease-b","dest":"lease-c"}'),
    step('{"step":"link.release","peer":"p","lease":"lease-c"}'),
    step('{"step":"link.release","peer":"p","lease":"lease-a"}'),
    step('{"step":"peer.advertise","peer":"q","domain":"platform-guid","value":"peer-2"}'),
    step('{"step":"link.connect","peer":"q","lease":"lease-q","op":"conn1"}'),
    step('{"step":"link.established","peer":"q","op":"conn1"}'),
    step('{"step":"link.loss","peer":"q"}')
  ],
  expected: [
    norm({ step: 'link.sharing', ok: true, supported: true, staged: 0, effects: '' }),
    ADVERTISE_P_SEEN,
    norm({
      step: 'link.connect',
      ok: true,
      op_id: '@op0',
      peer_key: 'platform-guid:peer-1',
      staged: 1,
      effects: 'central.connect#@op0:connection.connect'
    }),
    norm({
      step: 'link.established',
      ok: true,
      settle: 'settled',
      terminal: 'succeeded',
      cause: null,
      reached_radio: true,
      commit: 'committed',
      suppressed: 0,
      connection: 'connected',
      staged: 5,
      effects: ''
    }),
    norm({
      step: 'link.borrow',
      ok: true,
      op_id: '@op1',
      lease_count: 2,
      staged: 1,
      effects: 'central.borrow#@op1:connection.borrow'
    }),
    norm({
      step: 'link.transfer',
      ok: true,
      lease_count: 2,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'link.release',
      ok: true,
      released: false,
      lease_count: 1,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'link.release',
      ok: true,
      released: true,
      lease_count: 0,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'peer.advertise',
      ok: true,
      peer_key: 'platform-guid:peer-2',
      domain: 'platform-guid',
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'link.connect',
      ok: true,
      op_id: '@op2',
      peer_key: 'platform-guid:peer-2',
      staged: 1,
      effects: 'central.connect#@op2:connection.connect'
    }),
    norm({
      step: 'link.established',
      ok: true,
      settle: 'settled',
      terminal: 'succeeded',
      cause: null,
      reached_radio: true,
      commit: 'committed',
      suppressed: 0,
      connection: 'connected',
      staged: 5,
      effects: ''
    }),
    norm({
      step: 'link.loss',
      ok: true,
      state: 'lost',
      connection: 'lost',
      staged: 0,
      effects: ''
    })
  ]
}

/** Staged program closing `connection.two-client-arbitration`. */
export const PROGRAM_CONNECTION_ARBITRATION: StagedProgram = {
  scenarioId: 'connection.two-client-arbitration',
  provenance:
    'Pins transcribe join arbitration (a second link.connect to the live link joins: ok with a ' +
    'central.borrow effect; releasing the joiner keeps the first lease: released:false, ' +
    'lease_count:1) and the loss-vs-disconnect race (loss wins; disconnect fails ' +
    'lifecycle.invalid-state); captured from a staged run. The kernel runs sharing-by-default ' +
    '(no link.sharing step precedes admission), so already-owned never fires here; the ' +
    'exclusive-mode refusal stays pinned by the Rust staged_drive lease test.',
  steps: [
    ADVERTISE_P,
    step('{"step":"link.connect","peer":"p","lease":"lease-a","op":"conn0"}'),
    step('{"step":"link.established","peer":"p","op":"conn0"}'),
    step('{"step":"link.connect","peer":"p","lease":"lease-b","op":"conn1"}'),
    step('{"step":"link.release","peer":"p","lease":"lease-b"}'),
    step('{"step":"link.loss","peer":"p"}'),
    step('{"step":"link.disconnect","peer":"p","lease":"lease-a"}')
  ],
  expected: [
    ADVERTISE_P_SEEN,
    norm({
      step: 'link.connect',
      ok: true,
      op_id: '@op0',
      peer_key: 'platform-guid:peer-1',
      staged: 1,
      effects: 'central.connect#@op0:connection.connect'
    }),
    norm({
      step: 'link.established',
      ok: true,
      settle: 'settled',
      terminal: 'succeeded',
      cause: null,
      reached_radio: true,
      commit: 'committed',
      suppressed: 0,
      connection: 'connected',
      staged: 5,
      effects: ''
    }),
    norm({
      step: 'link.connect',
      ok: true,
      op_id: '@op1',
      peer_key: 'platform-guid:peer-1',
      staged: 1,
      effects: 'central.borrow#@op1:connection.borrow'
    }),
    norm({
      step: 'link.release',
      ok: true,
      released: false,
      lease_count: 1,
      staged: 0,
      effects: ''
    }),
    norm({ step: 'link.loss', ok: true, state: 'lost', connection: 'lost', staged: 0, effects: '' }),
    norm({
      step: 'link.disconnect',
      ok: false,
      error: 'lifecycle.invalid-state|core|staged-link-disconnect|central.connection.transition'
    })
  ]
}

function resolveStep(service: string, extra: string): string {
  return step(`{"step":"gatt.resolve","peer":"p","service":"${service}","service_occurrence":0${extra}}`)
}

function discoverDupDb(owner: string): string {
  return step(
    `{"step":"gatt.discover","peer":"p","owner":"${owner}","services":[{` +
      `"uuid":"${STAGED_SVC}","occurrence":0,"characteristics":[{` +
      `"uuid":"${STAGED_CHR}","occurrence":0,"properties":"read+write+notify",` +
      `"descriptors":[{"uuid":"${STAGED_CCCD}","occurrence":0}]},` +
      `{"uuid":"${STAGED_CHR}","occurrence":1,"properties":"read"}]}]}`
  )
}

/** Staged program closing `gatt.discovery-complete-paths-and-services-changed`. */
export const PROGRAM_GATT_DISCOVERY: StagedProgram = {
  scenarioId: 'gatt.discovery-complete-paths-and-services-changed',
  provenance:
    'Pins transcribe path resolution (ambiguous/not-found), stale-handle after services-changed, ' +
    'rediscovery re-arming, and driver-side read echo (read.taken survives invalidation while the ' +
    'core path stays stale); captured from a staged run of this program. ' +
    'F04: rediscovery counts current-generation paths only (revive, no stale accumulation).',
  steps: [
    ADVERTISE_P,
    step('{"step":"link.connect","peer":"p","lease":"lease-a","op":"conn0"}'),
    step('{"step":"link.established","peer":"p","op":"conn0"}'),
    discoverDupDb('lease-a'),
    resolveStep(STAGED_SVC, `,"characteristic":"${STAGED_CHR}"`),
    resolveStep(STAGED_SVC, `,"characteristic":"${STAGED_CHR}","characteristic_occurrence":1`),
    resolveStep(STAGED_SVC, `,"characteristic":"99999999-1234-5678-1234-56789abcdef9"`),
    step('{"step":"gatt.services-changed","peer":"p"}'),
    step('{"step":"gatt.read","op":"r0","path":1,"value":"aa","settle":"success"}'),
    step('{"step":"gatt.require-rediscovery","peer":"p"}'),
    discoverDupDb('lease-a'),
    step('{"step":"gatt.read","op":"r1","path":3,"value":"bb","settle":"success"}'),
    // Post-invalidation echo pin: `services-changed` stales the database,
    // yet `read.taken` still mirrors the scripted `bb` — the echo never
    // transits the core.
    step('{"step":"gatt.services-changed","peer":"p"}'),
    step('{"step":"read.taken","op":"r1"}')
  ],
  expected: [
    ADVERTISE_P_SEEN,
    norm({
      step: 'link.connect',
      ok: true,
      op_id: '@op0',
      peer_key: 'platform-guid:peer-1',
      staged: 1,
      effects: 'central.connect#@op0:connection.connect'
    }),
    norm({
      step: 'link.established',
      ok: true,
      settle: 'settled',
      terminal: 'succeeded',
      cause: null,
      reached_radio: true,
      commit: 'committed',
      suppressed: 0,
      connection: 'connected',
      staged: 5,
      effects: ''
    }),
    norm({
      step: 'gatt.discover',
      ok: true,
      peer_key: 'platform-guid:peer-1',
      services: 1,
      paths: 4,
      first_path: 0,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'gatt.resolve',
      ok: false,
      error: 'gatt.ambiguous-path|gatt|staged-gatt-resolve|path.resolve'
    }),
    norm({
      step: 'gatt.resolve',
      ok: true,
      path: 3,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'gatt.resolve',
      ok: false,
      error: 'gatt.not-found|gatt|staged-gatt-resolve|path.resolve'
    }),
    norm({
      step: 'gatt.services-changed',
      ok: true,
      peer_key: 'platform-guid:peer-1',
      database: 'changed',
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'gatt.read',
      ok: false,
      error: 'gatt.stale-handle|gatt|staged-gatt-read|path.generation'
    }),
    norm({
      step: 'gatt.require-rediscovery',
      ok: true,
      peer_key: 'platform-guid:peer-1',
      database: 'undiscovered',
      staged: 0,
      effects: ''
    }),
    // F04 contract update (justified): rediscovery revives identical-selector
    // stale slots and reports current-generation paths only — the old pin
    // (paths:8 = 4 stale + 4 new, first_path:4) transcribed the pre-fix
    // stale accumulation F04 removed.
    norm({
      step: 'gatt.discover',
      ok: true,
      peer_key: 'platform-guid:peer-1',
      services: 1,
      paths: 4,
      first_path: 0,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'gatt.read',
      ok: true,
      op_id: '@op1',
      settle: 'settled',
      terminal: 'succeeded',
      cause: null,
      reached_radio: true,
      commit: 'committed',
      suppressed: 0,
      bytes: 'bb',
      staged: 6,
      effects: 'central.read#@op1:gatt.read'
    }),
    norm({
      step: 'gatt.services-changed',
      ok: true,
      peer_key: 'platform-guid:peer-1',
      database: 'changed',
      staged: 0,
      effects: ''
    }),
    norm({ step: 'read.taken', ok: true, bytes: 'bb', staged: 0, effects: '' })
  ]
}

/** Staged program closing `gatt.reads-descriptors-write-policy-and-dispatched-cancellation`. */
export const PROGRAM_GATT_IO: StagedProgram = {
  scenarioId: 'gatt.reads-descriptors-write-policy-and-dispatched-cancellation',
  provenance:
    'Pins transcribe write-mode/maximum gating, descriptor IO, cancel-before/after-dispatch commits, ' +
    'and driver-side read echo (read.taken bytes mirror the scripted value, never the core); ' +
    'captured from a staged run of this program.',
  steps: [
    ADVERTISE_P,
    step('{"step":"link.connect","peer":"p","lease":"lease-a","op":"conn0"}'),
    step('{"step":"link.established","peer":"p","op":"conn0"}'),
    discoverDb('lease-a'),
    step(
      '{"step":"gatt.write","op":"w0","path":1,"value":"aabb","mode":"with-response","maximum":512,"settle":"success"}'
    ),
    step('{"step":"gatt.write","op":"w1","path":1,"value":"aabb","mode":"quantum-entangle","maximum":512}'),
    step(
      '{"step":"gatt.write","op":"w2","path":1,"value":"aabb","mode":"with-response","mode_supported":false,"maximum":512}'
    ),
    step('{"step":"gatt.write","op":"w3","path":1,"value":"aabb","mode":"with-response"}'),
    step('{"step":"gatt.write","op":"w4","path":1,"value":"aabbcc","mode":"with-response","maximum":2}'),
    step('{"step":"gatt.write-descriptor","op":"w5","path":2,"value":"0000"}'),
    step('{"step":"gatt.read-descriptor","op":"r1","path":2,"value":"0100","settle":"success"}'),
    step('{"step":"gatt.read","op":"r0","path":1,"value":"aa","settle":"dispatched"}'),
    step('{"step":"op.cancel","op":"r0"}'),
    step('{"step":"gatt.read","op":"r2","path":1,"value":"aa","settle":"admitted"}'),
    step('{"step":"op.cancel","op":"r2"}'),
    // Driver-side echo pins: `read.taken` mirrors the scripted value (`aa`
    // for the admitted-then-cancelled `r2`), never the core; unknown names
    // read null.
    step('{"step":"read.taken","op":"r2"}'),
    step('{"step":"read.taken","op":"ghost"}')
  ],
  expected: [
    ADVERTISE_P_SEEN,
    norm({
      step: 'link.connect',
      ok: true,
      op_id: '@op0',
      peer_key: 'platform-guid:peer-1',
      staged: 1,
      effects: 'central.connect#@op0:connection.connect'
    }),
    norm({
      step: 'link.established',
      ok: true,
      settle: 'settled',
      terminal: 'succeeded',
      cause: null,
      reached_radio: true,
      commit: 'committed',
      suppressed: 0,
      connection: 'connected',
      staged: 5,
      effects: ''
    }),
    norm({
      step: 'gatt.discover',
      ok: true,
      peer_key: 'platform-guid:peer-1',
      services: 1,
      paths: 3,
      first_path: 0,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'gatt.write',
      ok: true,
      op_id: '@op1',
      settle: 'settled',
      terminal: 'succeeded',
      cause: null,
      reached_radio: true,
      commit: 'committed',
      suppressed: 0,
      bytes: 'aabb',
      staged: 6,
      effects: 'central.write#@op1:gatt.write'
    }),
    norm({
      step: 'gatt.write',
      ok: false,
      error: 'argument.invalid|gatt|staged-gatt-write|write.mode'
    }),
    norm({
      step: 'gatt.write',
      ok: false,
      error: 'capability.unsupported|capability|staged-gatt-write|write.mode'
    }),
    norm({
      step: 'gatt.write',
      ok: false,
      error: 'capability.unavailable|capability|staged-gatt-write|write.maximum'
    }),
    norm({
      step: 'gatt.write',
      ok: false,
      error: 'bytes.too-large|gatt|staged-gatt-write|write.length'
    }),
    norm({
      step: 'gatt.write-descriptor',
      ok: false,
      error: 'gatt.cccd-managed|gatt|staged-gatt-write-descriptor|write.cccd'
    }),
    norm({
      step: 'gatt.read-descriptor',
      ok: true,
      op_id: '@op2',
      settle: 'settled',
      terminal: 'succeeded',
      cause: null,
      reached_radio: true,
      commit: 'committed',
      suppressed: 0,
      bytes: '0100',
      staged: 6,
      effects: 'central.read-descriptor#@op2:gatt.read-descriptor'
    }),
    norm({
      step: 'gatt.read',
      ok: true,
      op_id: '@op3',
      bytes: 'aa',
      staged: 3,
      effects: 'central.read#@op3:gatt.read'
    }),
    norm({
      step: 'op.cancel',
      ok: true,
      settle: 'settled',
      terminal: 'aborted',
      cause: 'operation.aborted',
      reached_radio: false,
      commit: 'released',
      suppressed: 0,
      staged: 3,
      effects: ''
    }),
    norm({
      step: 'gatt.read',
      ok: true,
      op_id: '@op4',
      bytes: 'aa',
      staged: 1,
      effects: 'central.read#@op4:gatt.read'
    }),
    norm({
      step: 'op.cancel',
      ok: true,
      settle: 'settled',
      terminal: 'aborted',
      cause: 'operation.aborted',
      reached_radio: false,
      commit: 'not-dispatched',
      suppressed: 0,
      staged: 3,
      effects: ''
    }),
    norm({ step: 'read.taken', ok: true, bytes: 'aa', staged: 0, effects: '' }),
    norm({ step: 'read.taken', ok: true, bytes: null, staged: 0, effects: '' })
  ]
}

function linkSetup(lease: string, op: string): string[] {
  return [
    ADVERTISE_P,
    step(`{"step":"link.connect","peer":"p","lease":"${lease}","op":"${op}"}`),
    step(`{"step":"link.established","peer":"p","op":"${op}"}`)
  ]
}

function linkSetupSeen(opToken: string): string[] {
  return [
    ADVERTISE_P_SEEN,
    norm({
      step: 'link.connect',
      ok: true,
      op_id: opToken,
      peer_key: 'platform-guid:peer-1',
      staged: 1,
      effects: `central.connect#${opToken}:connection.connect`
    }),
    norm({
      step: 'link.established',
      ok: true,
      settle: 'settled',
      terminal: 'succeeded',
      cause: null,
      reached_radio: true,
      commit: 'committed',
      suppressed: 0,
      connection: 'connected',
      staged: 5,
      effects: ''
    })
  ]
}

/** Staged program closing `subscription.enable-ready-shared-cccd-and-fanout`. */
export const PROGRAM_SUBSCRIPTION_FANOUT: StagedProgram = {
  scenarioId: 'subscription.enable-ready-shared-cccd-and-fanout',
  provenance:
    'Pins transcribe subscribe-enable fanout (one delivery per ready consumer; take bytes are core ' +
    'observations, not echo); captured from a staged run of this program.',
  steps: [
    ...linkSetup('lease-a', 'conn0'),
    discoverDb('lease-a'),
    step('{"step":"sub.subscribe","op":"sub0","path":1,"consumer":"c0"}'),
    step('{"step":"sub.subscribe","op":"sub1","path":1,"consumer":"c1"}'),
    step('{"step":"sub.settle-enable","path":1,"success":true,"consumer":"c0"}'),
    step('{"step":"sub.notify","path":1,"value":"bb","consumer":"c0"}'),
    step('{"step":"sub.take","path":1,"consumer":"c0"}'),
    step('{"step":"sub.take","path":1,"consumer":"c1"}')
  ],
  expected: [
    ...linkSetupSeen('@op0'),
    norm({
      step: 'gatt.discover',
      ok: true,
      peer_key: 'platform-guid:peer-1',
      services: 1,
      paths: 3,
      first_path: 0,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'sub.subscribe',
      ok: true,
      op_id: '@op1',
      consumer: 'enabling',
      cccd: false,
      staged: 1,
      effects: 'central.subscribe-enable#@op1:subscribe.enable'
    }),
    norm({
      step: 'sub.subscribe',
      ok: true,
      op_id: '@op2',
      consumer: 'enabling',
      cccd: false,
      staged: 1,
      effects: ''
    }),
    norm({
      step: 'sub.settle-enable',
      ok: true,
      consumer: 'ready',
      cccd: true,
      staged: 10,
      effects: ''
    }),
    norm({
      step: 'sub.notify',
      ok: true,
      bytes: 'bb',
      delivery: 'c0=delivered;c1=delivered',
      consumer: 'ready',
      cccd: true,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'sub.take',
      ok: true,
      bytes: 'bb',
      consumer: 'ready',
      cccd: true,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'sub.take',
      ok: true,
      bytes: 'bb',
      consumer: 'ready',
      cccd: true,
      staged: 0,
      effects: ''
    })
  ]
}

/** Second staged service UUID and a non-CCCD descriptor UUID for the duplicate-UUID world. */
export const STAGED_SVC_2 = '12345678-1234-5678-1234-56789abcdef2'
export const STAGED_USER_DESCRIPTION = '00002901-0000-1000-8000-00805f9b34fb'

function discoverDuplicateUuidWorld(owner: string): string {
  const characteristic = (occurrence: number, descriptors: string) =>
    `{"uuid":"${STAGED_CHR}","occurrence":${occurrence},"properties":"read+notify"${descriptors}}`
  const userDescriptions =
    `,"descriptors":[{"uuid":"${STAGED_USER_DESCRIPTION}","occurrence":0},` +
    `{"uuid":"${STAGED_USER_DESCRIPTION}","occurrence":1}]`
  return step(
    `{"step":"gatt.discover","peer":"p","owner":"${owner}","services":[` +
      `{"uuid":"${STAGED_SVC}","occurrence":0,"characteristics":[${characteristic(0, userDescriptions)},${characteristic(1, '')}]},` +
      `{"uuid":"${STAGED_SVC_2}","occurrence":0,"characteristics":[${characteristic(0, '')}]},` +
      `{"uuid":"${STAGED_SVC}","occurrence":1,"characteristics":[${characteristic(0, '')}]}]}`
  )
}

function resolveAt(service: string, serviceOccurrence: number, extra: string): string {
  return step(
    `{"step":"gatt.resolve","peer":"p","service":"${service}","service_occurrence":${serviceOccurrence}${extra}}`
  )
}

/** Staged program closing `gatt.duplicate-uuid-occurrences-route-exactly`. */
export const PROGRAM_GATT_DUPLICATE_OCCURRENCES: StagedProgram = {
  scenarioId: 'gatt.duplicate-uuid-occurrences-route-exactly',
  provenance:
    'Pins transcribe complete-path resolution over a database with a second service UUID and a ' +
    'same-UUID occurrence 1 at the service, characteristic and descriptor levels (a UUID alone is ' +
    'ambiguous; each occurrence resolves to its own path), and notification routing to the consumer ' +
    'on the addressed instance only; captured from a staged run of this program.',
  steps: [
    ...linkSetup('lease-a', 'conn0'),
    discoverDuplicateUuidWorld('lease-a'),
    resolveAt(STAGED_SVC, 0, `,"characteristic":"${STAGED_CHR}"`),
    resolveAt(STAGED_SVC, 0, `,"characteristic":"${STAGED_CHR}","characteristic_occurrence":0`),
    resolveAt(STAGED_SVC, 0, `,"characteristic":"${STAGED_CHR}","characteristic_occurrence":1`),
    resolveAt(STAGED_SVC_2, 0, `,"characteristic":"${STAGED_CHR}","characteristic_occurrence":0`),
    resolveAt(STAGED_SVC, 1, `,"characteristic":"${STAGED_CHR}","characteristic_occurrence":0`),
    resolveAt(
      STAGED_SVC,
      0,
      `,"characteristic":"${STAGED_CHR}","characteristic_occurrence":0,"descriptor":"${STAGED_USER_DESCRIPTION}"`
    ),
    resolveAt(
      STAGED_SVC,
      0,
      `,"characteristic":"${STAGED_CHR}","characteristic_occurrence":0,"descriptor":"${STAGED_USER_DESCRIPTION}","descriptor_occurrence":1`
    ),
    step('{"step":"sub.subscribe","op":"sub0","path":1,"consumer":"c0"}'),
    step('{"step":"sub.subscribe","op":"sub1","path":4,"consumer":"c1"}'),
    step('{"step":"sub.subscribe","op":"sub2","path":8,"consumer":"c2"}'),
    step('{"step":"sub.settle-enable","path":1,"success":true,"consumer":"c0"}'),
    step('{"step":"sub.settle-enable","path":4,"success":true,"consumer":"c1"}'),
    step('{"step":"sub.settle-enable","path":8,"success":true,"consumer":"c2"}'),
    step('{"step":"sub.notify","path":4,"value":"b1","consumer":"c1"}'),
    step('{"step":"sub.take","path":4,"consumer":"c1"}'),
    step('{"step":"sub.notify","path":8,"value":"c2","consumer":"c2"}'),
    step('{"step":"sub.take","path":8,"consumer":"c2"}'),
    step('{"step":"sub.notify","path":1,"value":"a0","consumer":"c0"}'),
    step('{"step":"sub.take","path":1,"consumer":"c0"}')
  ],
  expected: [
    ...linkSetupSeen('@op0'),
    norm({
      step: 'gatt.discover',
      ok: true,
      peer_key: 'platform-guid:peer-1',
      services: 3,
      paths: 9,
      first_path: 0,
      staged: 0,
      effects: ''
    }),
    norm({ step: 'gatt.resolve', ok: false, error: 'gatt.ambiguous-path|gatt|staged-gatt-resolve|path.resolve' }),
    norm({ step: 'gatt.resolve', ok: true, path: 1, staged: 0, effects: '' }),
    norm({ step: 'gatt.resolve', ok: true, path: 4, staged: 0, effects: '' }),
    norm({ step: 'gatt.resolve', ok: true, path: 6, staged: 0, effects: '' }),
    norm({ step: 'gatt.resolve', ok: true, path: 8, staged: 0, effects: '' }),
    norm({ step: 'gatt.resolve', ok: false, error: 'gatt.ambiguous-path|gatt|staged-gatt-resolve|path.resolve' }),
    norm({ step: 'gatt.resolve', ok: true, path: 3, staged: 0, effects: '' }),
    norm({
      step: 'sub.subscribe',
      ok: true,
      op_id: '@op1',
      consumer: 'enabling',
      cccd: false,
      staged: 1,
      effects: 'central.subscribe-enable#@op1:subscribe.enable'
    }),
    norm({
      step: 'sub.subscribe',
      ok: true,
      op_id: '@op2',
      consumer: 'enabling',
      cccd: false,
      staged: 1,
      effects: 'central.subscribe-enable#@op2:subscribe.enable'
    }),
    norm({
      step: 'sub.subscribe',
      ok: true,
      op_id: '@op3',
      consumer: 'enabling',
      cccd: false,
      staged: 1,
      effects: 'central.subscribe-enable#@op3:subscribe.enable'
    }),
    norm({ step: 'sub.settle-enable', ok: true, consumer: 'ready', cccd: true, staged: 5, effects: '' }),
    norm({ step: 'sub.settle-enable', ok: true, consumer: 'ready', cccd: true, staged: 5, effects: '' }),
    norm({ step: 'sub.settle-enable', ok: true, consumer: 'ready', cccd: true, staged: 5, effects: '' }),
    norm({
      step: 'sub.notify',
      ok: true,
      bytes: 'b1',
      delivery: 'c1=delivered',
      consumer: 'ready',
      cccd: true,
      staged: 0,
      effects: ''
    }),
    norm({ step: 'sub.take', ok: true, bytes: 'b1', consumer: 'ready', cccd: true, staged: 0, effects: '' }),
    norm({
      step: 'sub.notify',
      ok: true,
      bytes: 'c2',
      delivery: 'c2=delivered',
      consumer: 'ready',
      cccd: true,
      staged: 0,
      effects: ''
    }),
    norm({ step: 'sub.take', ok: true, bytes: 'c2', consumer: 'ready', cccd: true, staged: 0, effects: '' }),
    norm({
      step: 'sub.notify',
      ok: true,
      bytes: 'a0',
      delivery: 'c0=delivered',
      consumer: 'ready',
      cccd: true,
      staged: 0,
      effects: ''
    }),
    norm({ step: 'sub.take', ok: true, bytes: 'a0', consumer: 'ready', cccd: true, staged: 0, effects: '' })
  ]
}

const OVERFLOW_BYTES_200 = 'ab'.repeat(200)

/** Staged program closing `subscription.pre-ready-overflow-controls-and-late-quarantine`. */
export const PROGRAM_SUBSCRIPTION_OVERFLOW: StagedProgram = {
  scenarioId: 'subscription.pre-ready-overflow-controls-and-late-quarantine',
  provenance:
    'Pins transcribe pre-ready quarantine and error-policy overflow terminals; captured from a ' +
    'staged run of this program.',
  steps: [
    ...linkSetup('lease-a', 'conn0'),
    discoverDb('lease-a'),
    step('{"step":"sub.subscribe","op":"sub0","path":1,"consumer":"c0","policy":"error","items":1,"bytes":128}'),
    step('{"step":"sub.notify","path":1,"value":"aa","consumer":"c0"}'),
    step('{"step":"sub.quarantined","path":1,"consumer":"c0"}'),
    step('{"step":"sub.settle-enable","path":1,"success":true,"consumer":"c0"}'),
    step(`{"step":"sub.notify","path":1,"value":"${OVERFLOW_BYTES_200}","consumer":"c0"}`),
    step('{"step":"sub.take-terminal","path":1,"consumer":"c0"}')
  ],
  expected: [
    ...linkSetupSeen('@op0'),
    norm({
      step: 'gatt.discover',
      ok: true,
      peer_key: 'platform-guid:peer-1',
      services: 1,
      paths: 3,
      first_path: 0,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'sub.subscribe',
      ok: true,
      op_id: '@op1',
      consumer: 'enabling',
      cccd: false,
      staged: 1,
      effects: 'central.subscribe-enable#@op1:subscribe.enable'
    }),
    norm({
      step: 'sub.notify',
      ok: true,
      bytes: 'aa',
      delivery: 'c0=quarantined-pre-ready',
      consumer: 'enabling',
      cccd: false,
      staged: 0,
      effects: ''
    }),
    norm({ step: 'sub.quarantined', ok: true, quarantined: 1, staged: 0, effects: '' }),
    norm({
      step: 'sub.settle-enable',
      ok: true,
      consumer: 'ready',
      cccd: true,
      staged: 5,
      effects: ''
    }),
    norm({
      step: 'sub.notify',
      ok: true,
      bytes: OVERFLOW_BYTES_200,
      delivery: 'c0=delivered',
      consumer: 'failed',
      cccd: true,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'sub.take-terminal',
      ok: true,
      reason: 'overflow',
      dropped_items: 1,
      dropped_bytes: 200,
      replaced_items: 0,
      consumer: 'failed',
      cccd: true,
      staged: 0,
      effects: ''
    })
  ]
}

/** Staged program closing `lifecycle.destroy-idempotency-admission-and-exact-settlement`. */
export const PROGRAM_LIFECYCLE_DESTROY: StagedProgram = {
  scenarioId: 'lifecycle.destroy-idempotency-admission-and-exact-settlement',
  provenance:
    'Pins transcribe destroy idempotency (released twice) and post-destroy arbitration; captured ' +
    'from a staged run of this program.',
  steps: [
    step('{"step":"scan.start","op":"s0","owner":"owner-a"}'),
    step('{"step":"staged.destroy"}'),
    step('{"step":"staged.destroy"}'),
    step('{"step":"scan.start","op":"s1","owner":"owner-b"}'),
    step('{"step":"op.expire-sweep","now":99}')
  ],
  expected: [
    norm({
      step: 'scan.start',
      ok: true,
      op_id: '@op0',
      owner: 'owner-a',
      staged: 1,
      effects: 'central.scan-start#@op0:scan.start'
    }),
    norm({ step: 'staged.destroy', ok: true, state: 'released', staged: 2, effects: '' }),
    norm({ step: 'staged.destroy', ok: true, state: 'released', staged: 0, effects: '' }),
    norm({
      step: 'scan.start',
      ok: false,
      error: 'scan.already-active|core|staged-scan-start|scan.arbitration'
    }),
    norm({ step: 'op.expire-sweep', ok: true, settled: 0, truncated: false, staged: 0, effects: '' })
  ]
}

/** Staged program closing `scenario.scan-connect-discover-read-notify-destroy`. */
export const PROGRAM_VERTICAL: StagedProgram = {
  scenarioId: 'scenario.scan-connect-discover-read-notify-destroy',
  provenance:
    'Pins transcribe the full scan-to-destroy vertical receipts (connect/read/subscribe/notify/take ' +
    'sequencing); captured from a staged run of this program.',
  steps: [
    step('{"step":"scan.start","op":"scan0","owner":"owner-a"}'),
    step('{"step":"scan.platform","op":"scan0","event":"platform-started"}'),
    ADVERTISE_P,
    step('{"step":"link.connect","peer":"p","lease":"lease-a","op":"conn0"}'),
    step('{"step":"link.established","peer":"p","op":"conn0"}'),
    discoverDb('lease-a'),
    step('{"step":"gatt.read","op":"read0","path":1,"value":"deadbeef","settle":"success"}'),
    step('{"step":"sub.subscribe","op":"sub0","path":1,"consumer":"c0"}'),
    step('{"step":"sub.settle-enable","path":1,"success":true,"consumer":"c0"}'),
    step('{"step":"sub.notify","path":1,"value":"0102","consumer":"c0"}'),
    step('{"step":"sub.take","path":1,"consumer":"c0"}'),
    step('{"step":"sub.unsubscribe","path":1,"consumer":"c0"}'),
    step('{"step":"sub.settle-disable","path":1,"consumer":"c0"}'),
    step('{"step":"staged.destroy"}')
  ],
  expected: [
    norm({
      step: 'scan.start',
      ok: true,
      op_id: '@op0',
      owner: 'owner-a',
      staged: 1,
      effects: 'central.scan-start#@op0:scan.start'
    }),
    norm({
      step: 'scan.platform',
      ok: true,
      event: 'platform-started',
      state: 'active',
      staged: 0,
      effects: 'central.scan-settled#@op0:scan.platform-started'
    }),
    ADVERTISE_P_SEEN,
    norm({
      step: 'link.connect',
      ok: true,
      op_id: '@op1',
      peer_key: 'platform-guid:peer-1',
      staged: 1,
      effects: 'central.connect#@op1:connection.connect'
    }),
    norm({
      step: 'link.established',
      ok: true,
      settle: 'settled',
      terminal: 'succeeded',
      cause: null,
      reached_radio: true,
      commit: 'committed',
      suppressed: 0,
      connection: 'connected',
      staged: 5,
      effects: ''
    }),
    norm({
      step: 'gatt.discover',
      ok: true,
      peer_key: 'platform-guid:peer-1',
      services: 1,
      paths: 3,
      first_path: 0,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'gatt.read',
      ok: true,
      op_id: '@op2',
      settle: 'settled',
      terminal: 'succeeded',
      cause: null,
      reached_radio: true,
      commit: 'committed',
      suppressed: 0,
      bytes: 'deadbeef',
      staged: 6,
      effects: 'central.read#@op2:gatt.read'
    }),
    norm({
      step: 'sub.subscribe',
      ok: true,
      op_id: '@op3',
      consumer: 'enabling',
      cccd: false,
      staged: 1,
      effects: 'central.subscribe-enable#@op3:subscribe.enable'
    }),
    norm({
      step: 'sub.settle-enable',
      ok: true,
      consumer: 'ready',
      cccd: true,
      staged: 5,
      effects: ''
    }),
    norm({
      step: 'sub.notify',
      ok: true,
      bytes: '0102',
      delivery: 'c0=delivered',
      consumer: 'ready',
      cccd: true,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'sub.take',
      ok: true,
      bytes: '0102',
      consumer: 'ready',
      cccd: true,
      staged: 0,
      effects: ''
    }),
    norm({
      step: 'sub.unsubscribe',
      ok: true,
      physical_disable: true,
      consumer: 'removing',
      cccd: false,
      staged: 1,
      effects: 'central.subscribe-disable#@op4:subscribe.disable'
    }),
    norm({
      step: 'sub.settle-disable',
      ok: true,
      consumer: 'removed',
      cccd: false,
      staged: 5,
      effects: ''
    }),
    norm({ step: 'staged.destroy', ok: true, state: 'released', staged: 2, effects: '' })
  ]
}

/**
 * Sharpened stay-open probe: scenarios that genuinely need real radio keep
 * pinning their loud `request-ble-transition` rejection AND prove the
 * missing source through the staged surface (an adapter/host-scoped
 * capability the synthetic radio cannot project fails closed with
 * `capability.unavailable`, never faked present).
 */
export interface StayOpenProbe {
  readonly scenarioId: string
  /** Staged steps proving the missing source. */
  readonly steps: readonly string[]
  /** Normalized expected staged observations. */
  readonly expected: readonly string[]
  /** Why real radio is still required (recorded in the candidate). */
  readonly reason: string
}

function stayOpenIdentity(scenarioId: string): StayOpenProbe {
  return {
    scenarioId,
    steps: [CAP_PROJECT, CAP_ROWS, capCheck('adapter.enumeration')],
    expected: [
      CAP_PROJECTED,
      CAP_ROWS_SEEN,
      norm({
        step: 'cap.check',
        ok: false,
        error: 'capability.unavailable|capability|staged-cap-check|capability.unknown'
      })
    ],
    reason:
      'no adapter enumeration or peer handshake on this boundary: provider/adapter ' +
      'identity and version negotiation need a real host adapter and a live peer, ' +
      'so the staged core projects no identity-scoped capability (proven by the ' +
      'capability.unavailable probe above) and the transition stays loudly unwired.'
  }
}

/** Stay-open probes for the six scenarios that genuinely need real radio. */
export const STAY_OPEN_STAGED_PROBES: readonly StayOpenProbe[] = [
  stayOpenIdentity('identity.provider-loadability-and-adapter-availability'),
  stayOpenIdentity('identity.adapter-selection-and-unique-instance'),
  stayOpenIdentity('identity.valid-all-axis-negotiation'),
  stayOpenIdentity('identity.version-skew-and-malformed-offers'),
  {
    scenarioId: 'adapter.atomic-snapshot-and-watch',
    steps: [CAP_PROJECT, capCheck('adapter.watch')],
    expected: [
      CAP_PROJECTED,
      norm({
        step: 'cap.check',
        ok: false,
        error: 'capability.unavailable|capability|staged-cap-check|capability.unknown'
      })
    ],
    reason:
      'no adapter state source on this boundary: atomic snapshots and watches ' +
      'need a live OS adapter emitting state transitions, so the staged core ' +
      'projects no adapter.watch capability (proven above) and set-adapter-state ' +
      'stays loudly unwired.'
  },
  {
    scenarioId: 'diagnostics.trace-redaction-and-resource-counters',
    steps: [step('{"step":"staged.counters"}')],
    expected: [
      norm({
        step: 'staged.counters',
        ok: true,
        staged_total: 0,
        dropped_not_staged: 0,
        truncated_sweeps: 0,
        cap: STAGED_BATCH_CAP,
        staged: 0,
        effects: ''
      })
    ],
    reason:
      'no operation-traffic trace sink on this boundary: diagnostics trace ' +
      'redaction needs real traffic through a redacting sink, so the staged ' +
      'core exposes only bounded-batch accounting (proven above) and the ' +
      'transition stays loudly unwired.'
  }
]

/** The twelve transition-proving staged programs (one per closed scenario). */
export const STAGED_PROGRAMS: readonly StagedProgram[] = [
  PROGRAM_CAPABILITY,
  PROGRAM_SCAN_OWNER,
  PROGRAM_SCAN_FAIRNESS,
  PROGRAM_CONNECTION_LEASE,
  PROGRAM_CONNECTION_ARBITRATION,
  PROGRAM_GATT_DISCOVERY,
  PROGRAM_GATT_DUPLICATE_OCCURRENCES,
  PROGRAM_GATT_IO,
  PROGRAM_SUBSCRIPTION_FANOUT,
  PROGRAM_SUBSCRIPTION_OVERFLOW,
  PROGRAM_LIFECYCLE_DESTROY,
  PROGRAM_VERTICAL
]

/** Finds the staged program closing one scenario, if it has one. */
export function stagedProgramFor(scenarioId: string): StagedProgram | undefined {
  return STAGED_PROGRAMS.find(program => program.scenarioId === scenarioId)
}

/** Finds the stay-open probe for one scenario, if it has one. */
export function stayOpenProbeFor(scenarioId: string): StayOpenProbe | undefined {
  return STAY_OPEN_STAGED_PROBES.find(probe => probe.scenarioId === scenarioId)
}

/** Contract-level staged observations of the Rust backend for one program. */
export interface StagedScenarioRow {
  readonly scenarioId: string
  /** Normalized observations, one per program step, in order. */
  readonly observations: readonly string[]
  /** Normalized batch accounting after the program. */
  readonly counters: string
  /** Newline-joined drained log (raw, for sequencing audits). */
  readonly drained: string
}

/**
 * Observes the Rust backend for one staged program on a FRESH native
 * session (one session per scenario, closed before return). Must-succeed
 * plumbing lets native rejections propagate (an unexpected regression
 * fails its scenario loudly); step-level observations are data and are
 * returned verbatim for EQUAL comparison against the pins above.
 */
export function observeStagedScenario(addon: RustNativeAddon, program: StagedProgram): StagedScenarioRow {
  const driver = new RustBackendDriver(addon, RUST_PARITY_REVISION)
  const raw: string[] = program.steps.map((line, index) => {
    const outcome = driver.stagedStep(line)
    if (!outcome.ok) {
      throw new Error(
        `staged program ${program.scenarioId} step ${index} rejected at the binding lifetime: ` +
          `${outcome.error.code}|${outcome.error.domain}|${outcome.error.operation}|${outcome.error.detail}`
      )
    }
    return outcome.value
  })
  const observations = normalizeStagedLines(raw)
  const countersOutcome = driver.stagedCounters()
  if (!countersOutcome.ok) {
    throw new Error(`staged program ${program.scenarioId} counters rejected at the binding lifetime`)
  }
  const counters = normalizeStagedLines([countersOutcome.value])[0] ?? ''
  const drainedOutcome = driver.stagedDrainLog()
  if (!drainedOutcome.ok) {
    throw new Error(`staged program ${program.scenarioId} drain rejected at the binding lifetime`)
  }
  driver.close()
  return { scenarioId: program.scenarioId, observations, counters, drained: drainedOutcome.value }
}

/** Observes one stay-open probe program on a FRESH native session. */
export function observeStayOpenProbe(addon: RustNativeAddon, probe: StayOpenProbe): readonly string[] {
  const driver = new RustBackendDriver(addon, RUST_PARITY_REVISION)
  const raw: string[] = probe.steps.map((line, index) => {
    const outcome = driver.stagedStep(line)
    if (!outcome.ok) {
      throw new Error(`stay-open probe ${probe.scenarioId} step ${index} rejected at the binding lifetime`)
    }
    return outcome.value
  })
  const observations = normalizeStagedLines(raw)
  driver.close()
  return observations
}
