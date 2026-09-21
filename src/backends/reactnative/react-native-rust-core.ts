// src/backends/reactnative/react-native-rust-core.ts
//
// The React Native seam over the process-owned Rust mobile host
// (docs/MOBILE_RUST_WIRE.md, wire `ubm-mobile-wire/1`). The production
// implementation is `createReactNativeRustCoreBinding` over the
// `UnifiedBleRustCore` TurboModule; tests inject a binding built by the same
// function over a deterministic native module, so every byte crosses the real
// production serializer.
//
// The seam never constructs (or falls back to) a TypeScript radio: a missing
// module, a foreign identity or revision, or a malformed admission fails
// loudly before any radio operation.

import { contractError } from '../../backend-contract/errors'
import { EXPECTED_NATIVE_BUILD_IDENTITY } from '../../generated/native-build-identity'
import type { NativeBuildIdentityRecord } from '../../native-build-identity-check'
import type { WireDrainBatch, WireJsonObject, WireOp, WireOpResults, WireRestorationIdentity } from './rust-core-wire'

/**
 * The linked `ubm-core` contract revision a native Rust core must report.
 * It is the revision the package's native build identity was sealed with.
 */
export const RUST_CORE_CONTRACT_REVISION: string = EXPECTED_NATIVE_BUILD_IDENTITY.contractRevision

/** One admitted session lease on the process-owned Rust mobile host. */
export interface ReactNativeRustCoreSession {
  /** The host-issued session id (decimal string). */
  readonly sessionId: string
  /** The verified build identity of the binary serving this session. */
  readonly buildIdentity: NativeBuildIdentityRecord
  /**
   * Runs one wire operation: arguments are serialized by the production
   * codec and the envelope is parsed strictly. A failure rejects with the
   * exact contract identity Rust reported, including the write commit state.
   */
  invoke<Op extends WireOp>(op: Op, args: WireJsonObject): Promise<WireOpResults[Op]>
  /** Drains queued records; ordinals are checked against the last delivered one. */
  drain(maxItems: number, maxBytes: number): Promise<WireDrainBatch>
  /** Registers the one wake listener of this session; returns its remover. */
  onWake(listener: () => void): () => void
  /** Ends the lease after `session.dispose` reported its record. Idempotent; a failure permits a retry. */
  close(): Promise<void>
}

/** Restoration identity request (`restorationIdentity`). */
export interface RustCoreRestorationIdentityRequest {
  readonly restorationId: string
  readonly generation: string
}

/** Native Rust core entry. */
export interface ReactNativeRustCoreBinding {
  /** Verifies the binary identity, then admits one session lease. */
  openSession(owner: string): Promise<ReactNativeRustCoreSession>
  /**
   * Persists the declared background standing order in the native owner so
   * an OS wake (no JavaScript) can execute it. Optional: a binding whose
   * native module predates it omits the method, and a non-`record-only`
   * declaration then fails fast with `capability.unsupported` instead of a
   * silent record-only.
   */
  declareBackgroundContinuation?(declarationJson: string): Promise<void>
  /**
   * Drains the continuation backlog the wake queued (verbatim
   * `{batches, disposed}` claim JSON). Optional like the declare method; a
   * missing claim answers `capability.unsupported`, never an invented empty
   * backlog.
   */
  claimContinuation?(maxItems: number, maxBytes: number): Promise<string>
  /**
   * Reports the continuation posture (verbatim status JSON). Optional like
   * the declare method.
   */
  continuationStatus?(): Promise<string>
  /** Cryptographically secure random bytes from the platform CSPRNG (1..1024). */
  randomBytes(length: number): Promise<Uint8Array>
  /** The app-declared restoration identity (Info.plist / manifest). */
  restorationIdentity(request: RustCoreRestorationIdentityRequest): Promise<WireRestorationIdentity>
  /**
   * The identity the app configured natively (Info.plist
   * `UnifiedBleProtocolRestorationId` / `…Generation`) without naming it
   * from JS, or `null` when it configured none (always on Android). The
   * legacy native module read this authority at init.
   */
  configuredRestorationIdentity(): Promise<WireRestorationIdentity | null>
}

function hasFunction(candidate: object, name: string): boolean {
  return typeof Reflect.get(candidate, name) === 'function'
}

function isBinding(candidate: unknown): candidate is ReactNativeRustCoreBinding {
  return (
    typeof candidate === 'object' &&
    candidate !== null &&
    hasFunction(candidate, 'openSession') &&
    hasFunction(candidate, 'randomBytes') &&
    hasFunction(candidate, 'restorationIdentity') &&
    hasFunction(candidate, 'configuredRestorationIdentity')
  )
}

/**
 * Resolves an injected binding. Anything but a well-shaped binding fails
 * with `capability.unsupported`: there is no TypeScript fallback.
 */
export function resolveReactNativeRustCoreBinding(candidate: unknown): ReactNativeRustCoreBinding {
  if (!isBinding(candidate)) {
    throw contractError('capability.unsupported', 'capability', 'react-native-manager.rust-core-missing')
  }
  return candidate
}
