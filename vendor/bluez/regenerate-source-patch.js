#!/usr/bin/env node
'use strict'

// Mechanical source-asset regeneration. Never builds or installs a daemon.
const fs = require('node:fs')
const path = require('node:path')
const os = require('node:os')
const crypto = require('node:crypto')
const { spawnSync } = require('node:child_process')

const [archive, patched] = process.argv.slice(2)
if (!archive || !patched || !path.isAbsolute(archive) || !path.isAbsolute(patched)) {
  throw new Error('usage: regenerate-source-patch.js /absolute/upstream.tar.xz /absolute/patched-source')
}
const manifestPath = path.join(__dirname, 'source-asset-manifest.json')
const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'))
const digest = data => crypto.createHash('sha256').update(data).digest('hex')
if (digest(fs.readFileSync(archive)) !== manifest.upstream.sha256) {
  throw new Error('upstream archive hash mismatch; no source assets changed')
}
const files = [
  'Makefile.am',
  'Makefile.in',
  'src/adapter.c',
  'src/bearer.c',
  'src/device.c',
  'src/device.h',
  'src/gatt-client.c',
  'src/gatt-client.h',
  'src/gatt-database.c',
  'src/shared/att.c',
  'src/shared/att.h',
  'src/shared/gatt-client.c',
  'src/shared/gatt-client.h',
  'src/ubm-gatt-state.h',
  'src/ubm-le-lease.c',
  'src/ubm-le-lease.h',
  'unit/test-ubm-att-exchange.c',
  'unit/test-ubm-bonded-notify.c',
  'unit/test-ubm-device.c',
  'unit/test-ubm-gatt-projection.c',
  'unit/test-ubm-gatt-state.c',
  'unit/test-ubm-le-lease.c',
  'unit/test-ubm-refresh.c',
  'unit/test-ubm-scan-filter.c',
  'unit/test-ubm-acquired-failure.c'
]
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-bluez-patch-owner-'))
try {
  const extraction = spawnSync('tar', ['-xJf', archive, '-C', scratch], { encoding: 'utf8' })
  if (extraction.status !== 0) throw new Error(extraction.stderr || 'upstream extraction failed')
  const baseline = path.join(scratch, `bluez-${manifest.version}`)
  let patch = ''
  for (const file of files) {
    const original = path.join(baseline, file)
    const modified = path.join(patched, file)
    if (!fs.statSync(modified).isFile()) throw new Error(`missing patched source ${file}`)
    const result = spawnSync(
      'diff',
      [
        '-u',
        '--label',
        `a/${file}`,
        '--label',
        `b/${file}`,
        fs.existsSync(original) ? original : '/dev/null',
        modified
      ],
      { encoding: 'utf8' }
    )
    if (result.status !== 0 && result.status !== 1) throw new Error(result.stderr || `diff failed: ${file}`)
    patch += result.stdout
  }
  if (!patch) throw new Error('refusing an empty source patch')
  const producer = fs.readFileSync(path.join(patched, 'src/ubm-le-lease.c'), 'utf8')
  const contract = /uint32_t protocol = (\d+), lease = (\d+), gatt = (\d+);/.exec(producer)
  if (!contract) throw new Error('maintained producer has no exact authority version declaration')
  manifest.distribution.linuxAuthorityContract = contract.slice(1).map(Number)
  fs.writeFileSync(path.join(__dirname, manifest.patch.file), patch)
  manifest.patch.sha256 = digest(patch)
  fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`)
  process.stdout.write(`Regenerated source patch ${manifest.patch.sha256}\n`)
} finally {
  // This directory was generated above, contains no user checkout, and is never
  // obtained from a caller-supplied destructive target.
  fs.rmSync(scratch, { recursive: true })
}
