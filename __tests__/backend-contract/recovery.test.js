const { BLE_ERROR_CODES } = require('../../src/backend-contract/errors')
const { recoveryForCode } = require('../../src/backend-contract/recovery')

describe('backend recovery catalog', () => {
  test('provides deterministic recovery metadata for representative contract codes', () => {
    expect(recoveryForCode('connection.failed', 'recovery.test')).toEqual({
      disposition: 'retry-with-backoff',
      actions: [{ kind: 'reconnect' }]
    })
    expect(recoveryForCode('capability.limited', 'recovery.test')).toEqual({
      disposition: 'none',
      actions: []
    })
    expect(recoveryForCode('permission.denied', 'recovery.test')).toEqual({
      disposition: 'after-user-action',
      actions: [
        { kind: 'request-permission', permission: 'recovery.test' },
        { kind: 'open-settings', target: 'app' }
      ]
    })
  })

  test('has a recovery entry for every canonical error code', () => {
    expect(BLE_ERROR_CODES).toHaveLength(67)
    for (const code of BLE_ERROR_CODES) {
      const recovery = recoveryForCode(code, 'recovery.test')
      expect(recovery).toHaveProperty('disposition')
      expect(Array.isArray(recovery.actions)).toBe(true)
    }
  })
})

// PR210-35: recovery advice follows the operation's own answer about
// retryability. An aborted or timed-out operation that was dispatched and may
// have committed (a write) is `never` retryable; telling the caller to retry it
// would invite a second commit, so the advice is to verify state instead.
describe('recovery follows the reported retryability', () => {
  const { recoveryForError } = require('../../src/backend-contract/recovery')

  test.each(['operation.aborted', 'operation.timed-out'])(
    '%s that is never retryable advises verifying state, never retrying',
    code => {
      const recovery = recoveryForError({ code, operation: 'core.gatt.write', retryability: 'never' })
      expect(recovery).toEqual({ disposition: 'caller-policy', actions: [{ kind: 'verify-state' }] })
      expect(recovery.actions.some(action => action.kind === 'retry')).toBe(false)
    }
  )

  test.each(['operation.aborted', 'operation.timed-out'])('%s left to the caller keeps the retry advice', code => {
    expect(recoveryForError({ code, operation: 'core.gatt.read', retryability: 'caller-decides' })).toEqual({
      disposition: 'caller-policy',
      actions: [{ kind: 'retry', afterMs: null }]
    })
  })

  test('every other code keeps its catalog advice for either retryability', () => {
    for (const code of BLE_ERROR_CODES) {
      if (code === 'operation.aborted' || code === 'operation.timed-out') continue
      for (const retryability of ['never', 'caller-decides']) {
        expect(recoveryForError({ code, operation: 'recovery.test', retryability })).toEqual(
          recoveryForCode(code, 'recovery.test')
        )
      }
    }
  })

  test('recoveryForCode keeps its code-derived advice', () => {
    expect(recoveryForCode('operation.timed-out', 'recovery.test')).toEqual({
      disposition: 'caller-policy',
      actions: [{ kind: 'retry', afterMs: null }]
    })
  })

  // F8: a stream overflow commits nothing at the peripheral — repeating the
  // scan or subscription is the caller's policy — so its retryability agrees
  // with its catalog advice (retry with backoff), instead of claiming `never`
  // while advising a retry.
  test('stream.overflow retryability and recovery agree', () => {
    const { retryabilityForCode } = require('../../src/backend-contract/errors')
    const { recoveryForError } = require('../../src/backend-contract/recovery')
    expect(retryabilityForCode('stream.overflow')).toBe('caller-decides')
    expect(recoveryForCode('stream.overflow', 'recovery.test')).toEqual({
      disposition: 'retry-with-backoff',
      actions: [{ kind: 'retry', afterMs: null }]
    })
    expect(
      recoveryForError({ code: 'stream.overflow', operation: 'recovery.test', retryability: 'caller-decides' })
    ).toEqual(recoveryForCode('stream.overflow', 'recovery.test'))
  })
})

// PR210-42: the operation's owner reports `commit`. An `uncertain` commit
// (a dispatched write that may have reached the peer) is never replayed,
// whatever the code; `not-dispatched` keeps the catalog advice.
describe('recovery follows the reported commit state', () => {
  const { recoveryForError } = require('../../src/backend-contract/recovery')

  test('an uncertain commit advises verify-state and never retry for every code', () => {
    for (const code of BLE_ERROR_CODES) {
      for (const retryability of ['never', 'caller-decides']) {
        const recovery = recoveryForError({ code, operation: 'core.gatt.write', retryability, commit: 'uncertain' })
        expect(recovery.disposition).toBe('caller-policy')
        expect(recovery.actions.at(-1)).toEqual({ kind: 'verify-state' })
        expect(recovery.actions.some(action => action.kind === 'retry')).toBe(false)
      }
    }
  })

  test('an uncertain commit keeps the prerequisite actions needed to verify', () => {
    expect(
      recoveryForError({
        code: 'operation.disconnected',
        operation: 'core.gatt.write',
        retryability: 'never',
        commit: 'uncertain'
      })
    ).toEqual({ disposition: 'caller-policy', actions: [{ kind: 'reconnect' }, { kind: 'verify-state' }] })
    expect(
      recoveryForError({
        code: 'operation.timed-out',
        operation: 'core.gatt.write',
        retryability: 'never',
        commit: 'uncertain'
      })
    ).toEqual({ disposition: 'caller-policy', actions: [{ kind: 'verify-state' }] })
  })

  test('not-dispatched keeps the catalog advice', () => {
    const { retryabilityForCode } = require('../../src/backend-contract/errors')
    for (const code of BLE_ERROR_CODES) {
      expect(
        recoveryForError({
          code,
          operation: 'recovery.test',
          retryability: retryabilityForCode(code),
          commit: 'not-dispatched'
        })
      ).toEqual(recoveryForCode(code, 'recovery.test'))
    }
    expect(
      recoveryForError({
        code: 'operation.timed-out',
        operation: 'recovery.test',
        retryability: 'never',
        commit: 'not-dispatched'
      })
    ).toEqual(recoveryForCode('operation.timed-out', 'recovery.test'))
  })

  test('a null or absent commit falls back to the retryability rule', () => {
    for (const commit of [null, undefined]) {
      expect(
        recoveryForError({ code: 'operation.aborted', operation: 'core.gatt.write', retryability: 'never', commit })
      ).toEqual({ disposition: 'caller-policy', actions: [{ kind: 'verify-state' }] })
      expect(
        recoveryForError({
          code: 'operation.disconnected',
          operation: 'core.gatt.write',
          retryability: 'never',
          commit
        })
      ).toEqual(recoveryForCode('operation.disconnected', 'core.gatt.write'))
    }
  })
})
