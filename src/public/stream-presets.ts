// src/public/stream-presets.ts

import { capacity, type Capacity } from '../backend-contract/primitives'
import type { OverflowPolicy } from '../backend-contract/streams'

export type StreamPreset = 'latest' | 'balanced' | 'lossless-bounded' | 'custom'

export interface StreamBudget {
  readonly itemCapacity: Capacity
  readonly byteCapacity: Capacity
  readonly reservedControlCapacity: Capacity
  readonly overflowPolicy: OverflowPolicy
}

export interface StreamPresetInput {
  readonly preset?: StreamPreset
  readonly custom?: Partial<StreamBudget> & { readonly overflowPolicy?: OverflowPolicy }
}

/**
 * Maps a public stream preset to exact bounded capacities.
 *
 * - `latest`: UI/status where newest value matters (bounded drop-oldest)
 * - `balanced`: normal scan/sensor notifications (bounded drop-oldest with visible notice)
 * - `lossless-bounded`: command/response that must fail rather than drop (terminal overflow)
 * - `custom`: caller-supplied budgets via `/advanced`
 */
export function resolveStreamPreset(input: StreamPresetInput = {}): StreamBudget {
  const preset = input.preset ?? 'balanced'
  switch (preset) {
    case 'latest':
      return Object.freeze({
        itemCapacity: capacity(1),
        byteCapacity: capacity(4 * 1024),
        reservedControlCapacity: capacity(2),
        overflowPolicy: 'drop-oldest' as const
      })
    case 'balanced':
      return Object.freeze({
        itemCapacity: capacity(32),
        byteCapacity: capacity(16 * 1024),
        reservedControlCapacity: capacity(2),
        overflowPolicy: 'drop-oldest' as const
      })
    case 'lossless-bounded':
      return Object.freeze({
        itemCapacity: capacity(64),
        byteCapacity: capacity(64 * 1024),
        reservedControlCapacity: capacity(2),
        overflowPolicy: 'error' as const
      })
    case 'custom': {
      const custom = input.custom
      if (custom === undefined || custom.itemCapacity === undefined || custom.byteCapacity === undefined) {
        throw new Error('custom stream preset requires itemCapacity and byteCapacity')
      }
      return Object.freeze({
        itemCapacity: custom.itemCapacity,
        byteCapacity: custom.byteCapacity,
        reservedControlCapacity: custom.reservedControlCapacity ?? capacity(2),
        overflowPolicy: custom.overflowPolicy ?? 'drop-oldest'
      })
    }
    default:
      throw new Error(`unknown stream preset: ${String(preset)}`)
  }
}

export const STREAM_PRESET_DEFAULTS = Object.freeze({
  scan: 'balanced' as StreamPreset,
  notification: 'balanced' as StreamPreset,
  indication: 'lossless-bounded' as StreamPreset
})
