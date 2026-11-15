const { createWebRecordingLiveOwners } = require('../../src/web/web-recording-live-owners')

test('a source and persisted control version mismatch cannot dispatch stop', async () => {
  const owners = createWebRecordingLiveOwners()
  const identity = { recordingId: 'sample', producerIncarnation: 'producer', ownerEpoch: '1' }
  const stopOrdered = jest.fn()
  owners.install(identity, { recordingVersion: 1, supervisor: {}, stopOrdered })
  await expect(owners.stopOrdered(identity, 2)).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
  expect(stopOrdered).not.toHaveBeenCalled()
})

test('a released owner remains installed until its exact finished outcome is durably recoverable', async () => {
  const owners = createWebRecordingLiveOwners()
  const identity = { recordingId: 'sample', producerIncarnation: 'producer', ownerEpoch: '1' }
  const result = {
    recording: { version: 1, state: 'finished', error: null },
    hostCleanup: { state: 'released', failures: [] }
  }
  let retained = false
  const owner = { supervisor: {}, stopResult: async () => result, terminalRetained: async actual => retained && actual === result }
  owners.install(identity, owner)
  await expect(owners.remove(identity, owner)).rejects.toMatchObject({ normalized: { code: 'capability.unavailable' } })
  expect(owners.has(identity)).toBe(true)
  retained = true
  await owners.remove(identity, owner)
  expect(owners.has(identity)).toBe(false)
})
