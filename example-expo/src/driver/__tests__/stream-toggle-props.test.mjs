import { test } from 'node:test'
import assert from 'node:assert/strict'
import { streamToggleProps } from '../stream-toggle-props.ts'

test('remote and touch activation enable ACC and disable ECG with explicit checked accessibility state', () => {
  const changes = []
  const acc = streamToggleProps('ACC', false, value => changes.push(value))
  assert.equal(acc.accessibilityRole, 'switch')
  assert.equal(acc.accessibilityLabel, 'ACC on next start')
  assert.deepEqual(acc.accessibilityState, { checked: false })
  assert.equal(acc.focusable, true)
  acc.onPress()
  const ecg = streamToggleProps('ECG', true, value => changes.push(value))
  assert.deepEqual(ecg.accessibilityState, { checked: true })
  ecg.onPress()
  assert.deepEqual(changes, [true, false])
})
