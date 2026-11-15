// __tests__/backends/desktop/desktop-terminal-evidence-canonical-bytes.test.js
'use strict'

const crypto = require('node:crypto')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const { nextItem, openBackend, realBinding, scanOptions } = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { encodeTerminalEvidence } = require('../../../src/core/terminal-evidence-codec')
const { decodeTerminalEvidence } = require('../../../src/core/terminal-evidence-codec')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createDesktopCoreBleManager } = require('../../../src/node-desktop-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

jest.setTimeout(30000)

async function recoverAfterSourceRelease(stage, recipient, leaseId, count) {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const page = await stage.recoverTerminalEvidenceWithStatus({ producer: 'scan', recipient, leaseId })
    if (!page.busy && page.entries.length === count) return page.entries
    await new Promise(resolve => setTimeout(resolve, 10))
  }
  throw new Error('native source reservations did not hand off to recovery')
}

test('public desktop recovery retries a failed native terminal stage while the manager remains alive', async () => {
  const { spawn } = require('node:child_process')
  const platform = { darwin: 'corebluetooth', linux: 'bluez', win32: 'winrt' }[process.platform]
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-desktop-stage-retry-'))
  const harness = realBinding(platform)
  const manager = await createDesktopCoreBleManager(platform, {
    binding: harness.binding,
    terminalEvidenceStore: { directory, maxBytes: '4096' },
    terminalEvidence: { maximumCanonicalBytes: 1024, maximumDurableBytes: 4096 }
  })
  let locker
  let destroyed = false
  try {
    const scan = await manager.scan()
    const next = nextItem(scan.observations[Symbol.asyncIterator](), 5000)
    locker = spawn(
      process.execPath,
      [
        '-e',
        "const {DatabaseSync}=require('node:sqlite'); const db=new DatabaseSync(process.argv[1]); db.exec('BEGIN IMMEDIATE'); process.stdout.write('locked\\n'); process.stdin.once('data',()=>{db.exec('ROLLBACK');db.close()})",
        path.join(directory, 'terminal-evidence-v1.sqlite')
      ],
      { stdio: ['pipe', 'pipe', 'pipe'] }
    )
    await new Promise((resolve, reject) => {
      locker.stdout.once('data', resolve)
      locker.once('error', reject)
      locker.once('exit', code => reject(new Error(`SQLite locker exited early: ${code}`)))
    })
    await harness.opened.at(-1).stageScanTerminated(true, 'original terminal after busy storage')
    expect(await next).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
    locker.stdin.end('release')
    await new Promise(resolve => locker.once('exit', resolve))
    let page
    for (let attempt = 0; attempt < 100; attempt += 1) {
      page = await manager.terminalEvidenceRecovery.listWithStatus()
      if (!page.busyProducers.includes('scan') && page.records.length === 1) break
      await new Promise(resolve => setTimeout(resolve, 10))
    }
    expect(page.busyProducers).not.toContain('scan')
    const rows = page.records
    expect(rows).toHaveLength(1)
    const recovered = await manager.terminalEvidenceRecovery.read(rows[0].handle, rows[0].byteLength)
    expect(decodeTerminalEvidence(recovered.canonicalBytes)).toMatchObject({
      terminal: {
        reason: 'source-failed',
        error: { platform: { safeMessage: 'original terminal after busy storage' } }
      }
    })
    await manager.terminalEvidenceRecovery.acknowledge(rows[0].handle)
    await expect(scan.stop()).resolves.toMatchObject({ state: 'released' })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    destroyed = true
  } finally {
    if (locker?.exitCode === null) locker.kill()
    if (!destroyed) await manager.destroy()
    fs.rmSync(directory, { recursive: true, force: true })
  }
})

