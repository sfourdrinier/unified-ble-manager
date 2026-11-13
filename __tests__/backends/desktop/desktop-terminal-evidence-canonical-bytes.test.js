'use strict'

const crypto = require('node:crypto')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const { nextItem, openBackend, scanOptions } = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { encodeTerminalEvidence } = require('../../../src/core/terminal-evidence-codec')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

jest.setTimeout(30000)

const PLATFORMS = ['bluez', 'corebluetooth', 'winrt']

function canonicalTerminal(prefix, detail) {
  return Object.freeze({
    kind: 'terminal',
    reason: 'source-failed',
    droppedItems: 0,
    droppedBytes: 0,
    replacedItems: 0,
    error: Object.freeze({
      code: 'scan.start-failed',
      domain: 'scan',
      operation: `${prefix}.scan.terminated`,
      retryability: 'never',
      platform: Object.freeze({
        domain: 'desktop-rust-core',
        code: 'scan-terminated',
        safeMessage: detail.slice(0, 512),
        metadata: Object.freeze({})
      })
    })
  })
}

function scalarScope(prefix) {
  return Object.freeze({
    // A literal U+FFFD is a valid scalar and must remain on the established
    // string identity path; it must not be confused with the raw sidecar
    // required for an unpaired UTF-16 unit.
    managerId: `desktop-manager-${prefix}-"\\\u2028😀\ufffd`,
    operationId: `${prefix}.scan-terminal-\n\t😀`,
    incarnation: '1'
  })
}

function unpairedScope(prefix) {
  return Object.freeze({
    // JSON.stringify escapes each unpaired unit independently; it never joins
    // a high surrogate in one field to a low surrogate in another.
    managerId: `desktop-manager-${prefix}-\ud83d`,
    operationId: `${prefix}.scan-terminal-\ude00`,
    incarnation: '1'
  })
}

function scopeUtf16le(producer, evidenceScope) {
  const hasUnpairedCodeUnit = value => {
    for (let index = 0; index < value.length; index += 1) {
      const unit = value.charCodeAt(index)
      if (unit >= 0xd800 && unit <= 0xdbff) {
        const next = value.charCodeAt(index + 1)
        if (next >= 0xdc00 && next <= 0xdfff) {
          index += 1
          continue
        }
        return true
      }
      if (unit >= 0xdc00 && unit <= 0xdfff) return true
    }
    return false
  }
  const fields = [producer, evidenceScope.managerId, evidenceScope.operationId, evidenceScope.incarnation]
  if (fields.every(field => !hasUnpairedCodeUnit(field))) return undefined
  const byteLength = fields.reduce((total, field) => total + 4 + field.length * 2, 0)
  const bytes = new Uint8Array(byteLength)
  const view = new DataView(bytes.buffer)
  let offset = 0
  for (const field of fields) {
    view.setUint32(offset, field.length, true)
    offset += 4
  }
  for (const field of fields) {
    for (let index = 0; index < field.length; index += 1) {
      view.setUint16(offset, field.charCodeAt(index), true)
      offset += 2
    }
  }
  return bytes
}

async function withLedger(platform, run) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-desktop-terminal-canonical-'))
  const opened = await openBackend(platform, { terminalEvidenceStore: { directory, maxBytes: '8192' } })
  try {
    return await run(opened)
  } finally {
    await opened.backend.destroy()
    fs.rmSync(directory, { recursive: true, force: true })
  }
}

async function withPublicLedger(platform, run) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-desktop-terminal-public-'))
  const harness = require('../../helpers/desktop-rust-core-harness').realBinding(platform)
  const now = () => performance.now()
  const provider = createTestDesktopRustCoreBackendProvider({
    platform,
    owner: `terminal-public-${platform}`,
    now,
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: { bluez: 'linux', corebluetooth: 'darwin', winrt: 'win32' }[platform],
    terminalEvidenceStore: { directory, maxBytes: '4096' }
  })
  const manager = await createPublicBleManager(
    await createNodeBleManagerFromProvider(provider, DESKTOP_RUST_CORE_PROFILES[platform].compatibility, {
      now,
      terminalEvidence: { maximumCanonicalBytes: 512, maximumDurableBytes: 4096 }
    }),
    now
  )
  try {
    return await run({ manager, stage: harness.opened.at(-1), harness })
  } finally {
    await manager.destroy()
    fs.rmSync(directory, { recursive: true, force: true })
  }
}

test.each(PLATFORMS)('%s native scan source stages exactly the JS canonical terminal bytes', async platform => {
  await withLedger(platform, async ({ stage }) => {
    const prefix = DESKTOP_RUST_CORE_PROFILES[platform].operationPrefix
    const evidenceScope = scalarScope(prefix)
    const rawScope = scopeUtf16le('scan', evidenceScope)
    // 511 ASCII code units followed by an astral character makes slice(0,
    // 512) end on the high surrogate. The Rust source must emit the same
    // JSON escape sequence before JavaScript sees this terminal.
    const detail = `${'a'.repeat(511)}😀 after-boundary`
    const canonicalBytes = encodeTerminalEvidence(canonicalTerminal(prefix, detail), evidenceScope)
    const reservation = await stage.reserveTerminalEvidence({
      producer: 'scan',
      maximumBytes: String(canonicalBytes.byteLength),
      formatVersion: 1,
      scope: evidenceScope,
      ...(rawScope === undefined ? {} : { scopeUtf16le: rawScope })
    })
    const recipient = `trusted-${prefix}`
    await stage.bindTerminalEvidenceRecipient(reservation.reservationId, recipient)
    await stage.startScan({
      owner: `native-canonical-${prefix}`,
      serviceUuids: [],
      duplicatePolicy: 'all',
      terminalEvidenceScope: evidenceScope,
      terminalEvidenceOperation: `${prefix}.scan.terminated`,
      ...(rawScope === undefined ? {} : { terminalEvidenceScopeUtf16le: rawScope })
    })
    await stage.stageScanTerminated(true, detail)
    let scanTerminal
    do {
      scanTerminal = await stage.takeScanTerminalEvent()
    } while (scanTerminal === null)
    expect(scanTerminal.terminalEvidence).toMatchObject({ reservationId: reservation.reservationId })
    // Source teardown relinquishes only its reservation; staged bytes remain
    // available to the authenticated recovery lease.
    await stage.releaseTerminalEvidence(reservation.reservationId)
    const recovered = await stage.recoverTerminalEvidence({ producer: 'scan', recipient, leaseId: `lease-${prefix}` })
    expect(recovered).toHaveLength(1)
    expect(Uint8Array.from(recovered[0].canonicalBytes)).toEqual(canonicalBytes)
    expect(recovered[0].digest).toBe(crypto.createHash('sha256').update(canonicalBytes).digest('hex'))
  })
})

