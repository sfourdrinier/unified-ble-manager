// src/NativeUnifiedBleRustCore.ts
//
// Codegen spec for the production React Native session facade over the
// process-owned Rust mobile host (docs/MOBILE_RUST_WIRE.md). Every value
// crossing this boundary is a JSON string written by Rust or a primitive:
// Java/Swift never parse or re-shape operation arguments or results, so the
// wire schema exists only in Rust and in rust-core-wire.ts. All methods are
// asynchronous; nothing blocks the module thread.

import type { CodegenTypes, TurboModule } from 'react-native'
import { TurboModuleRegistry } from 'react-native'

export interface RustCoreSessionWake {
  sessionId: string
}

export interface Spec extends TurboModule {
  /**
   * Admits one session lease on the process host (installing the host on
   * first use). Resolves Rust's admission JSON
   * `{sessionId, contractRevision, wireRevision, buildIdentity}`; rejects
   * with Rust's structured wire failure JSON as the message when admission
   * is refused (for example a wire-revision mismatch).
   */
  openSession(owner: string, expectedWireRevision: string): Promise<string>
  /** Runs one wire operation; resolves Rust's envelope JSON verbatim. */
  invoke(sessionId: string, op: string, argsJson: string): Promise<string>
  /** Drains queued records; resolves Rust's `{more, records}` JSON verbatim. */
  drain(sessionId: string, maxItems: number, maxBytes: number): Promise<string>
  /**
   * Ends the session lease after `session.dispose` has reported its cleanup
   * record. Idempotent.
   */
  closeSession(sessionId: string): Promise<void>
  /** Rust-answered `ubm-native-build-identity/1` JSON of the loaded binary. */
  nativeBuildIdentity(): Promise<string>
  /** Rust-answered contract revision of the loaded binary. */
  contractRevision(): Promise<string>
  /** Rust-answered wire revision of the loaded binary. */
  wireRevision(): Promise<string>
  /**
   * `length` cryptographically secure random bytes from the platform CSPRNG,
   * as strict padded base64 (RFC 4648 §4). JS rejects `length` outside
   * 1..1024 before calling.
   */
  randomBytes(length: number): Promise<string>
  /**
   * The app-declared restoration identity (Info.plist / manifest values).
   * `requestJson` is `{restorationId, generation}`; resolves
   * `{applicationId, restorationId, generation, restoreIdentifier,
   * namespaceValue, clientId, hostSessionScope}` JSON.
   */
  restorationIdentity(requestJson: string): Promise<string>
  /**
   * Persists the declared background standing order (`background.continuation`
   * canonical JSON) in the native owner so an OS wake with no JavaScript can
   * execute it. Resolves once persisted; rejects with the native reason when
   * the declaration is malformed.
   */
  declareBackgroundContinuation(declarationJson: string): Promise<void>
  /**
   * Reports the continuation posture for Diagnostics: the declared strategy
   * and the last wake outcome. Resolves the status JSON verbatim.
   */
  continuationStatus(): Promise<string>
  /**
   * Drains the continuation backlog (verbatim drain batches for the JS
   * codec) and disposes the continuation session. Resolves
   * `{batches, disposed}` JSON verbatim.
   */
  claimContinuation(maxItems: number, maxBytes: number): Promise<string>
  /** One emission per armed Rust wake; JS drains until `more` is false. */
  readonly onSessionWake: CodegenTypes.EventEmitter<RustCoreSessionWake>
}

export default TurboModuleRegistry.getEnforcing<Spec>('UnifiedBleRustCore')
