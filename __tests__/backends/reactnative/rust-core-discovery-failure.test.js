// __tests__/backends/reactnative/rust-core-discovery-failure.test.js
//
// Finding 95: the mobile owner registers the whole database or refuses the
// discovery with a typed failure. The wire answer carries no `skipped` list,
// and the refusal reaches the manager unchanged, with no partial database.
// The answers cross the production binding and serializer from the owner
// double, which speaks `ubm-mobile-wire/1` as the Rust owner does.

const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

async function connectedManager(platform) {
  const harness = rustCoreHarness({ platform })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const peerId = manager.attachedBackend.backend.peerIdForNativeId(DEFAULT_PEER)
  const connection = await manager.connect(peerId, NO_OPTIONS)
  return { native: harness.native, manager, connection }
}

describe.each(['android', 'apple'])('%s discovery refusals reach the manager whole and typed', platform => {
  test.each([
    ['a malformed platform UUID', 'protocol.malformed', 'gatt', 'discovery.snapshot.uuid'],
    ['a database past the ATT handle space', 'capability.limited', 'gatt', 'discovery.database-bound'],
    ['a discovery without a core lease', 'argument.invalid', 'core', 'path.owner']
  ])('the owner refusing %s is %s', async (_case, code, domain, operation) => {
    const { native, manager, connection } = await connectedManager(platform)
    try {
      native.failNext('gatt.discover', code, domain, operation)
      const failure = await connection.discover(NO_OPTIONS).then(
        () => null,
        error => error
      )
      expect(failure?.normalized).toMatchObject({ code, domain, operation })
      // No partial database became current: the next discovery is whole.
      const database = await connection.discover(NO_OPTIONS)
      const snapshot = await database.snapshot()
      expect(snapshot.services.length).toBeGreaterThan(0)
    } finally {
      await manager.destroy()
    }
  })

  test('a discovery answer that still carries `skipped` is refused as malformed, never read', async () => {
    const { native, manager, connection } = await connectedManager(platform)
    try {
      const run = native.run.bind(native)
      native.run = async (session, op, args) => {
        const result = await run(session, op, args)
        return op === 'gatt.discover' ? { ...result, skipped: [] } : result
      }
      const failure = await connection.discover(NO_OPTIONS).then(
        () => null,
        error => error
      )
      expect(failure?.normalized).toMatchObject({ code: 'protocol.malformed' })
    } finally {
      await manager.destroy()
    }
  })
})
