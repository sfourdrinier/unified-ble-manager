const { parseContinuationLinkOutcome } = require('../../../src/core/native-continuation-link')
const { parseContinuationRecoveryStatus } = require('../../../src/backends/reactnative/react-native-continuation-claim')

const unsupported = {
  code: 'capability.unsupported',
  domain: 'capability',
  operation: 'connection.request-mtu',
  detail: 'OS owns negotiation'
}

test('continuation link results report actual MTU or explicit unsupported, never an inferred minimum', () => {
  expect(parseContinuationLinkOutcome(undefined)).toBeUndefined()
  expect(parseContinuationLinkOutcome({ mtu: { requested: 512, outcome: 'negotiated', mtu: 247 } })).toEqual({
    mtu: { requested: 512, outcome: 'negotiated', mtu: 247 }
  })
  const outcome = parseContinuationLinkOutcome({ mtu: { requested: 512, outcome: 'unsupported', error: unsupported } })
  expect(outcome.mtu.error.code).toBe('capability.unsupported')
  expect(outcome.mtu.mtu).toBeUndefined()
  const status = parseContinuationRecoveryStatus({
    event: 'continuation.completed',
    strategy: 'native',
    attempt: 1,
    peerAddress: 'peer',
    resubscribed: 2,
    link: {
      mtu: {
        requested: 512,
        outcome: 'unsupported',
        error: unsupported
      }
    }
  })
  expect(status.link).toEqual(outcome)
})

test.each([
  null,
  {},
  { mtu: { requested: 512, outcome: 'negotiated', mtu: 518 } },
  { mtu: { requested: 512, outcome: 'negotiated', mtu: 22 } },
  { mtu: { requested: 512, outcome: 'negotiated', mtu: 247.5 } },
  { mtu: { requested: 512, outcome: 'unsupported', error: { ...unsupported, code: 'platform.failure' } } },
  { mtu: { requested: 512, outcome: 'unsupported', error: unsupported, mtu: 512 } },
  { mtu: { requested: 512, outcome: 'negotiated', mtu: 247, ignored: true } }
])('malformed continuation link result fails closed (%p)', value => {
  expect(() => parseContinuationLinkOutcome(value)).toThrow()
})
