const { bootstrap } = require('../electron/helpers/public-bootstrap')
const { createElectronRendererBleManager } = require('../../src/electron-renderer')
const { createTauriBleManagerWithEnvironment, TAURI_PLUGIN_COMPATIBILITY } = require('../../src/tauri')
const { IpcBleManager } = require('../../src/ipc/manager')

// Models the authenticated host response; no capability projection, public
// manager, IPC client, connection control or factory behavior is replaced.
function fixture(priorityState, accepted = true) {
  const current = bootstrap()
  current.core = {
    contractRevision: TAURI_PLUGIN_COMPATIBILITY.contractRevision,
    implementationVersion: 'review-fixture'
  }
  const priority = current.capabilities.descriptors.find(entry => entry.id === 'connection:priority')
  const limitations =
    priorityState === 'supported'
      ? []
      : [
          {
            code: 'winrt-preferred-parameters-22000',
            explanation: 'Modeled host supports preferred presets; acceptance is not a measured parameter outcome.',
            affectedGuarantee: 'observed connection parameter values'
          }
        ]
  Object.assign(priority, {
    state: priorityState,
    limitations,
    evidence: {
      ...priority.evidence,
      evidenceLevel:
        priorityState === 'supported' ? 'supported' : priorityState === 'limited' ? 'deterministic' : 'blocked',
      limitations
    }
  })
  const commands = []
  let nativePriorityDispatches = 0
  const invoke = async request => {
    if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: current }
    if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
    if (request.kind === 'event.ack') return { kind: 'event.ack' }
    const { command, payload } = request.envelope
    commands.push(command)
    if (command === 'connection.connect')
      return {
        kind: 'route',
        payload: {
          handle: 'connection-1',
          peerId: payload.peerId,
          connectionId: 'connection-id-1',
          ownerLeaseId: current.rendererLease.leaseId,
          connectionGeneration: 'connection-generation-1'
        }
      }
    if (command === 'connection.events.subscribe')
      return {
        kind: 'route',
        payload: {
          handle: payload.connectionEventsHandle,
          connectionId: payload.connectionId,
          connectionGeneration: payload.connectionGeneration,
          eventSchemaVersion: 2
        }
      }
    if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
    if (command === 'connection.events.unsubscribe' || command === 'connection.disconnect') {
      return { kind: 'route', payload: { state: 'released', failures: [] } }
    }
    if (command === 'connection.request-priority') {
      nativePriorityDispatches++
      return {
        kind: 'route',
        payload: {
          accepted,
          connectionId: 'connection-id-1',
          connectionGeneration: 'connection-generation-1'
        }
      }
    }
    throw new Error(`Unexpected fixture command: ${command}`)
  }
  return {
    current,
    commands,
    invoke,
    get nativePriorityDispatches() {
      return nativePriorityDispatches
    },
    transport: { invoke, subscribe: () => () => undefined, acknowledge: async () => ({ kind: 'event.ack' }) }
  }
}

class Channel {
  onmessage = null
}

for (const host of ['electron', 'tauri']) {
  for (const state of ['limited', 'supported', 'unavailable', 'unsupported']) {
    test(`actual ${host} factory preserves incoming ${state} priority and native request answers`, async () => {
      const h = fixture(state)
      const manager =
        host === 'electron'
          ? await createElectronRendererBleManager({ transport: h.transport })
          : await createTauriBleManagerWithEnvironment({
              invoke: async (_command, args) => h.invoke(args.request),
              Channel
            })
      try {
        expect(h.current.capabilities.descriptors.find(entry => entry.id === 'connection:priority').state).toBe(state)
        expect(manager.capabilities.get('connection:priority')).toMatchObject({
          state,
          limitations: h.current.capabilities.descriptors.find(entry => entry.id === 'connection:priority').limitations
        })
        const connection = await manager.connect('windows-peer')
        for (const priority of ['balanced', 'low-power', 'high-throughput']) {
          if (state === 'supported' || state === 'limited') {
            await expect(connection.controls.requestPriority(priority)).resolves.toMatchObject({
              state: 'accepted',
              requested: priority
            })
          } else {
            await expect(connection.controls.requestPriority(priority)).rejects.toMatchObject({
              code: `capability.${state}`
            })
          }
        }
        const expectedDispatches = state === 'supported' || state === 'limited' ? 3 : 0
        expect(h.commands.filter(command => command === 'connection.request-priority')).toHaveLength(expectedDispatches)
        expect(h.nativePriorityDispatches).toBe(expectedDispatches)
        await connection.disconnect()
      } finally {
        await manager.destroy()
      }
    })
  }
}

test('CONTROL: the real lower IPC connection can reach the same accepted priority host route', async () => {
  const h = fixture('limited')
  const ipc = await IpcBleManager.create(h.transport)
  try {
    const connection = await ipc.connect('windows-peer')
    expect(await connection.requestPriority('balanced')).toBe(true)
    expect(h.commands.filter(command => command === 'connection.request-priority')).toHaveLength(1)
    expect(h.nativePriorityDispatches).toBe(1)
    await connection.disconnect()
  } finally {
    await ipc.destroy()
  }
})

for (const host of ['electron', 'tauri']) {
  test(`${host} factory preserves rejected native priority request`, async () => {
    const h = fixture('limited', false)
    const manager =
      host === 'electron'
        ? await createElectronRendererBleManager({ transport: h.transport })
        : await createTauriBleManagerWithEnvironment({
            invoke: async (_command, args) => h.invoke(args.request),
            Channel
          })
    try {
      const connection = await manager.connect('windows-peer')
      await expect(connection.controls.requestPriority('balanced')).resolves.toMatchObject({
        state: 'rejected',
        requested: 'balanced'
      })
      expect(h.nativePriorityDispatches).toBe(1)
      await connection.disconnect()
    } finally {
      await manager.destroy()
    }
  })
}
