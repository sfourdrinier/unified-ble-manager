// __tests__/host-identity-random-source.test.js
// Hosts without WebCrypto (React Native/Hermes) supply entropy explicitly.
const {
  createEphemeralHostIdentity,
  normalizeBleManagerCreateOptions
} = require('../src/public/host-identity')
const { BackendContractError } = require('../src/backend-contract/errors')

function expectCode(run, code) {
  try {
    run()
    throw new Error(`expected ${code}`)
  } catch (error) {
    expect(error).toBeInstanceOf(BackendContractError)
    expect(error.normalized.code).toBe(code)
  }
}

describe('injectable random source', () => {
  test('normalize accepts randomBytes and preserves it', () => {
    const randomBytes = length => new Uint8Array(length).fill(7)
    const normalized = normalizeBleManagerCreateOptions({ randomBytes })
    expect(normalized.randomBytes).toBe(randomBytes)
  })

  test('normalize rejects a non-function randomBytes', () => {
    expectCode(() => normalizeBleManagerCreateOptions({ randomBytes: 'nope' }), 'argument.invalid')
  })

  test('normalize still rejects unknown keys', () => {
    expectCode(() => normalizeBleManagerCreateOptions({ nope: 1 }), 'argument.invalid')
  })

  test('an injected source is used instead of globalThis.crypto', () => {
    let asked = 0
    const identity = createEphemeralHostIdentity({
      randomBytes: length => {
        asked += 1
        return new Uint8Array(length).fill(0xab)
      }
    })
    expect(asked).toBe(3)
    expect(identity.attachmentNonce).toBe('ab'.repeat(16))
    expect(identity.operationNonce).toBe('ab'.repeat(8))
  })

  test('without WebCrypto and without injection the failure names the fix', () => {
    const saved = Object.getOwnPropertyDescriptor(globalThis, 'crypto')
    // Simulate Hermes: no WebCrypto at all.
    Object.defineProperty(globalThis, 'crypto', { value: undefined, configurable: true })
    try {
      let message = ''
      try {
        createEphemeralHostIdentity()
      } catch (error) {
        message = String(error.message ?? '')
      }
      expect(message).toContain('randomBytes')
    } finally {
      if (saved) Object.defineProperty(globalThis, 'crypto', saved)
      else Reflect.deleteProperty(globalThis, 'crypto')
    }
  })
})
