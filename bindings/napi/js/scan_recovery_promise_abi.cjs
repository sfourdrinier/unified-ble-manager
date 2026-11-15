'use strict'

// Load the actual compiled addon and exercise the Promise ABI of the trusted
// scan-recovery surface. No recovery is provisioned here: an empty recording
// id must reject from Rust, which proves the exported async method settles a
// Node Promise instead of only compiling as a Rust #[napi] method.
const assert = require('node:assert/strict')
const path = require('node:path')

const addon = require(
  process.env.UBM_NAPI_ADDON || path.join(__dirname, '..', `ubm_echo.${process.platform}-${process.arch}.node`)
)

async function main() {
  const recordings = new addon.ContinuationRecordingStore()
  const recovery = recordings.scanFailureRecovery()
  const rejectedAttach = recovery.attach('')
  assert.equal(typeof rejectedAttach?.then, 'function', 'attach returns a Node Promise')
  await assert.rejects(rejectedAttach, /argument\.invalid/u)

  const rejectedInventory = recovery.owners('forged-lease')
  assert.equal(typeof rejectedInventory?.then, 'function', 'owners returns a Node Promise')
  await assert.rejects(rejectedInventory, /unknown or revoked scan recovery handle/u)
  console.log('scan recovery Promise ABI smoke passed')
}

main().catch(error => {
  console.error(error)
  process.exitCode = 1
})
