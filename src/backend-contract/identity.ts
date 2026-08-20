// src/backend-contract/identity.ts

import type { BoundedAsyncStream } from './streams'
import type {
  AdapterId,
  AttachmentId,
  BackendCompatibilityOffer,
  BackendInstanceId,
  CoreVersionAxes,
  GenerationId,
  HostNeutralVersionAxes,
  IpcVersionAxes,
  MonotonicTimestamp,
  NativeVersionAxes,
  SerializableRecord
} from './primitives'

export type HostKind = 'browser' | 'native-mobile' | 'node' | 'desktop-native' | 'desktop-webview' | 'test'
export type AdapterAvailability = 'available' | 'unavailable' | 'unsupported' | 'unknown'
export type AdapterAuthorization = 'granted' | 'denied' | 'restricted' | 'not-determined' | 'unavailable' | 'unknown'
export type AdapterPower = 'on' | 'off' | 'resetting' | 'unsupported' | 'unknown'
export interface AdapterStateSnapshot<Attachment extends string> {
  readonly availability: AdapterAvailability
  /**
   * `'unknown'` when the platform exposes no per-application Bluetooth
   * authorization concept at all, or when this host did not query one. It is
   * the absence of a measurement and never a denial, which is why the other
   * five values cannot express it: `'not-determined'` asserts a pending user
   * decision and `'unavailable'` asserts the platform withheld access, so a
   * backend that did not measure must report `'unknown'` rather than pick one,
   * exactly as `availability` and `power` already do. `safeReason` states why.
   * Readiness decisions go through `isAuthorizationBlocking`, never through a
   * direct comparison.
   */
  readonly authorization: AdapterAuthorization
  readonly power: AdapterPower
  readonly backendGeneration: GenerationId<'backend-generation', Attachment>
  readonly updatedAt: MonotonicTimestamp
  readonly safeReason: string | null
}

/**
 * The one readiness predicate for `authorization`, shared by every backend so
 * the semantics cannot drift.
 *
 * Only an explicit negative blocks. `'denied'`, `'restricted'` and
 * `'unavailable'` are decisions the platform has already made against us;
 * everything else is the absence of a decision and must not be treated as one:
 *
 * - `'unknown'` means nothing was measured.
 * - `'not-determined'` means the user has not been asked yet. Reading the
 *   authorization state does not prompt on any platform — the prompt is raised
 *   by *using* the radio. Refusing to use it while the answer is pending would
 *   make the state self-perpetuating: the prompt could never appear and a fresh
 *   install would stay unauthorized forever. Attempting the operation lets the
 *   platform ask, and a genuine refusal then arrives as a real error with the
 *   `permission.*` reason the backends already produce.
 */
export function isAuthorizationBlocking(authorization: AdapterAuthorization): boolean {
  return authorization === 'denied' || authorization === 'restricted' || authorization === 'unavailable'
}
export interface AdapterStateWatch<Attachment extends string> {
  readonly initial: AdapterStateSnapshot<Attachment>
  readonly transitions: BoundedAsyncStream<AdapterStateSnapshot<Attachment>>
}
export interface BackendRuntimeMetadata {
  readonly hostKind: HostKind
  readonly implementationVersion: string
  readonly diagnostics: SerializableRecord
}
export interface AdapterDescriptor<Attachment extends string> {
  readonly adapterId: AdapterId<Attachment>
  readonly displayName: string | null
  readonly state: AdapterStateSnapshot<Attachment>
  readonly adapterGeneration: GenerationId<'adapter-generation', Attachment>
  readonly limitations: readonly string[]
}
export interface AttachmentRecord<Attachment extends string> {
  readonly attachmentId: AttachmentId<Attachment>
  readonly backendInstanceId: BackendInstanceId<Attachment>
  readonly backendGeneration: GenerationId<'backend-generation', Attachment>
  readonly adapter: AdapterDescriptor<Attachment>
}

/** Compares the complete immutable backend attachment tuple. */
export function attachmentRecordsEqual<Attachment extends string>(
  left: AttachmentRecord<Attachment>,
  right: AttachmentRecord<Attachment>
): boolean {
  return (
    left.attachmentId === right.attachmentId &&
    left.backendInstanceId === right.backendInstanceId &&
    left.backendGeneration === right.backendGeneration &&
    left.adapter.adapterId === right.adapter.adapterId &&
    left.adapter.adapterGeneration === right.adapter.adapterGeneration
  )
}
export interface BackendIdentityBase<Attachment extends string, Axes extends CoreVersionAxes> {
  readonly registeredBackendId: string
  readonly registeredPlatformId: string
  readonly attachment: AttachmentRecord<Attachment>
  readonly versions: Axes
  readonly runtime: BackendRuntimeMetadata
}
export type HostNeutralBackendIdentity<Attachment extends string> = BackendIdentityBase<
  Attachment,
  HostNeutralVersionAxes
>
export type NativeBackendIdentity<Attachment extends string> = BackendIdentityBase<Attachment, NativeVersionAxes>
export type IpcBackendIdentity<Attachment extends string> = BackendIdentityBase<Attachment, IpcVersionAxes>
export type BackendIdentity<Attachment extends string> =
  | HostNeutralBackendIdentity<Attachment>
  | NativeBackendIdentity<Attachment>
  | IpcBackendIdentity<Attachment>
export interface ProviderDescriptor {
  readonly providerId: string
  readonly hostKind: HostKind
  readonly loadability: 'loadable' | 'unavailable'
  readonly compatibility:
    | BackendCompatibilityOffer
    | import('./primitives').NativeCompatibilityOffer
    | import('./primitives').IpcCompatibilityOffer
}
export interface AdapterSelection<Attachment extends string> {
  readonly selectedAdapterId: AdapterId<Attachment>
}
export interface BackendProvider<Attachment extends string, Identity extends BackendIdentity<Attachment>> {
  readonly descriptor: ProviderDescriptor
  listAdapters(): Promise<readonly AdapterDescriptor<Attachment>[]>
  create(selection: AdapterSelection<Attachment>): Promise<import('./backend').BleCentralBackend<Attachment, Identity>>
}
