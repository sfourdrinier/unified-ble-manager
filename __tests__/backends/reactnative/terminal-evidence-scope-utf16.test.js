const {
  terminalEvidenceScopeForNative,
  terminalEvidenceScopeFromNative
} = require('../../../src/backends/reactnative/terminal-evidence-scope-utf16')

test('React Native terminal evidence preserves exceptional UTF-16 scope bytes and reconstructs them', () => {
  const scope = Object.freeze({ managerId: 'm\ud800x\udc00', operationId: '😀', incarnation: 'i' })
  const native = terminalEvidenceScopeForNative('scan', scope)
  expect(native).toEqual({
    managerId: 'm�x�',
    operationId: '😀',
    incarnation: 'i',
    scopeUtf16leBase64: expect.any(String)
  })
  expect(terminalEvidenceScopeFromNative('scan', native, 'test.recovery')).toEqual(scope)
  expect(() => terminalEvidenceScopeFromNative('subscription', native, 'test.recovery')).toThrow(/protocol\.malformed/)
})

test('React Native terminal evidence leaves scalar legacy scopes byte-identical and sidecar-free', () => {
  const scope = Object.freeze({ managerId: 'manager', operationId: '😀', incarnation: '1' })
  const native = terminalEvidenceScopeForNative('subscription', scope)
  expect(native).toEqual(scope)
  expect(terminalEvidenceScopeFromNative('subscription', native, 'test.recovery')).toEqual(scope)
})
