// __tests__/backends/reactnative/rust-core-bridge-shape.test.js
//
// Finding F2B-3: the react-native rust-core bridge verifies its handles
// against BRIDGE_SHAPES instead of casting them into place. Those tables
// must cover the whole prototype of the internal class they bridge (a new
// member must extend the table, never slip past the check), and every table
// entry must name a real member of that prototype. The production asserts
// run on every creation in the other suites; this suite pins the tables.

const InternalShapes = require('../../../src/manager/ble-manager')
const { BRIDGE_SHAPES, assertInternalScanSession } = require('../../../src/backends/reactnative/react-native-rust-core-manager')

const PAIRS = Object.freeze([
  ['manager', 'BleManager'],
  ['scanSession', 'ScanSession'],
  ['connection', 'Connection'],
  ['discoveredDatabase', 'DiscoveredGattDatabase'],
  ['subscription', 'Subscription']
])

function prototypeMembers(klass) {
  return Object.getOwnPropertyNames(klass.prototype)
    .filter(name => name !== 'constructor')
    .map(name => {
      const descriptor = Object.getOwnPropertyDescriptor(klass.prototype, name)
      return { name, kind: descriptor.get !== undefined ? 'property' : 'method' }
    })
}

describe('bridge shape tables cover their internal prototypes', () => {
  test.each(PAIRS)('%s covers every prototype member of %s', (shape, klassName) => {
    const table = BRIDGE_SHAPES[shape]
    expect(table).toBeDefined()
    const covered = new Set([...table.methods, ...table.properties])
    const missing = prototypeMembers(InternalShapes[klassName]).filter(member => !covered.has(member.name))
    expect(missing).toEqual([])
  })

  test.each(PAIRS)('%s names only real prototype members with matching kinds', (shape, klassName) => {
    const table = BRIDGE_SHAPES[shape]
    const members = new Map(prototypeMembers(InternalShapes[klassName]).map(member => [member.name, member.kind]))
    for (const name of table.methods) {
      expect(members.get(name)).toBe('method')
    }
    for (const name of table.properties) {
      expect(members.get(name)).toBe('property')
    }
  })

  test('the guards reject a misshapen value instead of retyping it', () => {
    expect(() => assertInternalScanSession(null)).toThrow('lifecycle.invariant-violation')
    expect(() => assertInternalScanSession({ stop: () => undefined })).toThrow(
      'lifecycle.invariant-violation'
    )
    expect(() =>
      assertInternalScanSession({
        scanSessionId: 's',
        leaseId: 'l',
        shareToken: null,
        observations: {},
        stop: 'not-a-function'
      })
    ).toThrow('lifecycle.invariant-violation')
  })
})
