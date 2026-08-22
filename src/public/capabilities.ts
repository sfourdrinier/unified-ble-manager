// src/public/capabilities.ts — application capability projection

import { BleError } from './errors'
import { contractError } from '../backend-contract/errors'
import {
  snapshotCapabilityDescriptor,
  validateCapabilitySnapshot,
  type BuiltInFeatureId,
  type CapabilityDescriptor,
  type FeatureId
} from '../backend-contract/capabilities'
import type { IpcCapabilitySnapshotV2 } from '../ipc/protocol'

interface CapabilitySource {
  capability(id: FeatureId): CapabilityDescriptor | null
  capabilities(): readonly CapabilityDescriptor[]
}

export interface BleCapabilities {
  supports(id: BuiltInFeatureId): boolean
  /** Returns undefined for an unknown extension identifier. */
  get(id: FeatureId): CapabilityDescriptor | undefined
  require(id: BuiltInFeatureId): CapabilityDescriptor
  list(): readonly CapabilityDescriptor[]
}

export function assertDirectConnectionCapability(
  descriptor: CapabilityDescriptor | null | undefined,
  operation: string
): void {
  if (descriptor === undefined || descriptor === null || descriptor.state === 'unsupported') {
    throw contractError('capability.unsupported', 'connection', operation)
  }
  if (descriptor.state === 'unavailable') {
    throw contractError('capability.unavailable', 'connection', operation)
  }
}

export class PublicBleCapabilities implements BleCapabilities {
  constructor(private readonly internal: CapabilitySource) {}

  supports(id: BuiltInFeatureId): boolean {
    return this.get(id)?.state === 'supported'
  }

  get(id: FeatureId): CapabilityDescriptor | undefined {
    return this.internal.capability(id) ?? undefined
  }

  require(id: BuiltInFeatureId): CapabilityDescriptor {
    const descriptor = this.get(id)
    if (descriptor === undefined) {
      throw new BleError('capability.unsupported', 'capability', `ble-capabilities.require.${id}`)
    }
    if (descriptor.state === 'limited') {
      throw new BleError('capability.limited', 'capability', `ble-capabilities.require.${id}`, {
        limitations: descriptor.limitations
      })
    }
    if (descriptor.state === 'unsupported') {
      throw new BleError('capability.unsupported', 'capability', `ble-capabilities.require.${id}`, {
        limitations: descriptor.limitations
      })
    }
    if (descriptor.state === 'unavailable') {
      throw new BleError('capability.unavailable', 'capability', `ble-capabilities.require.${id}`, {
        limitations: descriptor.limitations
      })
    }
    return descriptor
  }

  list(): readonly CapabilityDescriptor[] {
    return this.internal.capabilities()
  }
}

/** Projects a complete trusted-host snapshot without reconstructing evidence or TCK data. */
export function createPublicBleCapabilities(
  snapshot: IpcCapabilitySnapshotV2,
  expectedBackendGeneration: string,
  requireCatalogComplete = false
): PublicBleCapabilities {
  validateCapabilitySnapshot(snapshot, expectedBackendGeneration, requireCatalogComplete)
  const descriptors = new Map<string, CapabilityDescriptor>()
  for (const descriptor of snapshot.descriptors) {
    descriptors.set(descriptor.id, snapshotCapabilityDescriptor(descriptor))
  }
  const values = Object.freeze([...descriptors.values()])
  return new PublicBleCapabilities({
    capability: id => values.find(descriptor => descriptor.id === id) ?? null,
    capabilities: () => values
  })
}

export type { BuiltInFeatureId, CapabilityDescriptor, FeatureId } from '../backend-contract/capabilities'
