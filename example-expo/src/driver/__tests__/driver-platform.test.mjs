// example-expo/src/driver/__tests__/driver-platform.test.mjs
//
// The Expo adapter reports Apple TV as platform `tvos` (react-native-tvos
// keeps Platform.OS === 'ios' and signals TV through Platform.isTV), so the
// TV host gets a distinct host id on the control server. Phones are unchanged.

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { resolveExpoDriverPlatform } from '../expo-driver-platform.ts'

test('reports tvos on Apple TV and leaves phone platforms unchanged', () => {
  assert.equal(resolveExpoDriverPlatform('ios', true), 'tvos')
  assert.equal(resolveExpoDriverPlatform('ios', false), 'ios')
  assert.equal(resolveExpoDriverPlatform('android', false), 'android')
  assert.equal(resolveExpoDriverPlatform('android', true), 'android')
})
