// src/tck/test-only-fault-hooks.ts
//
// Test-only fault/time injection hooks (UBM 5.0 TCK card, Phase 2 work item 5).
// Virtual clocks and fault injection stay out of production exports. Every
// entry point here fail-closes unless the explicit test-only marker is armed
// and the process is not a production shipping context.

import type { BackendTckFactory, BackendTckFixture, TckScenarioController } from './contracts'
import type { BleCentralBackend } from '../backend-contract/backend'
import type { BackendIdentity } from '../backend-contract/identity'

/** Fail-closed marker: fault/time hooks run only when this global is armed in tests. */
export const TCK_TEST_ONLY_MARKER = '__UBM_TCK_TEST_ONLY__' as const

function readTestOnlyMarker(holder: object): unknown {
  return Reflect.get(holder, TCK_TEST_ONLY_MARKER)
}

function markerArmed(): boolean {
  return readTestOnlyMarker(globalThis) === true
}

function isProductionContext(): boolean {
  return process.env.NODE_ENV === 'production'
}

/** Throws unless the caller is inside the armed test-only context. Fail-closed. */
export function assertTestOnlyFaultContext(caller: string): void {
  if (caller.length === 0) {
    throw new Error('tck-test-only-fault-hooks: caller must not be empty')
  }
  if (isProductionContext()) {
    throw new Error(`tck-test-only-fault-hooks.${caller}: test-only hooks are forbidden in production`)
  }
  if (!markerArmed()) {
    throw new Error(`tck-test-only-fault-hooks.${caller}: test-only marker is not armed`)
  }
}

export interface TestOnlyFaultHookInput<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
> {
  readonly controller: TckScenarioController
  readonly factory: BackendTckFactory<Attachment, Identity, Backend>
}

export interface TestOnlyFaultHooks {
  readonly advanceTimeMs: (milliseconds: number) => Promise<void>
  readonly emitNotification: (
    input: Record<string, never> | import('../backend-contract/primitives').SerializableRecord
  ) => Promise<void>
  readonly queueAdvertisement: () => Promise<void>
}

/**
 * Creates deterministic time/fault inputs for tests. The hooks can change the
 * test boundary but cannot manufacture facts, receipts, or proof labels.
 */
export function createTestOnlyFaultHooks<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(input: TestOnlyFaultHookInput<Attachment, Identity, Backend>): TestOnlyFaultHooks {
  assertTestOnlyFaultContext('createTestOnlyFaultHooks')
  const controller = input.controller
  if (input.factory.backendId.length === 0) {
    throw new Error('tck-test-only-fault-hooks: factory backendId must not be empty')
  }
  return Object.freeze({
    advanceTimeMs: async (milliseconds: number): Promise<void> => {
      assertTestOnlyFaultContext('advanceTimeMs')
      if (!Number.isSafeInteger(milliseconds) || milliseconds < 0) {
        throw new Error('tck-test-only-fault-hooks.advanceTimeMs: milliseconds must be a non-negative safe integer')
      }
      await controller.perform('advance-time', Object.freeze({ milliseconds }))
    },
    emitNotification: async (
      notification: import('../backend-contract/primitives').SerializableRecord
    ): Promise<void> => {
      assertTestOnlyFaultContext('emitNotification')
      // Really emits through the scenario controller: well-formed inputs are
      // delivered as notifications, malformed inputs reject loudly in
      // perform(). A silent no-op here would let a test believe it injected a
      // notification while nothing happened.
      await controller.perform('emit-notification', notification)
    },
    queueAdvertisement: async (): Promise<void> => {
      assertTestOnlyFaultContext('queueAdvertisement')
      await controller.perform('queue-advertisement', Object.freeze({}))
    }
  })
}

/**
 * Proves no reference/fault exports entered the production package entries.
 * The production root (src/index.ts), backend-sdk entry, and testing entry
 * must not expose test-only fault construction. Fails closed: any loader
 * failure, or any entry that is not an inspectable object, reports "not clean".
 */
export function isProductionEntryCleanOfTestOnlyFaultExports(
  loader: (specifier: string) => unknown = require
): boolean {
  const forbidden = ['createTestOnlyFaultHooks', 'TCK_TEST_ONLY_MARKER', 'assertTestOnlyFaultContext']
  let entries: readonly object[]
  try {
    entries = [
      requireProductionEntry(loader, '../index'),
      requireProductionEntry(loader, '../backend-sdk'),
      requireProductionEntry(loader, '../testing')
    ]
  } catch {
    return false
  }
  for (const entry of entries) {
    for (const name of forbidden) {
      if (name in entry) {
        return false
      }
    }
  }
  return true
}

function requireProductionEntry(loader: (specifier: string) => unknown, specifier: string): object {
  const loaded: unknown = loader(specifier)
  if (typeof loaded === 'object' && loaded !== null) {
    return loaded
  }
  throw new Error(`tck-test-only-fault-hooks: production entry ${specifier} did not load an inspectable object`)
}

export type { BackendTckFixture }
