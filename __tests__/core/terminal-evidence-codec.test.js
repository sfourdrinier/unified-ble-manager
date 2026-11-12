const {
  encodeTerminalEvidence,
  decodeTerminalEvidence,
  decodeTerminalEvidenceError
} = require('../../src/core/terminal-evidence-codec')
test('canonical terminal evidence roundtrips nested metadata and bytes', () => {
  const terminal = {
    kind: 'terminal',
    reason: 'source-failed',
    droppedItems: 0,
    droppedBytes: 0,
    replacedItems: 0,
    error: {
      code: 'x',
      domain: 'scan',
      operation: 'scan',
      retryability: 'never',
      platform: {
        domain: 'native',
        code: 'x',
        safeMessage: 'no',
        metadata: { raw: new Uint8Array([0, 255]), list: [1, { ok: true }] }
      }
    }
  }
  const bytes = encodeTerminalEvidence(terminal, { managerId: 'm', operationId: 'o', incarnation: 'i' })
  expect(decodeTerminalEvidence(bytes)).toMatchObject({
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'o', incarnation: 'i' }
  })
  expect(decodeTerminalEvidence(bytes).terminal.error.platform.metadata.raw).toEqual(new Uint8Array([0, 255]))
  expect(encodeTerminalEvidence(terminal, { incarnation: 'i', operationId: 'o', managerId: 'm' })).toEqual(bytes)
})
test('canonical recovery bytes retain lone-surrogate scope identity without a recovery-list sidecar', () => {
  const scope = { managerId: 'manager\ud800', operationId: 'operation\udc00', incarnation: 'incarnation' }
  const bytes = encodeTerminalEvidence(
    { kind: 'terminal', reason: 'closed', droppedItems: 0, droppedBytes: 0, replacedItems: 0 },
    scope
  )
  expect(decodeTerminalEvidence(bytes).scope).toEqual(scope)
})
test('strictly rejects forged terminal error vocabulary', () => {
  expect(() =>
    decodeTerminalEvidenceError({
      code: 'forged',
      domain: 'scan',
      operation: 'scan',
      retryability: 'never',
      platform: null
    })
  ).toThrow('terminal-evidence.error')
  expect(
    decodeTerminalEvidenceError({
      code: 'platform.failure',
      domain: 'scan',
      operation: 'scan',
      retryability: 'never',
      platform: null
    })
  ).toMatchObject({ code: 'platform.failure' })
})
