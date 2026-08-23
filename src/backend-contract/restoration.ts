// src/backend-contract/restoration.ts

import { contractError } from './errors'
import type {
  AttachmentId,
  BackendInstanceId,
  ClientId,
  GenerationId,
  NativeVersionAxes,
  PeerId,
  SerializableRecord
} from './primitives'

export const RESTORATION_DERIVATION_DOMAIN = 'ubm-restoration-v1'
export const DEFAULT_RESTORATION_GENERATION = '1'

const restorationTokenPattern = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/

export interface RestorationBootstrapRequest {
  readonly restorationId: string
  readonly generation: string
}

/** Values derived and returned by the trusted native host before attachment open. */
export interface NativeRestorationBootstrapIdentity {
  readonly applicationId: string
  readonly restorationId: string
  readonly generation: string
  readonly restoreIdentifier: string
  readonly namespaceValue: string
  readonly clientId: string
  readonly hostSessionScope: string
}

export function normalizeRestorationBootstrapRequest(input: {
  readonly restorationId: string
  readonly generation?: string
}): RestorationBootstrapRequest {
  const keys = Object.keys(input)
  if (keys.some(key => key !== 'restorationId' && key !== 'generation')) {
    throw contractError('argument.invalid', 'restoration', 'restoration.bootstrap.unknown-key')
  }
  const restorationId = requiredRestorationToken(input.restorationId, 'restoration.bootstrap.restoration-id')
  const generation =
    input.generation === undefined
      ? DEFAULT_RESTORATION_GENERATION
      : requiredRestorationToken(input.generation, 'restoration.bootstrap.generation', 64)
  return Object.freeze({ restorationId, generation })
}

function requiredRestorationToken(value: string, label: string, maximumLength = 128): string {
  if (
    typeof value !== 'string' ||
    value.length === 0 ||
    value.length > maximumLength ||
    !restorationTokenPattern.test(value)
  ) {
    throw contractError('argument.invalid', 'restoration', label)
  }
  return value
}

export interface RestorationJournalRecord<Attachment extends string> {
  readonly recordVersion: number
  readonly namespace: string
  readonly attachmentId: AttachmentId<Attachment>
  readonly backendInstanceId: BackendInstanceId<Attachment>
  readonly backendGeneration: GenerationId<'backend-generation', Attachment>
  readonly ordinal: number
  readonly adoptionEpoch: GenerationId<'restoration-epoch', Attachment>
  readonly kind: 'adapter' | 'connection' | 'subscription' | 'event'
  readonly peerId: PeerId<Attachment> | null
  readonly payload: SerializableRecord
}
export interface RestorationJournal<Attachment extends string> {
  readonly records: readonly RestorationJournalRecord<Attachment>[]
  readonly capacity: number
  readonly byteCapacity: number
  readonly overflow: 'reject-restoration' | 'drop-oldest-with-notice'
}
export interface RestorationAdoptionRequest<Attachment extends string> {
  readonly namespace: string
  readonly attachmentId: AttachmentId<Attachment>
  readonly expectedBackendInstanceId: BackendInstanceId<Attachment>
  readonly expectedEpoch: GenerationId<'restoration-epoch', Attachment>
  readonly expectedVersions: NativeVersionAxes
}
export interface AuthenticatedRestorationClient<Attachment extends string> {
  readonly clientId: ClientId<Attachment, string>
  readonly hostSessionScope: string
}
export interface RestorationAdoptionResult<Attachment extends string> {
  readonly attachmentId: AttachmentId<Attachment>
  readonly receiptId: string | null
  readonly namespace: string
  readonly boundClientId: ClientId<Attachment, string> | null
  readonly adoptionEpoch: GenerationId<'restoration-epoch', Attachment> | null
  readonly outcome:
    | 'adopted'
    | 'already-consumed'
    | 'attachment-mismatch'
    | 'backend-mismatch'
    | 'namespace-mismatch'
    | 'epoch-mismatch'
  readonly replayedRecords: readonly RestorationJournalRecord<Attachment>[]
}
export interface ProviderRestorationAuthority<Attachment extends string> {
  lookup(
    client: AuthenticatedRestorationClient<Attachment>,
    request: RestorationAdoptionRequest<Attachment>
  ): Promise<RestorationJournal<Attachment> | null>
  consume(
    client: AuthenticatedRestorationClient<Attachment>,
    request: RestorationAdoptionRequest<Attachment>,
    result: RestorationAdoptionResult<Attachment>
  ): Promise<void>
}
export interface RestorationCoordinator<Attachment extends string> {
  adopt(
    client: AuthenticatedRestorationClient<Attachment>,
    request: RestorationAdoptionRequest<Attachment>
  ): Promise<RestorationAdoptionResult<Attachment>>
}
/** Provider-owned restoration authority that one constructed manager can invoke. */
export interface ManagerRestorationCapability<Attachment extends string> {
  readonly client: AuthenticatedRestorationClient<Attachment>
  readonly coordinator: RestorationCoordinator<Attachment>
}