test('node manager gives owner and joined scans independently recoverable native terminal evidence', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-desktop-joined-evidence-'))
  const harness = realBinding('bluez')
  const now = () => performance.now()
  const provider = createTestDesktopRustCoreBackendProvider({
    platform: 'bluez',
    owner: 'joined-terminal-test',
    now,
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: 'linux',
    terminalEvidenceStore: { directory, maxBytes: '4096' }
  })
  const manager = await createNodeBleManagerFromProvider(provider, DESKTOP_RUST_CORE_PROFILES.bluez.compatibility, {
    now,
    terminalEvidence: { maximumCanonicalBytes: 1024, maximumDurableBytes: 4096 }
  })
  try {
    const owner = await manager.scan(scanOptions({ sharing: { mode: 'owner', allowSharing: true } }))
    const joined = await manager.scan(
      scanOptions({ sharing: { mode: 'join', sharedLeaseId: owner.leaseId, token: owner.shareToken } })
    )
    const ownerIterator = owner.observations[Symbol.asyncIterator]()
    const joinedIterator = joined.observations[Symbol.asyncIterator]()
    const ownerNext = nextItem(ownerIterator, 5000)
    const joinedNext = nextItem(joinedIterator, 5000)
    await harness.opened.at(-1).stageScanTerminated(true, 'joined source ended')
    expect(await ownerNext).toMatchObject({
      kind: 'terminal',
      reason: 'source-failed',
      error: { platform: { safeMessage: 'joined source ended' } }
    })
    expect(await joinedNext).toMatchObject({
      kind: 'terminal',
      reason: 'source-failed',
      error: { platform: { safeMessage: 'joined source ended' } }
    })
    await ownerIterator.next()
    await joinedIterator.next()
    await owner.stop()
    await joined.stop()
    const reserves = harness.calls.filter(([name]) => name === 'reserveTerminalEvidence').map(([, args]) => args[0])
    expect(reserves).toHaveLength(2)
    expect(reserves[0].scope).not.toEqual(reserves[1].scope)
    const recipient = harness.calls.find(([name]) => name === 'bindTerminalEvidenceRecipient')[1][1]
    const rows = await recoverAfterSourceRelease(harness.opened.at(-1), recipient, 'joined-test-recovery', 2)
    expect(rows).toHaveLength(2)
    expect(rows.map(row => decodeTerminalEvidence(Uint8Array.from(row.canonicalBytes)).scope)).toEqual(
      expect.arrayContaining(reserves.map(request => request.scope))
    )
    await harness.opened.at(-1).releaseTerminalEvidenceRecoveryLease('joined-test-recovery')
  } finally {
    await manager.destroy()
    fs.rmSync(directory, { recursive: true, force: true })
  }
})

test.each(['joined', 'owner'])('node manager %s stop publishes scoped native evidence', async stopped => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-desktop-joined-stop-'))
  const harness = realBinding('bluez')
  const now = () => performance.now()
  const provider = createTestDesktopRustCoreBackendProvider({
    platform: 'bluez',
    owner: `joined-stop-${stopped}`,
    now,
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: 'linux',
    terminalEvidenceStore: { directory, maxBytes: '4096' }
  })
  const manager = await createNodeBleManagerFromProvider(provider, DESKTOP_RUST_CORE_PROFILES.bluez.compatibility, {
    now,
    terminalEvidence: { maximumCanonicalBytes: 1024, maximumDurableBytes: 4096 }
  })
  try {
    const owner = await manager.scan(scanOptions({ sharing: { mode: 'owner', allowSharing: true } }))
    const joined = await manager.scan(
      scanOptions({ sharing: { mode: 'join', sharedLeaseId: owner.leaseId, token: owner.shareToken } })
    )
    const joinedIterator = joined.observations[Symbol.asyncIterator]()
    const ownerIterator = owner.observations[Symbol.asyncIterator]()
    const joinedNext = nextItem(joinedIterator, 5000)
    const ownerNext = nextItem(ownerIterator, 5000)
    await expect((stopped === 'owner' ? owner : joined).stop()).resolves.toMatchObject({ state: 'released' })
    expect(await joinedNext).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
    await joinedIterator.next()
    if (stopped === 'joined') {
      expect(harness.opened.at(-1).operationalScanId()).not.toBeNull()
      await owner.stop()
    }
    expect(await ownerNext).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
    await ownerIterator.next()
    const reserves = harness.calls.filter(([name]) => name === 'reserveTerminalEvidence').map(([, args]) => args[0])
    const recipient = harness.calls.find(([name]) => name === 'bindTerminalEvidenceRecipient')[1][1]
    const rows = await recoverAfterSourceRelease(harness.opened.at(-1), recipient, `stop-recovery-${stopped}`, 2)
    expect(rows).toHaveLength(2)
    expect(rows.map(row => decodeTerminalEvidence(Uint8Array.from(row.canonicalBytes)).scope)).toEqual(
      expect.arrayContaining(reserves.map(request => request.scope))
    )
    await harness.opened.at(-1).releaseTerminalEvidenceRecoveryLease(`stop-recovery-${stopped}`)
  } finally {
    await manager.destroy()
    fs.rmSync(directory, { recursive: true, force: true })
  }
})

const PLATFORMS = ['bluez', 'corebluetooth', 'winrt']

