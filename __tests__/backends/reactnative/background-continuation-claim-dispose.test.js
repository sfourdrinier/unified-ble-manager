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
        batches: [],
        disposed: false,
        disposeFailure: 'session.dispose reported release-failed; the session is kept for a retry'
      },
      NATIVE_DECLARATION
    )
    expect(backlog.disposed).toBe(false)
    expect(backlog.disposeFailure).toMatch(/release-failed/)
  })

  it('a clean dispose carries no failure', () => {
    const backlog = aggregateContinuationClaim({ batches: [], disposed: true }, NATIVE_DECLARATION)
    expect(backlog.disposed).toBe(true)
    expect(backlog.disposeFailure).toBe(null)
  })
})

describe('continuation status disclaimer', () => {
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
