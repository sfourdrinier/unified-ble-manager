import { test } from 'node:test'
import assert from 'node:assert/strict'
import { ScanDetailsScenario } from '../scenarios/scan-details.ts'
import { adapterHostManager } from '../host.ts'
import { createFakeHost, createFakeManager, stream } from './fake-host.mjs'

for (const [durationMs, count] of [[3000, 1], [10000, 1], [500.75, 1], [3000, 0]]) {
  test(`scan-details forwards its finite ${durationMs}ms lifetime and reports ${count} observations on native end without JS timers`, async () => {
    const { manager, calls } = createFakeManager()
    const observations = stream()
    const state = stream()
    let admitted
    const started = new Promise(resolve => { admitted = resolve })
    manager.scan = async options => {
      admitted(options)
      return {
        state,
        observations,
        async stop() {
          calls.push('scan.stop')
          state.end()
          observations.end()
          return { state: 'released', failures: [] }
        }
      }
    }
    const host = createFakeHost({ manager, adapterHostManager })
    const scenario = new ScanDetailsScenario(host)
    const running = scenario.dispatch('scan', { durationMs })
    const options = await started
    assert.equal(options.timeoutMs, Math.floor(durationMs))
    if (count > 0) observations.push({ kind: 'value', value: {
      peer: { id: 'peer-h10', name: 'Polar H10 1234', reference: null, sources: ['test'] },
      localName: 'Polar H10 1234', rssi: -60, connectable: true,
      serviceUuids: [], manufacturerData: [], serviceData: null
    } })
    observations.push({ kind: 'terminal', reason: 'operation-timed-out', droppedItems: 0, droppedBytes: 0, replacedItems: 0 })
    observations.end()
    state.end()
    const summary = await running
    assert.equal(host.runtime.now(), 0, 'the JavaScript duration timer did not run')
    assert.equal(summary.observations, count)
    assert.equal(summary.uniquePeers, count)
    assert.equal(summary.peers.length, count)
    assert.ok(scenario.recentEvents().some(event => event.kind === 'stream-terminal' && event.data.reason === 'operation-timed-out'))
    assert.ok(calls.includes('scan.stop'))
    assert.ok(calls.includes('manager.destroy'))
  })
}