if (process.env.UBM_RETAINED_TERMINAL_FIXTURE_DIRECTORY) {
  test('one joined stage failure preserves its source debt while the owner commits its own terminal', async () => {
    await withLedger(
      'bluez',
      async ({ backend, stage, harness }) => {
        const ownerScope = scalarScope('failure-owner')
        const joinedScope = scalarScope('failure-joined')
        const ownerReservation = await backend.scanner.reserveTerminalEvidence({
          producer: 'scan',
          maximumBytes: 1024,
          formatVersion: 1,
          scope: ownerScope
        })
        const joinedReservation = await backend.scanner.reserveTerminalEvidence({
          producer: 'scan',
          maximumBytes: 1024,
          formatVersion: 1,
          scope: joinedScope
        })
        const owner = await backend.scanner.start(
          scanOptions({ terminalEvidenceScope: ownerScope, sharing: { mode: 'owner', allowSharing: true } })
        )
        const joined = await backend.scanner.join(owner.leaseId, owner.shareToken, undefined, joinedScope)
        const binds = harness.calls.filter(([name]) => name === 'bindTerminalEvidenceRecipient')
        const [reservationId, recipient] = binds[1][1]
        await stage.stageTerminalEvidence(reservationId, recipient, Uint8Array.from([1, 2, 3]))
        const ownerNext = nextItem(owner.observations[Symbol.asyncIterator](), 5000)
        const joinedNext = nextItem(joined.observations[Symbol.asyncIterator](), 5000)
        await stage.stageScanTerminated(true, 'original sibling terminal')
        expect(await ownerNext).toMatchObject({
          kind: 'terminal',
          reason: 'source-failed',
          error: { platform: { safeMessage: 'original sibling terminal' } },
          evidence: { producer: 'scan', scope: ownerScope }
        })
        expect(await joinedNext).toMatchObject({
          kind: 'terminal',
          reason: 'source-failed',
          error: { code: 'argument.invalid' }
        })
        await ownerReservation.release()
        const partial = await backend.recoverTerminalEvidenceForHostWithStatus('partial-scan-recovery')
        expect(partial.busyProducers).toContain('scan')
        expect(partial.records).toHaveLength(1)
        const recovered = await backend.readRecoveredTerminalEvidence(partial.records[0].reference, 1024)
        expect(decodeTerminalEvidence(recovered.canonicalBytes)).toMatchObject({
          scope: ownerScope,
          terminal: { error: { platform: { safeMessage: 'original sibling terminal' } } }
        })
        await backend.releaseTerminalEvidenceRecoveryLease('partial-scan-recovery')
        await expect(joinedReservation.release()).rejects.toThrow()
        const retained = await stage.takeScanTerminalEvent()
        expect(retained).toMatchObject({
          detail: 'original sibling terminal',
          joinedTerminalEvidence: [{ reservationId, stageFailure: expect.any(String) }]
        })
      },
      true
    )
  })
}

test.each(['joined', 'owner'])(
  '%s error overflow commits its exact native terminal before publishing',
  async target => {
    await withLedger('bluez', async ({ backend, stage, harness }) => {
      const scope = scalarScope('joined-overflow')
      const reservation = await backend.scanner.reserveTerminalEvidence({
        producer: 'scan',
        maximumBytes: 1024,
        formatVersion: 1,
        scope
      })
      const owner = await backend.scanner.start(
        scanOptions({
          ...(target === 'owner' ? { terminalEvidenceScope: scope } : {}),
          delivery: { itemCapacity: 1, byteCapacity: 4096, reservedControlCapacity: 1024, overflowPolicy: 'error' },
          sharing: { mode: 'owner', allowSharing: true }
        })
      )
      const joined = await backend.scanner.join(
        owner.leaseId,
        owner.shareToken,
        undefined,
        target === 'joined' ? scope : undefined
      )
      await stage.stageAdvertisement({ peerId: 'overflow-a', rssi: -50 })
      await stage.stageAdvertisement({ peerId: 'overflow-b', rssi: -51 })
      for (
        let attempt = 0;
        attempt < 100 && !harness.calls.some(([name]) => name === 'detachJoinedScanEvidence');
        attempt += 1
      )
        await new Promise(resolve => setTimeout(resolve, 10))
      expect(harness.calls.some(([name]) => name === 'detachJoinedScanEvidence')).toBe(true)
      const iterator = (target === 'joined' ? joined : owner).observations[Symbol.asyncIterator]()
      const first = await nextItem(iterator, 5000)
      const terminal = first.kind === 'terminal' ? first : await nextItem(iterator, 5000)
      expect(terminal).toMatchObject({
        kind: 'terminal',
        reason: 'overflow',
        droppedItems: 1,
        droppedBytes: 512,
        evidence: { producer: 'scan', scope }
      })
      const read = await backend.scanner.readTerminalEvidence(terminal.evidence.ref, 1024)
      expect(decodeTerminalEvidence(read.canonicalBytes)).toMatchObject({
        scope,
        terminal: { reason: 'overflow', droppedItems: 1, droppedBytes: 512, replacedItems: 0 }
      })
      expect(stage.operationalScanId()).not.toBeNull()
      await joined.stop()
      await reservation.release()
      await owner.stop()
    })
  }
)

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

