// src/NativeUnifiedBleRustCore.ts
//
// Codegen spec for the production React Native session facade over the
// process-owned Rust mobile host (docs/MOBILE_RUST_WIRE.md). Session invoke
// and drain use Rust's ubm-mobile-wire/1 JSON unchanged; Java/Swift do not
// re-shape those operations. The OS accessory chooser and saved-authorized
// directory are separate versioned native-control JSON seams. All methods
// are asynchronous; nothing blocks the module thread.

import type { CodegenTypes, TurboModule } from 'react-native'
import { TurboModuleRegistry } from 'react-native'

export interface RustCoreSessionWake {
  sessionId: string
}

export interface Spec extends TurboModule {
  /** ubm-accessory-chooser/1 OS setup, available on configured iOS 18+ hosts. */
  chooseAccessory(requestId: string, optionsJson: string, timeoutMs: number): Promise<string>
  cancelAccessoryChoice(requestId: string): Promise<void>
  accessoryChooserAvailable(): Promise<boolean>
  /** ubm-accessory-authorized/1 OS-saved ASK Bluetooth identifiers; no radio or picker. */
  authorizedAccessories(): Promise<string>
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
  /** Durable recording controls, independent of a BLE session. Paths are native-owned. */
  continuationRecordingStatus(id: string): Promise<string>
  continuationRecordingPrepare(id: string, maxItems: number, maxBytes: number): Promise<string>
  continuationRecordingAcknowledge(id: string, token: string): Promise<string>
  continuationRecordingStop(id: string): Promise<string>
  continuationRecordingClear(id: string): Promise<string>
  /** Prepares a sealed continuation handoff. JS must acknowledge claimToken only after decoding batches. */
  prepareContinuationClaim(maxItems: number, maxBytes: number): Promise<string>
  /** Acknowledges a decoded prepared handoff and performs retryable native cleanup. */
  acknowledgeContinuationClaim(claimToken: string): Promise<string>
  /** One emission per armed Rust wake; JS drains until `more` is false. */
  readonly onSessionWake: CodegenTypes.EventEmitter<RustCoreSessionWake>
}

export default TurboModuleRegistry.getEnforcing<Spec>('UnifiedBleRustCore')
