// src/tck/public-manager-seam.ts
//
// Interchangeable public-manager construction seam (UBM 5.0 TCK card, Phase 2).
// The SAME existing public scenarios run against the pinned TypeScript
// reference and, later, a real Rust backend. This slice ships the seam, the
// pinned reference binding, and a future-Rust stub interface with
// deterministic-adapter hooks. No Rust in this slice. No replacement DSL,
// no second TCK.

import type { BleCentralBackend } from '../backend-contract/backend'
import type { BackendIdentity } from '../backend-contract/identity'
import type { SerializableRecord } from '../backend-contract/primitives'
import { UNIFIED_BLE_IMPLEMENTATION_VERSION } from '../implementation-version'
import type { BackendTckFactory, BackendTckFixture, TckFact, TckScenarioDefinition } from './contracts'

/** Pinned TypeScript reference, recorded outside the shipped graph. */
export const PINNED_TS_REFERENCE = Object.freeze({
  kind: 'ts-reference',
  baseSha: '8c8195dd0430ff847d9492ce32f4e63ea3a5df1d',
  baseShortSha: '8c8195dd',
  implementationVersion: UNIFIED_BLE_IMPLEMENTATION_VERSION
})

export type PublicManagerSeamKind = 'ts-reference' | 'rust-stub'

export interface TsReferenceManagerSeam {
  readonly kind: 'ts-reference'
  readonly reference: typeof PINNED_TS_REFERENCE
  runPublicScenario<
    Attachment extends string,
    Identity extends BackendIdentity<Attachment>,
    Backend extends BleCentralBackend<Attachment, Identity>
  >(
    factory: BackendTckFactory<Attachment, Identity, Backend>,
    fixture: BackendTckFixture<Attachment, Identity, Backend>,
    definition: TckScenarioDefinition
  ): Promise<readonly TckFact[]>
}

export interface RustDeterministicAdapterHooks {
  readonly onDeterministicAdapter: (input: SerializableRecord) => Promise<void>
}

export interface RustBackendStubSeam {
  readonly kind: 'rust-stub'
  readonly deterministicAdapterHook: (input: SerializableRecord) => Promise<void>
  createPublicManager<
    Attachment extends string,
    Identity extends BackendIdentity<Attachment>,
    Backend extends BleCentralBackend<Attachment, Identity>
  >(
    factory: BackendTckFactory<Attachment, Identity, Backend>,
    fixture: BackendTckFixture<Attachment, Identity, Backend>,
    definition: TckScenarioDefinition
  ): Promise<never>
}

/** Binds the seam to the pinned TypeScript reference construction. */
export function createTsReferenceManagerSeam(): TsReferenceManagerSeam {
  const runPublicScenario: TsReferenceManagerSeam['runPublicScenario'] = async (factory, fixture, definition) => {
    // Lazy require avoids a static cycle with the runner, which owns the
    // scenario executors. The seam delegates to the SAME runner-owned path.

    const runner = require('./runner-observers') as {
      executeRunnerOwnedTckScenario: typeof import('./runner-observers').executeRunnerOwnedTckScenario
    }
    return runner.executeRunnerOwnedTckScenario(factory, fixture, definition)
  }
  return Object.freeze({ kind: 'ts-reference', reference: PINNED_TS_REFERENCE, runPublicScenario })
}

/**
 * Stub for the future Rust backend. It exposes the seam shape and the
 * deterministic-adapter hook so U2/U3/U7 wiring can land later, but it never
 * manufactures passing facts: every manager construction rejects with an
 * explicit unimplemented marker.
 */
export function createRustBackendStubSeam(hooks: RustDeterministicAdapterHooks): RustBackendStubSeam {
  if (typeof hooks.onDeterministicAdapter !== 'function') {
    throw new Error('rust-backend-stub-seam: onDeterministicAdapter must be a function')
  }
  return Object.freeze({
    kind: 'rust-stub',
    deterministicAdapterHook: async (input: SerializableRecord): Promise<void> => {
      await hooks.onDeterministicAdapter(input)
    },
    createPublicManager: async (): Promise<never> => {
      throw new Error('rust-backend-unimplemented: no Rust central exists in this slice')
    }
  })
}
