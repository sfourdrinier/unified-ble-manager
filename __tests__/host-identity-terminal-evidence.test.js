const { normalizeBleManagerCreateOptions } = require('../src/public/host-identity')

test('preserves separate terminal and durable aggregate maxima for explicit host reservation', () => {
  expect(normalizeBleManagerCreateOptions({ terminalEvidence: { maximumCanonicalBytes: 512, maximumDurableBytes: 2048 } })).toEqual({ terminalEvidence: { maximumCanonicalBytes: 512, maximumDurableBytes: 2048 } })
})

test('rejects terminal-evidence maxima that cannot bound a durable reservation', () => {
  expect(() => normalizeBleManagerCreateOptions({ terminalEvidence: { maximumCanonicalBytes: 0 } })).toThrow('options.terminal-evidence.maximum-canonical-bytes')
  expect(() => normalizeBleManagerCreateOptions({ terminalEvidence: { maximumCanonicalBytes: 512, maximumDurableBytes: 511 } })).toThrow('options.terminal-evidence.maximum-durable-bytes')
})
