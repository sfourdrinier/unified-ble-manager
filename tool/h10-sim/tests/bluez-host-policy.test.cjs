const assert = require('node:assert/strict')
const test = require('node:test')
const { reverseDiscoveryDisabled } = require('./bluez-host-policy.cjs')

test('dedicated simulator host requires one active false value in General', () => {
  assert.equal(reverseDiscoveryDisabled('[General]\nReverseServiceDiscovery = false\n'), true)
  assert.equal(reverseDiscoveryDisabled('[General]\n#ReverseServiceDiscovery = false\n'), false)
  assert.equal(reverseDiscoveryDisabled('[General]\nReverseServiceDiscovery = true\n'), false)
  assert.equal(reverseDiscoveryDisabled('[Other]\nReverseServiceDiscovery = false\n'), false)
  assert.equal(
    reverseDiscoveryDisabled('[General]\nReverseServiceDiscovery = false\nReverseServiceDiscovery = true\n'),
    false
  )
  assert.equal(
    reverseDiscoveryDisabled('[General]\nReverseServiceDiscovery = false\nReverseServiceDiscovery = false\n'),
    false
  )
})
