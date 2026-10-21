// example-expo/src/driver/__tests__/app-state.test.mjs

import assert from 'node:assert/strict'
import test from 'node:test'
import { readAppState } from '../app-state.ts'

test('readAppState preserves known React Native states and marks only active foreground', () => {
  for (const state of ['active', 'background', 'inactive']) {
    assert.deepEqual(readAppState(state), { state, foreground: state === 'active' })
  }
})

test('readAppState reports unavailable current state as unknown and backgrounded', () => {
  assert.deepEqual(readAppState(null), { state: 'unknown', foreground: false })
  assert.deepEqual(readAppState(undefined), { state: 'unknown', foreground: false })
})
