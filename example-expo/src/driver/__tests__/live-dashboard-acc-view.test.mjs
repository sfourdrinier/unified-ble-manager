import { test } from 'node:test'
import assert from 'node:assert/strict'
import { accTileView } from '../live-dashboard-acc-view.ts'

test('ACC presentation preserves signed milli-g axes and actual acquisition settings', () => {
  const points = [
    { x: -2000, y: 1000, z: 0 },
    { x: 3, y: -4, z: 5 }
  ]
  const view = accTileView({
    accDisplay: points,
    lastAccMilliG: points[1],
    accSamples: 200,
    accSettings: { sampleRateHz: 200, resolutionBits: 16, rangeG: 2 },
    pmdDroppedItems: 3,
    pmdDroppedBytes: 19,
    pmdReplacedItems: 1
  })
  assert.deepEqual(view.points, points)
  assert.deepEqual(view.last, points[1])
  assert.equal(view.rangeG, 2)
  assert.equal(view.sampleRateHz, 200)
  assert.equal(view.samples, 200)
  assert.match(view.loss, /3.*19.*1/)
  assert.equal(view.error, null)
})

test('missing ACC remains visibly inactive; malformed data and backend failures are explicit', () => {
  assert.deepEqual(accTileView({}).points, [])
  assert.equal(accTileView({}).rangeG, null)
  assert.match(accTileView({ accDisplay: [{ x: 0, y: 'bad', z: 1 }] }).error, /malformed/i)
  assert.match(
    accTileView({ accError: { code: 'operation.refused', message: 'ACC refused' } }).error,
    /operation.refused/
  )
})
