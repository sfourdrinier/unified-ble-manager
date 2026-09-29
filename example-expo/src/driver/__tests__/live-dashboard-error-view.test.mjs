import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { tileFailureView } from '../live-dashboard-error-view.ts'

test('dashboard tile renders precise structured failure and clears absent errors', () => {
  const failure = { code: 'pmd.control-point-timeout', message: 'No matching PMD response', detail: null }
  assert.deepEqual(JSON.parse(tileFailureView({ error: failure })), failure)
  assert.equal(tileFailureView({ error: null }), null)
  assert.equal(tileFailureView({}), null)
  const screen = readFileSync(new URL('../../screens/MainStack/LiveDashboardScreen/LiveDashboardScreen.tsx', import.meta.url), 'utf8')
  assert.match(screen, /error: tileFailureView\(record\)/)
  assert.match(screen, /\{tile\.error\}/)
})
