// Run in a fresh secure local browser context. Uses real IndexedDB and Web Locks; no BLE adapter or external service.
import { openWebRecordingDatabase } from '../../src/web/web-recording-database'
import { createWebTerminalEvidenceSource } from '../../src/web/web-terminal-evidence'

const assert = (condition, message) => {
  if (!condition) throw new Error(message)
}
const hex = bytes => Array.from(new Uint8Array(bytes), byte => byte.toString(16).padStart(2, '0')).join('')
const digest = async value => hex(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(value)))
const digestBytes = async value => hex(await crypto.subtle.digest('SHA-256', value))

async function main() {
  assert(
    globalThis.isSecureContext && globalThis.indexedDB && navigator.locks && crypto.subtle,
    'secure IndexedDB, Web Locks and Web Crypto required'
  )
  const database = await openWebRecordingDatabase(indexedDB)
  const options = { database, locks: navigator.locks, digest, digestBytes, maximumRetainedBytes: 256 }
  try {
    for (const state of ['committed', 'staged']) {
      const request = {
        producer: 'subscription',
        maximumBytes: 64,
        formatVersion: 1,
        scope: { managerId: 'browser-fixture', operationId: `terminal-${state}`, incarnation: '1' }
      }
      const source = createWebTerminalEvidenceSource(options)
      const reservation = await source.reserveTerminalEvidence(request)
      const bytes = new TextEncoder().encode('terminal-bytes')
      const result = await reservation.commit({ canonicalBytes: bytes, digest: await digestBytes(bytes) })
      assert(
        result.durability === 'committed' && result.byteLength === bytes.byteLength,
        'terminal evidence did not commit'
      )
      await reservation.release()
      if (state === 'staged') {
        // Retain the exact staged format that survives a failed second commit.
        await new Promise((resolve, reject) => {
          const tx = database.transaction(['terminalEvidence'], 'readwrite')
          const store = tx.objectStore('terminalEvidence')
          const read = store.get(result.ref)
          read.onsuccess = () => store.put({ ...read.result, state: 'staged' })
          tx.oncomplete = resolve
          tx.onabort = () => reject(tx.error)
        })
      }
      const listed = await source.listTerminalEvidence()
      assert(
        listed.some(row => row.ref === result.ref && row.state === state),
        'real IndexedDB listing failed'
      )
      const resumed = createWebTerminalEvidenceSource(options)
      const reattached = await resumed.reserveTerminalEvidence(request)
      await reattached.release()
      const retained = await resumed.readTerminalEvidence(result.ref, 64)
      assert(
        retained.kind === (state === 'committed' ? 'committed' : 'unresolved'),
        'runtime release deleted durable evidence'
      )
      await resumed.retryPendingTerminalEvidence()
      const recovered = await resumed.readTerminalEvidence(result.ref, 64)
      assert(
        recovered.kind === 'committed' &&
          recovered.digest === result.digest &&
          Array.from(recovered.canonicalBytes).join(',') === Array.from(bytes).join(','),
        'retry changed retained evidence'
      )
      await resumed.releaseTerminalEvidence(result.ref)
      assert(
        (await resumed.readTerminalEvidence(result.ref, 64)).kind === 'missing',
        'recovery ACK did not retire evidence'
      )
    }
  } finally {
    database.close()
  }
  document.body.textContent =
    'PASS real IndexedDB and Web Locks: list, reattach, release, staged retry, exact bytes and recovery ACK'
}
main().catch(error => {
  document.body.textContent = `FAIL ${error.stack || error}`
})
