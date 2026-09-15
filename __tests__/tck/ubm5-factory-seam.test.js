// __tests__/tck/ubm5-factory-seam.test.js
//
// UBM 5.0 TCK factory-seam slice (trackourhealth/bun-mono#1188, TCK card; enables U2/U3/U7).
// Test-first: this suite is the RED gate for src/tck/public-manager-seam.ts.

const { baseTckScenarios } = require('../../src/tck/scenarios')
const { executeRunnerOwnedTckScenario } = require('../../src/tck/runner-observers')
const { createDeterministicBackendTckFactory } = require('../../src/tck/deterministic/deterministic-tck-factory')
const {
  PINNED_TS_REFERENCE,
  createTsReferenceManagerSeam,
  createRustBackendStubSeam
} = require('../../src/tck/public-manager-seam')

describe('UBM5 TCK factory seam', () => {
  test('pins the TS reference outside the shipped graph (base SHA + version)', () => {
    expect(PINNED_TS_REFERENCE).toMatchObject({
      kind: 'ts-reference',
      baseShortSha: '8c8195dd',
      implementationVersion: '4.0.28'
    })
    expect(String(PINNED_TS_REFERENCE.baseSha)).toContain('8c8195dd')
    expect(String(PINNED_TS_REFERENCE.baseSha)).toBe('8c8195dd0430ff847d9492ce32f4e63ea3a5df1d')
  })

  test('reference seam injects public-manager construction for the SAME public scenarios', async () => {
    const seam = createTsReferenceManagerSeam()
    expect(seam.kind).toBe('ts-reference')
    expect(seam.reference).toMatchObject({
      baseShortSha: '8c8195dd',
      implementationVersion: '4.0.28'
    })

    const definition = baseTckScenarios.find(
      candidate => candidate.id === 'scenario.scan-connect-discover-read-notify-destroy'
    )
    if (definition === undefined) {
      throw new Error('vertical-slice TCK definition is missing')
    }
    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(Object.freeze({ scenarioId: definition.id }))
    try {
      const direct = await executeRunnerOwnedTckScenario(factory, fixture, definition)
      expect(direct.every(fact => fact.holds)).toBe(true)
    } finally {
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }

    const fixture2 = await factory.create(Object.freeze({ scenarioId: definition.id }))
    try {
      const viaSeam = await seam.runPublicScenario(factory, fixture2, definition)
      expect(viaSeam.map(fact => fact.id)).toEqual(definition.requiredFacts)
      expect(viaSeam.every(fact => fact.holds)).toBe(true)
    } finally {
      expect(await fixture2.dispose()).toEqual({ state: 'released', failures: [] })
    }
  })

  test('future Rust backend arrives as a stub interface with deterministic-adapter hooks (no Rust, no fake pass)', async () => {
    const stub = createRustBackendStubSeam({
      onDeterministicAdapter: async () => undefined
    })
    expect(stub.kind).toBe('rust-stub')
    expect(typeof stub.deterministicAdapterHook).toBe('function')

    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(
      Object.freeze({ scenarioId: 'scenario.scan-connect-discover-read-notify-destroy' })
    )
    try {
      await expect(
        stub.createPublicManager(factory, fixture, {
          id: 'scenario.scan-connect-discover-read-notify-destroy',
          execution: 'base',
          requiredFacts: [],
          requiredControllerActions: []
        })
      ).rejects.toThrow('rust-backend-unimplemented')
    } finally {
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
  })

  test('stub deterministic-adapter hook runs without manufacturing facts', async () => {
    const seen = []
    const stub = createRustBackendStubSeam({
      onDeterministicAdapter: async input => {
        seen.push(input)
      }
    })
    await stub.deterministicAdapterHook(Object.freeze({ probe: 'hook-wiring' }))
    expect(seen).toHaveLength(1)
  })

  test('runner injection runs the SAME scenario via the reference seam option', async () => {
    const { executePublicTckScenario } = require('../../src/tck/runner-public-scenarios')
    const definition = baseTckScenarios.find(
      candidate => candidate.id === 'gatt.discovery-complete-paths-and-services-changed'
    )
    if (definition === undefined) {
      throw new Error('gatt discovery TCK definition is missing')
    }
    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(Object.freeze({ scenarioId: definition.id }))
    try {
      const facts = await executePublicTckScenario(factory, fixture, definition, { kind: 'ts-reference' })
      expect(facts.map(fact => fact.id)).toEqual(definition.requiredFacts)
      expect(facts.every(fact => fact.holds)).toBe(true)
    } finally {
      const { TckAssertionError } = require('../../src/tck/contracts')
      expect(TckAssertionError).toBeDefined()
      await fixture.controller.settle(
        (async () => {
          try {
            return true
          } finally {
            // manager cleanup is owned by executePublicTckScenario; fixture dispose below covers backend
          }
        })()
      )
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
  })

  test('runner injection rejects the Rust stub without a fake pass', async () => {
    const { executePublicTckScenario } = require('../../src/tck/runner-public-scenarios')
    const definition = baseTckScenarios.find(
      candidate => candidate.id === 'gatt.discovery-complete-paths-and-services-changed'
    )
    if (definition === undefined) {
      throw new Error('gatt discovery TCK definition is missing')
    }
    const factory = createDeterministicBackendTckFactory()
    const fixture = await factory.create(Object.freeze({ scenarioId: definition.id }))
    try {
      await expect(executePublicTckScenario(factory, fixture, definition, { kind: 'rust-stub' })).rejects.toThrow(
        'rust-backend-unimplemented'
      )
    } finally {
      expect(await fixture.dispose()).toEqual({ state: 'released', failures: [] })
    }
  })
})
