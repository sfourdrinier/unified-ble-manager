const { rustCoreHarness, environment, scanOptions } = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicPeerDirectory } = require('../../../src/public/peer-directory')

const options = Object.freeze({ signal: null, deadline: null })

test.each(['android', 'apple'])(
  '%s scanned directory records preserve their native clock and scope',
  async platform => {
    const scopes = []
    for (let index = 0; index < 2; index += 1) {
      const harness = rustCoreHarness({ platform })
      const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
      const backend = manager.attachedBackend.backend
      try {
        const scan = await manager.scan(scanOptions())
        harness.native.emitAdvertisement(DEFAULT_PEER, {
          localName: 'Observed clock peer',
          rssi: -42,
          observedAtMs: 1234
        })
        const observed = await scan.observations[Symbol.asyncIterator]().next()
        await scan.stop()
        const connection = await manager.connect(observed.value.value.device.id, options)
        const raw = await backend.peers.connected(options)
        expect(raw).toHaveLength(1)
        const directory = createPublicPeerDirectory(backend.peers, () => 900000)
        const connected = await directory.connected()
        expect(raw[0].clockScope).toEqual(expect.any(String))
        expect(raw[0].clockScope.length).toBeGreaterThan(0)
        scopes.push(raw[0].clockScope)
        const known = await directory.known()
        const resolved = await directory.resolve(raw[0].reference)
        for (const peer of [connected[0], known[0], resolved]) {
          expect(peer).toMatchObject({ name: 'Observed clock peer', rssi: -42, state: { lastSeenAtMonotonicMs: 1234 } })
        }
        expect((await backend.peers.known(options))[0].clockScope).toBe(raw[0].clockScope)
        expect((await backend.peers.resolve(raw[0].reference, options)).clockScope).toBe(raw[0].clockScope)
        expect((await connection.disconnect()).state).toBe('released')
      } finally {
        expect((await manager.destroy()).state).toBe('released')
      }
    }
    expect(scopes[0]).not.toBe(scopes[1])
  }
)
