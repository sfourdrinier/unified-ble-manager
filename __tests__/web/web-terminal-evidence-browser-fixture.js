// Run in a secure local browser context. Uses real IndexedDB and Web Locks; no BLE adapter or network.
import { openWebRecordingDatabase } from '../../src/web/web-recording-database'
import { createWebTerminalEvidenceSource } from '../../src/web/web-terminal-evidence'

const assert = (condition, message) => { if (!condition) throw new Error(message) }
const hex = bytes => Array.from(new Uint8Array(bytes), byte => byte.toString(16).padStart(2, '0')).join('')
const digest = async value => hex(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(value)))
const digestBytes = async value => hex(await crypto.subtle.digest('SHA-256', value))

async function main() {
  assert(globalThis.isSecureContext && globalThis.indexedDB && navigator.locks && crypto.subtle, 'secure IndexedDB, Web Locks and Web Crypto required')
  const database = await openWebRecordingDatabase(indexedDB)
  const source = createWebTerminalEvidenceSource({ database, locks: navigator.locks, digest, digestBytes, maximumRetainedBytes: 256 })
  const reservation = await source.reserveTerminalEvidence({
    producer: 'subscription', maximumBytes: 64, formatVersion: 1,
    scope: { managerId: 'browser-fixture', operationId: 'terminal-1', incarnation: '1' }
  })
  const bytes = new TextEncoder().encode('terminal-bytes')
  const result = await reservation.commit({ canonicalBytes: bytes, digest: await digestBytes(bytes) })
  assert(result.durability === 'committed' && result.byteLength === bytes.byteLength, 'terminal evidence did not commit')
  await reservation.release()
  database.close()
  document.body.textContent = 'PASS real IndexedDB and Web Locks durable terminal evidence reservation and commit'
}
main().catch(error => { document.body.textContent = `FAIL ${error.stack || error}` })
