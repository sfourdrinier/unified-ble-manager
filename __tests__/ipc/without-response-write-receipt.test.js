// __tests__/ipc/without-response-write-receipt.test.js
//
// Findings F1/F2: a renderer without-response write that succeeds on the host
// must decode as success on every IPC host. Electron main forwards the
// provider receipt verbatim (`commitState: 'unknown'` per the contract), and
// the Tauri dispatcher reports the same contract word — the shared renderer
// decoder (`IpcBleManager`/`IpcGattDatabase`) must accept it instead of
// throwing `protocol.malformed ipc-manager.gatt-write-receipt`.

'use strict'

const { IpcGattDatabase } = require('../../src/ipc/manager')

const SERVICE_UUID = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC_UUID = '00002a39-0000-1000-8000-00805f9b34fb'

function stubIpcLink(route) {
  const attachment = {
    attachmentId: 'attachment-write',
    backendInstanceId: 'backend-write',
    backendGeneration: 'backend-generation-write',
    adapter: { adapterId: 'adapter-write', adapterGeneration: 'adapter-generation-write' }
  }
  const manager = { bootstrap: { attachment }, route }
  const connection = {
    peerId: 'peer-write',
    connectionId: 'connection-write',
    ownerLeaseId: 'lease-write',
    connectionGeneration: 'generation-write',
    registerDatabase() {}
  }
  return { manager, connection }
}

function writeDatabasePayload() {
  return {
    schemaVersion: 2,
    handle: 'database-write',
    databaseId: 'database-id-write',
    databaseGeneration: 'database-generation-write',
    services: [{ uuid: SERVICE_UUID, occurrence: '0', primary: true, includedServices: [] }],
    characteristics: [
      {
        handle: 'characteristic-write',
        serviceUuid: SERVICE_UUID,
        serviceOccurrence: '0',
        characteristicUuid: CHARACTERISTIC_UUID,
        characteristicOccurrence: '0',
        properties: ['write', 'write-without-response']
      }
    ],
    descriptors: []
  }
}

// What Electron main puts on the wire: the provider receipt forwarded
// verbatim (src/electron/main-router.ts), i.e. `commitState: 'unknown'` for a
// successful without-response write.
function electronMainReceipt() {
  return {
    terminal: { correlation: 'write-1', outcome: 'succeeded', cause: null },
    mode: 'without-response',
    commitState: 'unknown',
    bytesSubmitted: 3
  }
}

// What the Tauri dispatcher puts on the wire for the same physical event
// (native/tauri/src/btleplug_dispatcher.rs): the contract word, identical to
// Electron — one vocabulary, not one per host.
function tauriDispatcherReceipt() {
  return {
    terminal: { correlation: 'write-1', outcome: 'succeeded', cause: null },
    mode: 'without-response',
    commitState: 'unknown',
    bytesSubmitted: 3
  }
}

async function writeWithoutResponse(receipt) {
  const { manager, connection } = stubIpcLink(async () => receipt)
  const database = IpcGattDatabase.fromPayload(manager, connection, writeDatabasePayload())
  const snapshot = await database.snapshot()
  const path = snapshot.characteristics[0].path
  return database.write(path, new Uint8Array([1, 2, 3]), { mode: 'without-response' })
}

describe('without-response write receipt (findings F1/F2)', () => {
  test('an Electron without-response success decodes instead of raising protocol.malformed', async () => {
    const receipt = await writeWithoutResponse(electronMainReceipt())
    expect(receipt.mode).toBe('without-response')
    expect(receipt.commitState).toBe('unknown')
    expect(receipt.bytesSubmitted).toBe(3)
    expect(receipt.terminal).toMatchObject({ outcome: 'succeeded', cause: null })
  })

  test('a Tauri without-response success decodes with the same contract word as Electron', async () => {
    const receipt = await writeWithoutResponse(tauriDispatcherReceipt())
    expect(receipt.mode).toBe('without-response')
    expect(receipt.commitState).toBe('unknown')
    expect(receipt.bytesSubmitted).toBe(3)
    expect(receipt.terminal).toMatchObject({ outcome: 'succeeded', cause: null })
  })

  test('a with-response success still requires the confirmed commit state', async () => {
    const { manager, connection } = stubIpcLink(async () => ({
      terminal: { correlation: 'write-1', outcome: 'succeeded', cause: null },
      mode: 'with-response',
      commitState: 'confirmed',
      bytesSubmitted: 2
    }))
    const database = IpcGattDatabase.fromPayload(manager, connection, writeDatabasePayload())
    const snapshot = await database.snapshot()
    const receipt = await database.write(snapshot.characteristics[0].path, new Uint8Array([1, 2]), {
      mode: 'with-response'
    })
    expect(receipt.commitState).toBe('confirmed')
  })

  test('a host-specific accepted commit state is malformed on every host', async () => {
    const failure = await writeWithoutResponse({
      terminal: { correlation: 'write-1', outcome: 'succeeded', cause: null },
      mode: 'without-response',
      commitState: 'accepted',
      bytesSubmitted: 3
    }).then(
      () => null,
      error => error
    )
    expect(failure).not.toBeNull()
    expect(failure.normalized ?? failure).toMatchObject({ code: 'protocol.malformed' })
  })
})
