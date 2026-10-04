const { parseContinuationStatus } = require('../../../src/backends/reactnative/react-native-continuation-claim')
const androidCleanupWake = require('../../fixtures/android-continuation-cleanup-wake.json')
const status = lastWake => ({ strategy: 'native', peerId: null, resubscribe: 0, malformedDeclarations: 0, lastWake })
const wake = {
  observedAtMs: 1,
  event: 'continuation.completed',
  strategy: 'headless-task',
  peerAddress: 'peer',
  code: null,
  reason: null,
  stage: 'task-dispatched'
}

test('startup diagnostic is nullable for compatible hosts and malformed records fail closed', () => {
  expect(parseContinuationStatus(status(null)).startupFailure).toBeNull()
  expect(parseContinuationStatus({ ...status(null), startupFailure: null }).startupFailure).toBeNull()
  for (const startupFailure of [
    'failure',
    {},
    { code: 'platform.failure', domain: 'platform', operation: 'bootstrap', platform: { code: 550 } }
  ]) {
    expect(() => parseContinuationStatus({ ...status(null), startupFailure })).toThrow()
  }
})

test('wake success explicitly reports task dispatch or foreground service start, never task completion', () => {
  expect(parseContinuationStatus(status(wake)).lastWake.stage).toBe('task-dispatched')
  expect(
    parseContinuationStatus(status({ ...wake, strategy: 'foreground-service', stage: 'foreground-service-started' }))
      .lastWake.stage
  ).toBe('foreground-service-started')
})

test.each([
  { ...wake, stage: 'task-completed' },
  { ...wake, stage: undefined },
  { ...wake, strategy: 'foreground-service' },
  { ...wake, strategy: 'native' },
  { ...wake, event: 'continuation.failed' }
])('mismatched wake phase fails closed', value => {
  expect(() => parseContinuationStatus(status(value))).toThrow()
})

test('failed wake retains structured platform refusal, without a success stage', () => {
  const { stage, ...base } = wake
  const failure = {
    ...base,
    event: 'continuation.failed',
    code: 'platform.failure',
    reason: 'OS refused',
    platform: {
      domain: 'android',
      code: 'android.app.ForegroundServiceStartNotAllowedException',
      message: 'OS refused',
      metadata: {}
    }
  }
  expect(parseContinuationStatus(status(failure)).lastWake.platform.code).toBe(failure.platform.code)
  expect(() => parseContinuationStatus(status({ ...wake, platform: failure.platform }))).toThrow()
})

test('the Android-emitted cleanup-failure fixture crosses the actual strict status decoder', () => {
  // PresenceForegroundContinuationTest pins this same fixture to Kotlin output.
  const parsed = parseContinuationStatus(status(androidCleanupWake))
  expect(parsed.lastWake.platform.metadata).toEqual(androidCleanupWake.platform.metadata)
  expect(parsed.lastWake.platform.metadata.cleanupFailure0Message).toBe('stop refused')
})
