import { test } from 'node:test'
import assert from 'node:assert/strict'
import { buildH10Continuation } from '../polar-continuation.ts'
import { buildStartAccCommand, buildStartEcgCommand } from '../polar-pmd.ts'

test('native H10 recipe subscribes before declaring sequential STOP/START acknowledgements', () => {
  const declaration = buildH10Continuation({
    peerId: 'known-peer',
    ecg: true,
    acc: { sampleRateHz: 200, resolutionBits: 16, rangeG: 8 }
  })
  assert.equal(declaration.onAppearance, 'native')
  assert.equal(declaration.peerId, 'known-peer')
  assert.equal(declaration.resubscribe.length, 3)
  assert.deepEqual(
    declaration.setup.map(step => [...step.value]),
    [
      [3, 0],
      [...buildStartEcgCommand()],
      [3, 2],
      [...buildStartAccCommand({ sampleRateHz: 200, resolutionBits: 16, rangeG: 8 })]
    ]
  )
  for (const [index, step] of declaration.setup.entries()) {
    assert.equal(step.response.subscriptionIndex, 1)
    assert.deepEqual([...step.response.prefix], [0xf0, step.value[0], step.value[1]])
    assert.equal(step.response.status.offset, 3)
    assert.deepEqual(step.response.status.accepted, index % 2 === 0 ? [0, 6] : [0])
    assert.equal(step.response.minLength, 4)
    assert.equal(step.response.maxLength, 5)
    assert.deepEqual(step.response.trailing, { offset: 4, accepted: [0] })
  }
})

test('native recipe covers all H10 ACC combinations without a second command encoder', () => {
  for (const sampleRateHz of [25, 50, 100, 200]) {
    for (const rangeG of [2, 4, 8]) {
      const acc = { sampleRateHz, resolutionBits: 16, rangeG }
      const declaration = buildH10Continuation({ ecg: false, acc })
      assert.equal(declaration.setup.length, 2)
      assert.deepEqual(declaration.setup[1].value, buildStartAccCommand(acc))
    }
  }
})

test('HR-only recipe does not admit PMD work or retain mutable recipe buffers across calls', () => {
  const hr = buildH10Continuation({ ecg: false })
  assert.equal(hr.resubscribe.length, 1)
  assert.deepEqual(hr.setup, [])
  const first = buildH10Continuation({ ecg: true })
  first.setup[0].value.fill(99)
  first.setup[0].response.prefix.fill(99)
  const next = buildH10Continuation({ ecg: true })
  assert.deepEqual([...next.setup[0].value], [3, 0])
  assert.deepEqual([...next.setup[0].response.prefix], [0xf0, 3, 0])
})
