// __tests__/backends/reactnative/background-continuation-claim-dispose.test.js
//
// FXN: a dispose that reports failures must reach the app (the session is
// kept for retry, never cleared), and a status answer must carry the
// deferred-strategy disclaimer instead of implying execution.

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 34 },
  TurboModuleRegistry: { get: () => null },
  NativeModules: {}
}))

const {
  aggregateContinuationClaim,
  parseContinuationStatus
} = require('../../../src/backends/reactnative/react-native-continuation-claim')

const NATIVE_DECLARATION = Object.freeze({
  onAppearance: 'native',
  resubscribe: Object.freeze([
    { serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb', characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb' }
  ])
})

describe('continuation dispose failure', () => {
  it('a release-failed dispose reaches the app with the session kept', () => {
    const backlog = aggregateContinuationClaim(
      {
        consumerCount: 1,
        selectors: [
          {
            serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
            serviceOccurrence: 1,
            characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
            characteristicOccurrence: 1
          }
        ],
        batches: [],
        disposed: false,
        afterCutoffLoss: { items: 0, bytes: 0 },
        disposeFailure: 'session.dispose reported release-failed; the session is kept for a retry'
      },
      NATIVE_DECLARATION
    )
    expect(backlog.disposed).toBe(false)
    expect(backlog.disposeFailure).toMatch(/release-failed/)
  })

  it('a clean dispose carries no failure', () => {
    const backlog = aggregateContinuationClaim(
      {
        consumerCount: 1,
        selectors: [
          {
            serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
            serviceOccurrence: 1,
            characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
            characteristicOccurrence: 1
          }
        ],
        batches: [],
        disposed: true,
        afterCutoffLoss: { items: 0, bytes: 0 }
      },
      NATIVE_DECLARATION
    )
    expect(backlog.disposed).toBe(true)
    expect(backlog.disposeFailure).toBe(null)
  })

  it('refuses negative session and status counts', () => {
    expect(() =>
      aggregateContinuationClaim({
        consumerCount: -1,
        selectors: [],
        batches: [],
        disposed: false,
        afterCutoffLoss: { items: 0, bytes: 0 }
      })
    ).toThrow()
    expect(() =>
      parseContinuationStatus({
        strategy: 'native',
        peerId: null,
        resubscribe: -1,
        malformedDeclarations: 0,
        lastWake: null,
        detail: null
      })
    ).toThrow()
  })
})

describe('continuation status disclaimer', () => {
  const posture = { strategy: 'native', peerId: 'peer', resubscribe: 1, malformedDeclarations: 0, lastWake: null }

  it('exposes recovery failure separately from the original wake with its authoritative retryability', () => {
    const lastRecovery = {
      event: 'continuation.failed',
      strategy: 'native',
      attempt: 2,
      retryability: 'never',
      error: {
        code: 'permission.denied',
        domain: 'platform',
        operation: 'connection.connect',
        detail: 'revoked',
        platform: null
      }
    }
    const status = parseContinuationStatus({ ...posture, lastRecovery })
    expect(status.lastRecovery).toEqual(lastRecovery)
    expect(status.lastWake).toBeNull()
  })

  it('exposes successful recovery and rejects malformed or contradictory outcomes', () => {
    const lastRecovery = {
      event: 'continuation.completed',
      strategy: 'native',
      attempt: 3,
      peerAddress: 'peer',
      resubscribed: 1
    }
    expect(parseContinuationStatus({ ...posture, lastRecovery }).lastRecovery).toEqual(lastRecovery)
    expect(parseContinuationStatus(posture).lastRecovery).toBeNull()
    for (const invalid of [
      { ...lastRecovery, attempt: 0 },
      { ...lastRecovery, resubscribed: -1 },
      { ...lastRecovery, error: {} }
    ]) {
      expect(() => parseContinuationStatus({ ...posture, lastRecovery: invalid })).toThrow()
    }
  })

  it('a status answer carries the deferred-strategy disclaimer', () => {
    const status = parseContinuationStatus(
      JSON.stringify({
        strategy: 'native',
        peerId: 'A0:9E:1A:E9:B9:3D',
        resubscribe: 1,
        malformedDeclarations: 0,
        lastWake: null,
        detail: 'native continuation is not implemented in this release'
      })
    )
    expect(status.peerId).toBe('A0:9E:1A:E9:B9:3D')
    expect(status.detail).toMatch(/not implemented in this release/)
  })

  it('a status without the disclaimer parses with null detail', () => {
    const status = parseContinuationStatus(
      JSON.stringify({
        strategy: 'record-only',
        peerId: null,
        resubscribe: 0,
        malformedDeclarations: 0,
        lastWake: null
      })
    )
    expect(status.detail).toBe(null)
  })
})