function runRetainedLedgerProcess() {
  const { spawnSync } = require('node:child_process')
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-desktop-retained-terminal-'))
  try {
    const child = spawnSync(
      process.execPath,
      [
        require.resolve('jest/bin/jest'),
        '--config',
        'jest.package.config.js',
        '--runInBand',
        '--runTestsByPath',
        __filename
      ],
      {
        cwd: path.resolve(__dirname, '../../..'),
        env: { ...process.env, UBM_RETAINED_TERMINAL_FIXTURE_DIRECTORY: directory },
        encoding: 'utf8',
        timeout: 25000,
        maxBuffer: 1024 * 1024
      }
    )
    if (child.error) throw child.error
    if (child.status !== 0) throw new Error(`retained terminal fixture failed: ${child.stdout}\n${child.stderr}`)
  } finally {
    // The child owns deliberately unreleased native handles until process exit.
    fs.rmSync(directory, { recursive: true, force: true })
  }
}

async function withLedger(platform, run, retainsDebt = false) {
  const retainedDirectory = process.env.UBM_RETAINED_TERMINAL_FIXTURE_DIRECTORY
  if (retainsDebt && !retainedDirectory) throw new Error('retained debt requires process isolation')
  const directory = retainsDebt
    ? fs.mkdtempSync(path.join(retainedDirectory, 'ledger-'))
    : fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-desktop-terminal-canonical-'))
  const opened = await openBackend(platform, { terminalEvidenceStore: { directory, maxBytes: '8192' } })
  try {
    return await run(opened)
  } finally {
    const disposal = await opened.backend.destroy()
    if (retainsDebt) expect(disposal).toMatchObject({ state: 'release-failed' })
    else fs.rmSync(directory, { recursive: true, force: true })
  }
}

async function withNativeLedger(platform, run) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-desktop-terminal-canonical-'))
  const harness = realBinding(platform)
  const stage = await harness.binding.openSynthetic(`terminal-canonical-${platform}`, {
    terminalEvidenceDirectory: directory,
    terminalEvidenceMaxBytes: '8192'
  })
  try {
    return await run({ stage })
  } finally {
    await stage.close()
    fs.rmSync(directory, { recursive: true })
  }
}

async function withPublicLedger(platform, run, retainsDebt = false) {
  const retainedDirectory = process.env.UBM_RETAINED_TERMINAL_FIXTURE_DIRECTORY
  if (retainsDebt && !retainedDirectory) throw new Error('retained debt requires process isolation')
  const directory = retainsDebt
    ? fs.mkdtempSync(path.join(retainedDirectory, 'ledger-'))
    : fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-desktop-terminal-public-'))
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
    const disposal = await manager.destroy()
    if (retainsDebt) expect(disposal).toMatchObject({ state: 'release-failed' })
    else fs.rmSync(directory, { recursive: true, force: true })
  }
}

test.each(PLATFORMS)('%s native scan source stages exactly the JS canonical terminal bytes', async platform => {
  await withNativeLedger(platform, async ({ stage }) => {
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
  await withNativeLedger(platform, async ({ stage }) => {
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
    const recovered = await stage.recoverTerminalEvidence({
      producer: 'scan',
      recipient,
      leaseId: `unpaired-${prefix}`
    })
    expect(recovered).toHaveLength(1)
    expect(Uint8Array.from(recovered[0].canonicalBytes)).toEqual(canonicalBytes)
    expect(Uint8Array.from(recovered[0].scopeUtf16le)).toEqual(rawScope)
  })
})

test.each(PLATFORMS)(
  '%s provider sends the exact unpaired UTF-16 scope sidecar to reserve and start',
  async platform => {
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
  }
)

if (process.env.UBM_RETAINED_TERMINAL_FIXTURE_DIRECTORY) {
  test.each(PLATFORMS)('%s public manager preserves the exact scan terminal staging conflict', async platform => {
    await withPublicLedger(
      platform,
      async ({ manager, stage, harness }) => {
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

        // The original source terminal still needs staging. Releasing its only
        // reservation here would erase the unresolved native recovery debt.
        await expect(stage.releaseTerminalEvidence(reservationId)).rejects.toThrow()
        expect(stage.retryFailedScanEvidence(recipient).failures).toHaveLength(1)
        const retained = await stage.takeScanTerminalEvent()
        expect(retained).toMatchObject({ kind: 'stage-failed', detail: 'conflicting stage must remain visible' })
        await scan.stop()
      },
      true
    )
  })
}

if (!process.env.UBM_RETAINED_TERMINAL_FIXTURE_DIRECTORY) {
  test('retained terminal conflicts preserve debt through process teardown', runRetainedLedgerProcess)
}
