import type { SerializableRecord } from '../backend-contract/primitives'

// This brand never crosses IPC. ElectronRendererBleClient installs it only
// after accepting an event for its authenticated renderer lease; generic IPC
// clients and backend SDKs cannot mint it by adding a similarly named field.
const authenticatedStageFailures = new WeakSet<SerializableRecord>()

export function markAuthenticatedElectronTerminalEvidenceStageFailure(item: SerializableRecord): void {
  authenticatedStageFailures.add(item)
}

export function isAuthenticatedElectronTerminalEvidenceStageFailure(item: SerializableRecord): boolean {
  return authenticatedStageFailures.has(item)
}
