// src/public/capabilities.ts — application capability projection

import { BleError } from './errors'
import type { BuiltInFeatureId, CapabilityDescriptor, FeatureId } from '../backend-contract/capabilities'

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

/** Explicit fail-closed projection used until a host supplies its capability snapshot. */
export class UnavailableBleCapabilities implements BleCapabilities {
  supports(_id: BuiltInFeatureId): boolean {
    return false
  }

  get(_id: FeatureId): CapabilityDescriptor | undefined {
    return undefined
  }

  require(id: BuiltInFeatureId): CapabilityDescriptor {
    throw new BleError('capability.unavailable', 'capability', `ble-capabilities.require.${id}`)
  }

  list(): readonly CapabilityDescriptor[] {
    return []
  }
}

export type { BuiltInFeatureId, CapabilityDescriptor, FeatureId } from '../backend-contract/capabilities'
