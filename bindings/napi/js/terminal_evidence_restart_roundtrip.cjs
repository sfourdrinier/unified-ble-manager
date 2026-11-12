'use strict';
const assert = require('node:assert/strict');
const { mkdtempSync, rmSync } = require('node:fs');
const { tmpdir } = require('node:os');
const { join } = require('node:path');
const { spawnSync } = require('node:child_process');
const { createHash } = require('node:crypto');
const addon = require(process.env.UBM_NAPI_ADDON || join(__dirname, '..', `ubm_echo.${process.platform}-${process.arch}.node`));
const directory = process.argv[2] || mkdtempSync(join(tmpdir(), 'ubm-terminal-restart-'));
async function run() {
  const central = await addon.UbmCentral.openSynthetic('terminal-restart', { platform: 'bluez', terminalEvidenceDirectory: directory, terminalEvidenceMaxBytes: '1024' });
  if (process.argv[3] === 'recover') {
    const rows = await central.recoverTerminalEvidence('scan', 'trusted-recipient');
    assert.equal(rows.length, 1); assert.equal(rows[0].digest, createHash('sha256').update(Buffer.from([1,2,3])).digest('hex'));
    const committed = rows[0].ref ? rows[0] : await central.commitTerminalEvidence({ reservationId: rows[0].reservationId, canonicalBytes: rows[0].canonicalBytes, digest: rows[0].digest });
    // Source teardown only abandons a reservation id. The opaque committed
    // reference is retired solely by the authenticated recovery ACK.
    await central.acknowledgeRecoveredTerminalEvidence({ ref: committed.ref, producer: 'scan', recipient: 'trusted-recipient' }); await central.close(); return;
  }
  const reserved = await central.reserveTerminalEvidence({ producer: 'scan', maximumBytes: '64', formatVersion: 1, scope: { managerId: 'm', operationId: 'o', incarnation: 'i' } });
  const bytes = Buffer.from([1,2,3]);
  await central.stageTerminalEvidence(reserved.reservationId, 'trusted-recipient', bytes);
  // Deliberately exit after durable staging: the child must promote the exact
  // staged bytes after a fresh process opens the native ledger.
  await central.close();
  const child = spawnSync(process.execPath, [__filename, directory, 'recover'], { env: process.env, stdio: 'inherit' });
  assert.equal(child.status, 0); rmSync(directory, { recursive: true, force: true });
}
run().catch(error => { console.error(error); process.exitCode = 1; });
