// src/NativeUnifiedBleRustCore.ts
//
// Codegen spec for the production React Native session facade over the
// process-owned Rust mobile host (docs/MOBILE_RUST_WIRE.md). Session invoke
// and drain use Rust's ubm-mobile-wire/3 JSON unchanged; Java/Swift do not
// re-shape those operations. The OS accessory chooser and saved-authorized
// directory are separate versioned native-control JSON seams. All methods
// are asynchronous; nothing blocks the module thread.

import type { CodegenTypes, TurboModule } from 'react-native'
import { TurboModuleRegistry } from 'react-native'

export interface RustCoreSessionWake {
  sessionId: string
}

export interface RustCoreRecoveryHint {
  kind: string
}

export interface Spec extends TurboModule {
  /** Trusted bootstrap only: fixes the process ledger bound before host install. */
  configureTerminalEvidenceMaximumBytes(maximumBytes: string): Promise<void>
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
  /** Selects a qualified scan terminal format for this exact session. */
  negotiateScanEndVersion(sessionId: string, offeredJson: string): Promise<string>
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
  /** Session-owned ordered recording controls; the version probe has no radio effects. */
  orderedRecordingVersion(): Promise<number>
  orderedRecordingReserve(sessionId: string, id: string): Promise<string>
  orderedRecordingStartStatus(sessionId: string, handle: string): Promise<string>
  orderedRecordingRetire(sessionId: string, handle: string): Promise<string>
  orderedRecordingStart(
    sessionId: string,
    handle: string,
    id: string,
    maxBytes: string,
    maxRecords: string,
    declarationJson: string,
    budgetMs: number
  ): Promise<string>
  orderedRecordingContext(sessionId: string, id: string): Promise<string>
  /** Trusted private-store context only; native refuses while an owner is live. */
  orderedRecordingOfflineContext(id: string): Promise<string>
  orderedRecordingOfflineStatus(id: string): Promise<string>
  orderedRecordingOfflineStop(id: string): Promise<string>
  orderedRecordingStop(sessionId: string, id: string): Promise<string>
  continuationRecordingStatus(id: string): Promise<string>
  continuationRecordingPrepare(id: string, maxItems: number, maxBytes: number): Promise<string>
  continuationRecordingAcknowledge(id: string, token: string): Promise<string>
  continuationRecordingStop(id: string): Promise<string>
  continuationRecordingClear(id: string): Promise<string>
  /** Binds an existing host-owned ordered recording to the Apple source-failure spool. */
  notificationFailureSpoolConfigure(recordingId: string, maxBytes: string, maxRecords: string): Promise<string>
  /** Prepares a sealed continuation handoff. JS must acknowledge claimToken only after decoding batches. */
  prepareContinuationClaim(maxItems: number, maxBytes: number): Promise<string>
  /** Acknowledges a decoded prepared handoff and performs retryable native cleanup. */
  acknowledgeContinuationClaim(claimToken: string): Promise<string>
  /** Android retained claim protocol; receipts remain retryable until retired. */
  prepareRetainedContinuationClaimV1?(maxItems: number, maxBytes: number): Promise<string>
  acknowledgeRetainedContinuationClaimV1?(claimToken: string): Promise<string>
  retireRetainedContinuationClaimReceiptV1?(claimToken: string): Promise<string>
  /** Explicit V2 retained-prefix negotiation. Never falls back to V1. */
  prepareRetainedContinuationClaimV2?(maxItems: number, maxBytes: number): Promise<string>
  acknowledgeRetainedContinuationClaimV2?(claimToken: string): Promise<string>
  /** Read or acknowledge the route-bound peer failure record returned only by a V2 claim. */
  readPeerFailureRecovery?(recoveryRecord: string, offset: string): Promise<string>
  acknowledgePeerFailureRecovery?(recoveryRecord: string): Promise<string>
  /** Native-selected retained terminal evidence. Callers receive opaque handles only. */
  terminalEvidenceRecoveryOpenSelected?(): Promise<string>
  terminalEvidenceRecoveryList?(leaseId: string, maxItems: number, maxBytes: number): Promise<string>
  terminalEvidenceRecoveryListWithStatus?(leaseId: string, maxItems: number, maxBytes: number): Promise<string>
  terminalEvidenceRecoveryRead?(handle: string, offset: string, maxBytes: number): Promise<string>
  terminalEvidenceRecoveryAcknowledge?(handle: string): Promise<string>
  /** Native-selected, opaque-handle scan-failure recovery. No method accepts a recording id or path. */
  scanFailureRecoveryOpenSelected(): Promise<string>
  scanFailureRecoveryOwners(leaseId: string): Promise<string>
  scanFailureRecoveryMember(ownerHandle: string, index: number): Promise<string>
  scanFailureRecoveryHead(ownerHandle: string, index: number): Promise<string>
  scanFailureRecoveryControl(headHandle: string, operation: string): Promise<string>
  scanFailureRecoveryPendingCleanup(leaseId: string): Promise<string>
  scanFailureRecoveryRetireCleanup(cleanupReceiptId: string): Promise<void>
  /** Owner-scoped raw terminal controls; native returns `{cleanupReceiptId}`. */
  scanFailureRecoveryRawControl(ownerHandle: string, operation: string): Promise<string>
  /** Raw source metadata selected through an opaque owner handle. */
  scanFailureRecoveryRawInfo(ownerHandle: string): Promise<string>
  /** Raw source member metadata selected through an opaque owner handle. */
  scanFailureRecoveryRawMember(ownerHandle: string, index: number): Promise<string>
  /** One emission per armed Rust wake; JS drains until `more` is false. */
  readonly onSessionWake: CodegenTypes.EventEmitter<RustCoreSessionWake>
  /** Payload-free cleanup hint; inspect recordings for retained data and failures. */
  readonly onRecoveryHint: CodegenTypes.EventEmitter<RustCoreRecoveryHint>
}

export default TurboModuleRegistry.getEnforcing<Spec>('UnifiedBleRustCore')
