import { test } from 'node:test'
import assert from 'node:assert/strict'
import { recordingControls } from '../recording-controls.ts'

test('capacity-reached captures remain stoppable, cannot be cleared until stopped, and remain exportable', () => {
  assert.deepEqual(recordingControls('capacity-reached', true, false), {
    record: false,
    stop: true,
    clear: false,
    export: true
  })
  assert.deepEqual(recordingControls('capacity-reached', false, false), {
    record: false,
    stop: false,
    clear: true,
    export: true
  })
  assert.deepEqual(recordingControls('stopped', false, false), {
    record: false,
    stop: false,
    clear: true,
    export: true
  })
})

test('capture and export button admission follows ownership, including an in-flight export', () => {
  assert.deepEqual(recordingControls('empty', false, false), { record: true, stop: false, clear: false, export: false })
  assert.deepEqual(recordingControls('recording', true, false), {
    record: false,
    stop: true,
    clear: false,
    export: false
  })
  assert.deepEqual(recordingControls('stopped', false, true), {
    record: false,
    stop: false,
    clear: false,
    export: false
  })
  assert.deepEqual(recordingControls('unknown', false, false), {
    record: false,
    stop: false,
    clear: false,
    export: false
  })
})
