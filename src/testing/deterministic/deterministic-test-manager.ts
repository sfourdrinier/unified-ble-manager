import type { BackendCompatibilityOffer } from '../../backend-contract/primitives'
import { opaqueId, version, versionRange } from '../../backend-contract/primitives'
import type { AttachmentRecord } from '../../backend-contract/identity'
import {
  attachBleBackend,
  createBleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} from '../../manager/ble-manager'
import { createPublicBleManager, type BleManager } from '../../public/ble-manager'
import {
  createDeterministicTestBackend,
  type DeterministicBackendFixture,
  type DeterministicBackendOptions
} from './deterministic-test-backend'

const deterministicCompatibility: BackendCompatibilityOffer = Object.freeze({
  backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
  capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
  eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
  traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
})

export interface DeterministicTestBleManagerOptions {
  readonly backend?: DeterministicBackendOptions
  readonly now?: () => number
}

export interface DeterministicTestBleManager {
  readonly manager: BleManager
  readonly fixture: DeterministicBackendFixture
  readonly attachment: AttachmentRecord<string>
}

export async function createDeterministicTestBleManager(
  options: DeterministicTestBleManagerOptions = {}
): Promise<DeterministicTestBleManager> {
  const fixture = createDeterministicTestBackend(options.backend)
  const attached = await attachBleBackend(fixture.backend, deterministicCompatibility)
  const authority = createManagerOwnershipAuthority(attached)
  const now = options.now ?? (() => Number(fixture.controller.clock.now()))
  const internal = await createBleManager(
    {
      attachedBackend: attached,
      clientId: opaqueId('deterministic-test-client', 'client', 'testing:deterministic'),
      managerId: opaqueId('deterministic-test-manager', 'manager', 'testing:deterministic'),
      ownerMode: 'owning'
    },
    authority,
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now }
  )
  return Object.freeze({
    manager: await createPublicBleManager(internal, now),
    fixture,
    attachment: attached.attachment.attachment
  })
}
