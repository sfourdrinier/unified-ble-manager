const {
  encodeTerminalEvidence,
  decodeTerminalEvidence,
  decodeTerminalEvidenceError,
  hydrateTerminalEvidence
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
  expect(JSON.parse(new TextDecoder().decode(bytes))).not.toHaveProperty('encodingVersion')
  expect(new TextDecoder().decode(bytes)).toBe(
    '{"formatVersion":1,"scope":{"incarnation":"i","managerId":"m","operationId":"o"},"terminal":{"droppedBytes":0,"droppedItems":0,"error":{"code":"x","domain":"scan","operation":"scan","platform":{"code":"x","domain":"native","metadata":{"list":[1,{"ok":true}],"raw":{"$bytes":"00ff"}},"safeMessage":"no"},"retryability":"never"},"kind":"terminal","reason":"source-failed","replacedItems":0}}'
  )
  expect(decodeTerminalEvidence(bytes)).toMatchObject({
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'o', incarnation: 'i' }
  })
  expect(decodeTerminalEvidence(bytes).terminal.error.platform.metadata.raw).toEqual(new Uint8Array([0, 255]))
  expect(encodeTerminalEvidence(terminal, { incarnation: 'i', operationId: 'o', managerId: 'm' })).toEqual(bytes)
})
test('canonical terminal evidence preserves ordinary singleton $bytes records without changing byte values', () => {
  const terminal = {
    kind: 'terminal',
    reason: 'source-failed',
    droppedItems: 0,
    droppedBytes: 0,
    replacedItems: 0,
    error: {
      code: 'platform.failure',
      domain: 'platform',
      operation: 'scan.start',
      retryability: 'never',
      platform: {
        domain: 'native',
        code: 'E_NATIVE',
        safeMessage: 'failure',
        metadata: {
          validLiteral: { $bytes: '00ff' },
          nonHexLiteral: { $bytes: 'not-hex' },
          nested: {
            values: [{ $bytes: 'beef' }, new Uint8Array([1, 2]), { child: { $bytes: '' } }]
          }
        }
      }
    }
  }
  const bytes = encodeTerminalEvidence(terminal, { managerId: 'm', operationId: 'o', incarnation: 'i' })
  expect(JSON.parse(new TextDecoder().decode(bytes)).encodingVersion).toBe(2)
  expect(decodeTerminalEvidence(bytes).terminal.error.platform.metadata).toEqual(terminal.error.platform.metadata)
})
test('unmarked v1 terminal evidence continues to revive legacy byte wrappers', () => {
  const legacy = new TextEncoder().encode(
    '{"formatVersion":1,"scope":{"incarnation":"i","managerId":"m","operationId":"o"},"terminal":{"error":{"platform":{"metadata":{"bytes":{"$bytes":"00ff"},"ordinary":{"$bytes":"00ff","keep":true}}}},"kind":"terminal"}}'
  )
  const metadata = decodeTerminalEvidence(legacy).terminal.error.platform.metadata
  expect(metadata.bytes).toEqual(new Uint8Array([0, 255]))
  expect(metadata.ordinary).toEqual({ $bytes: '00ff', keep: true })
})
test('rejects an unknown terminal evidence encoding version', () => {
  const unsupported = new TextEncoder().encode(
    '{"encodingVersion":3,"formatVersion":1,"scope":{"incarnation":"i","managerId":"m","operationId":"o"},"terminal":{"kind":"terminal"}}'
  )
  expect(() => decodeTerminalEvidence(unsupported)).toThrow('terminal-evidence.decode')
})
test('rejects an invalid marked singleton $bytes representation', () => {
  const invalid = new TextEncoder().encode(
    '{"encodingVersion":2,"formatVersion":1,"scope":{"incarnation":"i","managerId":"m","operationId":"o"},"terminal":{"error":{"platform":{"metadata":{"literal":{"$bytes":["first","second"]}}}},"kind":"terminal"}}'
  )
  expect(() => decodeTerminalEvidence(invalid)).toThrow('terminal-evidence.bytes')
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

test.each(['uncertain', 'not-dispatched', null])(
  'hydrates terminal error commit %s and ordered cleanup failure exactly',
  async commit => {
    const scope = { managerId: 'manager', operationId: 'operation', incarnation: 'incarnation' }
    const orderedFailure = {
      code: 'platform.failure',
      domain: 'gatt',
      operation: 'terminal.cleanup',
      detail: 'primary cleanup failed',
      platformDomain: 'native',
      platformCode: 'E_PRIMARY',
      platformMessage: null,
      platformMetadata: {},
      platformCleanup: [
        {
          domain: 'cleanup',
          code: 'E_CLEANUP',
          message: 'secondary cleanup failed',
          metadata: {},
          cleanup: [{ domain: 'nested', code: 'E_NESTED', message: null, metadata: {} }]
        }
      ],
      platformCleanupFailures: [
        {
          code: 'platform.failure',
          domain: 'gatt',
          operation: 'terminal.cleanup.fence',
          detail: null,
          platformDomain: null,
          platformCode: null,
          platformMessage: null,
          platformMetadata: {}
        }
      ]
    }
    const error = {
      code: orderedFailure.code,
      domain: orderedFailure.domain,
      operation: orderedFailure.operation,
      retryability: 'never',
      commit,
      orderedFailure,
      platform: null
    }
    const bytes = encodeTerminalEvidence(
      { kind: 'terminal', reason: 'source-failed', droppedItems: 0, droppedBytes: 0, replacedItems: 0, error },
      scope
    )
    const notice = {
      kind: 'terminal',
      reason: 'source-failed',
      droppedItems: 0,
      droppedBytes: 0,
      replacedItems: 0,
      evidence: {
        producer: 'scan',
        ref: 'terminal-ref',
        byteLength: bytes.byteLength,
        digest: 'sha256:terminal',
        formatVersion: 1,
        durability: 'committed',
        scope
      }
    }

    await expect(
      hydrateTerminalEvidence(
        { readTerminalEvidence: async () => ({ kind: 'committed', canonicalBytes: bytes, digest: 'sha256:terminal' }) },
        notice,
        bytes.byteLength
      )
    ).resolves.toMatchObject({ error })
  }
)

test.each([
  { commit: 'maybe' },
  { commit: {} },
  { commit: undefined },
  { orderedFailure: {} },
  { orderedFailure: undefined },
  {
    orderedFailure: {
      code: 'platform.failure',
      domain: 'gatt',
      operation: 'different-operation',
      detail: null,
      platformDomain: null,
      platformCode: null,
      platformMessage: null,
      platformMetadata: {}
    }
  }
])('rejects malformed optional terminal error field %#', optional => {
  expect(() =>
    decodeTerminalEvidenceError({
      code: 'platform.failure',
      domain: 'gatt',
      operation: 'terminal.cleanup',
      retryability: 'never',
      platform: null,
      ...optional
    })
  ).toThrow('terminal-evidence.error')
})
