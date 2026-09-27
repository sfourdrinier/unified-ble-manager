const { createNativeContinuationControl } = require('../src/backends/desktop/native-continuation-controller')

const context = { hostDomain: 'ubm-mobile', scope: 'react-native-native' }
const ok = value => JSON.stringify({ ok: true, value })

test('neutral control preserves mobile transport context without changing desktop defaults', async () => {
  for (const [options, domain, scope] of [
    [context, 'ubm-mobile', 'react-native-native'],
    [undefined, 'ubm-desktop', 'desktop-native']
  ]) {
    const control = createNativeContinuationControl(
      {
        describeBacklog: async () => {
          throw new Error('bridge refused')
        }
      },
      options
    )
    await expect(control.status()).rejects.toMatchObject({
      code: 'platform.failure',
      operation: `${scope}.continuation.status`,
      platform: { domain, code: 'native-bridge', safeMessage: 'bridge refused' }
    })
    await expect(createNativeContinuationControl({}, options).status()).rejects.toMatchObject({
      code: 'capability.unsupported',
      operation: `${scope}.continuation.native-owner`
    })
  }
})

test('mobile status fallback cause and malformed status use the selected context', async () => {
  const access = {
    describeBacklog: async () =>
      ok({
        queuedData: 0,
        continuationOutcome: null,
        lastError: {
          code: 'connection.failed',
          domain: 'connection',
          operation: 'connection.connect',
          detail: 'refused'
        }
      })
  }
  const control = createNativeContinuationControl(access, context)
  expect((await control.status()).lastError.platform.domain).toBe('ubm-mobile')
  access.describeBacklog = async () => ok({ queuedData: -1, lastError: null, continuationOutcome: null })
  await expect(control.status()).rejects.toMatchObject({
    code: 'protocol.malformed',
    operation: 'react-native-native.continuation.status.queued-data'
  })
})

test('backend SDK exposes the same canonical transport-neutral factory', () => {
  expect(require('../src/backend-sdk').createNativeContinuationControl).toBe(createNativeContinuationControl)
})
