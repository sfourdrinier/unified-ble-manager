// __tests__/backends/bluez/bluez-pairing-generation.test.js
//
// The privileged pairing-generation path. The rules under test are all about
// what happens when something goes wrong while an adapter-wide setting is held,
// because that is where a library can quietly weaken a host's security or lie
// about a bond it created.
const {
  generationForSecureConnections,
  withPairingGeneration
} = require('../../../src/backends/bluez/bluez-pairing-generation')

function controller(initial, overrides = {}) {
  const calls = []
  let current = initial
  return {
    calls,
    read: async adapterId => {
      calls.push(['read', adapterId])
      return current
    },
    set: async (adapterId, generation) => {
      calls.push(['set', adapterId, generation])
      if (overrides.failSetTo === generation) throw new Error(`cannot set ${generation}`)
      current = generation
    },
    get current() {
      return current
    }
  }
}

const noReport = () => undefined

describe('selecting an LE pairing generation', () => {
  test("'prefer' is not accepted, so deferring to the platform can never touch adapter state", () => {
    // A compile-time fact in TypeScript; asserted here for the JS surface.
    expect(generationForSecureConnections('require')).toBe('required')
    expect(generationForSecureConnections('disallow')).toBe('legacy-only')
  })

  test('holds the adapter for the pairing and puts it back afterwards', async () => {
    const adapter = controller('enabled')

    await expect(
      withPairingGeneration(adapter, '/org/bluez/hci0', 'legacy-only', async () => 'bonded', noReport)
    ).resolves.toBe('bonded')

    expect(adapter.calls).toEqual([
      ['read', '/org/bluez/hci0'],
      ['set', '/org/bluez/hci0', 'legacy-only'],
      ['set', '/org/bluez/hci0', 'enabled']
    ])
    expect(adapter.current).toBe('enabled')
  })

  test('does not touch an adapter that is already at the requested generation', async () => {
    const adapter = controller('legacy-only')

    await withPairingGeneration(adapter, '/org/bluez/hci0', 'legacy-only', async () => 'bonded', noReport)

    expect(adapter.calls).toEqual([['read', '/org/bluez/hci0']])
  })

  test('restores the adapter when the pairing fails, and reports the pairing error', async () => {
    const adapter = controller('enabled')

    await expect(
      withPairingGeneration(
        adapter,
        '/org/bluez/hci0',
        'legacy-only',
        async () => {
          throw new Error('peer refused')
        },
        noReport
      )
    ).rejects.toThrow('peer refused')

    expect(adapter.current).toBe('enabled')
  })

  /**
   * The rule this release exists to enforce: a bond that was actually created
   * is never reported as if it never happened. A failed restore leaves the
   * adapter misconfigured, which is serious - but it is a different fact from
   * "you are not bonded", and the caller asked the second question.
   */
  test('a bond survives a failed restore: the pairing succeeds and the restore is reported', async () => {
    const adapter = controller('enabled', { failSetTo: 'enabled' })
    const reported = []

    await expect(
      withPairingGeneration(adapter, '/org/bluez/hci0', 'legacy-only', async () => 'bonded', failure =>
        reported.push(failure)
      )
    ).resolves.toBe('bonded')

    expect(reported).toEqual([
      {
        adapterId: '/org/bluez/hci0',
        heldGeneration: 'legacy-only',
        intendedGeneration: 'enabled',
        detail: 'cannot set enabled'
      }
    ])
  })

  /**
   * When both fail, the caller asked about the pairing - so that error is the
   * one thrown, and the restore failure rides along rather than displacing it.
   */
  test('a failed pairing keeps its own error and carries the restore failure with it', async () => {
    const adapter = controller('enabled', { failSetTo: 'enabled' })

    const error = await withPairingGeneration(
      adapter,
      '/org/bluez/hci0',
      'legacy-only',
      async () => {
        throw new Error('peer refused')
      },
      noReport
    ).catch(thrown => thrown)

    expect(error.message).toBe('peer refused')
    expect(error.pairingGenerationRestoreFailure).toMatchObject({
      heldGeneration: 'legacy-only',
      intendedGeneration: 'enabled'
    })
  })

  /**
   * A `set` that rejects may still have half-applied - the command reached the
   * kernel and the response read failed. Treating that as "nothing happened"
   * is how a controller ends up silently stuck in LE Legacy.
   */
  test('attempts a restore even when the initial set fails', async () => {
    const adapter = controller('enabled', { failSetTo: 'legacy-only' })
    const reported = []

    await expect(
      withPairingGeneration(adapter, '/org/bluez/hci0', 'legacy-only', async () => 'bonded', failure =>
        reported.push(failure)
      )
    ).rejects.toThrow('cannot set legacy-only')

    // Put back rather than assumed untouched.
    expect(adapter.calls).toContainEqual(['set', '/org/bluez/hci0', 'enabled'])
    expect(adapter.current).toBe('enabled')
    expect(reported).toEqual([])
  })

  /**
   * A host whose logger throws - a closed stderr, a broken transport - must not
   * be able to turn a completed bond into a reported failure. The reporter is
   * caller-supplied, so the module's contract has to hold for any of them.
   */
  test('a reporter that throws cannot change the pairing outcome', async () => {
    const adapter = controller('enabled', { failSetTo: 'enabled' })

    await expect(
      withPairingGeneration(adapter, '/org/bluez/hci0', 'legacy-only', async () => 'bonded', () => {
        throw new Error('EPIPE')
      })
    ).resolves.toBe('bonded')
  })

  /** A controller that answers with nonsense is a host defect, named as one. */
  test('rejects a generation the controller invented', async () => {
    const adapter = { read: async () => undefined, set: async () => undefined }

    await expect(
      withPairingGeneration(adapter, '/org/bluez/hci0', 'legacy-only', async () => 'bonded', () => undefined)
    ).rejects.toThrow(/expected 'legacy-only', 'enabled' or 'required'/)
  })

  /**
   * The setting is adapter-wide, so two overlapping pairings interleave: the
   * second reads the first's temporary value as "previous" and restores to it,
   * leaving the adapter wrong whichever finishes last. Peer-level arbitration
   * cannot see this - the peers differ, the adapter does not.
   */
  test('concurrent pairings on one adapter do not corrupt the restore value', async () => {
    const adapter = controller('enabled')
    let releaseFirst
    const firstStarted = new Promise(resolve => {
      releaseFirst = resolve
    })

    const first = withPairingGeneration(
      adapter,
      '/org/bluez/hci0',
      'legacy-only',
      () => firstStarted,
      noReport
    )
    const second = withPairingGeneration(adapter, '/org/bluez/hci0', 'required', async () => 'second', noReport)

    releaseFirst('first')
    await expect(first).resolves.toBe('first')
    await expect(second).resolves.toBe('second')

    expect(adapter.current).toBe('enabled')
  })
})
