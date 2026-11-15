// Bundle with `bun build __tests__/web/web-real-indexeddb-upgrade-fixture.ts
// --target=browser --outfile=<directory>/fixture.js`. Serve that directory on
// localhost with a page loading /fixture.js as a module. Use a fresh browser
// storage partition: this deliberately upgrades its UBM database to version 3.
// The page reports PASS or FAIL. It uses real IndexedDB and no BLE.
import { createWebRecordingController } from '../../src/web'

const finish = (message: string) => {
  document.body.textContent = message
}

async function run() {
  const controller = await createWebRecordingController()
  const upgraded = await new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open('ubm-ordered-recordings', 3)
    request.onupgradeneeded = () => request.result.createObjectStore('upgradeEvidence')
    request.onerror = () => reject(request.error)
    request.onblocked = () => reject(new Error('version 3 upgrade was blocked'))
    request.onsuccess = () => resolve(request.result)
  })
  if (upgraded.version !== 3 || !upgraded.objectStoreNames.contains('recordings')) {
    throw new Error('upgrade did not preserve prior stores')
  }
  upgraded.close()
  for (const operation of [() => controller.contextOrdered('browser-upgrade'), () => createWebRecordingController()]) {
    let code: unknown
    try {
      await operation()
    } catch (error) {
      code = error instanceof Error && 'code' in error ? error.code : undefined
    }
    if (code !== 'protocol.incompatible') throw new Error(`expected protocol.incompatible, received ${String(code)}`)
  }
  finish(
    'PASS real IndexedDB version 3 upgrade preserves stores and both existing and new public controllers reject incompatible schema'
  )
}

run().catch(error => finish(`FAIL ${error instanceof Error ? error.stack : String(error)}`))