test.each(PLATFORMS)('%s preserves unpaired UTF-16 scope units in native canonical bytes', async platform => {
  await withLedger(platform, async ({ stage }) => {
    const prefix = DESKTOP_RUST_CORE_PROFILES[platform].operationPrefix
    const evidenceScope = unpairedScope(prefix)
    const rawScope = scopeUtf16le('scan', evidenceScope)
    const detail = 'scope transport must not rewrite canonical identity'
    const canonicalBytes = encodeTerminalEvidence(canonicalTerminal(prefix, detail), evidenceScope)
    const reservation = await stage.reserveTerminalEvidence({
      producer: 'scan',
      maximumBytes: String(canonicalBytes.byteLength),
      formatVersion: 1,
      scope: evidenceScope,
      scopeUtf16le: rawScope
    })
    const recipient = `trusted-unpaired-${prefix}`
    await stage.bindTerminalEvidenceRecipient(reservation.reservationId, recipient)
    await stage.startScan({
      owner: `native-unpaired-${prefix}`,
      serviceUuids: [],
      duplicatePolicy: 'all',
      terminalEvidenceScope: evidenceScope,
      terminalEvidenceOperation: `${prefix}.scan.terminated`,
      terminalEvidenceScopeUtf16le: rawScope
    })
    await stage.stageScanTerminated(true, detail)
    let scanTerminal
    do {
      scanTerminal = await stage.takeScanTerminalEvent()
    } while (scanTerminal === null)
    await stage.releaseTerminalEvidence(reservation.reservationId)
    const recovered = await stage.recoverTerminalEvidence({ producer: 'scan', recipient, leaseId: `unpaired-${prefix}` })
    expect(recovered).toHaveLength(1)
    expect(Uint8Array.from(recovered[0].canonicalBytes)).toEqual(canonicalBytes)
    expect(Uint8Array.from(recovered[0].scopeUtf16le)).toEqual(rawScope)
  })
})

test.each(PLATFORMS)('%s provider sends the exact unpaired UTF-16 scope sidecar to reserve and start', async platform => {
  await withLedger(platform, async ({ backend, harness }) => {
    const prefix = DESKTOP_RUST_CORE_PROFILES[platform].operationPrefix
    const evidenceScope = unpairedScope(prefix)
    const expectedRawScope = scopeUtf16le('scan', evidenceScope)
    const reservation = await backend.scanner.reserveTerminalEvidence({
      producer: 'scan',
      maximumBytes: 512,
      formatVersion: 1,
      scope: evidenceScope
    })
    const lease = await backend.scanner.start(scanOptions({ terminalEvidenceScope: evidenceScope }))
    const reserve = harness.calls.find(([name]) => name === 'reserveTerminalEvidence')[1][0]
    const start = harness.calls.find(([name]) => name === 'startScan')[1][0]
    expect(Uint8Array.from(reserve.scopeUtf16le)).toEqual(expectedRawScope)
    expect(Uint8Array.from(start.terminalEvidenceScopeUtf16le)).toEqual(expectedRawScope)
    await lease.stop()
    await reservation.release()
  })
})

test.each(PLATFORMS)('%s public manager preserves the exact scan terminal staging conflict', async platform => {
  await withPublicLedger(platform, async ({ manager, stage, harness }) => {
    const prefix = DESKTOP_RUST_CORE_PROFILES[platform].operationPrefix
    const scan = await manager.scan()
    const observations = scan.observations[Symbol.asyncIterator]()
    const bind = harness.calls.find(([name]) => name === 'bindTerminalEvidenceRecipient')[1]
    const reservationId = bind[0]
    const recipient = bind[1]
    const earlier = Uint8Array.from([0x65, 0x61, 0x72, 0x6c, 0x69, 0x65, 0x72])
    expect(typeof recipient).toBe('string')
    await stage.stageTerminalEvidence(reservationId, recipient, earlier)

    await stage.stageScanTerminated(true, 'conflicting stage must remain visible')
    expect(await nextItem(observations, 5000)).toMatchObject({
      kind: 'terminal',
      reason: 'source-failed',
      error: { code: 'argument.invalid', operation: `${prefix}.scan.terminal-evidence-stage` }
    })

    await stage.releaseTerminalEvidence(reservationId)
    const leaseId = `conflict-lease-${prefix}`
    const recovered = await stage.recoverTerminalEvidence({
      producer: 'scan',
      recipient,
      leaseId
    })
    try {
      expect(recovered).toHaveLength(1)
      expect(Uint8Array.from(recovered[0].canonicalBytes)).toEqual(earlier)
    } finally {
      await stage.releaseTerminalEvidenceRecoveryLease(leaseId)
    }
    await scan.stop()
  })
})
